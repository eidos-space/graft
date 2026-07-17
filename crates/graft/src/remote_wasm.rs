use std::{
    collections::{BTreeMap, btree_map::Entry},
    ops::Range,
    sync::{Arc, Mutex},
};

use bilrost::{Message, OwnedMessage};
use bytes::Bytes;
use futures::{Stream, stream};
use serde::{Deserialize, Serialize};
use thiserror::Error;

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
}

#[derive(Debug, Clone, Default)]
pub struct Remote {
    objects: Arc<Mutex<BTreeMap<String, Bytes>>>,
}

impl Remote {
    pub fn with_config(config: RemoteConfig) -> Result<Self> {
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
}

fn commit_path(log: &LogId, lsn: LSN) -> String {
    format!("logs/{}/commits/{lsn}", log.serialize())
}

fn segment_path(sid: &SegmentId) -> String {
    format!("segments/{}", sid.serialize())
}
