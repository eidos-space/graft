use std::{
    collections::{HashMap, HashSet},
    env, fmt, future,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

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
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::Zeroizing;

pub mod segment;

const REMOTE_CONCURRENCY: usize = 5;
const GRAFT_PROTOCOL_HEADER: &str = "Graft-Protocol";
const GRAFT_PROTOCOL_VERSION: &str = "1";
const GRAFT_REQUEST_ID_HEADER: &str = "X-Graft-Request-Id";
const RECEIVE_PACK_HEADER_PACK_ID: &str = "x-graft-pack-id";
const RECEIVE_PACK_HEADER_PACK_BYTES: &str = "x-graft-pack-bytes";
const RECEIVE_PACK_HEADER_INDEX_BYTES: &str = "x-graft-index-bytes";
const RECEIVE_PACK_HEADER_REPLACEMENT_HEX: &str = "x-graft-ref-replacement-hex";
const RECEIVE_BUNDLE_HEADER_MANIFEST_BYTES: &str = "x-graft-bundle-manifest-bytes";
static HTTP_REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

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

    #[error(
        "HTTP remote protocol mismatch for `{path}`: expected response header `Graft-Protocol: {expected}`, received {received:?}"
    )]
    HttpProtocolMismatch {
        path: String,
        expected: &'static str,
        received: Option<String>,
    },

    #[error("Failed to decode file: {0}")]
    Decode(#[from] bilrost::DecodeError),

    #[error("remote lock `{path}` is already held")]
    LockBusy { path: String },

    #[error("remote object `{path}` changed during compare-and-swap")]
    CompareAndSwap { path: String },
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum RemoteCredentialErr {
    #[error("remote credential name must be a non-empty repository remote name")]
    InvalidRemoteName,

    #[error("HTTP bearer token must not be empty")]
    EmptyBearerToken,

    #[error("environment-backed remote credentials cannot be changed")]
    EnvironmentCredentialsReadOnly,
}

#[derive(Clone)]
struct BearerToken(Zeroizing<String>);

impl BearerToken {
    fn new(token: String) -> Self {
        Self(Zeroizing::new(token))
    }

    fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted bearer token]")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteCredentialMode {
    Environment,
    Explicit,
}

/// In-memory credentials used when constructing repository remotes.
///
/// [`Self::explicit`] never reads process environment variables. Clones share the same protected
/// store so a long-lived repository session can rotate or clear a bearer token without writing it
/// into repository config.
#[derive(Clone)]
pub struct RemoteCredentials {
    mode: RemoteCredentialMode,
    http_bearer_tokens: Arc<RwLock<HashMap<String, BearerToken>>>,
    http_client: Arc<RwLock<Option<reqwest::Client>>>,
    http_upload_client: Arc<RwLock<Option<reqwest::Client>>>,
}

impl fmt::Debug for RemoteCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteCredentials")
            .field("mode", &self.mode)
            .field(
                "http_bearer_token_count",
                &self.http_bearer_tokens.read().len(),
            )
            .field(
                "http_client_initialized",
                &self.http_client.read().is_some(),
            )
            .field(
                "http_upload_client_initialized",
                &self.http_upload_client.read().is_some(),
            )
            .finish()
    }
}

impl Default for RemoteCredentials {
    fn default() -> Self {
        Self::explicit()
    }
}

impl RemoteCredentials {
    /// Creates an explicit-only credential store that never falls back to process environment.
    pub fn explicit() -> Self {
        Self {
            mode: RemoteCredentialMode::Explicit,
            http_bearer_tokens: Arc::default(),
            http_client: Arc::default(),
            http_upload_client: Arc::default(),
        }
    }

    /// Creates the legacy CLI credential policy that reads the configured token environment
    /// variable, falling back to `GRAFT_REMOTE_TOKEN`.
    ///
    /// Embedders should use [`Self::explicit`] instead.
    pub fn environment() -> Self {
        Self {
            mode: RemoteCredentialMode::Environment,
            http_bearer_tokens: Arc::default(),
            http_client: Arc::default(),
            http_upload_client: Arc::default(),
        }
    }

    /// Sets a bearer token for one repository remote name, such as `origin`.
    pub fn set_http_bearer_token(
        &self,
        remote_name: &str,
        token: String,
    ) -> std::result::Result<(), RemoteCredentialErr> {
        if self.mode != RemoteCredentialMode::Explicit {
            return Err(RemoteCredentialErr::EnvironmentCredentialsReadOnly);
        }
        if !valid_credential_remote_name(remote_name) {
            return Err(RemoteCredentialErr::InvalidRemoteName);
        }
        if token.is_empty() {
            return Err(RemoteCredentialErr::EmptyBearerToken);
        }
        self.http_bearer_tokens
            .write()
            .insert(remote_name.to_string(), BearerToken::new(token));
        Ok(())
    }

    /// Clears the bearer token for one repository remote name.
    pub fn clear_http_bearer_token(
        &self,
        remote_name: &str,
    ) -> std::result::Result<(), RemoteCredentialErr> {
        if self.mode != RemoteCredentialMode::Explicit {
            return Err(RemoteCredentialErr::EnvironmentCredentialsReadOnly);
        }
        if !valid_credential_remote_name(remote_name) {
            return Err(RemoteCredentialErr::InvalidRemoteName);
        }
        self.http_bearer_tokens.write().remove(remote_name);
        Ok(())
    }

    /// Redacts every bearer token held by this store from an error or diagnostic message.
    pub fn redact(&self, message: &str) -> String {
        self.http_bearer_tokens
            .read()
            .values()
            .fold(message.to_string(), |redacted, token| {
                redacted.replace(token.expose(), "[redacted]")
            })
    }

    /// Starts the next top-level repository command with fresh HTTP connection pools.
    ///
    /// Remotes already built from this store retain their clients, so requests within one command
    /// still reuse connections. Repository sessions call this between commands because some
    /// HTTP/1.1 proxies leave otherwise idle pooled connections unusable.
    pub fn reset_http_clients(&self) {
        *self.http_client.write() = None;
        *self.http_upload_client.write() = None;
    }

    fn http_bearer_token(&self, remote_name: &str, token_env: Option<&str>) -> Option<BearerToken> {
        match self.mode {
            RemoteCredentialMode::Explicit => {
                self.http_bearer_tokens.read().get(remote_name).cloned()
            }
            RemoteCredentialMode::Environment => token_env
                .or(Some("GRAFT_REMOTE_TOKEN"))
                .and_then(|name| env::var(name).ok())
                .filter(|token| !token.is_empty())
                .map(BearerToken::new),
        }
    }

