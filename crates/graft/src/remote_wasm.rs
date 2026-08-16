use std::{
    collections::{BTreeMap, btree_map::Entry},
    fmt,
    ops::Range,
    path::Path,
    sync::{Arc, Mutex},
};

use bilrost::{Message, OwnedMessage};
use bytes::Bytes;
use futures::{Stream, stream};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::core::{LogId, SegmentId, commit::Commit, lsn::LSN};

#[path = "remote/segment.rs"]
pub mod segment;

#[derive(Debug, Error)]
pub enum RemoteErr {
    #[error("Failed to decode file: {0}")]
    Decode(#[from] bilrost::DecodeError),

    #[error("remote object `{path}` was not found")]
    NotFound { path: String },

    #[error("remote object `{path}` already exists")]
    Precondition { path: String },

    #[error("remote lock `{path}` is already held")]
    LockBusy { path: String },

    #[error("remote object `{path}` changed during compare-and-swap")]
    CompareAndSwap { path: String },

    #[error("invalid byte range for remote object `{path}`")]
    InvalidRange { path: String },

    #[error("{0} remotes are not available in the browser demo")]
    UnsupportedInBrowser(&'static str),

    #[error("bundled remote object `{path}` is too large")]
    BundledObjectTooLarge { path: String },
}

impl RemoteErr {
    pub fn precondition_failed(&self) -> bool {
        matches!(self, Self::Precondition { .. })
    }

    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound { .. })
    }
}

pub type Result<T> = std::result::Result<T, RemoteErr>;

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum RemoteCredentialErr {
    #[error("remote credential name must be a non-empty repository remote name")]
    InvalidRemoteName,

    #[error("HTTP bearer token must not be empty")]
    EmptyBearerToken,

    #[error("environment-backed remote credentials cannot be changed")]
    EnvironmentCredentialsReadOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteCredentialMode {
    Environment,
    Explicit,
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

#[derive(Clone)]
pub struct RemoteCredentials {
    mode: RemoteCredentialMode,
    http_bearer_tokens: Arc<Mutex<BTreeMap<String, BearerToken>>>,
}

impl fmt::Debug for RemoteCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteCredentials")
            .field("mode", &self.mode)
            .field(
                "http_bearer_token_count",
                &self
                    .http_bearer_tokens
                    .lock()
                    .expect("browser credential mutex poisoned")
                    .len(),
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
    pub fn explicit() -> Self {
        Self {
            mode: RemoteCredentialMode::Explicit,
            http_bearer_tokens: Arc::default(),
        }
    }

    pub fn environment() -> Self {
        Self {
            mode: RemoteCredentialMode::Environment,
            http_bearer_tokens: Arc::default(),
        }
    }

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
            .lock()
            .expect("browser credential mutex poisoned")
            .insert(remote_name.to_string(), BearerToken::new(token));
        Ok(())
    }

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
        self.http_bearer_tokens
            .lock()
            .expect("browser credential mutex poisoned")
            .remove(remote_name);
        Ok(())
    }

    pub fn redact(&self, message: &str) -> String {
        self.http_bearer_tokens
            .lock()
            .expect("browser credential mutex poisoned")
            .values()
            .fold(message.to_string(), |redacted, token| {
                redacted.replace(token.expose(), "[redacted]")
            })
    }

    pub fn reset_http_clients(&self) {}
}

fn valid_credential_remote_name(remote_name: &str) -> bool {
    !remote_name.is_empty()
        && !remote_name
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        && !remote_name.contains(['/', '\\'])
}

#[derive(Debug, Deserialize, Serialize, Default, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteConfig {
    #[default]
    Memory,
    Fs {
        root: String,
    },
    S3Compatible {
        bucket: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        endpoint: Option<String>,
    },
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

    pub fn build_with_credentials(
        self,
        remote_name: &str,
        credentials: &RemoteCredentials,
    ) -> Result<Remote> {
        Remote::with_config_and_credentials(self, remote_name, credentials)
    }
}

#[derive(Debug)]
pub(crate) struct RemoteObjectPack {
    pack_path: String,
    pack: Bytes,
    index_path: String,
    index: Bytes,
}

