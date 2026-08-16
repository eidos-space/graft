use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    env, fmt, fs, future,
    io::Write,
    ops::Range,
    path::{Component, Path},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use crate::core::{LogId, SegmentId, cbe::CBE64, commit::Commit, lsn::LSN};
use crate::repo::{
    TransferDirection, TransferProgressHandle, begin_transfer_progress,
    declare_transfer_progress_total,
};
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
const UPLOAD_BUNDLE_HEADER_TOTAL_BYTES: &str = "x-graft-bundle-total-bytes";
const READ_BUNDLE_HEADER_OBJECTS: &str = "x-graft-bundle-objects";
const MULTIPART_HEADER_OBJECT_BYTES: &str = "x-graft-object-bytes";
const MULTIPART_HEADER_UPLOAD_ID: &str = "x-graft-upload-id";
const MULTIPART_HEADER_PART_NUMBER: &str = "x-graft-part-number";
const MAX_UPLOAD_BUNDLE_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_UPLOAD_BUNDLE_OBJECTS: usize = 65_536;
const MAX_UPLOAD_BUNDLE_PATH_BYTES: usize = 768;
const MAX_READ_BUNDLE_OBJECTS: usize = 256;
const MAX_READ_BUNDLE_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MULTIPART_DISCOVERY_THRESHOLD_BYTES: usize = 64 * 1024 * 1024;
const MAX_MULTIPART_PARTS: usize = 10_000;
const MULTIPART_PART_ATTEMPTS: usize = 3;
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

    #[error("HTTP remote `{operation}` transport failed ({kind})")]
    HttpTransport {
        operation: &'static str,
        kind: HttpTransportErrorKind,
        #[source]
        source: reqwest::Error,
    },

    #[error(
        "remote publication for `{path}` was not confirmed; remote still has the expected ref and retry is safe"
    )]
    PublicationUnconfirmed {
        path: String,
        #[source]
        source: Box<RemoteErr>,
    },

    #[error("remote publication outcome for `{path}` could not be confirmed")]
    PublicationOutcomeUnknown {
        path: String,
        #[source]
        publication_error: Box<RemoteErr>,
        reconciliation_error: Box<RemoteErr>,
    },

    #[error("upload-bundle filesystem error: {0}")]
    UploadBundleIo(#[from] std::io::Error),

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpTransportErrorKind {
    Timeout,
    Connect,
    Request,
    Body,
    Other,
}

impl fmt::Display for HttpTransportErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Timeout => "timeout",
            Self::Connect => "connect",
            Self::Request => "request",
            Self::Body => "body",
            Self::Other => "other",
        })
    }
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
    http_probe_client: Arc<RwLock<Option<reqwest::Client>>>,
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
                "http_probe_client_initialized",
                &self.http_probe_client.read().is_some(),
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
            http_probe_client: Arc::default(),
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
            http_probe_client: Arc::default(),
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
        *self.http_probe_client.write() = None;
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

    fn http_probe_client(&self) -> Result<reqwest::Client> {
        if let Some(client) = self.http_probe_client.read().clone() {
            return Ok(client);
        }

        let candidate = build_http_client()?;
        let mut client = self.http_probe_client.write();
        Ok(client.get_or_insert(candidate).clone())
    }
}

fn build_http_client() -> std::result::Result<reqwest::Client, reqwest::Error> {
    reqwest::ClientBuilder::new()
        // HTTP/2 request-body multiplexing stalls behind common local proxies.
        // Reuse dedicated read, existence-probe, and mutation HTTP/1.1 pools
        // across the entire repository session so proxies never mix request
        // types with different response-body behavior on one connection.
        .http1_only()
        .hickory_dns(true)
        .connect_timeout(Duration::from_secs(5))
        // Do not put a wall-clock deadline on the shared client. Reads attach
        // their own bounded request timeout, while mutation uploads remain
        // progress/cancellation driven like Git smart HTTP. A fixed total
        // timeout incorrectly rejects healthy large pushes.
        .tcp_keepalive(Duration::from_secs(30))
        .tcp_keepalive_interval(Duration::from_secs(30))
        .tcp_keepalive_retries(5)
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
    fn http_transport(operation: &'static str, source: reqwest::Error) -> Self {
        let kind = if source.is_timeout() {
            HttpTransportErrorKind::Timeout
        } else if source.is_connect() {
            HttpTransportErrorKind::Connect
        } else if source.is_request() {
            HttpTransportErrorKind::Request
        } else if source.is_body() {
            HttpTransportErrorKind::Body
        } else {
            HttpTransportErrorKind::Other
        };
        Self::HttpTransport { operation, kind, source }
    }

    pub fn http_transport_kind(&self) -> Option<HttpTransportErrorKind> {
        match self {
            Self::HttpTransport { kind, .. } => Some(*kind),
            Self::PublicationUnconfirmed { source, .. } => source.http_transport_kind(),
            Self::PublicationOutcomeUnknown { publication_error, .. } => {
                publication_error.http_transport_kind()
            }
            _ => None,
        }
    }

    pub fn publication_unconfirmed(&self) -> bool {
        matches!(self, Self::PublicationUnconfirmed { .. })
    }

    pub fn publication_outcome_unknown(&self) -> bool {
        matches!(self, Self::PublicationOutcomeUnknown { .. })
    }

    fn may_have_published(&self) -> bool {
        matches!(
            self,
            Self::HttpTransport { .. } | Self::CompareAndSwap { .. }
        )
    }

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
    probe_client: reqwest::Client,
    upload_client: reqwest::Client,
    descriptor: Arc<tokio::sync::OnceCell<HttpRemoteDescriptor>>,
    request_timeout: Duration,
    url: String,
    token: Option<BearerToken>,
    #[cfg(test)]
    multipart_discovery_threshold: usize,
}

#[derive(Debug, Deserialize)]
struct HttpRemoteDescriptor {
    protocol: String,
    version: u8,
    #[serde(default)]
    capabilities: HashSet<String>,
    #[serde(default)]
    limits: HttpRemoteLimits,
}