    fn http_client(&self) -> Result<reqwest::Client> {
        if let Some(client) = self.http_client.read().clone() {
            return Ok(client);
        }

        let candidate = build_http_client()?;
        let mut client = self.http_client.write();
        Ok(client.get_or_insert(candidate).clone())
    }

    fn http_upload_client(&self) -> Result<reqwest::Client> {
        if let Some(client) = self.http_upload_client.read().clone() {
            return Ok(client);
        }

        let candidate = build_http_client()?;
        let mut client = self.http_upload_client.write();
        Ok(client.get_or_insert(candidate).clone())
    }
}

fn build_http_client() -> std::result::Result<reqwest::Client, reqwest::Error> {
    reqwest::ClientBuilder::new()
        // HTTP/2 request-body multiplexing stalls behind common local proxies.
        // Reuse dedicated read and mutation HTTP/1.1 pools across the entire
        // repository session so proxies never mix PUT bodies into read streams.
        .http1_only()
        .hickory_dns(true)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()
}

fn valid_credential_remote_name(remote_name: &str) -> bool {
    !remote_name.is_empty()
        && !remote_name
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        && !remote_name.contains(['/', '\\'])
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

    /// Builds a remote using only the supplied credential policy.
    pub fn build_with_credentials(
        self,
        remote_name: &str,
        credentials: &RemoteCredentials,
    ) -> Result<Remote> {
        Remote::with_config_and_credentials(self, remote_name, credentials)
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
    upload_client: reqwest::Client,
    url: String,
    token: Option<BearerToken>,
}

#[derive(Debug, Deserialize)]
struct HttpListResponse {
    paths: Vec<String>,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Debug)]
pub(crate) struct RemoteObjectPack {
    id: String,
    pack_path: String,
    pack: Bytes,
    index_path: String,
    index: Bytes,
}

#[derive(Debug)]
pub(crate) struct RemoteBundleObject {
    path: String,
    chunks: Vec<Bytes>,
    content_length: usize,
    allow_existing: bool,
}

impl RemoteBundleObject {
    pub(crate) fn new(
        path: String,
        chunks: impl IntoIterator<Item = Bytes>,
        allow_existing: bool,
    ) -> Result<Self> {
        let chunks = chunks.into_iter().collect::<Vec<_>>();
        let content_length = chunks.iter().try_fold(0_usize, |total, chunk| {
            total
                .checked_add(chunk.len())
                .ok_or_else(|| RemoteErr::HttpStatus {
                    status: 413,
                    path: path.clone(),
                    message: "bundled object length exceeds usize".to_string(),
                })
        })?;
        Ok(Self {
            path,
            chunks,
            content_length,
            allow_existing,
        })
    }

    fn bytes(&self) -> Bytes {
        if self.chunks.len() == 1 {
            return self.chunks[0].clone();
        }
        let mut bytes = Vec::with_capacity(self.content_length);
        for chunk in &self.chunks {
            bytes.extend_from_slice(chunk);
        }
        Bytes::from(bytes)
    }
}

#[derive(Serialize)]
struct ReceiveBundleManifest<'a> {
    version: u8,
    objects: Vec<ReceiveBundleManifestObject<'a>>,
}

#[derive(Serialize)]
struct ReceiveBundleManifestObject<'a> {
    path: &'a str,
    bytes: usize,
    allow_existing: bool,
}

impl RemoteObjectPack {
    pub(crate) fn new(id: String, pack: Bytes, index: Bytes) -> Self {
        Self {
            pack_path: format!("objects/pack/{id}.pack"),
            index_path: format!("objects/pack/{id}.idx"),
            id,
            pack,
            index,
        }
    }
}

enum HttpReceivePackResult {
    Published,
    Unsupported,
    RetryIndividually,
}

impl Remote {
    pub(crate) fn snapshot_upload_concurrency(&self) -> usize {
        match &self.backend {
            // A single HTTP/1 connection is materially more reliable through
            // high-latency proxies and lets the shared client reuse TLS state.
            RemoteBackend::Http(_) => 1,
            RemoteBackend::ObjectStore(_) => REMOTE_CONCURRENCY,
        }
    }

    pub fn with_config(config: RemoteConfig) -> Result<Self> {
        Self::with_config_and_credentials(config, "", &RemoteCredentials::environment())
    }