impl RemoteObjectPack {
    pub(crate) fn new(id: String, pack: Bytes, index: Bytes) -> Self {
        Self {
            pack_path: format!("objects/pack/{id}.pack"),
            pack,
            index_path: format!("objects/pack/{id}.idx"),
            index,
        }
    }
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
                .ok_or_else(|| RemoteErr::BundledObjectTooLarge { path: path.clone() })
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

pub(crate) enum UploadBundleOutcome {
    Downloaded,
    Unsupported,
}

#[derive(Debug, Clone, Default)]
pub struct Remote {
    objects: Arc<Mutex<BTreeMap<String, Bytes>>>,
}

pub(crate) enum ReadBundleOutcome {
    Downloaded(BTreeMap<String, Bytes>),
    Unsupported,
}

impl Remote {
    pub(crate) fn snapshot_upload_concurrency(&self) -> usize {
        5
    }

    pub fn with_config(config: RemoteConfig) -> Result<Self> {
        Self::with_config_and_credentials(config, "", &RemoteCredentials::environment())
    }

    pub fn with_config_and_credentials(
        config: RemoteConfig,
        _remote_name: &str,
        _credentials: &RemoteCredentials,
    ) -> Result<Self> {
        match config {
            RemoteConfig::Memory => Ok(Self::default()),
            RemoteConfig::Fs { .. } => Err(RemoteErr::UnsupportedInBrowser("filesystem")),
            RemoteConfig::S3Compatible { .. } => Err(RemoteErr::UnsupportedInBrowser("S3")),
            RemoteConfig::Http { .. } => Err(RemoteErr::UnsupportedInBrowser("HTTP")),
        }
    }

    pub fn stream_commits_ordered<I: IntoIterator<Item = LSN>>(
        &self,
        log: &LogId,
        lsns: I,
    ) -> impl Stream<Item = Result<Commit>> {
        let objects = self.objects.lock().expect("browser remote mutex poisoned");
        let mut commits = Vec::new();
        for lsn in lsns {
            let Some(bytes) = objects.get(&commit_path(log, lsn)) else {
                break;
            };
            commits.push(Commit::decode(bytes.clone()).map_err(Into::into));
        }
        stream::iter(commits)
    }

    pub async fn get_commit(&self, log: &LogId, lsn: LSN) -> Result<Option<Commit>> {
        self.get_raw(&commit_path(log, lsn))
            .await?
            .map(Commit::decode)
            .transpose()
            .map_err(Into::into)
    }

    pub(crate) async fn get_raw_bundle(&self, paths: &[String]) -> Result<ReadBundleOutcome> {
        let mut objects = BTreeMap::new();
        for path in paths {
            if let Some(bytes) = self.get_raw(path).await? {
                objects.insert(path.clone(), bytes);
            }
        }
        Ok(ReadBundleOutcome::Downloaded(objects))
    }

    pub async fn put_commit(&self, commit: &Commit) -> Result<()> {
        self.put_raw_if_not_exists(
            &commit_path(commit.log(), commit.lsn()),
            commit.encode_to_bytes(),
        )
        .await
    }

    pub async fn put_segment<I: IntoIterator<Item = Bytes>>(
        &self,
        sid: &SegmentId,
        chunks: I,
    ) -> Result<()> {
        let bytes = chunks
            .into_iter()
            .flat_map(|chunk| chunk.to_vec())
            .collect::<Vec<_>>();
        match self
            .put_raw_if_not_exists(&segment_path(sid), Bytes::from(bytes))
            .await
        {
            Ok(()) | Err(RemoteErr::Precondition { .. }) => Ok(()),
            Err(err) => Err(err),
        }
    }

    pub async fn has_segment(&self, sid: &SegmentId) -> Result<bool> {
        Ok(self.get_raw(&segment_path(sid)).await?.is_some())
    }

    pub async fn get_segment_range(&self, sid: &SegmentId, bytes: Range<u64>) -> Result<Bytes> {
        self.get_raw_range(&segment_path(sid), bytes).await
    }

    pub async fn get_raw(&self, path: &str) -> Result<Option<Bytes>> {
        Ok(self
            .objects
            .lock()
            .expect("browser remote mutex poisoned")
            .get(path)
            .cloned())
    }

    pub(crate) async fn has_raw(&self, path: &str) -> Result<bool> {
        Ok(self
            .objects
            .lock()
            .expect("browser remote mutex poisoned")
            .contains_key(path))
    }

    pub(crate) async fn download_upload_bundle(
        &self,
        _ref_path: &str,
        _root: &Path,
    ) -> Result<UploadBundleOutcome> {
        Ok(UploadBundleOutcome::Unsupported)
    }