#[derive(Debug, Default, Deserialize)]
struct HttpRemoteLimits {
    max_request_bytes: Option<usize>,
    multipart_part_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct HttpMultipartStartResponse {
    upload_id: String,
    total_bytes: usize,
    part_bytes: usize,
    #[serde(default)]
    uploaded_parts: Vec<HttpMultipartPart>,
}

#[derive(Debug, Deserialize)]
struct HttpMultipartPart {
    part_number: usize,
    bytes: usize,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadBundleManifest {
    version: u8,
    reference: UploadBundleReference,
    objects: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadBundleReference {
    path: String,
    value_hex: String,
}

pub(crate) enum UploadBundleOutcome {
    Downloaded,
    Unsupported,
}

pub(crate) enum ReadBundleOutcome {
    Downloaded(BTreeMap<String, Bytes>),
    Unsupported,
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
                        credentials.http_probe_client()?,
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
        self.has_raw(&path).await
    }

    pub(crate) async fn has_raw(&self, path: &str) -> Result<bool> {
        match &self.backend {
            RemoteBackend::ObjectStore(store) => match store.stat(path).await {
                Ok(_) => Ok(true),
                Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
                Err(err) => Err(err.into()),
            },
            RemoteBackend::Http(remote) => remote.has_raw(path).await,
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

    pub(crate) async fn get_raw_bundle(&self, paths: &[String]) -> Result<ReadBundleOutcome> {
        if paths.is_empty() {
            return Ok(ReadBundleOutcome::Downloaded(BTreeMap::new()));
        }
        let mut paths = paths.to_vec();
        paths.sort();
        paths.dedup();
        match &self.backend {
            RemoteBackend::Http(remote) => {
                let mut objects = BTreeMap::new();
                for paths in paths.chunks(MAX_READ_BUNDLE_OBJECTS) {
                    match remote.get_raw_bundle(paths).await? {
                        ReadBundleOutcome::Downloaded(batch) => objects.extend(batch),
                        ReadBundleOutcome::Unsupported => {
                            return Ok(ReadBundleOutcome::Unsupported);
                        }
                    }
                }
                Ok(ReadBundleOutcome::Downloaded(objects))
            }
            RemoteBackend::ObjectStore(_) => {
                let objects = stream::iter(paths)
                    .map(|path| async move {
                        Ok::<_, RemoteErr>((path.clone(), self.get_raw(&path).await?))
                    })
                    .buffered(REMOTE_CONCURRENCY)
                    .try_filter_map(
                        |(path, bytes)| async move { Ok(bytes.map(|bytes| (path, bytes))) },
                    )
                    .try_collect::<BTreeMap<_, _>>()
                    .await?;
                Ok(ReadBundleOutcome::Downloaded(objects))
            }
        }
    }

    pub(crate) async fn download_upload_bundle(
        &self,
        ref_path: &str,
        root: &Path,
    ) -> Result<UploadBundleOutcome> {
        match &self.backend {
            RemoteBackend::Http(remote) => remote.download_upload_bundle(ref_path, root).await,
            RemoteBackend::ObjectStore(_) => Ok(UploadBundleOutcome::Unsupported),
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
                .await
            {
                Ok(HttpReceivePackResult::Published) => return Ok(()),
                Ok(
                    HttpReceivePackResult::Unsupported | HttpReceivePackResult::RetryIndividually,
                ) => {
                    declare_transfer_progress_total(
                        TransferDirection::Upload,
                        planned_bundle_upload_bytes(&[], Some(pack), ref_path)?,
                    );
                }
                Err(err) => {
                    return self
                        .reconcile_publication_error(err, ref_path, expected, &replacement)
                        .await;
                }
            }
        }

        if let Some(pack) = pack.as_ref() {
            self.put_pack_objects(pack).await?;
        }
        match self
            .compare_and_swap_raw(ref_path, expected, replacement.clone())
            .await
        {
            Ok(()) => Ok(()),
            Err(err) => {
                self.reconcile_publication_error(err, ref_path, expected, &replacement)
                    .await
            }
        }
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
                .await
            {
                Ok(HttpReceivePackResult::Published) => return Ok(()),
                Ok(
                    HttpReceivePackResult::Unsupported | HttpReceivePackResult::RetryIndividually,
                ) => {
                    declare_transfer_progress_total(
                        TransferDirection::Upload,
                        planned_bundle_upload_bytes(&objects, Some(pack), ref_path)?,
                    );
                }
                Err(err) => {
                    return self
                        .reconcile_publication_error(err, ref_path, expected, &replacement)
                        .await;
                }
            }
        } else if matches!(&self.backend, RemoteBackend::Http(_)) {
            declare_transfer_progress_total(
                TransferDirection::Upload,
                planned_bundle_upload_bytes(&objects, None, ref_path)?,
            );
        }

        self.put_bundle_objects(&objects).await?;
        self.publish_object_pack_and_ref(pack, ref_path, expected, replacement)
            .await
    }

    async fn reconcile_publication_error(
        &self,
        error: RemoteErr,
        ref_path: &str,
        expected: Option<&[u8]>,
        replacement: &Bytes,
    ) -> Result<()> {
        if !error.may_have_published() {
            return Err(error);
        }

        match self.get_raw(ref_path).await {
            Ok(current) if current.as_ref() == Some(replacement) => Ok(()),
            Ok(current) if current.as_ref().map(Bytes::as_ref) == expected => {
                Err(RemoteErr::PublicationUnconfirmed {
                    path: ref_path.to_string(),
                    source: Box::new(error),
                })
            }
            Ok(_) => Err(RemoteErr::CompareAndSwap { path: ref_path.to_string() }),
            Err(reconciliation_error) => Err(RemoteErr::PublicationOutcomeUnknown {
                path: ref_path.to_string(),
                publication_error: Box::new(error),
                reconciliation_error: Box::new(reconciliation_error),
            }),
        }
    }

    async fn put_bundle_objects(&self, objects: &[RemoteBundleObject]) -> Result<()> {
        for object in objects {
            let upload = match &self.backend {
                RemoteBackend::Http(remote) => {
                    remote
                        .put_raw_if_not_exists_stream(&object.path, object.chunks.clone())
                        .await
                }
                RemoteBackend::ObjectStore(_) => {
                    self.put_raw_if_not_exists(&object.path, object.bytes())
                        .await
                }
            };
            match upload {
                Ok(()) => {}
                Err(err) if err.precondition_failed() && object.allow_existing => {}
                Err(err) if err.precondition_failed() => {
                    let bytes = object.bytes();
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
        Self::with_clients(url, token, client.clone(), client.clone(), client)
    }

    fn with_clients(
        url: String,
        token: Option<BearerToken>,
        client: reqwest::Client,
        probe_client: reqwest::Client,
        upload_client: reqwest::Client,
    ) -> Self {
        Self::with_clients_and_request_timeout(
            url,
            token,
            client,
            probe_client,
            upload_client,
            HTTP_REQUEST_TIMEOUT,
        )
    }

    fn with_clients_and_request_timeout(
        url: String,
        token: Option<BearerToken>,
        client: reqwest::Client,
        probe_client: reqwest::Client,
        upload_client: reqwest::Client,
        request_timeout: Duration,
    ) -> Self {
        Self {
            client,
            probe_client,
            upload_client,
            descriptor: Arc::new(tokio::sync::OnceCell::new()),
            request_timeout,
            url: url.trim_end_matches('/').to_string(),
            token,
            #[cfg(test)]
            multipart_discovery_threshold: MULTIPART_DISCOVERY_THRESHOLD_BYTES,
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

    async fn descriptor(&self) -> Result<&HttpRemoteDescriptor> {
        self.descriptor
            .get_or_try_init(|| async {
                let response = self
                    .send(
                        self.probe_request(reqwest::Method::GET, self.url.clone()),
                        "descriptor",
                        Some(0),
                    )
                    .await?;
                let response = Self::check_response(response, &self.url).await?;
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|err| RemoteErr::http_transport("descriptor_body", err))?;
                let descriptor: HttpRemoteDescriptor =
                    serde_json::from_slice(&bytes).map_err(|err| RemoteErr::HttpStatus {
                        status: 502,
                        path: self.url.clone(),
                        message: format!("invalid remote descriptor JSON: {err}"),
                    })?;
                if descriptor.protocol != "graft-remote" || descriptor.version != 1 {
                    return Err(RemoteErr::HttpStatus {
                        status: 502,
                        path: self.url.clone(),
                        message: "remote descriptor identifies an unsupported protocol".to_string(),
                    });
                }
                if descriptor
                    .limits
                    .max_request_bytes
                    .is_some_and(|bytes| bytes == 0)
                    || descriptor
                        .limits
                        .multipart_part_bytes
                        .is_some_and(|bytes| bytes == 0)
                {
                    return Err(RemoteErr::HttpStatus {
                        status: 502,
                        path: self.url.clone(),
                        message: "remote descriptor contains invalid request limits".to_string(),
                    });
                }
                if descriptor.capabilities.contains("multipart-object")
                    && descriptor.limits.multipart_part_bytes.is_none()
                {
                    return Err(RemoteErr::HttpStatus {
                        status: 502,
                        path: self.url.clone(),
                        message: "multipart remote does not advertise a part size".to_string(),
                    });
                }
                Ok(descriptor)
            })
            .await
    }

    async fn multipart_part_bytes(&self, content_length: usize) -> Result<Option<usize>> {
        let descriptor = match self.descriptor.get() {
            Some(descriptor) => descriptor,
            None if content_length <= self.multipart_discovery_threshold() => return Ok(None),
            None => self.descriptor().await?,
        };
        if !descriptor.capabilities.contains("multipart-object") {
            return Ok(None);
        }
        let direct_limit = descriptor
            .limits
            .max_request_bytes
            .unwrap_or_else(|| self.multipart_discovery_threshold());
        if content_length <= direct_limit {
            return Ok(None);
        }
        let part_bytes = descriptor.limits.multipart_part_bytes.unwrap();
        if content_length.div_ceil(part_bytes) > MAX_MULTIPART_PARTS {
            return Err(RemoteErr::HttpStatus {
                status: 413,
                path: self.url.clone(),
                message: "immutable object exceeds the advertised multipart limit".to_string(),
            });
        }
        Ok(Some(part_bytes))
    }

    async fn aggregate_request_requires_multipart(&self, content_length: usize) -> Result<bool> {
        if content_length <= self.multipart_discovery_threshold() {
            return Ok(false);
        }
        let descriptor = self.descriptor().await?;
        if !descriptor.capabilities.contains("multipart-object") {
            return Ok(false);
        }
        Ok(descriptor
            .limits
            .max_request_bytes
            .is_some_and(|limit| content_length > limit))
    }

    fn multipart_discovery_threshold(&self) -> usize {
        #[cfg(test)]
        {
            self.multipart_discovery_threshold
        }
        #[cfg(not(test))]
        {
            MULTIPART_DISCOVERY_THRESHOLD_BYTES
        }
    }

    fn request(&self, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
        self.request_with(&self.client, method, url)
            .timeout(self.request_timeout)
    }

    fn publication_request(&self, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
        self.request_with(&self.client, method, url)
    }

    fn upload_request(&self, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
        self.request_with(&self.upload_client, method, url)
    }

    fn probe_request(&self, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
        self.request_with(&self.probe_client, method, url)
            .timeout(self.request_timeout)
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
                Err(RemoteErr::http_transport(operation, err))
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
        if response.status().is_success() {
            Self::check_protocol(&response, path)?;
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
                self.probe_request(reqwest::Method::HEAD, self.raw_url("raw", path)),
                "head",
                Some(0),
            )
            .await?;
        let status = response.status().as_u16();
        match status {
            404 => {
                if Self::check_protocol(&response, path).is_ok() {
                    drop(response);
                    Ok(false)
                } else {
                    Self::check_response(response, path).await?;
                    unreachable!("an HTTP 404 cannot pass response validation")
                }
            }
            405 | 501 => {
                drop(response);
                self.has_raw_with_range(path).await
            }
            _ => {
                Self::check_response(response, path).await?;
                Ok(true)
            }
        }
    }

    async fn has_raw_with_range(&self, path: &str) -> Result<bool> {
        let response = self
            .send(
                self.probe_request(reqwest::Method::GET, self.raw_url("raw", path))
                    .header(reqwest::header::RANGE, "bytes=0-0"),
                "range_probe",
                Some(0),
            )
            .await?;
        let status = response.status().as_u16();
        match status {
            200 | 206 => {
                Self::check_protocol(&response, path)?;
                drop(response);
                Ok(true)
            }
            404 => {
                if Self::check_protocol(&response, path).is_ok() {
                    drop(response);
                    Ok(false)
                } else {
                    Self::check_response(response, path).await?;
                    unreachable!("an HTTP 404 cannot pass response validation")
                }
            }
            416 if response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"bytes */0")) =>
            {
                Self::check_protocol(&response, path)?;
                drop(response);
                Ok(true)
            }
            200..=299 => {
                Self::check_protocol(&response, path)?;
                drop(response);
                Err(RemoteErr::HttpStatus {
                    status,
                    path: path.to_string(),
                    message: "range existence probe returned an unexpected success status"
                        .to_string(),
                })
            }
            _ => {
                Self::check_response(response, path).await?;
                Err(RemoteErr::HttpStatus {
                    status,
                    path: path.to_string(),
                    message: "range existence probe returned an unexpected response".to_string(),
                })
            }
        }
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
            if Self::check_protocol(&response, path).is_ok() {
                Self::drain_response(response).await?;
                return Ok(None);
            }
            Self::check_response(response, path).await?;
            unreachable!("an HTTP 404 cannot pass response validation")
        }
        let response = Self::check_response(response, path).await?;
        Ok(Some(
            tracked_response_bytes(response, "get_body", TransferDirection::Download).await?,
        ))
    }

    async fn drain_response(response: reqwest::Response) -> Result<()> {
        let mut body = response.bytes_stream();
        while let Some(chunk) = body.next().await {
            chunk.map_err(|err| RemoteErr::http_transport("drain_body", err))?;
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
        tracked_response_bytes(response, "range_get_body", TransferDirection::Download).await
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
            let bytes = response
                .bytes()
                .await
                .map_err(|err| RemoteErr::http_transport("list_body", err))?;
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

    async fn download_upload_bundle(
        &self,
        ref_path: &str,
        root: &Path,
    ) -> Result<UploadBundleOutcome> {
        let response = self
            .send(
                self.request(
                    reqwest::Method::POST,
                    self.raw_url("upload-bundle", ref_path),
                )
                .header(reqwest::header::CONTENT_LENGTH, 0)
                .timeout(Duration::from_secs(30 * 60)),
                "upload_bundle",
                Some(0),
            )
            .await?;
        if matches!(response.status().as_u16(), 404 | 405)
            && Self::check_protocol(&response, ref_path).is_ok()
        {
            Self::drain_response(response).await?;
            return Ok(UploadBundleOutcome::Unsupported);
        }
        let response = Self::check_response(response, ref_path).await?;
        let manifest_bytes = upload_bundle_manifest_length(response.headers(), ref_path)?;
        let declared_total = upload_bundle_total_length(response.headers(), ref_path)?;
        let content_length = response.content_length();
        if declared_total.is_some() && content_length.is_some() && declared_total != content_length
        {
            return Err(upload_bundle_error(
                ref_path,
                "upload-bundle total length does not match Content-Length",
            ));
        }
        let total_bytes = declared_total.or(content_length);
        let mut body = HttpDownloadBody::new(response.bytes_stream(), total_bytes);
        let manifest = body.read_exact(manifest_bytes, ref_path).await?;
        let manifest = decode_upload_bundle_manifest(&manifest, ref_path)?;
        validate_upload_bundle_manifest(&manifest, ref_path)?;
        fs::create_dir_all(root)?;

        let mut previous_path: Option<String> = None;
        for _ in 0..manifest.objects {
            let header = body.read_exact(12, ref_path).await?;
            let path_bytes = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
            let object_bytes = u64::from_be_bytes(header[4..12].try_into().unwrap());
            if !(1..=MAX_UPLOAD_BUNDLE_PATH_BYTES).contains(&path_bytes) {
                return Err(upload_bundle_error(ref_path, "invalid object path length"));
            }
            let path = body.read_exact(path_bytes, ref_path).await?;
            let path = decode_upload_bundle_path(&path, previous_path.as_deref(), ref_path)?;
            let destination = root.join(&path);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)?;
            body.copy_exact(object_bytes, &mut file, ref_path).await?;
            file.flush()?;
            previous_path = Some(path);
        }
        body.require_end(ref_path).await?;
        write_upload_bundle_ref(root, &manifest.reference, ref_path)?;
        Ok(UploadBundleOutcome::Downloaded)
    }

    async fn get_raw_bundle(&self, paths: &[String]) -> Result<ReadBundleOutcome> {
        if paths.len() > MAX_READ_BUNDLE_OBJECTS {
            return Err(RemoteErr::HttpStatus {
                status: 413,
                path: "read-bundle".to_string(),
                message: format!("read-bundle exceeds {MAX_READ_BUNDLE_OBJECTS} objects"),
            });
        }
        let mut paths = paths.to_vec();
        paths.sort();
        paths.dedup();
        let manifest = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "paths": paths,
        }))
        .map_err(|err| RemoteErr::HttpStatus {
            status: 500,
            path: "read-bundle".to_string(),
            message: format!("failed to encode read-bundle manifest: {err}"),
        })?;
        let manifest_bytes = manifest.len() as u64;
        let response = self
            .send(
                self.request(reqwest::Method::POST, format!("{}/read-bundle", self.url))
                    .header(reqwest::header::CONTENT_LENGTH, manifest.len())
                    .body(manifest),
                "read_bundle",
                Some(manifest_bytes),
            )
            .await?;
        if matches!(response.status().as_u16(), 404 | 405 | 413)
            && Self::check_protocol(&response, "read-bundle").is_ok()
        {
            Self::drain_response(response).await?;
            return Ok(ReadBundleOutcome::Unsupported);
        }
        let response = Self::check_response(response, "read-bundle").await?;
        let objects = response
            .headers()
            .get(READ_BUNDLE_HEADER_OBJECTS)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|objects| *objects == paths.len())
            .ok_or_else(|| upload_bundle_error("read-bundle", "invalid object count"))?;
        let declared_total = upload_bundle_total_length(response.headers(), "read-bundle")?;
        let content_length = response.content_length();
        if declared_total.is_some() && content_length.is_some() && declared_total != content_length
        {
            return Err(upload_bundle_error(
                "read-bundle",
                "read-bundle total length does not match Content-Length",
            ));
        }
        let total_bytes = declared_total.or(content_length);
        if total_bytes.is_some_and(|bytes| bytes > MAX_READ_BUNDLE_RESPONSE_BYTES) {
            return Err(upload_bundle_error(
                "read-bundle",
                "read-bundle response exceeds the client limit",
            ));
        }
        let mut body = HttpDownloadBody::new(response.bytes_stream(), total_bytes);
        let expected = paths.into_iter().collect::<BTreeSet<_>>();
        let mut decoded = BTreeMap::new();
        let mut previous_path: Option<String> = None;
        for _ in 0..objects {
            let header = body.read_exact(12, "read-bundle").await?;
            let path_bytes = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
            let object_bytes = u64::from_be_bytes(header[4..12].try_into().unwrap());
            if !(1..=MAX_UPLOAD_BUNDLE_PATH_BYTES).contains(&path_bytes)
                || object_bytes > MAX_READ_BUNDLE_RESPONSE_BYTES
            {
                return Err(upload_bundle_error("read-bundle", "invalid object frame"));
            }
            let path = body.read_exact(path_bytes, "read-bundle").await?;
            let path = decode_upload_bundle_path(&path, previous_path.as_deref(), "read-bundle")?;
            let object_bytes = usize::try_from(object_bytes)
                .map_err(|_| upload_bundle_error("read-bundle", "object is too large"))?;
            let bytes = Bytes::from(body.read_exact(object_bytes, "read-bundle").await?);
            if !expected.contains(&path) || decoded.insert(path.clone(), bytes).is_some() {
                return Err(upload_bundle_error(
                    "read-bundle",
                    "response contains an unexpected object",
                ));
            }
            previous_path = Some(path);
        }
        body.require_end("read-bundle").await?;
        if decoded.len() != expected.len() {
            return Err(upload_bundle_error(
                "read-bundle",
                "response is missing a requested object",
            ));
        }
        Ok(ReadBundleOutcome::Downloaded(decoded))
    }