    pub fn with_config_and_credentials(
        config: RemoteConfig,
        remote_name: &str,
        credentials: &RemoteCredentials,
    ) -> Result<Self> {
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
                let token = credentials.http_bearer_token(remote_name, token_env.as_deref());
                return Ok(Self {
                    backend: RemoteBackend::Http(HttpRemote::with_clients(
                        url,
                        token,
                        credentials.http_client()?,
                        credentials.http_upload_client()?,
                    )),
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

    pub(crate) async fn publish_object_pack_and_ref(
        &self,
        pack: Option<RemoteObjectPack>,
        ref_path: &str,
        expected: Option<&[u8]>,
        replacement: impl Into<Bytes>,
    ) -> Result<()> {
        let replacement = replacement.into();
        if let (RemoteBackend::Http(remote), Some(pack)) = (&self.backend, pack.as_ref()) {
            match remote
                .receive_pack(pack, ref_path, expected, replacement.clone())
                .await?
            {
                HttpReceivePackResult::Published => return Ok(()),
                HttpReceivePackResult::Unsupported | HttpReceivePackResult::RetryIndividually => {}
            }
        }

        if let Some(pack) = pack.as_ref() {
            self.put_pack_objects(pack).await?;
        }
        self.compare_and_swap_raw(ref_path, expected, replacement)
            .await
    }

    pub(crate) async fn publish_object_bundle_and_ref(
        &self,
        objects: Vec<RemoteBundleObject>,
        pack: Option<RemoteObjectPack>,
        ref_path: &str,
        expected: Option<&[u8]>,
        replacement: impl Into<Bytes>,
    ) -> Result<()> {
        let replacement = replacement.into();
        if objects.is_empty() {
            return self
                .publish_object_pack_and_ref(pack, ref_path, expected, replacement)
                .await;
        }

        if let (RemoteBackend::Http(remote), Some(pack)) = (&self.backend, pack.as_ref()) {
            match remote
                .receive_bundle(&objects, pack, ref_path, expected, replacement.clone())
                .await?
            {
                HttpReceivePackResult::Published => return Ok(()),
                HttpReceivePackResult::Unsupported | HttpReceivePackResult::RetryIndividually => {}
            }
        }

        self.put_bundle_objects(&objects).await?;
        self.publish_object_pack_and_ref(pack, ref_path, expected, replacement)
            .await
    }

    async fn put_bundle_objects(&self, objects: &[RemoteBundleObject]) -> Result<()> {
        for object in objects {
            let bytes = object.bytes();
            match self
                .put_raw_if_not_exists(&object.path, bytes.clone())
                .await
            {
                Ok(()) => {}
                Err(err) if err.precondition_failed() && object.allow_existing => {}
                Err(err) if err.precondition_failed() => {
                    let existing = self.get_raw(&object.path).await?;
                    if existing.as_ref() != Some(&bytes) {
                        return Err(RemoteErr::HttpStatus {
                            status: 412,
                            path: object.path.clone(),
                            message: "remote immutable object has conflicting content".to_string(),
                        });
                    }
                }
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    async fn put_pack_objects(&self, pack: &RemoteObjectPack) -> Result<()> {
        for (path, bytes) in [
            (&pack.pack_path, pack.pack.clone()),
            (&pack.index_path, pack.index.clone()),
        ] {
            match self.put_raw_if_not_exists(path, bytes).await {
                Ok(()) => {}
                Err(err) if err.precondition_failed() => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
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
    #[cfg(test)]
    fn new(url: String, token: Option<BearerToken>) -> Result<Self> {
        Ok(Self::with_client(url, token, build_http_client()?))
    }

    #[cfg(test)]
    fn with_client(url: String, token: Option<BearerToken>, client: reqwest::Client) -> Self {
        Self::with_clients(url, token, client.clone(), client)
    }

    fn with_clients(
        url: String,
        token: Option<BearerToken>,
        client: reqwest::Client,
        upload_client: reqwest::Client,
    ) -> Self {
        Self {
            client,
            upload_client,
            url: url.trim_end_matches('/').to_string(),
            token,
        }
    }

    fn raw_url(&self, kind: &str, path: &str) -> String {
        format!("{}/{}/{}", self.url, kind, percent_encode_path(path))
    }

    fn list_url(&self, prefix: &str, cursor: Option<&str>) -> String {
        let mut url = format!(
            "{}/list?prefix={}",
            self.url,
            percent_encode_component(prefix)
        );
        if let Some(cursor) = cursor {
            url.push_str("&cursor=");
            url.push_str(&percent_encode_component(cursor));
        }
        url
    }

    fn request(&self, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
        self.request_with(&self.client, method, url)
    }

    fn upload_request(&self, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
        self.request_with(&self.upload_client, method, url)
    }

    fn request_with(
        &self,
        client: &reqwest::Client,
        method: reqwest::Method,
        url: String,
    ) -> reqwest::RequestBuilder {
        let request = client
            .request(method, url)
            .header(GRAFT_PROTOCOL_HEADER, GRAFT_PROTOCOL_VERSION);
        if let Some(token) = &self.token {
            request.bearer_auth(token.expose())
        } else {
            request
        }
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        operation: &'static str,
        request_bytes: Option<u64>,
    ) -> Result<reqwest::Response> {
        let request_id = format!(
            "{:x}-{:x}",
            std::process::id(),
            HTTP_REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let started = Instant::now();
        let result = request
            .header(GRAFT_REQUEST_ID_HEADER, &request_id)
            .send()
            .await;
        match result {
            Ok(response) => {
                let status = response.status().as_u16();
                let response_bytes = response.content_length();
                let server_timings = safe_server_timings(response.headers());
                crate::trace::emit_http(crate::trace::HttpTrace {
                    operation,
                    request_id: &request_id,
                    duration: started.elapsed(),
                    status: Some(status),
                    request_bytes,
                    response_bytes,
                    server_timings: &server_timings,
                });
                Ok(response)
            }
            Err(err) => {
                crate::trace::emit_http(crate::trace::HttpTrace {
                    operation,
                    request_id: &request_id,
                    duration: started.elapsed(),
                    status: None,
                    request_bytes,
                    response_bytes: None,
                    server_timings: &[],
                });
                Err(RemoteErr::HttpTransport(err))
            }
        }
    }

    fn check_protocol(response: &reqwest::Response, path: &str) -> Result<()> {
        let protocol_headers = response.headers().get_all(GRAFT_PROTOCOL_HEADER);
        let mut protocol_versions = protocol_headers.iter();
        let first = protocol_versions.next();
        if first.is_some_and(|value| value.as_bytes() == GRAFT_PROTOCOL_VERSION.as_bytes())
            && protocol_versions.next().is_none()
        {
            return Ok(());
        }
        let received = protocol_headers
            .iter()
            .fold(None, |received: Option<String>, value| {
                let value = String::from_utf8_lossy(value.as_bytes());
                Some(match received {
                    Some(received) => format!("{received}, {value}"),
                    None => value.into_owned(),
                })
            });
        Err(RemoteErr::HttpProtocolMismatch {
            path: path.to_string(),
            expected: GRAFT_PROTOCOL_VERSION,
            received,
        })
    }

    async fn check_response(response: reqwest::Response, path: &str) -> Result<reqwest::Response> {
        Self::check_protocol(&response, path)?;
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
            .send(
                self.request(reqwest::Method::HEAD, self.raw_url("raw", path)),
                "head",
                Some(0),
            )
            .await?;
        if response.status().as_u16() == 404 {
            Self::check_protocol(&response, path)?;
            return Ok(false);
        }
        Self::check_response(response, path).await?;
        Ok(true)
    }

    async fn get_raw(&self, path: &str) -> Result<Option<Bytes>> {
        let response = self
            .send(
                self.request(reqwest::Method::GET, self.raw_url("raw", path)),
                "get",
                Some(0),
            )
            .await?;
        if response.status().as_u16() == 404 {
            Self::check_protocol(&response, path)?;
            Self::drain_response(response).await?;
            return Ok(None);
        }
        let response = Self::check_response(response, path).await?;
        Ok(Some(
            response.bytes().await.map_err(RemoteErr::HttpTransport)?,
        ))
    }

    async fn drain_response(response: reqwest::Response) -> Result<()> {
        let mut body = response.bytes_stream();
        while let Some(chunk) = body.next().await {
            chunk.map_err(RemoteErr::HttpTransport)?;
        }
        Ok(())
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
            .send(
                self.request(reqwest::Method::GET, self.raw_url("raw", path))
                    .header(
                        reqwest::header::RANGE,
                        format!("bytes={}-{}", range.start, end),
                    ),
                "range_get",
                Some(0),
            )
            .await?;
        let response = Self::check_response(response, path).await?;
        response.bytes().await.map_err(RemoteErr::HttpTransport)
    }

    async fn list_raw(&self, prefix: &str) -> Result<Vec<String>> {
        let mut paths = Vec::new();
        let mut cursor = None;
        let mut seen_cursors = HashSet::new();

        loop {
            let response = self
                .send(
                    self.request(
                        reqwest::Method::GET,
                        self.list_url(prefix, cursor.as_deref()),
                    ),
                    "list",
                    Some(0),
                )
                .await?;
            let response = Self::check_response(response, prefix).await?;
            let bytes = response.bytes().await.map_err(RemoteErr::HttpTransport)?;
            let page: HttpListResponse =
                serde_json::from_slice(&bytes).map_err(|err| RemoteErr::HttpStatus {
                    status: 502,
                    path: prefix.to_string(),
                    message: format!("invalid list response JSON: {err}"),
                })?;
            paths.extend(page.paths);

            let Some(next_cursor) = page.next_cursor else {
                return Ok(paths);
            };
            if next_cursor.is_empty() || !seen_cursors.insert(next_cursor.clone()) {
                return Err(RemoteErr::HttpStatus {
                    status: 502,
                    path: prefix.to_string(),
                    message: "list response repeated an empty or previously seen cursor"
                        .to_string(),
                });
            }
            cursor = Some(next_cursor);
        }
    }

    async fn put_raw(&self, path: &str, bytes: Bytes) -> Result<()> {
        let request_bytes = bytes.len() as u64;
        let response = self
            .send(
                self.upload_request(reqwest::Method::PUT, self.raw_url("raw", path))
                    .body(bytes),
                "put",
                Some(request_bytes),
            )
            .await?;
        Self::check_response(response, path).await?;
        Ok(())
    }

    async fn put_raw_if_not_exists(&self, path: &str, bytes: Bytes) -> Result<()> {
        let request_bytes = bytes.len() as u64;
        let response = self
            .send(
                self.upload_request(
                    reqwest::Method::PUT,
                    self.raw_url("raw-if-not-exists", path),
                )
                .body(bytes),
                "immutable_put",
                Some(request_bytes),
            )
            .await?;
        Self::check_response(response, path).await?;
        Ok(())
    }

    async fn put_raw_if_not_exists_stream<I: IntoIterator<Item = Bytes>>(
        &self,
        path: &str,
        chunks: I,
    ) -> Result<()> {
        let chunks = chunks.into_iter().collect::<Vec<_>>();
        let content_length = chunks.iter().try_fold(0_usize, |total, chunk| {
            total
                .checked_add(chunk.len())
                .ok_or_else(|| RemoteErr::HttpStatus {
                    status: 413,
                    path: path.to_string(),
                    message: "streamed upload length exceeds usize".to_string(),
                })
        })?;
        let body = reqwest::Body::wrap_stream(stream::iter(
            chunks.into_iter().map(Ok::<Bytes, std::io::Error>),
        ));
        let response = self
            .send(
                self.upload_request(
                    reqwest::Method::PUT,
                    self.raw_url("raw-if-not-exists", path),
                )
                .header(reqwest::header::CONTENT_LENGTH, content_length)
                .body(body),
                "immutable_put",
                Some(content_length as u64),
            )
            .await?;
        Self::check_response(response, path).await?;
        Ok(())
    }

    async fn receive_pack(
        &self,
        pack: &RemoteObjectPack,
        ref_path: &str,
        expected: Option<&[u8]>,
        replacement: Bytes,
    ) -> Result<HttpReceivePackResult> {
        let content_length = pack
            .pack
            .len()
            .checked_add(pack.index.len())
            .ok_or_else(|| RemoteErr::HttpStatus {
                status: 413,
                path: ref_path.to_string(),
                message: "receive-pack body length exceeds usize".to_string(),
            })?;
        let body = reqwest::Body::wrap_stream(stream::iter(
            [pack.pack.clone(), pack.index.clone()]
                .into_iter()
                .map(Ok::<Bytes, std::io::Error>),
        ));
        let response = self
            .send(
                self.upload_request(
                    reqwest::Method::POST,
                    self.raw_url("receive-pack", ref_path),
                )
                .header(reqwest::header::CONTENT_LENGTH, content_length)
                .header(RECEIVE_PACK_HEADER_PACK_ID, &pack.id)
                .header(RECEIVE_PACK_HEADER_PACK_BYTES, pack.pack.len())
                .header(RECEIVE_PACK_HEADER_INDEX_BYTES, pack.index.len())
                .header(
                    RECEIVE_PACK_HEADER_REPLACEMENT_HEX,
                    hex_encode(&replacement),
                )
                .header("x-graft-expected-present", expected.is_some().to_string())
                .header(
                    "x-graft-expected-hex",
                    expected.map(hex_encode).unwrap_or_default(),
                )
                .body(body),
                "receive_pack",
                Some(content_length as u64),
            )
            .await?;
        Self::check_protocol(&response, ref_path)?;
        if matches!(response.status().as_u16(), 404 | 405) {
            Self::drain_response(response).await?;
            return Ok(HttpReceivePackResult::Unsupported);
        }
        Self::check_response(response, ref_path).await?;
        Ok(HttpReceivePackResult::Published)
    }

    async fn receive_bundle(
        &self,
        objects: &[RemoteBundleObject],
        pack: &RemoteObjectPack,
        ref_path: &str,
        expected: Option<&[u8]>,
        replacement: Bytes,
    ) -> Result<HttpReceivePackResult> {
        let manifest = ReceiveBundleManifest {
            version: 1,
            objects: objects
                .iter()
                .map(|object| ReceiveBundleManifestObject {
                    path: &object.path,
                    bytes: object.content_length,
                    allow_existing: object.allow_existing,
                })
                .collect(),
        };
        let manifest = serde_json::to_vec(&manifest).map_err(|err| RemoteErr::HttpStatus {
            status: 500,
            path: ref_path.to_string(),
            message: format!("failed to encode receive-bundle manifest: {err}"),
        })?;
        if manifest.len() > 16 * 1024 {
            return Err(RemoteErr::HttpStatus {
                status: 413,
                path: ref_path.to_string(),
                message: "receive-bundle manifest exceeds 16 KiB".to_string(),
            });
        }

        let object_bytes = objects.iter().try_fold(0_usize, |total, object| {
            total.checked_add(object.content_length)
        });
        let content_length = manifest
            .len()
            .checked_add(object_bytes.ok_or_else(|| RemoteErr::HttpStatus {
                status: 413,
                path: ref_path.to_string(),
                message: "receive-bundle object length exceeds usize".to_string(),
            })?)
            .and_then(|length| length.checked_add(pack.pack.len()))
            .and_then(|length| length.checked_add(pack.index.len()))
            .ok_or_else(|| RemoteErr::HttpStatus {
                status: 413,
                path: ref_path.to_string(),
                message: "receive-bundle body length exceeds usize".to_string(),
            })?;
        let chunks = std::iter::once(Bytes::from(manifest.clone()))
            .chain(
                objects
                    .iter()
                    .flat_map(|object| object.chunks.iter().cloned()),
            )
            .chain([pack.pack.clone(), pack.index.clone()])
            .collect::<Vec<_>>();
        let body = reqwest::Body::wrap_stream(stream::iter(
            chunks.into_iter().map(Ok::<Bytes, std::io::Error>),
        ));
        let response = self
            .send(
                // A receive-bundle is the only mutation after the ref read in the fast path.
                // Reuse that connection like Git smart HTTP; legacy PUTs and fallbacks retain
                // the isolated upload pool because mixed proxy traffic previously stalled.
                self.request(
                    reqwest::Method::POST,
                    self.raw_url("receive-bundle", ref_path),
                )
                .header(reqwest::header::CONTENT_LENGTH, content_length)
                .header(RECEIVE_BUNDLE_HEADER_MANIFEST_BYTES, manifest.len())
                .header(RECEIVE_PACK_HEADER_PACK_ID, &pack.id)
                .header(RECEIVE_PACK_HEADER_PACK_BYTES, pack.pack.len())
                .header(RECEIVE_PACK_HEADER_INDEX_BYTES, pack.index.len())
                .header(
                    RECEIVE_PACK_HEADER_REPLACEMENT_HEX,
                    hex_encode(&replacement),
                )
                .header("x-graft-expected-present", expected.is_some().to_string())
                .header(
                    "x-graft-expected-hex",
                    expected.map(hex_encode).unwrap_or_default(),
                )
                .body(body),
                "receive_bundle",
                Some(content_length as u64),
            )
            .await?;
        Self::check_protocol(&response, ref_path)?;
        match response.status().as_u16() {
            404 | 405 => {
                Self::drain_response(response).await?;
                return Ok(HttpReceivePackResult::Unsupported);
            }
            412 => {
                Self::drain_response(response).await?;
                return Ok(HttpReceivePackResult::RetryIndividually);
            }
            _ => {}
        }
        Self::check_response(response, ref_path).await?;
        Ok(HttpReceivePackResult::Published)
    }

    async fn delete_raw(&self, path: &str) -> Result<()> {
        let response = self
            .send(
                self.upload_request(reqwest::Method::DELETE, self.raw_url("raw", path)),
                "delete",
                Some(0),
            )
            .await?;
        Self::check_response(response, path).await?;
        Ok(())
    }

    async fn compare_and_swap_raw(
        &self,
        path: &str,
        expected: Option<&[u8]>,
        bytes: Bytes,
    ) -> Result<()> {
        let request_bytes = bytes.len() as u64;
        let response = self
            .send(
                self.upload_request(reqwest::Method::POST, self.raw_url("cas", path))
                    .header("x-graft-expected-present", expected.is_some().to_string())
                    .header(
                        "x-graft-expected-hex",
                        expected.map(hex_encode).unwrap_or_default(),
                    )
                    .body(bytes),
                "compare_and_swap",
                Some(request_bytes),
            )
            .await?;
        Self::check_response(response, path).await?;
        Ok(())
    }

    async fn compare_and_delete_raw(&self, path: &str, expected: Option<&[u8]>) -> Result<()> {
        let response = self
            .send(
                self.upload_request(reqwest::Method::POST, self.raw_url("cad", path))
                    .header("x-graft-expected-present", expected.is_some().to_string())
                    .header(
                        "x-graft-expected-hex",
                        expected.map(hex_encode).unwrap_or_default(),
                    ),
                "compare_and_delete",
                Some(0),
            )
            .await?;
        Self::check_response(response, path).await?;
        Ok(())
    }
}

fn safe_server_timings(headers: &reqwest::header::HeaderMap) -> Vec<(&'static str, f64)> {
    let mut timings = Vec::new();
    for value in headers.get_all("server-timing") {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for metric in value.split(',') {
            let mut parts = metric.trim().split(';');
            let Some(name) = parts.next().and_then(safe_server_timing_name) else {
                continue;
            };
            let Some(duration) = parts.find_map(|parameter| {
                parameter
                    .trim()
                    .strip_prefix("dur=")
                    .and_then(|value| value.parse::<f64>().ok())
                    .filter(|value| value.is_finite() && *value >= 0.0)
            }) else {
                continue;
            };
            if let Some(existing) = timings
                .iter_mut()
                .find(|(existing_name, _)| *existing_name == name)
            {
                existing.1 = duration;
            } else {
                timings.push((name, duration));
            }
        }
    }
    timings
}

fn safe_server_timing_name(value: &str) -> Option<&'static str> {
    match value {
        "auth" => Some("auth"),
        "directory" => Some("directory"),
        "total" => Some("total"),
        _ => None,
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn http_snapshot_uploads_are_serialized() {
        let http = RemoteConfig::Http {
            url: "https://example.com/org/repo".to_string(),
            token_env: None,
        }
        .build()
        .unwrap();
        let memory = RemoteConfig::Memory.build().unwrap();

        assert_eq!(http.snapshot_upload_concurrency(), 1);
        assert_eq!(memory.snapshot_upload_concurrency(), REMOTE_CONCURRENCY);
    }

    #[test]
    fn explicit_credentials_are_in_memory_only_and_redacted() {
        let credentials = RemoteCredentials::explicit();
        credentials
            .set_http_bearer_token("origin", "sdk-secret-token".to_string())
            .unwrap();
        let remote = RemoteConfig::Http {
            url: "https://example.com/org/repo".to_string(),
            token_env: Some("IGNORED_BY_EXPLICIT_CREDENTIALS".to_string()),
        }
        .build_with_credentials("origin", &credentials)
        .unwrap();
        let RemoteBackend::Http(http) = &remote.backend else {
            panic!("expected HTTP remote");
        };
        assert_eq!(
            http.token.as_ref().map(BearerToken::expose),
            Some("sdk-secret-token")
        );
        assert!(!format!("{remote:?}").contains("sdk-secret-token"));
        assert_eq!(
            credentials.redact("request failed for sdk-secret-token"),
            "request failed for [redacted]"
        );
    }

    #[test]
    fn explicit_credentials_do_not_fall_back_to_token_env() {
        let remote = RemoteConfig::Http {
            url: "https://example.com/org/repo".to_string(),
            token_env: Some("SOME_PROCESS_TOKEN".to_string()),
        }
        .build_with_credentials("origin", &RemoteCredentials::explicit())
        .unwrap();
        let RemoteBackend::Http(http) = &remote.backend else {
            panic!("expected HTTP remote");
        };
        assert!(http.token.is_none());
    }

    async fn serve_http_response(
        status: &str,
        protocol_versions: &[&str],
    ) -> (String, tokio::task::JoinHandle<String>) {
        let protocol_versions = protocol_versions
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_string();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0_u8; 1024];
                let read = stream.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let protocol_headers = protocol_versions
                .iter()
                .map(|version| format!("Graft-Protocol: {version}\r\n"))
                .collect::<String>();
            let response = format!(
                "HTTP/1.1 {status}\r\n{protocol_headers}Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        (format!("http://{address}/org/repo"), task)
    }

    async fn serve_http_exchanges(
        statuses: &[&str],
    ) -> (String, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
        let statuses = statuses.iter().map(ToString::to_string).collect::<Vec<_>>();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::with_capacity(statuses.len());
            for status in statuses {
                let (mut stream, _) = listener.accept().await.unwrap();
                requests.push(read_http_request(&mut stream).await);
                let response = format!(
                    "HTTP/1.1 {status}\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (format!("http://{address}/org/repo"), task)
    }

    async fn read_http_request(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut request_bytes = None;
        loop {
            let mut buffer = [0_u8; 4096];
            let read = stream.read(&mut buffer).await.unwrap();
            if read == 0 {
                return request;
            }
            request.extend_from_slice(&buffer[..read]);
            if request_bytes.is_none()
                && let Some(header_end) =
                    request.windows(4).position(|window| window == b"\r\n\r\n")
            {
                let header_bytes = header_end + 4;
                request_bytes = Some(header_bytes + http_content_length(&request[..header_bytes]));
            }
            if request_bytes.is_some_and(|expected| request.len() >= expected) {
                return request;
            }
        }
    }

    fn http_content_length(headers: &[u8]) -> usize {
        String::from_utf8_lossy(headers)
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0)
    }

    fn http_request_body(request: &[u8]) -> &[u8] {
        let header_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap();
        &request[header_end + 4..]
    }

    #[tokio::test]
    async fn receive_pack_publishes_pack_index_and_ref_in_one_request() {
        let (url, requests) = serve_http_exchanges(&["204 No Content"]).await;
        let remote = RemoteConfig::Http { url, token_env: None }.build().unwrap();
        let pack_id = "a".repeat(64);
        remote
            .publish_object_pack_and_ref(
                Some(RemoteObjectPack::new(
                    pack_id.clone(),
                    Bytes::from_static(b"pack"),
                    Bytes::from_static(b"idx"),
                )),
                "refs/heads/main",
                None,
                "new\n",
            )
            .await
            .unwrap();

        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 1);
        let headers = String::from_utf8_lossy(&requests[0]);
        assert!(headers.starts_with("POST /org/repo/receive-pack/refs/heads/main "));
        assert!(
            headers
                .lines()
                .any(|line| { line.eq_ignore_ascii_case(&format!("x-graft-pack-id: {pack_id}")) })
        );
        assert!(
            headers
                .lines()
                .any(|line| line.eq_ignore_ascii_case("content-length: 7"))
        );
        assert_eq!(http_request_body(&requests[0]), b"packidx");
    }

    #[tokio::test]
    async fn receive_pack_falls_back_to_v1_object_and_cas_requests() {
        let (url, requests) = serve_http_exchanges(&[
            "404 Not Found",
            "204 No Content",
            "204 No Content",
            "204 No Content",
        ])
        .await;
        let remote = RemoteConfig::Http { url, token_env: None }.build().unwrap();
        let pack_id = "b".repeat(64);
        remote
            .publish_object_pack_and_ref(
                Some(RemoteObjectPack::new(
                    pack_id,
                    Bytes::from_static(b"pack"),
                    Bytes::from_static(b"idx"),
                )),
                "refs/heads/main",
                None,
                "new\n",
            )
            .await
            .unwrap();

        let requests = requests.await.unwrap();
        let request_lines = requests
            .iter()
            .map(|request| {
                String::from_utf8_lossy(request)
                    .lines()
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<_>>();
        assert!(request_lines[0].starts_with("POST /org/repo/receive-pack/"));
        assert!(request_lines[1].starts_with("PUT /org/repo/raw-if-not-exists/objects/pack/"));
        assert!(request_lines[2].starts_with("PUT /org/repo/raw-if-not-exists/objects/pack/"));
        assert!(request_lines[3].starts_with("POST /org/repo/cas/refs/heads/main "));
    }

    #[tokio::test]
    async fn receive_bundle_publishes_extra_objects_pack_index_and_ref_in_one_request() {
        let (url, requests) = serve_http_exchanges(&["204 No Content"]).await;
        let remote = RemoteConfig::Http { url, token_env: None }.build().unwrap();
        let pack_id = "c".repeat(64);
        remote
            .publish_object_bundle_and_ref(
                vec![
                    RemoteBundleObject::new(
                        "segments/example".to_string(),
                        [Bytes::from_static(b"segment")],
                        true,
                    )
                    .unwrap(),
                    RemoteBundleObject::new(
                        "logs/example/commits/0000000000000001".to_string(),
                        [Bytes::from_static(b"commit")],
                        false,
                    )
                    .unwrap(),
                ],
                Some(RemoteObjectPack::new(
                    pack_id,
                    Bytes::from_static(b"pack"),
                    Bytes::from_static(b"idx"),
                )),
                "refs/heads/main",
                None,
                "new\n",
            )
            .await
            .unwrap();

        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 1);
        let headers = String::from_utf8_lossy(&requests[0]);
        assert!(headers.starts_with("POST /org/repo/receive-bundle/refs/heads/main "));
        let manifest_len = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case(RECEIVE_BUNDLE_HEADER_MANIFEST_BYTES)
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        let body = http_request_body(&requests[0]);
        let manifest: serde_json::Value = serde_json::from_slice(&body[..manifest_len]).unwrap();
        assert_eq!(manifest["objects"].as_array().unwrap().len(), 2);
        assert_eq!(&body[manifest_len..], b"segmentcommitpackidx");
    }

    #[tokio::test]
    async fn receive_bundle_falls_back_to_objects_then_receive_pack() {
        let (url, requests) =
            serve_http_exchanges(&["404 Not Found", "204 No Content", "204 No Content"]).await;
        let remote = RemoteConfig::Http { url, token_env: None }.build().unwrap();
        remote
            .publish_object_bundle_and_ref(
                vec![
                    RemoteBundleObject::new(
                        "segments/example".to_string(),
                        [Bytes::from_static(b"segment")],
                        true,
                    )
                    .unwrap(),
                ],
                Some(RemoteObjectPack::new(
                    "d".repeat(64),
                    Bytes::from_static(b"pack"),
                    Bytes::from_static(b"idx"),
                )),
                "refs/heads/main",
                None,
                "new\n",
            )
            .await
            .unwrap();

        let request_lines = requests
            .await
            .unwrap()
            .iter()
            .map(|request| {
                String::from_utf8_lossy(request)
                    .lines()
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<_>>();
        assert!(request_lines[0].starts_with("POST /org/repo/receive-bundle/"));
        assert!(request_lines[1].starts_with("PUT /org/repo/raw-if-not-exists/segments/example"));
        assert!(request_lines[2].starts_with("POST /org/repo/receive-pack/"));
    }

    #[tokio::test]
    async fn receive_bundle_reuses_the_ref_read_connection() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let read = read_http_request(&mut stream).await;
            assert!(
                String::from_utf8_lossy(&read).starts_with("GET /org/repo/raw/refs/heads/main ")
            );
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\n\r\n",
                )
                .await
                .unwrap();

            let bundle = read_http_request(&mut stream).await;
            assert!(
                String::from_utf8_lossy(&bundle)
                    .starts_with("POST /org/repo/receive-bundle/refs/heads/main ")
            );
            stream
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let remote = RemoteConfig::Http {
            url: format!("http://{address}/org/repo"),
            token_env: None,
        }
        .build()
        .unwrap();

        assert!(remote.get_raw("refs/heads/main").await.unwrap().is_none());
        remote
            .publish_object_bundle_and_ref(
                vec![
                    RemoteBundleObject::new(
                        "segments/example".to_string(),
                        [Bytes::from_static(b"segment")],
                        true,
                    )
                    .unwrap(),
                ],
                Some(RemoteObjectPack::new(
                    "e".repeat(64),
                    Bytes::from_static(b"pack"),
                    Bytes::from_static(b"idx"),
                )),
                "refs/heads/main",
                None,
                "new\n",
            )
            .await
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_remote_sends_explicit_in_memory_bearer_token() {
        let credentials = RemoteCredentials::explicit();
        credentials
            .set_http_bearer_token("origin", "sdk-request-secret".to_string())
            .unwrap();
        let (url, request) = serve_http_response("204 No Content", &["1"]).await;
        let remote = RemoteConfig::Http {
            url,
            token_env: Some("IGNORED_BY_EXPLICIT_CREDENTIALS".to_string()),
        }
        .build_with_credentials("origin", &credentials)
        .unwrap();
        let RemoteBackend::Http(http) = &remote.backend else {
            panic!("expected HTTP remote");
        };

        assert!(http.has_raw("objects/one").await.unwrap());
        let request = request.await.unwrap();
        assert!(
            request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("Authorization: Bearer sdk-request-secret"))
        );
        assert!(!format!("{remote:?}").contains("sdk-request-secret"));
    }

    #[tokio::test]
    async fn repository_credentials_reuse_http_connections_across_remotes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            for _ in 0..2 {
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0_u8; 1024];
                    let read = stream.read(&mut buffer).await.unwrap();
                    request.extend_from_slice(&buffer[..read]);
                    if read == 0 || request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                stream
                    .write_all(
                        b"HTTP/1.1 204 No Content\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\n\r\n",
                    )
                    .await
                    .unwrap();
            }
        });
        let credentials = RemoteCredentials::explicit();
        let config = RemoteConfig::Http {
            url: format!("http://{address}/org/repo"),
            token_env: None,
        };
        let first = config
            .clone()
            .build_with_credentials("origin", &credentials)
            .unwrap();
        let second = config
            .build_with_credentials("origin", &credentials)
            .unwrap();

        assert!(first.has_segment(&SegmentId::random()).await.unwrap());
        assert!(second.has_segment(&SegmentId::random()).await.unwrap());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn resetting_repository_credentials_starts_a_fresh_read_connection() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_http_headers(&mut stream).await;
                stream
                    .write_all(
                        b"HTTP/1.1 204 No Content\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
            }
        });
        let credentials = RemoteCredentials::explicit();
        let config = RemoteConfig::Http {
            url: format!("http://{address}/org/repo"),
            token_env: None,
        };
        let first = config
            .clone()
            .build_with_credentials("origin", &credentials)
            .unwrap();
        assert!(first.has_segment(&SegmentId::random()).await.unwrap());

        credentials.reset_http_clients();
        let second = config
            .build_with_credentials("origin", &credentials)
            .unwrap();
        assert!(second.has_segment(&SegmentId::random()).await.unwrap());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn missing_get_drains_body_before_reusing_connection() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_headers(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nGraft-Protocol: 1\r\nContent-Length: 7\r\n\r\nmissing",
                )
                .await
                .unwrap();
            read_http_headers(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nGraft-Protocol: 1\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                )
                .await
                .unwrap();
        });
        let remote = HttpRemote::new(format!("http://{address}/org/repo"), None).unwrap();

        assert_eq!(remote.get_raw("refs/heads/main").await.unwrap(), None);
        let bytes = tokio::time::timeout(Duration::from_secs(1), remote.get_raw("refs/heads/main"))
            .await
            .expect("second GET did not reuse the drained response connection")
            .unwrap();
        assert_eq!(bytes, Some(Bytes::from_static(b"ok")));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn repository_credentials_separate_read_and_upload_connections() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut read_stream, _) = listener.accept().await.unwrap();
            read_http_headers(&mut read_stream).await;
            read_stream
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\n\r\n",
                )
                .await
                .unwrap();

            let (mut upload_stream, _) =
                tokio::time::timeout(Duration::from_secs(1), listener.accept())
                    .await
                    .expect("upload reused the read connection")
                    .unwrap();
            read_http_headers(&mut upload_stream).await;
            upload_stream
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let remote = RemoteConfig::Http {
            url: format!("http://{address}/org/repo"),
            token_env: None,
        }
        .build_with_credentials("origin", &RemoteCredentials::explicit())
        .unwrap();

        assert!(remote.has_segment(&SegmentId::random()).await.unwrap());
        remote
            .put_raw_if_not_exists("objects/one", Bytes::from_static(b"one"))
            .await
            .unwrap();
        server.await.unwrap();
    }

    async fn read_http_headers(stream: &mut tokio::net::TcpStream) {
        let mut request = Vec::new();
        loop {
            let mut buffer = [0_u8; 1024];
            let read = stream.read(&mut buffer).await.unwrap();
            request.extend_from_slice(&buffer[..read]);
            if read == 0 || request.windows(4).any(|window| window == b"\r\n\r\n") {
                return;
            }
        }
    }

    #[tokio::test]
    async fn streamed_http_upload_sends_exact_content_length() {
        let (url, request) = serve_http_response("204 No Content", &["1"]).await;
        let remote = HttpRemote::new(url, None).unwrap();

        remote
            .put_raw_if_not_exists_stream(
                "segments/example",
                [Bytes::from_static(b"abc"), Bytes::from_static(b"de")],
            )
            .await
            .unwrap();
        let request = request.await.unwrap();
        assert!(
            request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("Content-Length: 5"))
        );
        assert!(
            !request
                .lines()
                .any(|line| { line.eq_ignore_ascii_case("Transfer-Encoding: chunked") })
        );
    }

