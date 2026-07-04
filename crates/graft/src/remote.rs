use std::{env, future, ops::Range, time::Duration};

use crate::core::{LogId, SegmentId, cbe::CBE64, commit::Commit, lsn::LSN};
use bilrost::{Message, OwnedMessage};
use bytes::Bytes;
use futures::{
    Stream, StreamExt, TryStreamExt,
    stream::{self, FuturesOrdered},
};
use opendal::{
    ErrorKind, Operator,
    layers::{HttpClientLayer, RetryLayer},
    options::{ReadOptions, WriteOptions},
    raw::HttpClient,
    services::{Fs, Memory, S3},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod segment;

const REMOTE_CONCURRENCY: usize = 5;

enum RemotePath<'a> {
    /// Commits are stored at `/logs/{logid}/commits/{CBE64 hex LSN}`
    Commit(&'a LogId, LSN),

    /// Segments are stored at `/segments/{sid}`
    Segment(&'a SegmentId),
}

impl RemotePath<'_> {
    fn build(self) -> String {
        match self {
            Self::Commit(log, lsn) => format!(
                "logs/{}/commits/{}",
                &log.serialize(),
                &CBE64::from(lsn).to_string(),
            ),
            Self::Segment(sid) => format!("segments/{}", &sid.serialize()),
        }
    }
}

#[derive(Error, Debug)]
pub enum RemoteErr {
    #[error("Object store error: {0}")]
    ObjectStore(#[from] opendal::Error),

    #[error("HTTP client setup error: {0}")]
    SetupHttp(#[from] reqwest::Error),

    #[error("HTTP remote transport error: {0}")]
    HttpTransport(reqwest::Error),

    #[error("HTTP remote returned {status} for `{path}`: {message}")]
    HttpStatus {
        status: u16,
        path: String,
        message: String,
    },

    #[error("Failed to decode file: {0}")]
    Decode(#[from] bilrost::DecodeError),

    #[error("remote lock `{path}` is already held")]
    LockBusy { path: String },

    #[error("remote object `{path}` changed during compare-and-swap")]
    CompareAndSwap { path: String },
}

impl RemoteErr {
    fn objectstore_err_kind(&self) -> Option<opendal::ErrorKind> {
        if let RemoteErr::ObjectStore(err) = self {
            Some(err.kind())
        } else {
            None
        }
    }

    pub fn precondition_failed(&self) -> bool {
        matches!(
            self.objectstore_err_kind(),
            Some(opendal::ErrorKind::ConditionNotMatch)
        ) || matches!(self, RemoteErr::HttpStatus { status: 412, .. })
    }

    pub fn is_not_found(&self) -> bool {
        matches!(
            self.objectstore_err_kind(),
            Some(opendal::ErrorKind::NotFound)
        ) || matches!(self, RemoteErr::HttpStatus { status: 404, .. })
    }
}

pub type Result<T> = std::result::Result<T, RemoteErr>;

#[derive(Debug, Deserialize, Serialize, Default, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteConfig {
    /// In memory object store
    #[default]
    Memory,

    /// On disk object store
    Fs { root: String },

    /// S3 compatible object store.
    S3Compatible {
        bucket: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        endpoint: Option<String>,
    },

    /// Graft HTTP protocol remote served by a Worker or compatible service.
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_env: Option<String>,
    },
}

impl RemoteConfig {
    pub fn build(self) -> Result<Remote> {
        Remote::with_config(self)
    }
}

#[derive(Debug, Clone)]
pub struct Remote {
    backend: RemoteBackend,
}

#[derive(Debug, Clone)]
enum RemoteBackend {
    ObjectStore(Operator),
    Http(HttpRemote),
}

#[derive(Debug, Clone)]
struct HttpRemote {
    client: reqwest::Client,
    url: String,
    token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HttpListResponse {
    paths: Vec<String>,
}

impl Remote {
    pub fn with_config(config: RemoteConfig) -> Result<Self> {
        let backend = match config {
            RemoteConfig::Memory => Operator::new(Memory::default())?.finish(),
            RemoteConfig::Fs { root } => Operator::new(Fs::default().root(&root))?.finish(),
            RemoteConfig::S3Compatible { bucket, prefix, endpoint } => {
                let mut builder = S3::default().bucket(&bucket);
                if let Some(prefix) = prefix {
                    builder = builder.root(&prefix);
                }
                if let Some(endpoint) = endpoint {
                    builder = builder.endpoint(&endpoint);
                }
                let client = reqwest::ClientBuilder::new()
                    // use http1 to maximize throughput
                    // http2 routes all requests through a single connection
                    .http1_only()
                    // enable hickory DNS resolver for DNS caching
                    .hickory_dns(true)
                    .connect_timeout(Duration::from_secs(5))
                    // .tcp_user_timeout(Duration::from_secs(60))
                    .build()?;

                Operator::new(builder)?
                    .layer(HttpClientLayer::new(HttpClient::with(client)))
                    .layer(RetryLayer::new())
                    .finish()
            }
            RemoteConfig::Http { url, token_env } => {
                return Ok(Self {
                    backend: RemoteBackend::Http(HttpRemote::new(url, token_env)?),
                });
            }
        };

        Ok(Self {
            backend: RemoteBackend::ObjectStore(backend),
        })
    }

    /// Streams commits by LSN in the same order as the input iterator.
    /// Stops fetching commits as soon as we receive a `NotFound` error from the
    /// remote, thus even if `lsns` contains every LSN we will stop loading
    /// commits as soon as we reach the end of the log.
    pub fn stream_commits_ordered<I: IntoIterator<Item = LSN>>(
        &self,
        log: &LogId,
        lsns: I,
    ) -> impl Stream<Item = Result<Commit>> {
        // convert the set into a stream of chunks, such that the first chunk
        // only contains the first LSN, and the remaining chunks have a maximum
        // size of REPLAY_CONCURRENCY
        let mut lsns = lsns.into_iter();
        let first_chunk: Vec<LSN> = match lsns.next() {
            Some(lsn) => vec![lsn],
            None => vec![],
        };
        stream::once(future::ready(first_chunk))
            .chain(stream::iter(lsns).chunks(REMOTE_CONCURRENCY))
            .flat_map(|chunk| {
                chunk
                    .into_iter()
                    .map(|lsn| self.get_commit(log, lsn))
                    .collect::<FuturesOrdered<_>>()
            })
            .try_take_while(|result| future::ready(Ok(result.is_some())))
            .map_ok(|result| result.unwrap())
    }

    /// Fetches a single commit, returning None if the commit is not found.
    #[tracing::instrument(level = "trace", err(level = "debug"), skip(self))]
    pub async fn get_commit(&self, log: &LogId, lsn: LSN) -> Result<Option<Commit>> {
        let path = RemotePath::Commit(log, lsn).build();
        match &self.backend {
            RemoteBackend::ObjectStore(store) => match store.read(&path).await {
                Ok(res) => Ok(Some(Commit::decode(res)?)),
                Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
                Err(err) => Err(err.into()),
            },
            RemoteBackend::Http(remote) => Ok(remote
                .get_raw(&path)
                .await?
                .map(Commit::decode)
                .transpose()?),
        }
    }

    /// Atomically write a commit to the remote, returning
    /// `RemoteErr::ObjectStore(Error::AlreadyExists)` on a collision
    #[tracing::instrument(level = "debug", err(level = "debug"), skip(self, commit),
        fields(log = %commit.log, lsn = %commit.lsn, sid = ?commit.segment_id())
    )]
    pub async fn put_commit(&self, commit: &Commit) -> Result<()> {
        let path = RemotePath::Commit(commit.log(), commit.lsn()).build();
        match &self.backend {
            RemoteBackend::ObjectStore(store) => {
                store
                    .write_options(
                        &path,
                        commit.encode_to_bytes(),
                        WriteOptions {
                            // Perform an atomic write operation, returning
                            // a precondition error if the commit already exists
                            if_not_exists: true,
                            concurrent: REMOTE_CONCURRENCY,
                            ..WriteOptions::default()
                        },
                    )
                    .await?;
            }
            RemoteBackend::Http(remote) => {
                remote
                    .put_raw_if_not_exists(&path, commit.encode_to_bytes())
                    .await?;
            }
        }
        Ok(())
    }

    /// Uploads a segment to this Remote
    #[tracing::instrument(
        level = "debug",
        err(level = "debug"),
        skip(self, chunks),
        fields(size)
    )]
    pub async fn put_segment<I: IntoIterator<Item = Bytes>>(
        &self,
        sid: &SegmentId,
        chunks: I,
    ) -> Result<()> {
        let path = RemotePath::Segment(sid).build();
        if let RemoteBackend::Http(remote) = &self.backend {
            match remote.put_raw_if_not_exists_stream(&path, chunks).await {
                Ok(()) => return Ok(()),
                Err(err) if err.precondition_failed() => return Ok(()),
                Err(err) => return Err(err),
            }
        }
        let RemoteBackend::ObjectStore(store) = &self.backend else {
            unreachable!("HTTP backend handled above");
        };
        let result: std::result::Result<(), opendal::Error> = async {
            let mut w = store
                .writer_with(&path)
                .if_not_exists(true)
                .concurrent(REMOTE_CONCURRENCY)
                .await?;
            let mut size = 0;
            for chunk in chunks {
                size += chunk.len();
                w.write(chunk).await?;
            }
            tracing::Span::current().record("size", size);
            w.close().await?;
            Ok(())
        }
        .await;

        match result {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == ErrorKind::ConditionNotMatch => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    /// Returns true if a segment already exists on this Remote.
    #[tracing::instrument(level = "trace", err(level = "debug"), skip(self))]
    pub async fn has_segment(&self, sid: &SegmentId) -> Result<bool> {
        let path = RemotePath::Segment(sid).build();
        match &self.backend {
            RemoteBackend::ObjectStore(store) => match store.stat(&path).await {
                Ok(_) => Ok(true),
                Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
                Err(err) => Err(err.into()),
            },
            RemoteBackend::Http(remote) => remote.has_raw(&path).await,
        }
    }

    /// Reads a byte range of a segment
    #[tracing::instrument(level = "debug", err(level = "debug"), skip(self))]
    pub async fn get_segment_range(&self, sid: &SegmentId, bytes: Range<u64>) -> Result<Bytes> {
        let path = RemotePath::Segment(sid).build();
        self.get_raw_range(&path, bytes).await
    }

    #[tracing::instrument(level = "trace", err(level = "debug"), skip(self))]
    pub async fn get_raw(&self, path: &str) -> Result<Option<Bytes>> {
        match &self.backend {
            RemoteBackend::ObjectStore(store) => match store.read(path).await {
                Ok(res) => Ok(Some(res.to_bytes())),
                Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
                Err(err) => Err(err.into()),
            },
            RemoteBackend::Http(remote) => remote.get_raw(path).await,
        }
    }

    #[tracing::instrument(level = "trace", err(level = "debug"), skip(self))]
    pub async fn get_raw_range(&self, path: &str, bytes: Range<u64>) -> Result<Bytes> {
        match &self.backend {
            RemoteBackend::ObjectStore(store) => {
                let buffer = store
                    .read_options(
                        path,
                        ReadOptions {
                            range: bytes.into(),
                            concurrent: REMOTE_CONCURRENCY,
                            ..ReadOptions::default()
                        },
                    )
                    .await?;
                Ok(buffer.to_bytes())
            }
            RemoteBackend::Http(remote) => remote.get_raw_range(path, bytes).await,
        }
    }

    #[tracing::instrument(level = "trace", err(level = "debug"), skip(self))]
    pub async fn list_raw(&self, prefix: &str) -> Result<Vec<String>> {
        match &self.backend {
            RemoteBackend::ObjectStore(store) => Ok(store
                .list_with(prefix)
                .recursive(true)
                .await?
                .into_iter()
                .filter(|entry| entry.metadata().is_file())
                .map(|entry| entry.path().to_string())
                .collect()),
            RemoteBackend::Http(remote) => remote.list_raw(prefix).await,
        }
    }

    #[tracing::instrument(level = "trace", err(level = "debug"), skip(self, bytes))]
    pub async fn put_raw(&self, path: &str, bytes: impl Into<Bytes>) -> Result<()> {
        match &self.backend {
            RemoteBackend::ObjectStore(store) => {
                store.write(path, bytes.into()).await?;
            }
            RemoteBackend::Http(remote) => remote.put_raw(path, bytes.into()).await?,
        }
        Ok(())
    }

    #[tracing::instrument(level = "trace", err(level = "debug"), skip(self))]
    pub async fn delete_raw(&self, path: &str) -> Result<()> {
        match &self.backend {
            RemoteBackend::ObjectStore(store) => {
                store.delete(path).await?;
            }
            RemoteBackend::Http(remote) => remote.delete_raw(path).await?,
        }
        Ok(())
    }

    #[tracing::instrument(level = "trace", err(level = "debug"), skip(self, bytes))]
    pub async fn put_raw_if_not_exists(&self, path: &str, bytes: impl Into<Bytes>) -> Result<()> {
        match &self.backend {
            RemoteBackend::ObjectStore(store) => {
                store
                    .write_options(
                        path,
                        bytes.into(),
                        WriteOptions {
                            if_not_exists: true,
                            concurrent: REMOTE_CONCURRENCY,
                            ..WriteOptions::default()
                        },
                    )
                    .await?;
            }
            RemoteBackend::Http(remote) => remote.put_raw_if_not_exists(path, bytes.into()).await?,
        }
        Ok(())
    }

    #[tracing::instrument(level = "trace", err(level = "debug"), skip(self, expected, bytes))]
    pub async fn compare_and_swap_raw(
        &self,
        path: &str,
        expected: Option<&[u8]>,
        bytes: impl Into<Bytes>,
    ) -> Result<()> {
        if let RemoteBackend::Http(remote) = &self.backend {
            return remote
                .compare_and_swap_raw(path, expected, bytes.into())
                .await;
        }
        let RemoteBackend::ObjectStore(store) = &self.backend else {
            unreachable!("HTTP backend handled above");
        };
        let bytes = bytes.into();
        let lock_path = remote_lock_path(path);
        match self
            .put_raw_if_not_exists(&lock_path, "graft-lock-v1\n")
            .await
        {
            Ok(()) => {}
            Err(err) if err.precondition_failed() => {
                return Err(RemoteErr::LockBusy { path: lock_path });
            }
            Err(err) => return Err(err),
        }

        let result = async {
            let current = self.get_raw(path).await?;
            if current.as_ref().map(Bytes::as_ref) != expected {
                return Err(RemoteErr::CompareAndSwap { path: path.to_string() });
            }
            store.write(path, bytes).await?;
            Ok(())
        }
        .await;

        let unlock = self.delete_raw(&lock_path).await;
        result?;

        match unlock {
            Ok(()) => Ok(()),
            Err(err) if err.is_not_found() => Ok(()),
            Err(err) => Err(err),
        }
    }

    #[tracing::instrument(level = "trace", err(level = "debug"), skip(self, expected))]
    pub async fn compare_and_delete_raw(&self, path: &str, expected: Option<&[u8]>) -> Result<()> {
        if let RemoteBackend::Http(remote) = &self.backend {
            return remote.compare_and_delete_raw(path, expected).await;
        }
        let lock_path = remote_lock_path(path);
        match self
            .put_raw_if_not_exists(&lock_path, "graft-lock-v1\n")
            .await
        {
            Ok(()) => {}
            Err(err) if err.precondition_failed() => {
                return Err(RemoteErr::LockBusy { path: lock_path });
            }
            Err(err) => return Err(err),
        }

        let result = async {
            let current = self.get_raw(path).await?;
            if current.as_ref().map(Bytes::as_ref) != expected {
                return Err(RemoteErr::CompareAndSwap { path: path.to_string() });
            }
            self.delete_raw(path).await
        }
        .await;

        let unlock = self.delete_raw(&lock_path).await;
        result?;

        match unlock {
            Ok(()) => Ok(()),
            Err(err) if err.is_not_found() => Ok(()),
            Err(err) => Err(err),
        }
    }

    /// TESTONLY: list contents of this remote in a tree-like format
    #[cfg(test)]
    pub async fn testonly_format_tree(&self) -> String {
        use itertools::Itertools;
        use std::collections::BTreeMap;
        use text_trees::{
            AnchorPosition, FormatCharacters, TreeFormatting, TreeNode, TreeOrientation,
        };

        let paths = match &self.backend {
            RemoteBackend::ObjectStore(store) => store
                .list("")
                .await
                .unwrap()
                .into_iter()
                .map(|entry| entry.path().split("/").map(|s| s.to_string()).collect_vec())
                .collect_vec(),
            RemoteBackend::Http(_) => self
                .list_raw("")
                .await
                .unwrap()
                .into_iter()
                .map(|path| path.split("/").map(|s| s.to_string()).collect_vec())
                .collect_vec(),
        };

        #[derive(Default)]
        struct TreeBuilder {
            children: BTreeMap<String, TreeBuilder>,
        }

        impl TreeBuilder {
            fn insert(&mut self, parts: &[String]) {
                if parts.is_empty() {
                    return;
                }

                let first = &parts[0];
                let rest = &parts[1..];

                self.children.entry(first.clone()).or_default().insert(rest);
            }

            fn into_tree_node(self, name: String) -> TreeNode<String> {
                if self.children.is_empty() {
                    // This is a leaf node
                    TreeNode::new(name)
                } else {
                    // This is a directory node
                    let child_nodes = self
                        .children
                        .into_iter()
                        .map(|(name, builder)| builder.into_tree_node(name));
                    TreeNode::with_child_nodes(name, child_nodes)
                }
            }
        }

        let mut root = TreeBuilder::default();
        for path in paths {
            root.insert(&path);
        }

        root.into_tree_node(format!("{:?}", self.backend))
            .to_string_with_format(&TreeFormatting {
                prefix_str: None,
                orientation: TreeOrientation::TopDown,
                anchor: AnchorPosition::Left,
                chars: FormatCharacters::box_chars(),
            })
            .unwrap()
    }
}

impl HttpRemote {
    fn new(url: String, token_env: Option<String>) -> Result<Self> {
        let client = reqwest::ClientBuilder::new()
            .http1_only()
            .hickory_dns(true)
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        let token = token_env
            .as_deref()
            .or(Some("GRAFT_REMOTE_TOKEN"))
            .and_then(|name| env::var(name).ok())
            .filter(|token| !token.is_empty());
        Ok(Self {
            client,
            url: url.trim_end_matches('/').to_string(),
            token,
        })
    }

    fn raw_url(&self, kind: &str, path: &str) -> String {
        format!("{}/{}/{}", self.url, kind, percent_encode_path(path))
    }

    fn list_url(&self, prefix: &str) -> String {
        format!(
            "{}/list?prefix={}",
            self.url,
            percent_encode_component(prefix)
        )
    }

    fn request(&self, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
        let request = self.client.request(method, url);
        if let Some(token) = &self.token {
            request.bearer_auth(token)
        } else {
            request
        }
    }

    async fn check_response(response: reqwest::Response, path: &str) -> Result<reqwest::Response> {
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let message = response
            .text()
            .await
            .unwrap_or_else(|err| format!("failed to read error body: {err}"));
        match status {
            409 => Err(RemoteErr::CompareAndSwap { path: path.to_string() }),
            423 => Err(RemoteErr::LockBusy { path: path.to_string() }),
            _ => Err(RemoteErr::HttpStatus { status, path: path.to_string(), message }),
        }
    }

    async fn has_raw(&self, path: &str) -> Result<bool> {
        let response = self
            .request(reqwest::Method::HEAD, self.raw_url("raw", path))
            .send()
            .await
            .map_err(RemoteErr::HttpTransport)?;
        if response.status().as_u16() == 404 {
            return Ok(false);
        }
        Self::check_response(response, path).await?;
        Ok(true)
    }

    async fn get_raw(&self, path: &str) -> Result<Option<Bytes>> {
        let response = self
            .request(reqwest::Method::GET, self.raw_url("raw", path))
            .send()
            .await
            .map_err(RemoteErr::HttpTransport)?;
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        let response = Self::check_response(response, path).await?;
        Ok(Some(
            response.bytes().await.map_err(RemoteErr::HttpTransport)?,
        ))
    }

    async fn get_raw_range(&self, path: &str, range: Range<u64>) -> Result<Bytes> {
        let end = range
            .end
            .checked_sub(1)
            .ok_or_else(|| RemoteErr::HttpStatus {
                status: 416,
                path: path.to_string(),
                message: "empty byte range".to_string(),
            })?;
        let response = self
            .request(reqwest::Method::GET, self.raw_url("raw", path))
            .header(
                reqwest::header::RANGE,
                format!("bytes={}-{}", range.start, end),
            )
            .send()
            .await
            .map_err(RemoteErr::HttpTransport)?;
        let response = Self::check_response(response, path).await?;
        response.bytes().await.map_err(RemoteErr::HttpTransport)
    }

    async fn list_raw(&self, prefix: &str) -> Result<Vec<String>> {
        let response = self
            .request(reqwest::Method::GET, self.list_url(prefix))
            .send()
            .await
            .map_err(RemoteErr::HttpTransport)?;
        let response = Self::check_response(response, prefix).await?;
        let bytes = response.bytes().await.map_err(RemoteErr::HttpTransport)?;
        let list: HttpListResponse =
            serde_json::from_slice(&bytes).map_err(|err| RemoteErr::HttpStatus {
                status: 502,
                path: prefix.to_string(),
                message: format!("invalid list response JSON: {err}"),
            })?;
        Ok(list.paths)
    }

    async fn put_raw(&self, path: &str, bytes: Bytes) -> Result<()> {
        let response = self
            .request(reqwest::Method::PUT, self.raw_url("raw", path))
            .body(bytes)
            .send()
            .await
            .map_err(RemoteErr::HttpTransport)?;
        Self::check_response(response, path).await?;
        Ok(())
    }

    async fn put_raw_if_not_exists(&self, path: &str, bytes: Bytes) -> Result<()> {
        let response = self
            .request(
                reqwest::Method::PUT,
                self.raw_url("raw-if-not-exists", path),
            )
            .body(bytes)
            .send()
            .await
            .map_err(RemoteErr::HttpTransport)?;
        Self::check_response(response, path).await?;
        Ok(())
    }

    async fn put_raw_if_not_exists_stream<I: IntoIterator<Item = Bytes>>(
        &self,
        path: &str,
        chunks: I,
    ) -> Result<()> {
        let chunks = chunks.into_iter().collect::<Vec<_>>();
        let body = reqwest::Body::wrap_stream(stream::iter(
            chunks.into_iter().map(Ok::<Bytes, std::io::Error>),
        ));
        let response = self
            .request(
                reqwest::Method::PUT,
                self.raw_url("raw-if-not-exists", path),
            )
            .body(body)
            .send()
            .await
            .map_err(RemoteErr::HttpTransport)?;
        Self::check_response(response, path).await?;
        Ok(())
    }

    async fn delete_raw(&self, path: &str) -> Result<()> {
        let response = self
            .request(reqwest::Method::DELETE, self.raw_url("raw", path))
            .send()
            .await
            .map_err(RemoteErr::HttpTransport)?;
        Self::check_response(response, path).await?;
        Ok(())
    }

    async fn compare_and_swap_raw(
        &self,
        path: &str,
        expected: Option<&[u8]>,
        bytes: Bytes,
    ) -> Result<()> {
        let response = self
            .request(reqwest::Method::POST, self.raw_url("cas", path))
            .header("x-graft-expected-present", expected.is_some().to_string())
            .header(
                "x-graft-expected-hex",
                expected.map(hex_encode).unwrap_or_default(),
            )
            .body(bytes)
            .send()
            .await
            .map_err(RemoteErr::HttpTransport)?;
        Self::check_response(response, path).await?;
        Ok(())
    }

    async fn compare_and_delete_raw(&self, path: &str, expected: Option<&[u8]>) -> Result<()> {
        let response = self
            .request(reqwest::Method::POST, self.raw_url("cad", path))
            .header("x-graft-expected-present", expected.is_some().to_string())
            .header(
                "x-graft-expected-hex",
                expected.map(hex_encode).unwrap_or_default(),
            )
            .send()
            .await
            .map_err(RemoteErr::HttpTransport)?;
        Self::check_response(response, path).await?;
        Ok(())
    }
}

fn percent_encode_path(path: &str) -> String {
    path.split('/')
        .map(percent_encode_component)
        .collect::<Vec<_>>()
        .join("/")
}

fn percent_encode_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn remote_lock_path(path: &str) -> String {
    format!("locks/{path}.lock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_and_swap_raw_updates_only_when_expected_matches() {
        let remote = RemoteConfig::Memory.build().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();

        runtime.block_on(async {
            remote
                .compare_and_swap_raw("refs/heads/main", None, "a\n")
                .await
                .unwrap();
            assert_eq!(
                remote.get_raw("refs/heads/main").await.unwrap().unwrap(),
                Bytes::from_static(b"a\n")
            );

            assert!(matches!(
                remote
                    .compare_and_swap_raw("refs/heads/main", Some(b"wrong\n"), "b\n")
                    .await,
                Err(RemoteErr::CompareAndSwap { .. })
            ));
            assert_eq!(
                remote.get_raw("refs/heads/main").await.unwrap().unwrap(),
                Bytes::from_static(b"a\n")
            );

            remote
                .compare_and_swap_raw("refs/heads/main", Some(b"a\n"), "b\n")
                .await
                .unwrap();
            assert_eq!(
                remote.get_raw("refs/heads/main").await.unwrap().unwrap(),
                Bytes::from_static(b"b\n")
            );
        });
    }

    #[test]
    fn compare_and_swap_raw_releases_lock_after_failed_compare() {
        let remote = RemoteConfig::Memory.build().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();

        runtime.block_on(async {
            remote
                .compare_and_swap_raw("refs/heads/main", None, "a\n")
                .await
                .unwrap();
            assert!(matches!(
                remote
                    .compare_and_swap_raw("refs/heads/main", Some(b"stale\n"), "b\n")
                    .await,
                Err(RemoteErr::CompareAndSwap { .. })
            ));

            remote
                .compare_and_swap_raw("refs/heads/main", Some(b"a\n"), "b\n")
                .await
                .unwrap();
            assert_eq!(
                remote.get_raw("refs/heads/main").await.unwrap().unwrap(),
                Bytes::from_static(b"b\n")
            );
        });
    }

    #[test]
    fn compare_and_swap_raw_reports_busy_lock() {
        let remote = RemoteConfig::Memory.build().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();

        runtime.block_on(async {
            remote
                .put_raw_if_not_exists(&remote_lock_path("refs/heads/main"), "held\n")
                .await
                .unwrap();

            assert!(matches!(
                remote
                    .compare_and_swap_raw("refs/heads/main", None, "a\n")
                    .await,
                Err(RemoteErr::LockBusy { .. })
            ));
            assert!(remote.get_raw("refs/heads/main").await.unwrap().is_none());
        });
    }
}