    async fn put_raw(&self, path: &str, bytes: Bytes) -> Result<()> {
        let request_bytes = bytes.len() as u64;
        let response = self
            .send(
                self.upload_request(reqwest::Method::PUT, self.raw_url("raw", path))
                    .body(tracked_upload_body(vec![bytes], request_bytes)),
                "put",
                Some(request_bytes),
            )
            .await?;
        Self::check_response(response, path).await?;
        Ok(())
    }

    async fn put_raw_if_not_exists(&self, path: &str, bytes: Bytes) -> Result<()> {
        self.put_raw_if_not_exists_chunks(path, vec![bytes]).await
    }

    async fn put_raw_if_not_exists_stream<I: IntoIterator<Item = Bytes>>(
        &self,
        path: &str,
        chunks: I,
    ) -> Result<()> {
        self.put_raw_if_not_exists_chunks(path, chunks.into_iter().collect())
            .await
    }

    async fn put_raw_if_not_exists_chunks(&self, path: &str, chunks: Vec<Bytes>) -> Result<()> {
        let content_length = chunks.iter().try_fold(0_usize, |total, chunk| {
            total
                .checked_add(chunk.len())
                .ok_or_else(|| RemoteErr::HttpStatus {
                    status: 413,
                    path: path.to_string(),
                    message: "streamed upload length exceeds usize".to_string(),
                })
        })?;
        if let Some(part_bytes) = self.multipart_part_bytes(content_length).await? {
            return self
                .put_raw_if_not_exists_multipart(path, &chunks, content_length, part_bytes)
                .await;
        }

        let body = tracked_upload_body(chunks, content_length as u64);
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

    async fn put_raw_if_not_exists_multipart(
        &self,
        path: &str,
        chunks: &[Bytes],
        content_length: usize,
        part_bytes: usize,
    ) -> Result<()> {
        let response = self
            .send(
                self.upload_request(reqwest::Method::POST, self.raw_url("multipart-start", path))
                    .header(reqwest::header::CONTENT_LENGTH, 0)
                    .header(MULTIPART_HEADER_OBJECT_BYTES, content_length),
                "multipart_start",
                Some(0),
            )
            .await?;
        let response = Self::check_response(response, path).await?;
        let response_bytes = response
            .bytes()
            .await
            .map_err(|err| RemoteErr::http_transport("multipart_start_body", err))?;
        let upload: HttpMultipartStartResponse =
            serde_json::from_slice(&response_bytes).map_err(|err| RemoteErr::HttpStatus {
                status: 502,
                path: path.to_string(),
                message: format!("invalid multipart-start response: {err}"),
            })?;
        validate_multipart_start(&upload, content_length, part_bytes, path)?;
        let uploaded_parts = upload
            .uploaded_parts
            .iter()
            .map(|part| (part.part_number, part.bytes))
            .collect::<HashMap<_, _>>();
        let parts = multipart_chunks(chunks, part_bytes);
        for (index, part) in parts.iter().enumerate() {
            let part_number = index + 1;
            let length = part.iter().map(Bytes::len).sum::<usize>();
            if uploaded_parts.get(&part_number) == Some(&length) {
                continue;
            }
            self.put_multipart_part(path, &upload.upload_id, part_number, part, length)
                .await?;
        }

        let completion = self
            .send(
                self.upload_request(
                    reqwest::Method::POST,
                    self.raw_url("multipart-complete", path),
                )
                .header(reqwest::header::CONTENT_LENGTH, 0)
                .header(MULTIPART_HEADER_UPLOAD_ID, &upload.upload_id),
                "multipart_complete",
                Some(0),
            )
            .await;
        let response = match completion {
            Ok(response) => response,
            Err(error) => {
                return match self.has_raw(path).await {
                    Ok(true) => Ok(()),
                    _ => Err(error),
                };
            }
        };
        if response.status().is_success() {
            Self::check_response(response, path).await?;
            return Ok(());
        }
        let status = response.status().as_u16();
        let error = Self::check_response(response, path).await.unwrap_err();
        if matches!(status, 500..=599) && self.has_raw(path).await.unwrap_or(false) {
            return Ok(());
        }
        Err(error)
    }

    async fn put_multipart_part(
        &self,
        path: &str,
        upload_id: &str,
        part_number: usize,
        chunks: &[Bytes],
        content_length: usize,
    ) -> Result<()> {
        let mut last_error = None;
        for attempt in 0..MULTIPART_PART_ATTEMPTS {
            let request_chunks = chunks.to_vec();
            let body = tracked_upload_body(request_chunks, content_length as u64);
            let response = self
                .send(
                    self.upload_request(reqwest::Method::PUT, self.raw_url("multipart-part", path))
                        .header(reqwest::header::CONTENT_LENGTH, content_length)
                        .header(MULTIPART_HEADER_UPLOAD_ID, upload_id)
                        .header(MULTIPART_HEADER_PART_NUMBER, part_number)
                        .body(body),
                    "multipart_part",
                    Some(content_length as u64),
                )
                .await;
            match response {
                Ok(response) if response.status().is_success() => {
                    Self::check_response(response, path).await?;
                    return Ok(());
                }
                Ok(response)
                    if attempt + 1 < MULTIPART_PART_ATTEMPTS
                        && matches!(response.status().as_u16(), 429 | 500..=599) =>
                {
                    last_error = Some(Self::check_response(response, path).await.unwrap_err());
                }
                Ok(response) => {
                    Self::check_response(response, path).await?;
                    unreachable!("non-success multipart response passed validation")
                }
                Err(error) if attempt + 1 < MULTIPART_PART_ATTEMPTS => {
                    last_error = Some(error);
                }
                Err(error) => return Err(error),
            }
            tokio::time::sleep(Duration::from_millis(100 * (attempt as u64 + 1))).await;
        }
        Err(last_error.unwrap())
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
        if self
            .aggregate_request_requires_multipart(content_length)
            .await?
        {
            return Ok(HttpReceivePackResult::RetryIndividually);
        }
        let body = tracked_upload_body(
            vec![pack.pack.clone(), pack.index.clone()],
            content_length as u64,
        );
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
        if matches!(response.status().as_u16(), 404 | 405)
            && Self::check_protocol(&response, ref_path).is_ok()
        {
            Self::drain_response(response).await?;
            return Ok(HttpReceivePackResult::Unsupported);
        }
        if response.status().as_u16() == 413 {
            Self::drain_response(response).await?;
            return Ok(HttpReceivePackResult::RetryIndividually);
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
        if self
            .aggregate_request_requires_multipart(content_length)
            .await?
        {
            return Ok(HttpReceivePackResult::RetryIndividually);
        }
        let chunks = std::iter::once(Bytes::from(manifest.clone()))
            .chain(
                objects
                    .iter()
                    .flat_map(|object| object.chunks.iter().cloned()),
            )
            .chain([pack.pack.clone(), pack.index.clone()])
            .collect::<Vec<_>>();
        let body = tracked_upload_body(chunks, content_length as u64);
        let response = self
            .send(
                // A receive-bundle is the only mutation after the ref read in the fast path.
                // Reuse that connection like Git smart HTTP; legacy PUTs and fallbacks retain
                // the isolated upload pool because mixed proxy traffic previously stalled.
                self.publication_request(
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
        match response.status().as_u16() {
            413 => {
                // An edge proxy can reject the aggregate body before it reaches
                // Graft, so this response is not expected to carry Graft-Protocol.
                // Retry through immutable per-object writes, then publish the pack
                // and ref with the normal receive-pack/CAS path.
                Self::drain_response(response).await?;
                return Ok(HttpReceivePackResult::RetryIndividually);
            }
            404 | 405 if Self::check_protocol(&response, ref_path).is_ok() => {
                Self::drain_response(response).await?;
                return Ok(HttpReceivePackResult::Unsupported);
            }
            412 if Self::check_protocol(&response, ref_path).is_ok() => {
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

type DownloadStream =
    Pin<Box<dyn Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send + 'static>>;

struct HttpDownloadBody {
    stream: DownloadStream,
    buffered: Bytes,
    progress: Option<TransferProgressHandle>,
}

impl HttpDownloadBody {
    fn new(
        stream: impl Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send + 'static,
        total_bytes: Option<u64>,
    ) -> Self {
        Self {
            stream: Box::pin(stream),
            buffered: Bytes::new(),
            progress: begin_transfer_progress(TransferDirection::Download, total_bytes),
        }
    }

    async fn read_exact(&mut self, length: usize, path: &str) -> Result<Vec<u8>> {
        let mut output = Vec::with_capacity(length);
        while output.len() < length {
            let chunk = self
                .next_chunk()
                .await?
                .ok_or_else(|| upload_bundle_error(path, "upload-bundle response is truncated"))?;
            let needed = length - output.len();
            if chunk.len() <= needed {
                output.extend_from_slice(&chunk);
            } else {
                output.extend_from_slice(&chunk[..needed]);
                self.buffered = chunk.slice(needed..);
            }
        }
        Ok(output)
    }

    async fn copy_exact(
        &mut self,
        mut remaining: u64,
        file: &mut fs::File,
        path: &str,
    ) -> Result<()> {
        while remaining != 0 {
            let chunk = self
                .next_chunk()
                .await?
                .ok_or_else(|| upload_bundle_error(path, "upload-bundle object is truncated"))?;
            let take = usize::try_from(remaining.min(chunk.len() as u64)).unwrap();
            file.write_all(&chunk[..take])?;
            remaining -= take as u64;
            if take != chunk.len() {
                self.buffered = chunk.slice(take..);
            }
        }
        Ok(())
    }

    async fn require_end(&mut self, path: &str) -> Result<()> {
        if self.next_chunk().await?.is_some() {
            return Err(upload_bundle_error(
                path,
                "upload-bundle response has trailing bytes",
            ));
        }
        Ok(())
    }

    async fn next_chunk(&mut self) -> Result<Option<Bytes>> {
        if !self.buffered.is_empty() {
            return Ok(Some(std::mem::take(&mut self.buffered)));
        }
        loop {
            match self.stream.next().await {
                Some(Ok(bytes)) if bytes.is_empty() => {}
                Some(Ok(bytes)) => {
                    if let Some(progress) = self.progress.as_mut() {
                        progress.advance(bytes.len() as u64);
                    }
                    return Ok(Some(bytes));
                }
                Some(Err(err)) => {
                    return Err(RemoteErr::http_transport("upload_bundle_body", err));
                }
                None => {
                    if let Some(progress) = self.progress.as_mut() {
                        progress.finish();
                    }
                    return Ok(None);
                }
            }
        }
    }
}

fn tracked_upload_body(chunks: Vec<Bytes>, total_bytes: u64) -> reqwest::Body {
    let mut progress = begin_transfer_progress(TransferDirection::Upload, Some(total_bytes));
    reqwest::Body::wrap_stream(stream::iter(chunks.into_iter().map(move |bytes| {
        if let Some(progress) = progress.as_mut() {
            progress.advance(bytes.len() as u64);
        }
        Ok::<Bytes, std::io::Error>(bytes)
    })))
}

fn planned_bundle_upload_bytes(
    objects: &[RemoteBundleObject],
    pack: Option<&RemoteObjectPack>,
    path: &str,
) -> Result<u64> {
    let mut total = 0_u64;
    for bytes in objects.iter().map(|object| object.content_length).chain(
        pack.into_iter()
            .flat_map(|pack| [pack.pack.len(), pack.index.len()]),
    ) {
        total = total
            .checked_add(u64::try_from(bytes).map_err(|_| RemoteErr::HttpStatus {
                status: 413,
                path: path.to_string(),
                message: "planned upload length exceeds u64".to_string(),
            })?)
            .ok_or_else(|| RemoteErr::HttpStatus {
                status: 413,
                path: path.to_string(),
                message: "planned upload length exceeds u64".to_string(),
            })?;
    }
    Ok(total)
}

async fn tracked_response_bytes(
    response: reqwest::Response,
    operation: &'static str,
    direction: TransferDirection,
) -> Result<Bytes> {
    let total_bytes = response.content_length();
    let mut progress = begin_transfer_progress(direction, total_bytes);
    let mut body = response.bytes_stream();
    let mut output = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|err| RemoteErr::http_transport(operation, err))?;
        if let Some(progress) = progress.as_mut() {
            progress.advance(chunk.len() as u64);
        }
        output.extend_from_slice(&chunk);
    }
    if let Some(progress) = progress.as_mut() {
        progress.finish();
    }
    Ok(Bytes::from(output))
}

fn upload_bundle_manifest_length(
    headers: &reqwest::header::HeaderMap,
    path: &str,
) -> Result<usize> {
    let value = headers
        .get(RECEIVE_BUNDLE_HEADER_MANIFEST_BYTES)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|length| (1..=MAX_UPLOAD_BUNDLE_MANIFEST_BYTES).contains(length));
    value.ok_or_else(|| upload_bundle_error(path, "invalid upload-bundle manifest length"))
}

fn upload_bundle_total_length(
    headers: &reqwest::header::HeaderMap,
    path: &str,
) -> Result<Option<u64>> {
    let Some(value) = headers.get(UPLOAD_BUNDLE_HEADER_TOTAL_BYTES) else {
        return Ok(None);
    };
    let length = value
        .to_str()
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|length| *length > 0)
        .ok_or_else(|| upload_bundle_error(path, "invalid upload-bundle total length"))?;
    Ok(Some(length))
}

fn decode_upload_bundle_manifest(bytes: &[u8], path: &str) -> Result<UploadBundleManifest> {
    serde_json::from_slice(bytes)
        .map_err(|err| upload_bundle_error(path, format!("invalid upload-bundle manifest: {err}")))
}

fn validate_upload_bundle_manifest(manifest: &UploadBundleManifest, path: &str) -> Result<()> {
    if manifest.version != 1 {
        return Err(upload_bundle_error(
            path,
            "unsupported upload-bundle manifest version",
        ));
    }
    if manifest.reference.path != path {
        return Err(upload_bundle_error(
            path,
            "upload-bundle reference path does not match request",
        ));
    }
    if manifest.objects > MAX_UPLOAD_BUNDLE_OBJECTS {
        return Err(upload_bundle_error(
            path,
            "upload-bundle contains too many objects",
        ));
    }
    validate_upload_bundle_path(path, true)
        .map_err(|message| upload_bundle_error(path, message))?;
    let _ = decode_lower_hex(&manifest.reference.value_hex, path)?;
    Ok(())
}

fn decode_upload_bundle_path(
    bytes: &[u8],
    previous: Option<&str>,
    request: &str,
) -> Result<String> {
    let path = std::str::from_utf8(bytes)
        .map_err(|_| upload_bundle_error(request, "upload-bundle object path is not UTF-8"))?;
    validate_upload_bundle_path(path, false)
        .map_err(|message| upload_bundle_error(request, message))?;
    if previous.is_some_and(|previous| previous.as_bytes() >= path.as_bytes()) {
        return Err(upload_bundle_error(
            request,
            "upload-bundle object paths are not ordered",
        ));
    }
    Ok(path.to_string())
}

fn validate_upload_bundle_path(
    path: &str,
    transactional: bool,
) -> std::result::Result<(), &'static str> {
    if path.is_empty() || path.len() > MAX_UPLOAD_BUNDLE_PATH_BYTES || path.contains('\\') {
        return Err("upload-bundle contains an invalid path");
    }
    if path
        .split('/')
        .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
        || path.chars().any(char::is_control)
        || Path::new(path)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("upload-bundle contains an unsafe path");
    }
    let is_transactional = path == "HEAD" || path.starts_with("refs/");
    if is_transactional != transactional || path == "locks" || path.starts_with("locks/") {
        return Err("upload-bundle path has the wrong storage class");
    }
    Ok(())
}

fn write_upload_bundle_ref(
    root: &Path,
    reference: &UploadBundleReference,
    request: &str,
) -> Result<()> {
    let bytes = decode_lower_hex(&reference.value_hex, request)?;
    let destination = root.join(&reference.path);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    file.write_all(&bytes)?;
    file.flush()?;
    Ok(())
}

fn decode_lower_hex(value: &str, path: &str) -> Result<Vec<u8>> {
    if value.len() > 32 * 1024
        || !value.len().is_multiple_of(2)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(upload_bundle_error(
            path,
            "upload-bundle reference is not lowercase hexadecimal",
        ));
    }
    (0..value.len())
        .step_by(2)
        .map(|offset| u8::from_str_radix(&value[offset..offset + 2], 16))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| {
            upload_bundle_error(path, "upload-bundle reference is not lowercase hexadecimal")
        })
}

fn upload_bundle_error(path: &str, message: impl Into<String>) -> RemoteErr {
    RemoteErr::HttpStatus {
        status: 502,
        path: path.to_string(),
        message: message.into(),
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

fn validate_multipart_start(
    upload: &HttpMultipartStartResponse,
    total_bytes: usize,
    part_bytes: usize,
    path: &str,
) -> Result<()> {
    if upload.upload_id.is_empty()
        || upload.upload_id.len() > 1_024
        || upload.upload_id.chars().any(char::is_control)
        || upload.total_bytes != total_bytes
        || upload.part_bytes != part_bytes
    {
        return Err(RemoteErr::HttpStatus {
            status: 502,
            path: path.to_string(),
            message: "multipart-start returned an invalid upload session".to_string(),
        });
    }
    let part_count = total_bytes.div_ceil(part_bytes);
    let mut previous = 0;
    for part in &upload.uploaded_parts {
        if part.part_number <= previous || part.part_number > part_count {
            return Err(RemoteErr::HttpStatus {
                status: 502,
                path: path.to_string(),
                message: "multipart-start returned invalid uploaded parts".to_string(),
            });
        }
        let expected = if part.part_number == part_count {
            total_bytes - part_bytes * (part_count - 1)
        } else {
            part_bytes
        };
        if part.bytes != expected {
            return Err(RemoteErr::HttpStatus {
                status: 502,
                path: path.to_string(),
                message: "multipart-start returned an invalid uploaded part size".to_string(),
            });
        }
        previous = part.part_number;
    }
    Ok(())
}

fn multipart_chunks(chunks: &[Bytes], part_bytes: usize) -> Vec<Vec<Bytes>> {
    let total_bytes = chunks.iter().map(Bytes::len).sum::<usize>();
    let mut parts = Vec::with_capacity(total_bytes.div_ceil(part_bytes));
    let mut part = Vec::new();
    let mut part_len = 0;
    for chunk in chunks {
        let mut offset = 0;
        while offset < chunk.len() {
            let take = (part_bytes - part_len).min(chunk.len() - offset);
            part.push(chunk.slice(offset..offset + take));
            part_len += take;
            offset += take;
            if part_len == part_bytes {
                parts.push(std::mem::take(&mut part));
                part_len = 0;
            }
        }
    }
    if part_len != 0 {
        parts.push(part);
    }
    parts
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

    #[tokio::test]
    async fn read_bundle_chunks_requests_above_the_server_object_limit() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requested = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_http_request(&mut stream).await;
                let header_end = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .unwrap()
                    + 4;
                let manifest: serde_json::Value =
                    serde_json::from_slice(&request[header_end..]).unwrap();
                let paths = manifest["paths"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|path| path.as_str().unwrap().to_string())
                    .collect::<Vec<_>>();
                let objects = paths
                    .iter()
                    .map(|path| (path.as_str(), b"x".as_slice()))
                    .collect::<Vec<_>>();
                let body = encode_test_upload_bundle(&[], &objects);
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nGraft-Protocol: 1\r\n{READ_BUNDLE_HEADER_OBJECTS}: {}\r\n{UPLOAD_BUNDLE_HEADER_TOTAL_BYTES}: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    paths.len(),
                    body.len(),
                    body.len()
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                stream.write_all(&body).await.unwrap();
                requested.push(paths.len());
            }
            requested
        });