    #[tokio::test]
    async fn streamed_http_upload_completes_when_remote_rejects_before_reading_body() {
        let (url, request) = serve_http_response("412 Precondition Failed", &["1"]).await;
        let remote = HttpRemote::new(url, None).unwrap();

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            remote.put_raw_if_not_exists_stream(
                "segments/example",
                [Bytes::from(vec![7_u8; 64 * 1024])],
            ),
        )
        .await
        .expect("fixed-length upload stalled after an early response")
        .unwrap_err();
        assert!(result.precondition_failed());
        request.await.unwrap();
    }

    #[test]
    fn server_timing_trace_accepts_only_known_numeric_metrics() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "server-timing",
            "auth;dur=2.5;desc=secret, directory;dur=3, private_path;dur=999, total;dur=7"
                .parse()
                .unwrap(),
        );

        assert_eq!(
            safe_server_timings(&headers),
            vec![("auth", 2.5), ("directory", 3.0), ("total", 7.0)]
        );
    }

    #[tokio::test]
    async fn http_remote_sends_and_requires_protocol_version() {
        let (url, request) = serve_http_response("204 No Content", &["1"]).await;
        let remote = HttpRemote::new(url, None).unwrap();
        assert!(remote.has_raw("objects/one").await.unwrap());
        let request = request.await.unwrap();
        assert!(
            request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("Graft-Protocol: 1"))
        );

        let (url, request) = serve_http_response("204 No Content", &[]).await;
        let remote = HttpRemote::new(url, None).unwrap();
        assert!(matches!(
            remote.has_raw("objects/one").await,
            Err(RemoteErr::HttpProtocolMismatch { received: None, .. })
        ));
        request.await.unwrap();

        let (url, request) = serve_http_response("204 No Content", &["2"]).await;
        let remote = HttpRemote::new(url, None).unwrap();
        assert!(matches!(
            remote.has_raw("objects/one").await,
            Err(RemoteErr::HttpProtocolMismatch {
                received: Some(version),
                ..
            }) if version == "2"
        ));
        request.await.unwrap();

        let (url, request) = serve_http_response("204 No Content", &["1", "2"]).await;
        let remote = HttpRemote::new(url, None).unwrap();
        assert!(matches!(
            remote.has_raw("objects/one").await,
            Err(RemoteErr::HttpProtocolMismatch {
                received: Some(versions),
                ..
            }) if versions == "1, 2"
        ));
        request.await.unwrap();
    }

    #[tokio::test]
    async fn http_remote_preserves_conditional_status_contracts() {
        let (url, request) = serve_http_response("409 Conflict", &["1"]).await;
        let remote = HttpRemote::new(url, None).unwrap();
        assert!(matches!(
            remote
                .compare_and_swap_raw("refs/heads/main", None, Bytes::new())
                .await,
            Err(RemoteErr::CompareAndSwap { .. })
        ));
        request.await.unwrap();

        let (url, request) = serve_http_response("409 Conflict", &["1"]).await;
        let remote = HttpRemote::new(url, None).unwrap();
        assert!(matches!(
            remote.compare_and_delete_raw("refs/heads/main", None).await,
            Err(RemoteErr::CompareAndSwap { .. })
        ));
        request.await.unwrap();

        let (url, request) = serve_http_response("412 Precondition Failed", &["1"]).await;
        let remote = HttpRemote::new(url, None).unwrap();
        let error = remote
            .put_raw_if_not_exists("objects/one", Bytes::new())
            .await
            .unwrap_err();
        assert!(matches!(&error, RemoteErr::HttpStatus { status: 412, .. }));
        assert!(error.precondition_failed());
        request.await.unwrap();

        let (url, request) = serve_http_response("404 Not Found", &[]).await;
        let remote = HttpRemote::new(url, None).unwrap();
        assert!(matches!(
            remote.has_raw("objects/one").await,
            Err(RemoteErr::HttpProtocolMismatch { received: None, .. })
        ));
        request.await.unwrap();

        let (url, request) = serve_http_response("404 Not Found", &["1"]).await;
        let remote = HttpRemote::new(url, None).unwrap();
        assert!(!remote.has_raw("objects/one").await.unwrap());
        request.await.unwrap();
    }

    #[tokio::test]
    async fn http_remote_follows_list_cursors() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let bodies = [
                r#"{"paths":["objects/one"],"next_cursor":"opaque/+ cursor"}"#,
                r#"{"paths":["objects/two"]}"#,
            ];
            let mut requests = Vec::new();
            for body in bodies {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0_u8; 1024];
                    let read = stream.read(&mut buffer).await.unwrap();
                    request.extend_from_slice(&buffer[..read]);
                    if read == 0 || request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                requests.push(String::from_utf8(request).unwrap());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nGraft-Protocol: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });

        let remote = HttpRemote::new(format!("http://{address}/org/repo"), None).unwrap();
        assert_eq!(
            remote.list_raw("objects/").await.unwrap(),
            ["objects/one", "objects/two"]
        );
        let requests = server.await.unwrap();
        assert!(requests[0].starts_with("GET /org/repo/list?prefix=objects%2F "));
        assert!(
            requests[1]
                .starts_with("GET /org/repo/list?prefix=objects%2F&cursor=opaque%2F%2B%20cursor ")
        );
    }

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