    pub async fn get_raw_range(&self, path: &str, bytes: Range<u64>) -> Result<Bytes> {
        let value = self
            .get_raw(path)
            .await?
            .ok_or_else(|| RemoteErr::NotFound { path: path.to_string() })?;
        let start = usize::try_from(bytes.start).ok();
        let end = usize::try_from(bytes.end).ok();
        match (start, end) {
            (Some(start), Some(end)) if start <= end && end <= value.len() => {
                Ok(value.slice(start..end))
            }
            _ => Err(RemoteErr::InvalidRange { path: path.to_string() }),
        }
    }

    pub async fn list_raw(&self, prefix: &str) -> Result<Vec<String>> {
        Ok(self
            .objects
            .lock()
            .expect("browser remote mutex poisoned")
            .keys()
            .filter(|path| path.starts_with(prefix))
            .cloned()
            .collect())
    }

    pub async fn put_raw(&self, path: &str, bytes: impl Into<Bytes>) -> Result<()> {
        self.objects
            .lock()
            .expect("browser remote mutex poisoned")
            .insert(path.to_string(), bytes.into());
        Ok(())
    }

    pub async fn delete_raw(&self, path: &str) -> Result<()> {
        let removed = self
            .objects
            .lock()
            .expect("browser remote mutex poisoned")
            .remove(path);
        if removed.is_none() {
            return Err(RemoteErr::NotFound { path: path.to_string() });
        }
        Ok(())
    }

    pub async fn put_raw_if_not_exists(&self, path: &str, bytes: impl Into<Bytes>) -> Result<()> {
        match self
            .objects
            .lock()
            .expect("browser remote mutex poisoned")
            .entry(path.to_string())
        {
            Entry::Vacant(entry) => {
                entry.insert(bytes.into());
                Ok(())
            }
            Entry::Occupied(_) => Err(RemoteErr::Precondition { path: path.to_string() }),
        }
    }

    pub async fn compare_and_swap_raw(
        &self,
        path: &str,
        expected: Option<&[u8]>,
        bytes: impl Into<Bytes>,
    ) -> Result<()> {
        let mut objects = self.objects.lock().expect("browser remote mutex poisoned");
        if objects.get(path).map(Bytes::as_ref) != expected {
            return Err(RemoteErr::CompareAndSwap { path: path.to_string() });
        }
        objects.insert(path.to_string(), bytes.into());
        Ok(())
    }

    pub async fn compare_and_delete_raw(&self, path: &str, expected: Option<&[u8]>) -> Result<()> {
        let mut objects = self.objects.lock().expect("browser remote mutex poisoned");
        if objects.get(path).map(Bytes::as_ref) != expected {
            return Err(RemoteErr::CompareAndSwap { path: path.to_string() });
        }
        objects.remove(path);
        Ok(())
    }

    async fn put_object_pack(&self, pack: &RemoteObjectPack) -> Result<()> {
        for (path, bytes) in [
            (&pack.pack_path, pack.pack.clone()),
            (&pack.index_path, pack.index.clone()),
        ] {
            match self.put_raw_if_not_exists(path, bytes).await {
                Ok(()) | Err(RemoteErr::Precondition { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub(crate) async fn publish_object_bundle_and_ref(
        &self,
        objects: Vec<RemoteBundleObject>,
        pack: Option<RemoteObjectPack>,
        ref_path: &str,
        expected: Option<&[u8]>,
        replacement: impl Into<Bytes>,
    ) -> Result<()> {
        for object in objects {
            let bytes = object.bytes();
            match self
                .put_raw_if_not_exists(&object.path, bytes.clone())
                .await
            {
                Ok(()) => {}
                Err(RemoteErr::Precondition { .. }) if object.allow_existing => {}
                Err(RemoteErr::Precondition { .. }) => {
                    if self.get_raw(&object.path).await?.as_ref() != Some(&bytes) {
                        return Err(RemoteErr::Precondition { path: object.path });
                    }
                }
                Err(error) => return Err(error),
            }
        }
        if let Some(pack) = pack.as_ref() {
            self.put_object_pack(pack).await?;
        }
        self.compare_and_swap_raw(ref_path, expected, replacement)
            .await
    }
}

fn commit_path(log: &LogId, lsn: LSN) -> String {
    format!("logs/{}/commits/{lsn}", log.serialize())
}

fn segment_path(sid: &SegmentId) -> String {
    format!("segments/{}", sid.serialize())
}