        let remote = RemoteConfig::Http {
            url: format!("http://{address}/org/repo"),
            token_env: None,
        }
        .build()
        .unwrap();
        let paths = (0..=MAX_READ_BUNDLE_OBJECTS)
            .map(|index| format!("objects/{index:04}"))
            .collect::<Vec<_>>();
        let ReadBundleOutcome::Downloaded(objects) = remote.get_raw_bundle(&paths).await.unwrap()
        else {
            panic!("read-bundle unexpectedly fell back");
        };
        assert_eq!(objects.len(), paths.len());
        assert_eq!(server.await.unwrap(), vec![MAX_READ_BUNDLE_OBJECTS, 1]);
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

    async fn serve_http_messages(
        responses: &[&str],
    ) -> (String, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
        let responses = responses
            .iter()
            .map(|response| response.as_bytes().to_vec())
            .collect::<Vec<_>>();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::with_capacity(responses.len());
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                requests.push(read_http_request(&mut stream).await);
                stream.write_all(&response).await.unwrap();
            }
            requests
        });
        (format!("http://{address}/org/repo"), task)
    }

    async fn serve_lost_publication_response(
        reconciled_ref: Option<&[u8]>,
    ) -> (String, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
        let reconciled_ref = reconciled_ref.map(ToOwned::to_owned);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut publication_stream, _) = listener.accept().await.unwrap();
            let publication = read_http_request(&mut publication_stream).await;
            drop(publication_stream);

            let (mut reconciliation_stream, _) = listener.accept().await.unwrap();
            let reconciliation = read_http_request(&mut reconciliation_stream).await;
            match reconciled_ref {
                Some(value) => {
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\nGraft-Protocol: 1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        value.len()
                    );
                    reconciliation_stream
                        .write_all(headers.as_bytes())
                        .await
                        .unwrap();
                    reconciliation_stream.write_all(&value).await.unwrap();
                }
                None => {
                    reconciliation_stream
                        .write_all(
                            b"HTTP/1.1 404 Not Found\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                }
            }
            vec![publication, reconciliation]
        });
        (format!("http://{address}/org/repo"), task)
    }

    async fn serve_upload_bundle(
        manifest: serde_json::Value,
        objects: &[(&str, &[u8])],
    ) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        serve_upload_bundle_with_total_header(manifest, objects, UPLOAD_BUNDLE_HEADER_TOTAL_BYTES)
            .await
    }

    async fn serve_upload_bundle_with_total_header(
        manifest: serde_json::Value,
        objects: &[(&str, &[u8])],
        total_header: &'static str,
    ) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        let manifest = serde_json::to_vec(&manifest).unwrap();
        let body = encode_test_upload_bundle(&manifest, objects);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            let headers = format!(
                "HTTP/1.1 200 OK\r\nGraft-Protocol: 1\r\n{RECEIVE_BUNDLE_HEADER_MANIFEST_BYTES}: {}\r\n{total_header}: {}\r\nConnection: close\r\n\r\n",
                manifest.len(),
                body.len(),
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            for chunk in body.chunks(3) {
                stream.write_all(chunk).await.unwrap();
            }
            request
        });
        (format!("http://{address}/org/repo"), task)
    }

    fn encode_test_upload_bundle(manifest: &[u8], objects: &[(&str, &[u8])]) -> Vec<u8> {
        let mut body = manifest.to_vec();
        for (path, bytes) in objects {
            body.extend_from_slice(&(path.len() as u32).to_be_bytes());
            body.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            body.extend_from_slice(path.as_bytes());
            body.extend_from_slice(bytes);
        }
        body
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
    async fn upload_bundle_downloads_one_stream_into_a_local_remote() {
        let manifest = serde_json::json!({
            "version": 1,
            "reference": {
                "path": "refs/heads/main",
                "value_hex": hex_encode(b"commit-one\n"),
            },
            "objects": 2,
        });
        let (url, request) = serve_upload_bundle(
            manifest,
            &[
                ("objects/pack/example.idx", b"index"),
                ("objects/pack/example.pack", b"pack"),
            ],
        )
        .await;
        let remote = RemoteConfig::Http { url, token_env: None }.build().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        let reporter = crate::repo::TransferProgressReporter::new(move |progress| {
            captured.lock().unwrap().push(progress);
        });
        let scope = crate::repo::TransferProgressScope::enter(&reporter);

        assert!(matches!(
            remote
                .download_upload_bundle("refs/heads/main", destination.path())
                .await
                .unwrap(),
            UploadBundleOutcome::Downloaded
        ));
        assert_eq!(
            fs::read(destination.path().join("refs/heads/main")).unwrap(),
            b"commit-one\n"
        );
        assert_eq!(
            fs::read(destination.path().join("objects/pack/example.idx")).unwrap(),
            b"index"
        );
        assert_eq!(
            fs::read(destination.path().join("objects/pack/example.pack")).unwrap(),
            b"pack"
        );
        drop(scope);
        let last = events.lock().unwrap().last().copied().unwrap();
        assert_eq!(last.direction, TransferDirection::Download);
        assert_eq!(last.total_bytes, Some(last.transferred_bytes));
        assert!(last.transferred_bytes > 0);
        assert!(
            request
                .await
                .unwrap()
                .starts_with(b"POST /org/repo/upload-bundle/refs/heads/main ")
        );
    }

    #[tokio::test]
    async fn upload_bundle_uses_content_length_from_an_older_remote() {
        let manifest = serde_json::json!({
            "version": 1,
            "reference": {
                "path": "refs/heads/main",
                "value_hex": hex_encode(b"commit-one\n"),
            },
            "objects": 0,
        });
        let (url, request) =
            serve_upload_bundle_with_total_header(manifest, &[], "Content-Length").await;
        let remote = RemoteConfig::Http { url, token_env: None }.build().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        let reporter = crate::repo::TransferProgressReporter::new(move |progress| {
            captured.lock().unwrap().push(progress);
        });
        let scope = crate::repo::TransferProgressScope::enter(&reporter);

        assert!(matches!(
            remote
                .download_upload_bundle("refs/heads/main", destination.path())
                .await
                .unwrap(),
            UploadBundleOutcome::Downloaded
        ));
        drop(scope);
        let last = events.lock().unwrap().last().copied().unwrap();
        assert_eq!(last.total_bytes, Some(last.transferred_bytes));
        request.await.unwrap();
    }

    #[tokio::test]
    async fn upload_bundle_rejects_paths_outside_the_destination() {
        let manifest = serde_json::json!({
            "version": 1,
            "reference": {
                "path": "refs/heads/main",
                "value_hex": hex_encode(b"commit-one\n"),
            },
            "objects": 1,
        });
        let (url, request) = serve_upload_bundle(manifest, &[("../escaped", b"bad")]).await;
        let remote = RemoteConfig::Http { url, token_env: None }.build().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("bundle");

        assert!(matches!(
            remote
                .download_upload_bundle("refs/heads/main", &destination)
                .await,
            Err(RemoteErr::HttpStatus { status: 502, .. })
        ));
        assert!(!parent.path().join("escaped").exists());
        request.await.unwrap();
    }

    #[tokio::test]
    async fn upload_bundle_falls_back_when_the_remote_does_not_support_it() {
        let (url, request) = serve_http_response("404 Not Found", &["1"]).await;
        let remote = RemoteConfig::Http { url, token_env: None }.build().unwrap();
        let destination = tempfile::tempdir().unwrap();

        assert!(matches!(
            remote
                .download_upload_bundle("refs/heads/main", destination.path())
                .await
                .unwrap(),
            UploadBundleOutcome::Unsupported
        ));
        assert!(
            request
                .await
                .unwrap()
                .starts_with("POST /org/repo/upload-bundle/refs/heads/main ")
        );
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
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        let reporter = crate::repo::TransferProgressReporter::new(move |progress| {
            captured.lock().unwrap().push(progress);
        });
        let scope = crate::repo::TransferProgressScope::enter(&reporter);
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

        drop(scope);
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
        let events = events.lock().unwrap();
        assert!(
            events
                .windows(2)
                .all(|pair| { pair[0].total_bytes.unwrap() <= pair[1].total_bytes.unwrap() })
        );
        assert_eq!(events.last().unwrap().transferred_bytes, 14);
        assert_eq!(events.last().unwrap().total_bytes, Some(14));
    }

    #[tokio::test]
    async fn receive_pack_reconciles_a_lost_success_response_from_the_remote_ref() {
        let (url, requests) = serve_lost_publication_response(Some(b"new\n")).await;
        let remote = RemoteConfig::Http { url, token_env: None }.build().unwrap();

        remote
            .publish_object_pack_and_ref(
                Some(RemoteObjectPack::new(
                    "9".repeat(64),
                    Bytes::from_static(b"pack"),
                    Bytes::from_static(b"idx"),
                )),
                "refs/heads/main",
                Some(b"old\n"),
                "new\n",
            )
            .await
            .unwrap();

        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            String::from_utf8_lossy(&requests[0])
                .starts_with("POST /org/repo/receive-pack/refs/heads/main ")
        );
        assert!(
            String::from_utf8_lossy(&requests[1]).starts_with("GET /org/repo/raw/refs/heads/main ")
        );
    }

    #[tokio::test]
    async fn receive_pack_treats_an_identical_concurrent_publication_as_success() {
        let responses = [
            "HTTP/1.1 409 Conflict\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 200 OK\r\nGraft-Protocol: 1\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnew\n",
        ];
        let (url, requests) = serve_http_messages(&responses).await;
        let remote = RemoteConfig::Http { url, token_env: None }.build().unwrap();

        remote
            .publish_object_pack_and_ref(
                Some(RemoteObjectPack::new(
                    "7".repeat(64),
                    Bytes::from_static(b"pack"),
                    Bytes::from_static(b"idx"),
                )),
                "refs/heads/main",
                Some(b"old\n"),
                "new\n",
            )
            .await
            .unwrap();

        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            String::from_utf8_lossy(&requests[1]).starts_with("GET /org/repo/raw/refs/heads/main ")
        );
    }

    #[tokio::test]
    async fn fallback_cas_reconciles_a_lost_success_response_from_the_remote_ref() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for status in ["404 Not Found", "204 No Content", "204 No Content"] {
                let (mut stream, _) = listener.accept().await.unwrap();
                requests.push(read_http_request(&mut stream).await);
                let response = format!(
                    "HTTP/1.1 {status}\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }

            let (mut cas_stream, _) = listener.accept().await.unwrap();
            requests.push(read_http_request(&mut cas_stream).await);
            drop(cas_stream);

            let (mut reconciliation_stream, _) = listener.accept().await.unwrap();
            requests.push(read_http_request(&mut reconciliation_stream).await);
            reconciliation_stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nGraft-Protocol: 1\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnew\n",
                )
                .await
                .unwrap();
            requests
        });
        let remote = RemoteConfig::Http {
            url: format!("http://{address}/org/repo"),
            token_env: None,
        }
        .build()
        .unwrap();

        remote
            .publish_object_pack_and_ref(
                Some(RemoteObjectPack::new(
                    "8".repeat(64),
                    Bytes::from_static(b"pack"),
                    Bytes::from_static(b"idx"),
                )),
                "refs/heads/main",
                Some(b"old\n"),
                "new\n",
            )
            .await
            .unwrap();

        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 5);
        assert!(
            String::from_utf8_lossy(&requests[0])
                .starts_with("POST /org/repo/receive-pack/refs/heads/main ")
        );
        assert!(
            String::from_utf8_lossy(&requests[3])
                .starts_with("POST /org/repo/cas/refs/heads/main ")
        );
        assert!(
            String::from_utf8_lossy(&requests[4]).starts_with("GET /org/repo/raw/refs/heads/main ")
        );
    }

    #[tokio::test]
    async fn sequential_bundle_uploads_report_one_stable_total() {
        let (url, requests) =
            serve_http_exchanges(&["204 No Content", "204 No Content", "204 No Content"]).await;
        let remote = RemoteConfig::Http { url, token_env: None }.build().unwrap();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        let reporter = crate::repo::TransferProgressReporter::new(move |progress| {
            captured.lock().unwrap().push(progress);
        });
        let scope = crate::repo::TransferProgressScope::enter(&reporter);

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
                None,
                "refs/heads/main",
                None,
                "new\n",
            )
            .await
            .unwrap();

        drop(scope);
        {
            let events = events.lock().unwrap();
            assert!(!events.is_empty());
            assert!(events.iter().all(|event| {
                event.direction == TransferDirection::Upload && event.total_bytes == Some(13)
            }));
            assert_eq!(events.first().unwrap().transferred_bytes, 0);
            assert_eq!(events.last().unwrap().transferred_bytes, 13);
            assert!(events.len() <= 3);
        }
        assert_eq!(requests.await.unwrap().len(), 3);
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
    async fn receive_bundle_is_not_bounded_by_the_read_request_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            stream
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            request
        });
        let client = build_http_client().unwrap();
        let remote = Remote {
            backend: RemoteBackend::Http(HttpRemote::with_clients_and_request_timeout(
                format!("http://{address}/org/repo"),
                None,
                client.clone(),
                client.clone(),
                client,
                Duration::from_millis(25),
            )),
        };
        let payload = Bytes::from(vec![7_u8; 2 * 1024 * 1024]);

        remote
            .publish_object_bundle_and_ref(
                vec![
                    RemoteBundleObject::new("segments/slow".to_string(), [payload.clone()], true)
                        .unwrap(),
                ],
                Some(RemoteObjectPack::new(
                    "f".repeat(64),
                    Bytes::from_static(b"pack"),
                    Bytes::from_static(b"idx"),
                )),
                "refs/heads/main",
                None,
                "new\n",
            )
            .await
            .unwrap();

        let request = server.await.unwrap();
        assert!(
            String::from_utf8_lossy(&request)
                .starts_with("POST /org/repo/receive-bundle/refs/heads/main ")
        );
        assert_eq!(
            http_request_body(&request).len(),
            payload.len()
                + b"packidx".len()
                + serde_json::to_vec(&serde_json::json!({
                    "version": 1,
                    "objects": [{
                        "path": "segments/slow",
                        "bytes": payload.len(),
                        "allow_existing": true,
                    }],
                }))
                .unwrap()
                .len()
        );
    }

    #[tokio::test]
    async fn receive_bundle_reconciles_a_lost_success_response_from_the_remote_ref() {
        let (url, requests) = serve_lost_publication_response(Some(b"new\n")).await;
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
                    "1".repeat(64),
                    Bytes::from_static(b"pack"),
                    Bytes::from_static(b"idx"),
                )),
                "refs/heads/main",
                Some(b"old\n"),
                "new\n",
            )
            .await
            .unwrap();

        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            String::from_utf8_lossy(&requests[0])
                .starts_with("POST /org/repo/receive-bundle/refs/heads/main ")
        );
        assert!(
            String::from_utf8_lossy(&requests[1]).starts_with("GET /org/repo/raw/refs/heads/main ")
        );
    }

    #[tokio::test]
    async fn receive_bundle_reconciles_a_timed_out_response_from_the_remote_ref() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut publication_stream, _) = listener.accept().await.unwrap();
            let publication = read_http_request(&mut publication_stream).await;

            let (mut reconciliation_stream, _) = listener.accept().await.unwrap();
            let reconciliation = read_http_request(&mut reconciliation_stream).await;
            reconciliation_stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nGraft-Protocol: 1\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnew\n",
                )
                .await
                .unwrap();
            drop(publication_stream);
            vec![publication, reconciliation]
        });
        let timed_publication_client = reqwest::ClientBuilder::new()
            .http1_only()
            .connect_timeout(Duration::from_secs(1))
            .timeout(Duration::from_millis(25))
            .build()
            .unwrap();
        let fallback_client = build_http_client().unwrap();
        let remote = Remote {
            backend: RemoteBackend::Http(HttpRemote::with_clients_and_request_timeout(
                format!("http://{address}/org/repo"),
                None,
                timed_publication_client,
                fallback_client.clone(),
                fallback_client,
                Duration::from_secs(1),
            )),
        };

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
                    "5".repeat(64),
                    Bytes::from_static(b"pack"),
                    Bytes::from_static(b"idx"),
                )),
                "refs/heads/main",
                Some(b"old\n"),
                "new\n",
            )
            .await
            .unwrap();

        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            String::from_utf8_lossy(&requests[1]).starts_with("GET /org/repo/raw/refs/heads/main ")
        );
    }

    #[tokio::test]
    async fn receive_bundle_reports_retryable_unconfirmed_when_remote_ref_is_unchanged() {
        let (url, requests) = serve_lost_publication_response(Some(b"old\n")).await;
        let remote = RemoteConfig::Http { url, token_env: None }.build().unwrap();

        let error = remote
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
                    "2".repeat(64),
                    Bytes::from_static(b"pack"),
                    Bytes::from_static(b"idx"),
                )),
                "refs/heads/main",
                Some(b"old\n"),
                "new\n",
            )
            .await
            .unwrap_err();

        assert!(error.publication_unconfirmed(), "{error:?}");
        assert_eq!(requests.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn receive_bundle_reports_conflict_when_reconciliation_finds_another_ref() {
        let (url, requests) = serve_lost_publication_response(Some(b"other\n")).await;
        let remote = RemoteConfig::Http { url, token_env: None }.build().unwrap();

        let error = remote
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
                    "3".repeat(64),
                    Bytes::from_static(b"pack"),
                    Bytes::from_static(b"idx"),
                )),
                "refs/heads/main",
                Some(b"old\n"),
                "new\n",
            )
            .await
            .unwrap_err();

        assert!(
            matches!(error, RemoteErr::CompareAndSwap { .. }),
            "{error:?}"
        );
        assert_eq!(requests.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn receive_bundle_preserves_unknown_outcome_when_reconciliation_also_fails() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_http_request(&mut stream).await;
                drop(stream);
            }
        });
        let remote = RemoteConfig::Http {
            url: format!("http://{address}/org/repo"),
            token_env: None,
        }
        .build()
        .unwrap();

        let error = remote
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
                    "4".repeat(64),
                    Bytes::from_static(b"pack"),
                    Bytes::from_static(b"idx"),
                )),
                "refs/heads/main",
                Some(b"old\n"),
                "new\n",
            )
            .await
            .unwrap_err();

        assert!(error.publication_outcome_unknown(), "{error:?}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn receive_bundle_falls_back_to_objects_then_receive_pack() {
        let (url, requests) =
            serve_http_exchanges(&["404 Not Found", "204 No Content", "204 No Content"]).await;
        let remote = RemoteConfig::Http { url, token_env: None }.build().unwrap();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        let reporter = crate::repo::TransferProgressReporter::new(move |progress| {
            captured.lock().unwrap().push(progress);
        });
        let scope = crate::repo::TransferProgressScope::enter(&reporter);
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

        drop(scope);
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
        let events = events.lock().unwrap();
        assert!(
            events
                .windows(2)
                .all(|pair| { pair[0].total_bytes.unwrap() <= pair[1].total_bytes.unwrap() })
        );
        let last = events.last().unwrap();
        assert_eq!(last.transferred_bytes, last.total_bytes.unwrap());
    }

    #[tokio::test]
    async fn receive_bundle_fallback_accepts_an_allow_existing_put_race() {
        let responses = [
            "HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 412 Precondition Failed\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 204 No Content\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ];
        let (url, requests) = serve_http_messages(&responses).await;
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
    async fn receive_bundle_preserves_edge_statuses_without_protocol_headers() {
        for (response_status, expected_status) in [
            ("401 Unauthorized", 401),
            ("403 Forbidden", 403),
            ("502 Bad Gateway", 502),
        ] {
            let response = format!(
                "HTTP/1.1 {response_status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let (url, requests) = serve_http_messages(&[&response]).await;
            let remote = RemoteConfig::Http { url, token_env: None }.build().unwrap();

            let error = remote
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
                        "f".repeat(64),
                        Bytes::from_static(b"pack"),
                        Bytes::from_static(b"idx"),
                    )),
                    "refs/heads/main",
                    None,
                    "new\n",
                )
                .await
                .unwrap_err();

            assert!(
                matches!(error, RemoteErr::HttpStatus { status, .. } if status == expected_status),
                "{error:?}"
            );
            assert_eq!(requests.await.unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn receive_bundle_preserves_payload_too_large_for_an_individual_object() {
        let responses = [
            "HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ];
        let (url, requests) = serve_http_messages(&responses).await;
        let remote = RemoteConfig::Http { url, token_env: None }.build().unwrap();

        let error = remote
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
                    "1".repeat(64),
                    Bytes::from_static(b"pack"),
                    Bytes::from_static(b"idx"),
                )),
                "refs/heads/main",
                None,
                "new\n",
            )
            .await
            .unwrap_err();

        assert!(
            matches!(error, RemoteErr::HttpStatus { status: 413, .. }),
            "{error:?}"
        );
        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            String::from_utf8_lossy(&requests[1])
                .starts_with("PUT /org/repo/raw-if-not-exists/segments/example")
        );
    }

    #[tokio::test]
    async fn receive_bundle_fallback_reconciles_a_lost_receive_pack_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();

            let (mut bundle_stream, _) = listener.accept().await.unwrap();
            requests.push(read_http_request(&mut bundle_stream).await);
            bundle_stream
                .write_all(
                    b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();

            let (mut object_stream, _) = listener.accept().await.unwrap();
            requests.push(read_http_request(&mut object_stream).await);
            object_stream
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();

            let (mut publication_stream, _) = listener.accept().await.unwrap();
            requests.push(read_http_request(&mut publication_stream).await);
            drop(publication_stream);

            let (mut reconciliation_stream, _) = listener.accept().await.unwrap();
            requests.push(read_http_request(&mut reconciliation_stream).await);
            reconciliation_stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nGraft-Protocol: 1\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnew\n",
                )
                .await
                .unwrap();
            requests
        });
        let remote = RemoteConfig::Http {
            url: format!("http://{address}/org/repo"),
            token_env: None,
        }
        .build()
        .unwrap();

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
                    "2".repeat(64),
                    Bytes::from_static(b"pack"),
                    Bytes::from_static(b"idx"),
                )),
                "refs/heads/main",
                Some(b"old\n"),
                "new\n",
            )
            .await
            .unwrap();

        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 4);
        assert!(
            String::from_utf8_lossy(&requests[2])
                .starts_with("POST /org/repo/receive-pack/refs/heads/main ")
        );
        assert!(
            String::from_utf8_lossy(&requests[3]).starts_with("GET /org/repo/raw/refs/heads/main ")
        );
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
    async fn resetting_repository_credentials_starts_a_fresh_probe_connection() {
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
    async fn repository_credentials_separate_read_probe_and_upload_connections() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut read_stream, _) = listener.accept().await.unwrap();
            let read_request = read_http_request(&mut read_stream).await;
            read_stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\n\r\n",
                )
                .await
                .unwrap();

            let (mut probe_stream, _) =
                tokio::time::timeout(Duration::from_secs(1), listener.accept())
                    .await
                    .expect("probe reused the read connection")
                    .unwrap();
            let probe_request = read_http_request(&mut probe_stream).await;
            probe_stream
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\n\r\n",
                )
                .await
                .unwrap();

            let (mut upload_stream, _) =
                tokio::time::timeout(Duration::from_secs(1), listener.accept())
                    .await
                    .expect("upload reused the read or probe connection")
                    .unwrap();
            let upload_request = read_http_request(&mut upload_stream).await;
            upload_stream
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            (read_request, probe_request, upload_request)
        });
        let remote = RemoteConfig::Http {
            url: format!("http://{address}/org/repo"),
            token_env: None,
        }
        .build_with_credentials("origin", &RemoteCredentials::explicit())
        .unwrap();

        assert!(remote.get_raw("refs/heads/main").await.unwrap().is_none());
        assert!(remote.has_segment(&SegmentId::random()).await.unwrap());
        remote
            .put_raw_if_not_exists("objects/one", Bytes::from_static(b"one"))
            .await
            .unwrap();
        let (read_request, probe_request, upload_request) = server.await.unwrap();
        assert!(String::from_utf8_lossy(&read_request).starts_with("GET "));
        assert!(String::from_utf8_lossy(&probe_request).starts_with("HEAD "));
        assert!(String::from_utf8_lossy(&upload_request).starts_with("PUT "));
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
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        let reporter = crate::repo::TransferProgressReporter::new(move |progress| {
            captured.lock().unwrap().push(progress);
        });
        let scope = crate::repo::TransferProgressScope::enter(&reporter);

        remote
            .put_raw_if_not_exists_stream(
                "segments/example",
                [Bytes::from_static(b"abc"), Bytes::from_static(b"de")],
            )
            .await
            .unwrap();
        drop(scope);
        let last = events.lock().unwrap().last().copied().unwrap();
        assert_eq!(last.direction, TransferDirection::Upload);
        assert_eq!(last.transferred_bytes, 5);
        assert_eq!(last.total_bytes, Some(5));
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
    async fn large_http_upload_uses_and_resumes_multipart_parts() {
        let descriptor = serde_json::json!({
            "protocol": "graft-remote",
            "version": 1,
            "repository": "org/repo",
            "capabilities": ["multipart-object"],
            "limits": {
                "max_request_bytes": 64 * 1024,
                "multipart_part_bytes": 32 * 1024,
            },
        })
        .to_string();
        let start = serde_json::json!({
            "upload_id": "upload-1",
            "total_bytes": 70 * 1024,
            "part_bytes": 32 * 1024,
            "uploaded_parts": [{ "part_number": 1, "bytes": 32 * 1024 }],
        })
        .to_string();
        let json_response = |body: &str| {
            format!(
                "HTTP/1.1 200 OK\r\nGraft-Protocol: 1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        };
        let responses = [
            json_response(&descriptor),
            json_response(&start),
            "HTTP/1.1 204 No Content\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
            "HTTP/1.1 204 No Content\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
            "HTTP/1.1 204 No Content\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
        ];
        let response_refs = responses.iter().map(String::as_str).collect::<Vec<_>>();
        let (url, requests) = serve_http_messages(&response_refs).await;
        let mut remote = HttpRemote::new(url, None).unwrap();
        remote.multipart_discovery_threshold = 64 * 1024;
        let payload = Bytes::from(vec![7_u8; 70 * 1024]);

        remote
            .put_raw_if_not_exists_stream("segments/large", [payload.clone()])
            .await
            .unwrap();

        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 5);
        assert!(String::from_utf8_lossy(&requests[0]).starts_with("GET /org/repo HTTP/1.1"));
        assert!(
            String::from_utf8_lossy(&requests[1])
                .starts_with("POST /org/repo/multipart-start/segments/large")
        );
        assert!(
            String::from_utf8_lossy(&requests[2])
                .lines()
                .any(|line| line.eq_ignore_ascii_case("x-graft-part-number: 2"))
        );
        assert_eq!(
            http_request_body(&requests[2]),
            &payload[32 * 1024..64 * 1024]
        );
        assert!(
            String::from_utf8_lossy(&requests[3])
                .lines()
                .any(|line| line.eq_ignore_ascii_case("x-graft-part-number: 3"))
        );
        assert_eq!(http_request_body(&requests[3]), &payload[64 * 1024..]);
        assert!(
            String::from_utf8_lossy(&requests[4])
                .starts_with("POST /org/repo/multipart-complete/segments/large")
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
            Err(RemoteErr::HttpStatus { status: 404, .. })
        ));
        request.await.unwrap();

        let (url, request) = serve_http_response("404 Not Found", &["1"]).await;
        let remote = HttpRemote::new(url, None).unwrap();
        assert!(!remote.has_raw("objects/one").await.unwrap());
        request.await.unwrap();
    }

    #[tokio::test]
    async fn http_remote_falls_back_to_range_get_when_head_is_not_supported() {
        for (head_status, fallback_status) in [
            ("405 Method Not Allowed", "206 Partial Content"),
            ("501 Not Implemented", "200 OK"),
        ] {
            let (url, requests) = serve_http_exchanges(&[head_status, fallback_status]).await;
            let remote = HttpRemote::new(url, None).unwrap();

            assert!(remote.has_raw("segments/example").await.unwrap());

            let requests = requests.await.unwrap();
            let head = String::from_utf8_lossy(&requests[0]);
            let fallback = String::from_utf8_lossy(&requests[1]);
            assert!(head.starts_with("HEAD /org/repo/raw/segments/example "));
            assert!(fallback.starts_with("GET /org/repo/raw/segments/example "));
            assert!(
                fallback
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case("Range: bytes=0-0"))
            );
        }
    }

    #[tokio::test]
    async fn http_remote_head_probe_succeeds_without_fallback() {
        let (url, request) = serve_http_response("200 OK", &["1"]).await;
        let remote = HttpRemote::new(url, None).unwrap();

        assert!(remote.has_raw("segments/example").await.unwrap());
        assert!(
            request
                .await
                .unwrap()
                .starts_with("HEAD /org/repo/raw/segments/example ")
        );
    }

    #[tokio::test]
    async fn http_remote_head_probe_does_not_fallback_on_other_statuses() {
        for (response_status, expected_status) in [
            ("401 Unauthorized", 401),
            ("403 Forbidden", 403),
            ("500 Internal Server Error", 500),
        ] {
            let (url, request) = serve_http_response(response_status, &["1"]).await;
            let remote = HttpRemote::new(url, None).unwrap();
            let error = remote.has_raw("segments/example").await.unwrap_err();

            assert!(
                matches!(error, RemoteErr::HttpStatus { status, .. } if status == expected_status)
            );
            assert!(
                request
                    .await
                    .unwrap()
                    .starts_with("HEAD /org/repo/raw/segments/example ")
            );
        }

        let (url, request) = serve_http_response("404 Not Found", &["1"]).await;
        let remote = HttpRemote::new(url, None).unwrap();
        assert!(!remote.has_raw("segments/example").await.unwrap());
        request.await.unwrap();

        let responses = [
            "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 206 Partial Content\r\nGraft-Protocol: 1\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx",
        ];
        let (url, requests) = serve_http_messages(&responses).await;
        let remote = HttpRemote::new(url, None).unwrap();
        assert!(remote.has_raw("segments/example").await.unwrap());
        assert_eq!(requests.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn http_remote_range_probe_preserves_missing_and_error_statuses() {
        let (url, requests) =
            serve_http_exchanges(&["405 Method Not Allowed", "404 Not Found"]).await;
        let remote = HttpRemote::new(url, None).unwrap();
        assert!(!remote.has_raw("segments/example").await.unwrap());
        assert_eq!(requests.await.unwrap().len(), 2);

        for (fallback_status, expected_status) in [
            ("204 No Content", 204),
            ("401 Unauthorized", 401),
            ("500 Internal Server Error", 500),
        ] {
            let (url, requests) =
                serve_http_exchanges(&["405 Method Not Allowed", fallback_status]).await;
            let remote = HttpRemote::new(url, None).unwrap();
            let error = remote.has_raw("segments/example").await.unwrap_err();

            assert!(
                matches!(error, RemoteErr::HttpStatus { status, .. } if status == expected_status)
            );
            assert_eq!(requests.await.unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn http_remote_head_transport_error_is_preserved_without_retry() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            drop(stream);
            let no_retry = tokio::time::timeout(Duration::from_millis(250), listener.accept())
                .await
                .is_err();
            (request, no_retry)
        });
        let remote = HttpRemote::new(format!("http://{address}/org/repo"), None).unwrap();

        assert!(matches!(
            remote.has_raw("segments/example").await,
            Err(RemoteErr::HttpTransport { .. })
        ));
        let (request, no_retry) = server.await.unwrap();
        assert!(String::from_utf8_lossy(&request).starts_with("HEAD "));
        assert!(no_retry, "HEAD transport error was retried");
    }

    #[tokio::test]
    async fn http_remote_range_probe_transport_error_is_preserved() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut head_stream, _) = listener.accept().await.unwrap();
            read_http_request(&mut head_stream).await;
            head_stream
                .write_all(
                    b"HTTP/1.1 405 Method Not Allowed\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            drop(head_stream);

            let (get_stream, _) = listener.accept().await.unwrap();
            drop(get_stream);
        });
        let remote = HttpRemote::new(format!("http://{address}/org/repo"), None).unwrap();

        assert!(matches!(
            remote.has_raw("segments/example").await,
            Err(RemoteErr::HttpTransport { .. })
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_remote_range_probe_recognizes_an_empty_segment() {
        let responses = [
            "HTTP/1.1 405 Method Not Allowed\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 416 Range Not Satisfiable\r\nGraft-Protocol: 1\r\nContent-Range: bytes */0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ];
        let (url, requests) = serve_http_messages(&responses).await;
        let remote = HttpRemote::new(url, None).unwrap();

        assert!(remote.has_raw("segments/empty").await.unwrap());
        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            String::from_utf8_lossy(&requests[1])
                .lines()
                .any(|line| line.eq_ignore_ascii_case("Range: bytes=0-0"))
        );
    }

    #[tokio::test]
    async fn http_remote_range_probe_does_not_read_a_full_segment() {
        const LARGE_SEGMENT_BYTES: usize = 16 * 1024 * 1024;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut head_stream, _) = listener.accept().await.unwrap();
            read_http_request(&mut head_stream).await;
            head_stream
                .write_all(
                    b"HTTP/1.1 405 Method Not Allowed\r\nGraft-Protocol: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            drop(head_stream);

            let (mut get_stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut get_stream).await;
            let headers = format!(
                "HTTP/1.1 200 OK\r\nGraft-Protocol: 1\r\nContent-Length: {LARGE_SEGMENT_BYTES}\r\n\r\n"
            );
            get_stream.write_all(headers.as_bytes()).await.unwrap();
            get_stream.write_all(&[7]).await.unwrap();

            let mut buffer = [0_u8; 1];
            let released = matches!(
                tokio::time::timeout(Duration::from_secs(2), get_stream.read(&mut buffer)).await,
                Ok(Ok(0))
            );
            (request, released)
        });
        let remote = HttpRemote::new(format!("http://{address}/org/repo"), None).unwrap();

        let exists = tokio::time::timeout(Duration::from_secs(1), remote.has_raw("segments/large"))
            .await
            .expect("range probe waited for the full segment")
            .unwrap();
        assert!(exists);

        let (request, released) = server.await.unwrap();
        assert!(
            String::from_utf8_lossy(&request)
                .lines()
                .any(|line| line.eq_ignore_ascii_case("Range: bytes=0-0"))
        );
        assert!(released, "range probe response body was not released");
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
