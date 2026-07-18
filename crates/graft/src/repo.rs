use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Display},
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::{
    collections::HashMap,
    os::unix::fs::MetadataExt,
    sync::{Mutex, OnceLock},
};

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt, stream};

mod artifacts;
mod config;
mod config_methods;
mod history;
pub mod index;
mod inventory;
mod merge;
pub mod object;
mod refs;
mod refspec;
mod remote_objects;
mod staging;
mod sync;
mod worktree;
mod worktree_state;

pub use config::{
    CONFIG_KEY_FILES_EXTERNAL_PATHS, CONFIG_KEY_FILES_INLINE_TEXT_THRESHOLD,
    CONFIG_KEY_MERGE_DEFAULT_SEMANTIC_KEYS, CONFIG_KEY_MERGE_GENERATED_COLUMNS_PREFIX,
    CONFIG_KEY_MERGE_INTERNAL_RESOLVERS_PREFIX, CONFIG_KEY_MERGE_SCHEMA_RESOLVERS_PREFIX,
    CONFIG_KEY_MERGE_SEMANTIC_KEYS_PREFIX, CONFIG_KEY_TRACK_DEFAULT_ROOTS,
    CONFIG_KEY_TRACK_USER_ROOTS, CONFIG_KEY_WORKTREE_MATERIALIZE_SQLITE, FileConfig, MergeConfig,
    RepoConfig, RepoConfigEntry, TrackConfig, WorktreeConfig,
};
pub use object::CommitTableSummary;

use config::{
    config_entries, config_entry, config_generated_columns_table, config_internal_resolver_subject,
    config_schema_resolver_operation, config_semantic_keys_table, normalize_config_key,
    parse_config_bool_value, parse_config_byte_unit_value, parse_config_internal_resolver_value,
    parse_config_schema_resolver_value, parse_config_string_list_value,
};
use refspec::{ParsedRefspec, parse_fetch_refspec, parse_push_refspec};
use worktree::{
    IgnoreRules, artifact_storage_for_path, classify_artifact_bytes, classify_artifact_path,
    config_path_patterns_match, is_sqlite_database_file, is_sqlite_sidecar_file,
    normalize_repo_path, normalize_repo_path_key,
};

pub use worktree::validate_repo_path_identity;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    core::{
        LogId, VolumeId, byte_unit::ByteUnit, commit_hash::CommitHash, lsn::LSN, lsn::LSNRangeExt,
        page_count::PageCount,
    },
    remote::{RemoteConfig, RemoteErr},
    snapshot::Snapshot,
};

pub const GRAFT_DIR: &str = ".graft";
pub const GRAFT_IGNORE_FILE: &str = ".graftignore";
pub const REPOSITORY_FORMAT_VERSION: u32 = 2;
pub const OBJECT_FORMAT: &str = "blake3";
pub const DEFAULT_TEXT_DIFF_CONTENT_LIMIT: ByteUnit = ByteUnit::MB;
const NULL_OBJECT_ID: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const REFLOG_ACTOR: &str = "Graft <graft@example.invalid>";
const DEFAULT_LARGE_FILE_THRESHOLD: ByteUnit = ByteUnit::MB;
#[cfg(unix)]
const ARTIFACT_STAT_CACHE_MAX_ENTRIES: usize = 100_000;

const CONFIG_FILE: &str = "config.toml";
const HEAD_FILE: &str = "HEAD";
const MERGE_HEAD_FILE: &str = "MERGE_HEAD";
const ORIG_HEAD_FILE: &str = "ORIG_HEAD";
const DIR_REFS_HEADS: &str = "refs/heads";
const DIR_REFS_REMOTES: &str = "refs/remotes";
const DIR_REFS_TAGS: &str = "refs/tags";
const DIR_OBJECTS: &str = "objects";
const DIR_OBJECTS_PACK: &str = "objects/pack";
const DIR_STORE_FJALL: &str = "store/fjall";
const DIR_STORE_FILES: &str = "store/files";
const DIR_INDEX: &str = "index";
const DIR_LOCKS: &str = "locks";
const DIR_TMP: &str = "tmp";
const DIR_LOGS_REFS: &str = "logs/refs";
const DIR_LOGS_HEAD: &str = "logs/HEAD";
const SQLITE_DATABASE_MAGIC: &[u8; 16] = b"SQLite format 3\0";
const CONTENT_CLASS_SAMPLE_BYTES: usize = 8192;
const REMOTE_REF_READ_CONCURRENCY: usize = 5;
const REMOTE_OBJECT_PACK_VERSION: u32 = 1;
const REMOTE_OBJECT_PACK_MAGIC: &[u8] = b"graft-object-pack-v1\n";

#[derive(Debug, Error)]
pub enum RepoErr {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to decode TOML: {0}")]
    TomlDe(#[from] toml::de::Error),

    #[error("failed to encode TOML: {0}")]
    TomlSer(#[from] toml::ser::Error),

    #[error("no .graft repository found from {0}")]
    NotFound(PathBuf),

    #[error("unsupported repository format version {actual}; expected {expected}")]
    UnsupportedFormat { expected: u32, actual: u32 },

    #[error("unsupported object format `{actual}`; expected `{expected}`")]
    UnsupportedObjectFormat {
        expected: &'static str,
        actual: String,
    },

    #[error("invalid ref name `{0}`")]
    InvalidRefName(String),

    #[error("cannot create ref `{reference}` because `{existing}` already exists")]
    RefNameConflict { reference: String, existing: String },

    #[error("invalid remote name `{0}`")]
    InvalidRemoteName(String),

    #[error("invalid HEAD contents: {0}")]
    InvalidHead(String),

    #[error("branch `{0}` does not exist")]
    BranchNotFound(String),

    #[error("branch `{0}` already exists")]
    BranchExists(String),

    #[error("cannot delete current branch `{0}`")]
    BranchIsCurrent(String),

    #[error("branch `{branch}` is not fully merged")]
    BranchNotMerged { branch: String, target: String },

    #[error("tag `{0}` already exists")]
    TagExists(String),

    #[error("tag `{0}` does not exist")]
    TagNotFound(String),

    #[error("remote `{0}` already exists")]
    RemoteExists(String),

    #[error("HEAD does not point at a commit yet")]
    UnbornHead,

    #[error("no changes added to commit")]
    NoStagedChanges,

    #[error("cannot commit with unresolved index conflicts")]
    UnresolvedConflicts,

    #[error("merge already in progress")]
    MergeInProgress,

    #[error("no merge in progress")]
    NoMergeInProgress,

    #[error("pull target branch `{0}` is not the current branch")]
    NotCurrentBranch(String),

    #[error("commit `{0}` does not exist")]
    CommitNotFound(String),

    #[error("unknown revision `{0}`")]
    UnknownRevision(String),

    #[error("ambiguous revision `{0}`")]
    AmbiguousRevision(String),

    #[error("invalid revision `{0}`")]
    InvalidRevision(String),

    #[error("invalid refspec `{refspec}`: {message}")]
    InvalidRefspec { refspec: String, message: String },

    #[error("path `{path}` is outside repository worktree `{worktree}`")]
    PathOutsideWorktree { path: PathBuf, worktree: PathBuf },

    #[error("path `{0}` is not valid UTF-8")]
    NonUtf8Path(PathBuf),

    #[error("path `{path}` has an unsupported repository identity: {reason}")]
    UnsupportedPathIdentity { path: String, reason: &'static str },

    #[error("path `{path}` does not exist in revision `{rev}`")]
    PathNotFoundInRevision { path: String, rev: String },

    #[error("path `{0}` is not tracked")]
    PathNotTracked(String),

    #[error("path `{0}` is not a text artifact")]
    PathNotTextArtifact(String),

    #[error("text diff content limit must be greater than zero")]
    InvalidTextDiffContentLimit,

    #[error("path `{0}` is not conflicted")]
    PathNotConflicted(String),

    #[error("remote `{0}` does not exist")]
    RemoteNotFound(String),

    #[error("remote `{remote}` has no branch `{branch}`")]
    RemoteBranchNotFound { remote: String, branch: String },

    #[error(
        "push rejected because remote `{remote}/{remote_branch}` is not an ancestor of local `{local_branch}`"
    )]
    NonFastForward {
        remote: String,
        local_branch: String,
        remote_branch: String,
    },

    #[error("remote ref `{remote}/{branch}` changed during push; fetch and retry")]
    RemoteRefChanged { remote: String, branch: String },

    #[error("invalid remote object `{path}`: {message}")]
    InvalidRemoteObject { path: String, message: String },

    #[error("unknown repository config key `{0}`")]
    UnknownConfigKey(String),

    #[error("invalid repository config value `{value}` for `{key}`: {message}")]
    InvalidConfigValue {
        key: String,
        value: String,
        message: String,
    },

    #[error(transparent)]
    Object(#[from] object::ObjectErr),

    #[error(transparent)]
    Remote(#[from] RemoteErr),
}

pub type Result<T> = std::result::Result<T, RepoErr>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Head {
    Branch { name: String },
    Detached { commit: String },
}

impl Head {
    pub fn branch(name: impl Into<String>) -> Self {
        Self::Branch { name: name.into() }
    }

    fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if let Some(reference) = raw.strip_prefix("ref: ") {
            let branch = reference
                .strip_prefix("refs/heads/")
                .ok_or_else(|| RepoErr::InvalidHead(raw.to_string()))?;
            validate_ref_name(branch)?;
            Ok(Self::branch(branch))
        } else if raw.is_empty() {
            Err(RepoErr::InvalidHead(raw.to_string()))
        } else {
            Ok(Self::Detached { commit: raw.to_string() })
        }
    }

    fn serialize(&self) -> String {
        match self {
            Self::Branch { name } => format!("ref: refs/heads/{name}\n"),
            Self::Detached { commit } => format!("{commit}\n"),
        }
    }

    pub fn branch_name(&self) -> Option<&str> {
        match self {
            Self::Branch { name } => Some(name),
            Self::Detached { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchUpstream {
    pub remote: String,
    pub branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchInfo {
    pub name: String,
    pub target: Option<String>,
    pub current: bool,
    pub upstream: Option<BranchUpstream>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagInfo {
    pub name: String,
    pub object: String,
    pub target: String,
    pub annotated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteInfo {
    pub name: String,
    pub config: RemoteConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemotePruneOutcome {
    pub remote: String,
    pub branches: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchOutcome {
    pub remote: String,
    pub branch: String,
    pub head: String,
    pub commits: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchAllOutcome {
    pub remote: String,
    pub branches: Vec<FetchOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushOutcome {
    pub remote: String,
    pub local_branch: String,
    pub remote_branch: String,
    pub head: String,
    pub commits: usize,
    pub forced: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushAllOutcome {
    pub remote: String,
    pub branches: Vec<PushOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushRefspecBranch {
    pub local_branch: String,
    pub remote_branch: String,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteBranchRef {
    pub remote: String,
    pub branch: String,
    pub head: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteBranchHead {
    pub raw: Option<Bytes>,
    pub head: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemoteObjectPackIndex {
    version: u32,
    pack: String,
    objects: Vec<RemoteObjectPackEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemoteObjectPackEntry {
    id: object::ObjectId,
    offset: u64,
    len: u64,
}

#[derive(Debug, Default)]
struct RemoteObjectPackCache {
    indexes: Option<Vec<RemoteObjectPackIndex>>,
    packs: BTreeMap<String, Bytes>,
}

impl RemoteObjectPackCache {
    fn indexes(&mut self, remote: &crate::remote::Remote) -> Result<&[RemoteObjectPackIndex]> {
        if self.indexes.is_none() {
            self.indexes = Some(fetch_remote_object_pack_indexes(remote)?);
        }
        Ok(self.indexes.as_deref().expect("pack indexes initialized"))
    }

    fn pack_bytes(&mut self, remote: &crate::remote::Remote, pack: &str) -> Result<Bytes> {
        if let Some(bytes) = self.packs.get(pack) {
            return Ok(bytes.clone());
        }
        let bytes =
            block_on_remote(remote.get_raw(pack))?.ok_or_else(|| RepoErr::InvalidRemoteObject {
                path: pack.to_string(),
                message: "missing pack object".to_string(),
            })?;
        self.packs.insert(pack.to_string(), bytes.clone());
        Ok(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullOutcome {
    pub remote: String,
    pub remote_branch: String,
    pub local_branch: String,
    pub head: String,
    pub commits: usize,
    pub merge: MergeOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullPlan {
    pub remote: String,
    pub remote_branch: String,
    pub local_branch: String,
    pub fetch: FetchOutcome,
    pub merge: MergePlan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MergeOutcome {
    FastForward {
        from: Option<String>,
        to: String,
    },
    AlreadyUpToDate {
        head: String,
    },
    Merged {
        head: String,
        target: String,
        merge_base: Option<String>,
        staged: Vec<String>,
        conflicted: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePlan {
    pub rev: String,
    pub target: String,
    pub checkout: CheckoutPlan,
    pub outcome: MergeOutcome,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<index::Index>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeAbortPlan {
    pub target: String,
    pub checkout: CheckoutPlan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitObject {
    pub id: String,
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parents: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    pub message: String,
    pub timestamp_ms: u64,

    #[serde(default)]
    pub files: BTreeMap<String, CommitFileState>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifacts: BTreeMap<String, CommitArtifactState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<CommitPathChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tables: Vec<CommitTableSummary>,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub changed_tables: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitPathChange {
    pub path: String,
    pub change: RepoFileChange,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitFileState {
    pub volume: VolumeId,
    pub snapshot: RepoSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommitArtifactState {
    File {
        kind: RepoTrackedPathKind,
        oid: object::ObjectId,
        content_hash: object::ObjectId,
        size: u64,
    },
    LargeFile {
        kind: RepoTrackedPathKind,
        oid: object::ObjectId,
        content_hash: object::ObjectId,
        size: u64,
    },
}

impl CommitArtifactState {
    pub fn kind(&self) -> RepoTrackedPathKind {
        match self {
            Self::File { kind, .. } | Self::LargeFile { kind, .. } => *kind,
        }
    }

    pub fn oid(&self) -> &object::ObjectId {
        match self {
            Self::File { oid, .. } | Self::LargeFile { oid, .. } => oid,
        }
    }

    pub fn content_hash(&self) -> &object::ObjectId {
        match self {
            Self::File { content_hash, .. } | Self::LargeFile { content_hash, .. } => content_hash,
        }
    }

    pub fn size(&self) -> u64 {
        match self {
            Self::File { size, .. } | Self::LargeFile { size, .. } => *size,
        }
    }

    pub fn is_large(&self) -> bool {
        matches!(self, Self::LargeFile { .. })
    }
}

fn artifact_tracked_path_kind(state: &CommitArtifactState) -> RepoTrackedPathKind {
    state.kind()
}

fn artifact_diff_kind(
    before: Option<&CommitArtifactState>,
    after: Option<&CommitArtifactState>,
) -> RepoTrackedPathKind {
    after
        .or(before)
        .map(artifact_tracked_path_kind)
        .unwrap_or(RepoTrackedPathKind::BinaryFile)
}

fn artifact_tracked_path_storage(state: &CommitArtifactState) -> RepoPathStorage {
    match state {
        CommitArtifactState::File { .. } => RepoPathStorage::Inline,
        CommitArtifactState::LargeFile { .. } => RepoPathStorage::External,
    }
}

fn artifact_diff_storage(
    before: Option<&CommitArtifactState>,
    after: Option<&CommitArtifactState>,
) -> RepoPathStorage {
    after
        .or(before)
        .map(artifact_tracked_path_storage)
        .unwrap_or(RepoPathStorage::Inline)
}

fn default_path_storage(kind: RepoTrackedPathKind) -> RepoPathStorage {
    match kind {
        RepoTrackedPathKind::SqliteDatabase => RepoPathStorage::SqliteSnapshot,
        RepoTrackedPathKind::TextFile => RepoPathStorage::Inline,
        RepoTrackedPathKind::BinaryFile => RepoPathStorage::External,
    }
}

fn repo_path_kind_from_object_kind(kind: object::FileContentKind) -> RepoTrackedPathKind {
    match kind {
        object::FileContentKind::TextFile => RepoTrackedPathKind::TextFile,
        object::FileContentKind::BinaryFile => RepoTrackedPathKind::BinaryFile,
    }
}

fn object_kind_from_repo_path_kind(kind: RepoTrackedPathKind) -> object::FileContentKind {
    match kind {
        RepoTrackedPathKind::TextFile => object::FileContentKind::TextFile,
        RepoTrackedPathKind::SqliteDatabase | RepoTrackedPathKind::BinaryFile => {
            object::FileContentKind::BinaryFile
        }
    }
}

fn tracked_file_entry(
    path: String,
    stage: index::IndexStage,
    file: &CommitFileState,
) -> RepoTrackedPathEntry {
    let blob = object::Object::Blob(object::BlobObject::SqliteSnapshot(sqlite_snapshot_blob(
        file,
    )));
    RepoTrackedPathEntry {
        path,
        stage,
        kind: RepoTrackedPathKind::SqliteDatabase,
        storage: RepoPathStorage::SqliteSnapshot,
        mode: Some(object::TreeEntryMode::SqliteDatabase),
        oid: Some(blob.id()),
        size: None,
        page_count: Some(file.snapshot.page_count),
    }
}

fn tracked_artifact_entry(
    path: String,
    stage: index::IndexStage,
    artifact: &CommitArtifactState,
) -> RepoTrackedPathEntry {
    let kind = artifact_tracked_path_kind(artifact);
    RepoTrackedPathEntry {
        path,
        stage,
        kind,
        storage: artifact_tracked_path_storage(artifact),
        mode: Some(object::TreeEntryMode::Regular),
        oid: Some(artifact.oid().clone()),
        size: Some(artifact.size()),
        page_count: None,
    }
}

fn tracked_index_entry(entry: &index::IndexEntry) -> Option<RepoTrackedPathEntry> {
    if let Some(file) = &entry.file {
        let mut tracked = tracked_file_entry(entry.path.clone(), entry.stage, file);
        tracked.mode = entry.mode;
        tracked.oid = entry.oid.clone().or(tracked.oid);
        Some(tracked)
    } else if let Some(artifact) = &entry.artifact {
        let mut tracked = tracked_artifact_entry(entry.path.clone(), entry.stage, artifact);
        tracked.mode = entry.mode;
        tracked.oid = entry.oid.clone().or(tracked.oid);
        Some(tracked)
    } else {
        None
    }
}

fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoDiff {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub paths: Vec<RepoPathDiff>,
    pub files: Vec<RepoFileDiff>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<RepoArtifactDiff>,
}

impl RepoDiff {
    pub fn refresh_paths(&mut self) {
        self.paths = repo_diff_paths(&self.files, &self.artifacts);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoPathDiff {
    pub path: String,
    pub change: RepoFileChange,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoFileDiff {
    pub path: String,
    pub change: RepoFileChange,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
    pub from: Option<CommitFileState>,
    pub to: Option<CommitFileState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<RepoWorktreeFileState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoWorktreeFileState {
    pub page_count: PageCount,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoArtifactDiff {
    pub path: String,
    pub change: RepoFileChange,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
    pub from: Option<CommitArtifactState>,
    pub to: Option<CommitArtifactState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoTextContentDiff {
    pub path: String,
    pub change: RepoFileChange,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
    pub before: RepoTextContentState,
    pub after: RepoTextContentState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RepoTextContentState {
    Absent,
    Utf8 {
        content: String,
        size: u64,
        content_hash: object::ObjectId,
    },
    Base64 {
        content: String,
        size: u64,
        content_hash: object::ObjectId,
    },
    TooLarge {
        size: u64,
        content_hash: object::ObjectId,
    },
    MissingPayload {
        size: u64,
        content_hash: object::ObjectId,
    },
    InvalidUtf8 {
        size: u64,
        content_hash: object::ObjectId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoFileChange {
    Added,
    Deleted,
    Modified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetOutcome {
    pub target: String,
    pub mode: ResetMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetPlan {
    pub rev: String,
    pub target: String,
    pub mode: ResetMode,
    pub checkout: CheckoutPlan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutFileOutcome {
    pub target: String,
    pub path: String,
    pub state: CommitFileState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutFilePlan {
    pub target: String,
    pub path: String,
    pub state: CommitFileState,
    pub entry: index::IndexEntry,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutArtifactOutcome {
    pub target: String,
    pub path: String,
    pub state: CommitArtifactState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutArtifactPlan {
    pub target: String,
    pub path: String,
    pub state: CommitArtifactState,
    pub entry: index::IndexEntry,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutPlan {
    pub target: Option<String>,
    pub files: BTreeMap<String, CommitFileState>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifacts: BTreeMap<String, CommitArtifactState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchNewBranchPlan {
    pub branch: BranchInfo,
    pub checkout: CheckoutPlan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSnapshot {
    pub page_count: PageCount,
    pub ranges: Vec<RepoLogRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoLogRange {
    pub log: LogId,
    pub start: LSN,
    pub end: LSN,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<RepoStorageCommit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoStorageCommit {
    pub lsn: LSN,
    pub commit_hash: CommitHash,
}

impl RepoSnapshot {
    pub fn from_snapshot(snapshot: &Snapshot) -> Self {
        Self {
            page_count: snapshot.page_count,
            ranges: snapshot
                .iter()
                .map(|range| RepoLogRange {
                    log: range.log.clone(),
                    start: *range.lsns.start(),
                    end: *range.lsns.end(),
                    commits: Vec::new(),
                })
                .collect(),
        }
    }

    pub fn to_snapshot(&self) -> Snapshot {
        let Some((first, rest)) = self.ranges.split_first() else {
            return Snapshot::empty();
        };

        let mut snapshot =
            Snapshot::new(first.log.clone(), first.start..=first.end, self.page_count);
        for range in rest {
            snapshot.append(range.log.clone(), range.start..=range.end);
        }
        snapshot
    }

    pub fn expected_commit_count(&self) -> u64 {
        self.ranges
            .iter()
            .map(|range| (range.start..=range.end).len())
            .sum()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoStatus {
    pub worktree: PathBuf,
    pub graft_dir: PathBuf,
    pub repository_format_version: u32,
    pub head: Head,
    pub head_target: Option<String>,
    pub merge_head: Option<String>,
    pub orig_head: Option<String>,
    pub dirty: bool,
    #[serde(default)]
    pub has_unstaged_changes: bool,
    #[serde(default)]
    pub has_staged_changes: bool,
    #[serde(default)]
    pub has_conflicts: bool,
    #[serde(default)]
    pub work_in_progress: bool,
    #[serde(default)]
    pub counts: RepoStatusCounts,
    #[serde(default)]
    pub paths: Vec<RepoStatusPath>,
    #[serde(default)]
    pub unstaged: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unstaged_changes: Vec<RepoWorktreeChange>,
    pub staged: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub staged_changes: Vec<RepoStagedChange>,
    pub conflicted: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicted_changes: Vec<RepoConflictChange>,
    pub branches: Vec<BranchInfo>,
    pub remotes: Vec<RemoteInfo>,
    pub upstream: Option<BranchUpstream>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_status: Option<RepoUpstreamStatus>,
    #[serde(default)]
    pub ahead: usize,
    #[serde(default)]
    pub behind: usize,
}

impl RepoStatus {
    pub fn refresh_summary_flags(&mut self) {
        self.counts = RepoStatusCounts::from_status_parts(
            self.unstaged.len(),
            self.unstaged_changes.len(),
            self.staged.len(),
            self.staged_changes.len(),
            self.conflicted.len(),
            self.conflicted_changes.len(),
        );
        self.has_unstaged_changes = self.counts.unstaged > 0;
        self.has_staged_changes = self.counts.staged > 0;
        self.has_conflicts = self.counts.conflicted > 0;
        self.work_in_progress = self.has_unstaged_changes
            || self.has_staged_changes
            || self.has_conflicts
            || self.merge_head.is_some();
        self.dirty = self.has_unstaged_changes;
        self.paths = Self::status_paths_from_changes(
            &self.unstaged_changes,
            &self.staged_changes,
            &self.conflicted_changes,
        );
    }

    fn status_paths_from_changes(
        unstaged_changes: &[RepoWorktreeChange],
        staged_changes: &[RepoStagedChange],
        conflicted_changes: &[RepoConflictChange],
    ) -> Vec<RepoStatusPath> {
        #[derive(Default)]
        struct Builder {
            kind: Option<RepoTrackedPathKind>,
            storage: Option<RepoPathStorage>,
            unstaged_change: Option<RepoWorktreeChangeKind>,
            staged_change: Option<RepoFileChange>,
            conflicted: bool,
        }

        fn kind_priority(kind: RepoTrackedPathKind) -> u8 {
            match kind {
                RepoTrackedPathKind::TextFile => 1,
                RepoTrackedPathKind::BinaryFile => 1,
                RepoTrackedPathKind::SqliteDatabase => 3,
            }
        }

        fn storage_priority(storage: RepoPathStorage) -> u8 {
            match storage {
                RepoPathStorage::Inline => 1,
                RepoPathStorage::External => 2,
                RepoPathStorage::SqliteSnapshot => 3,
            }
        }

        fn record_kind(target: &mut Option<RepoTrackedPathKind>, kind: RepoTrackedPathKind) {
            if target.is_none_or(|existing| kind_priority(kind) > kind_priority(existing)) {
                *target = Some(kind);
            }
        }

        fn record_storage(target: &mut Option<RepoPathStorage>, storage: RepoPathStorage) {
            if target.is_none_or(|existing| storage_priority(storage) > storage_priority(existing))
            {
                *target = Some(storage);
            }
        }

        let mut paths = BTreeMap::<String, Builder>::new();

        for change in unstaged_changes {
            let entry = paths.entry(change.path.clone()).or_default();
            record_kind(&mut entry.kind, change.kind);
            record_storage(&mut entry.storage, change.storage);
            entry.unstaged_change = Some(change.change);
        }

        for change in staged_changes {
            let entry = paths.entry(change.path.clone()).or_default();
            record_kind(&mut entry.kind, change.kind);
            record_storage(&mut entry.storage, change.storage);
            entry.staged_change = Some(change.change);
        }

        for change in conflicted_changes {
            let entry = paths.entry(change.path.clone()).or_default();
            record_kind(&mut entry.kind, change.kind);
            record_storage(&mut entry.storage, change.storage);
            entry.conflicted = true;
        }

        paths
            .into_iter()
            .filter_map(|(path, entry)| {
                entry.kind.map(|kind| {
                    let index_status = if entry.conflicted {
                        RepoStatusPathState::Unmerged
                    } else {
                        entry
                            .staged_change
                            .map(RepoStatusPathState::from_staged_change)
                            .unwrap_or(RepoStatusPathState::None)
                    };
                    let worktree_status = if entry.conflicted {
                        RepoStatusPathState::Unmerged
                    } else {
                        entry
                            .unstaged_change
                            .map(RepoStatusPathState::from_worktree_change)
                            .unwrap_or(RepoStatusPathState::None)
                    };
                    let code = RepoStatusPathState::code(index_status, worktree_status);
                    RepoStatusPath {
                        path,
                        kind,
                        storage: entry.storage.unwrap_or_else(|| default_path_storage(kind)),
                        index_status,
                        worktree_status,
                        code,
                        unstaged_change: entry.unstaged_change,
                        staged_change: entry.staged_change,
                        conflicted: entry.conflicted,
                    }
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoStatusCounts {
    pub unstaged: usize,
    pub staged: usize,
    pub conflicted: usize,
}

impl RepoStatusCounts {
    fn from_status_parts(
        unstaged: usize,
        unstaged_changes: usize,
        staged: usize,
        staged_changes: usize,
        conflicted: usize,
        conflicted_changes: usize,
    ) -> Self {
        Self {
            unstaged: unstaged.max(unstaged_changes),
            staged: staged.max(staged_changes),
            conflicted: conflicted.max(conflicted_changes),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoStatusPath {
    pub path: String,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
    pub index_status: RepoStatusPathState,
    pub worktree_status: RepoStatusPathState,
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unstaged_change: Option<RepoWorktreeChangeKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_change: Option<RepoFileChange>,
    #[serde(default)]
    pub conflicted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoStatusPathState {
    None,
    Added,
    Modified,
    Deleted,
    Untracked,
    Unmerged,
}

impl RepoStatusPathState {
    fn from_staged_change(change: RepoFileChange) -> Self {
        match change {
            RepoFileChange::Added => Self::Added,
            RepoFileChange::Deleted => Self::Deleted,
            RepoFileChange::Modified => Self::Modified,
        }
    }

    fn from_worktree_change(change: RepoWorktreeChangeKind) -> Self {
        match change {
            RepoWorktreeChangeKind::Deleted => Self::Deleted,
            RepoWorktreeChangeKind::Modified => Self::Modified,
            RepoWorktreeChangeKind::Untracked => Self::Untracked,
        }
    }

    fn code(index: Self, worktree: Self) -> String {
        if index == Self::Unmerged || worktree == Self::Unmerged {
            return "UU".to_string();
        }
        if index == Self::None && worktree == Self::Untracked {
            return "??".to_string();
        }
        format!("{}{}", index.index_code(), worktree.worktree_code())
    }

    fn index_code(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Deleted => 'D',
            Self::Modified => 'M',
            Self::None | Self::Untracked | Self::Unmerged => ' ',
        }
    }

    fn worktree_code(self) -> char {
        match self {
            Self::Deleted => 'D',
            Self::Modified => 'M',
            Self::Untracked => '?',
            Self::None | Self::Added | Self::Unmerged => ' ',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoArtifactAudit {
    pub artifacts: usize,
    pub external_payloads: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<RepoArtifactAuditIssue>,
}

impl RepoArtifactAudit {
    pub fn ok(&self) -> bool {
        self.issues.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoArtifactRepairOutcome {
    pub remote: String,
    pub fetched_objects: usize,
    pub fetched_external_payloads: usize,
    pub before: RepoArtifactAudit,
    pub after: RepoArtifactAudit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoLargeFileFetchOutcome {
    pub remote: String,
    pub target: String,
    pub external_payloads: usize,
    pub already_present_payloads: usize,
    pub fetched_payloads: usize,
    pub fetched_bytes: u64,
    pub files: Vec<RepoLargeFileFetchEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoLargeFileFetchEntry {
    pub content_hash: object::ObjectId,
    pub size: u64,
    pub store_path: String,
    pub status: RepoLargeFileFetchStatus,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoLargeFileFetchStatus {
    Present,
    Fetched,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoLargeFileStatusOutcome {
    pub target: String,
    pub external_payloads: usize,
    pub present_payloads: usize,
    pub missing_payloads: usize,
    pub invalid_payloads: usize,
    pub present_bytes: u64,
    pub missing_bytes: u64,
    pub invalid_bytes: u64,
    pub files: Vec<RepoLargeFileStatusEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoLargeFileStatusEntry {
    pub content_hash: object::ObjectId,
    pub size: u64,
    pub store_path: String,
    pub status: RepoLargeFileStatusState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoLargeFileStatusState {
    Present,
    Missing,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoLargeFilePruneOutcome {
    pub dry_run: bool,
    pub referenced_payloads: usize,
    pub candidate_payloads: usize,
    pub candidate_bytes: u64,
    pub pruned_payloads: usize,
    pub pruned_bytes: u64,
    pub files: Vec<RepoLargeFilePruneEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoLargeFilePruneEntry {
    pub content_hash: object::ObjectId,
    pub size: u64,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoArtifactAuditIssue {
    pub path: String,
    pub kind: RepoArtifactAuditIssueKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oid: Option<object::ObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<object::ObjectId>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoArtifactAuditIssueKind {
    MissingObject,
    InvalidObject,
    MissingExternalPayload,
    InvalidExternalPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoTrackedPath {
    pub path: String,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_count: Option<PageCount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoTrackedPathDetail {
    pub path: String,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_count: Option<PageCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oid: Option<object::ObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<object::ObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_present: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_payload_present: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoTrackedPathEntry {
    pub path: String,
    pub stage: index::IndexStage,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<object::TreeEntryMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oid: Option<object::ObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_count: Option<PageCount>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoTrackedPathKind {
    SqliteDatabase,
    TextFile,
    BinaryFile,
}

impl Display for RepoTrackedPathKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SqliteDatabase => f.write_str("sqlite_database"),
            Self::TextFile => f.write_str("text_file"),
            Self::BinaryFile => f.write_str("binary_file"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoPathStorage {
    SqliteSnapshot,
    Inline,
    External,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoUpstreamStatus {
    pub remote: String,
    pub branch: String,
    pub local: String,
    pub remote_target: String,
    pub ahead: usize,
    pub behind: usize,
    pub state: RepoUpstreamState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoUpstreamState {
    UpToDate,
    Ahead,
    Behind,
    Diverged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoWorktreeChange {
    pub path: String,
    pub change: RepoWorktreeChangeKind,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoStagedChange {
    pub path: String,
    pub change: RepoFileChange,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoConflictChange {
    pub path: String,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoWorktreeChangeKind {
    Modified,
    Deleted,
    Untracked,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct WorktreeState {
    #[serde(default)]
    dirty: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    deleted: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    worktree: PathBuf,
    graft_dir: PathBuf,
}

impl Repository {
    pub fn init(worktree: impl AsRef<Path>) -> Result<Self> {
        let worktree = worktree.as_ref();
        fs::create_dir_all(worktree)?;

        let worktree = fs::canonicalize(worktree)?;
        let graft_dir = worktree.join(GRAFT_DIR);
        let repo = Self { worktree, graft_dir };

        repo.create_layout()?;

        if !repo.config_path().exists() {
            repo.write_config(&RepoConfig::default())?;
        } else {
            repo.ensure_supported_format()?;
        }

        if !repo.head_path().exists() {
            let default_branch = repo.config()?.core.default_branch;
            repo.write_head(&Head::branch(default_branch))?;
        }

        Ok(repo)
    }

    pub fn init_for_file(path: impl AsRef<Path>) -> Result<Self> {
        Self::init(worktree_for_file(path.as_ref()))
    }

    pub fn open(worktree: impl AsRef<Path>) -> Result<Self> {
        let worktree = fs::canonicalize(worktree)?;
        let graft_dir = worktree.join(GRAFT_DIR);
        if !graft_dir.is_dir() {
            return Err(RepoErr::NotFound(worktree));
        }

        let repo = Self { worktree, graft_dir };
        repo.ensure_supported_format()?;
        Ok(repo)
    }

    pub fn discover(start: impl AsRef<Path>) -> Result<Self> {
        let original = start.as_ref().to_path_buf();
        let mut current = normalize_discovery_start(start.as_ref())?;

        loop {
            let graft_dir = current.join(GRAFT_DIR);
            if graft_dir.is_dir() {
                return Self::open(&current);
            }

            if !current.pop() {
                return Err(RepoErr::NotFound(original));
            }
        }
    }

    pub fn discover_for_file(path: impl AsRef<Path>) -> Result<Self> {
        Self::discover(worktree_for_file(path.as_ref()))
    }

    pub fn worktree(&self) -> &Path {
        &self.worktree
    }

    pub fn graft_dir(&self) -> &Path {
        &self.graft_dir
    }

    pub fn store_dir(&self) -> PathBuf {
        self.graft_dir.join(DIR_STORE_FJALL)
    }

    pub fn file_store_dir(&self) -> PathBuf {
        self.graft_dir.join(DIR_STORE_FILES)
    }

    pub fn object_store(&self) -> object::LooseObjectStore {
        object::LooseObjectStore::new(self.graft_dir.join("objects"))
    }
}

impl Display for Head {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Head::Branch { name } => write!(f, "refs/heads/{name}"),
            Head::Detached { commit } => write!(f, "{commit}"),
        }
    }
}

fn branch_upstream_from_config(
    config: &RepoConfig,
    branch: &str,
) -> Result<Option<BranchUpstream>> {
    validate_ref_name(branch)?;
    let Some(branch_config) = config.branches.get(branch) else {
        return Ok(None);
    };
    let Some(remote) = &branch_config.remote else {
        return Ok(None);
    };
    let Some(merge) = &branch_config.merge else {
        return Ok(None);
    };

    validate_remote_name(remote)?;
    let branch = branch_from_merge_ref(merge)?;
    Ok(Some(BranchUpstream { remote: remote.clone(), branch }))
}

fn branch_merge_ref(branch: &str) -> String {
    format!("refs/heads/{branch}")
}

fn branch_from_merge_ref(merge: &str) -> Result<String> {
    let branch = merge.strip_prefix("refs/heads/").unwrap_or(merge);
    validate_ref_name(branch)?;
    Ok(branch.to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn normalize_commit_table_summary(tables: Vec<CommitTableSummary>) -> Vec<CommitTableSummary> {
    let mut by_name = BTreeMap::<String, CommitTableSummary>::new();
    for table in tables {
        if table.name.is_empty() || table.inserts + table.deletes + table.updates == 0 {
            continue;
        }
        by_name
            .entry(table.name.clone())
            .and_modify(|entry| {
                entry.inserts += table.inserts;
                entry.deletes += table.deletes;
                entry.updates += table.updates;
            })
            .or_insert(table);
    }
    by_name.into_values().collect()
}

fn write_file_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        for attempt in 0..100 {
            let tmp = parent.join(format!(
                ".graft-tmp-{}-{}-{attempt}",
                now_ms(),
                std::process::id()
            ));
            if tmp.exists() {
                continue;
            }
            fs::write(&tmp, bytes)?;
            return match fs::rename(&tmp, path) {
                Ok(()) => Ok(()),
                Err(err) => {
                    let _ = fs::remove_file(&tmp);
                    Err(err.into())
                }
            };
        }
    }
    fs::write(path, bytes)?;
    Ok(())
}

fn reflog_line(old: Option<&str>, new: Option<&str>, message: &str) -> String {
    format!(
        "{} {} {} {} +0000\t{}\n",
        reflog_value(old),
        reflog_value(new),
        REFLOG_ACTOR,
        now_ms(),
        sanitize_reflog_message(message)
    )
}

fn reflog_value(value: Option<&str>) -> &str {
    match value {
        Some(value) if !value.is_empty() => value,
        _ => NULL_OBJECT_ID,
    }
}

fn sanitize_reflog_message(message: &str) -> String {
    message
        .chars()
        .map(|ch| match ch {
            '\n' | '\r' | '\t' => ' ',
            ch => ch,
        })
        .collect()
}

fn sqlite_snapshot_blob(state: &CommitFileState) -> object::SqliteSnapshotBlob {
    object::SqliteSnapshotBlob {
        volume: state.volume.clone(),
        page_count: state.snapshot.page_count,
        ranges: state
            .snapshot
            .ranges
            .iter()
            .map(|range| object::SqliteSnapshotRange {
                log: range.log.clone(),
                start: range.start,
                end: range.end,
                commits: range
                    .commits
                    .iter()
                    .map(|commit| object::SqliteSnapshotCommit {
                        lsn: commit.lsn,
                        commit_hash: commit.commit_hash.clone(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn file_state_from_sqlite_snapshot_blob(blob: object::SqliteSnapshotBlob) -> CommitFileState {
    CommitFileState {
        volume: blob.volume,
        snapshot: RepoSnapshot {
            page_count: blob.page_count,
            ranges: blob
                .ranges
                .into_iter()
                .map(|range| RepoLogRange {
                    log: range.log,
                    start: range.start,
                    end: range.end,
                    commits: range
                        .commits
                        .into_iter()
                        .map(|commit| RepoStorageCommit {
                            lsn: commit.lsn,
                            commit_hash: commit.commit_hash,
                        })
                        .collect(),
                })
                .collect(),
        },
    }
}

fn artifact_state_from_blob(
    oid: object::ObjectId,
    blob: object::BlobObject,
) -> Result<CommitArtifactState> {
    match blob {
        object::BlobObject::File(blob) => {
            let content_hash = object::ObjectId::for_bytes(&blob.bytes);
            Ok(CommitArtifactState::File {
                kind: repo_path_kind_from_object_kind(blob.kind),
                oid,
                content_hash,
                size: blob.bytes.len() as u64,
            })
        }
        object::BlobObject::LargeFilePointer(blob) => Ok(CommitArtifactState::LargeFile {
            kind: repo_path_kind_from_object_kind(blob.kind),
            oid,
            content_hash: blob.content_hash,
            size: blob.size,
        }),
        object::BlobObject::SqliteSnapshot(_) => {
            Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "blob",
                message: "sqlite snapshot cannot be used as a regular artifact".to_string(),
            }))
        }
    }
}

fn validate_artifact_object_matches_state(
    state: &CommitArtifactState,
    object: &object::Object,
) -> Result<()> {
    match (state, object) {
        (
            CommitArtifactState::File { kind, content_hash, size, .. },
            object::Object::Blob(object::BlobObject::File(blob)),
        ) => {
            let actual_hash = object::ObjectId::for_bytes(&blob.bytes);
            if repo_path_kind_from_object_kind(blob.kind) == *kind
                && &actual_hash == content_hash
                && blob.bytes.len() as u64 == *size
            {
                Ok(())
            } else {
                Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                    kind: "blob",
                    message: format!(
                        "file blob metadata mismatch: expected kind {kind}, {size} byte(s), and hash {content_hash}, got kind {}, {} byte(s), and hash {actual_hash}",
                        repo_path_kind_from_object_kind(blob.kind),
                        blob.bytes.len()
                    ),
                }))
            }
        }
        (
            CommitArtifactState::LargeFile { kind, content_hash, size, .. },
            object::Object::Blob(object::BlobObject::LargeFilePointer(pointer)),
        ) => {
            if repo_path_kind_from_object_kind(pointer.kind) == *kind
                && &pointer.content_hash == content_hash
                && pointer.size == *size
            {
                Ok(())
            } else {
                Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                    kind: "blob",
                    message: format!(
                        "large file pointer mismatch: expected kind {kind}, {size} byte(s), and hash {content_hash}, got kind {}, {} byte(s), and hash {}",
                        repo_path_kind_from_object_kind(pointer.kind),
                        pointer.size,
                        pointer.content_hash
                    ),
                }))
            }
        }
        (CommitArtifactState::File { .. }, _) => {
            Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "blob",
                message: "artifact object is not a file blob".to_string(),
            }))
        }
        (CommitArtifactState::LargeFile { .. }, _) => {
            Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "blob",
                message: "artifact object is not a large file pointer".to_string(),
            }))
        }
    }
}

fn collect_large_file_payload_from_artifact(
    state: &CommitArtifactState,
    out: &mut BTreeSet<object::ObjectId>,
) {
    if let CommitArtifactState::LargeFile { content_hash, .. } = state {
        out.insert(content_hash.clone());
    }
}

fn large_file_content_relative_path(id: &object::ObjectId) -> String {
    let raw = id.as_str();
    format!("{DIR_STORE_FILES}/{}/{}", &raw[..2], &raw[2..])
}

fn validate_large_file_content(id: &object::ObjectId, size: u64, bytes: &[u8]) -> Result<()> {
    if bytes.len() as u64 != size {
        return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
            kind: "large-file",
            message: format!(
                "external payload {id} size mismatch: expected {size}, got {}",
                bytes.len()
            ),
        }));
    }
    let actual = object::ObjectId::for_bytes(bytes);
    if actual != *id {
        return Err(RepoErr::Object(object::ObjectErr::ObjectIdMismatch {
            expected: id.clone(),
            actual,
        }));
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArtifactStatFingerprint {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ArtifactStatCacheEntry {
    expected: object::ObjectId,
    fingerprint: ArtifactStatFingerprint,
    matches: bool,
}

#[cfg(unix)]
static ARTIFACT_STAT_CACHE: OnceLock<Mutex<HashMap<PathBuf, ArtifactStatCacheEntry>>> =
    OnceLock::new();

#[cfg(unix)]
fn artifact_stat_fingerprint(metadata: &fs::Metadata) -> ArtifactStatFingerprint {
    ArtifactStatFingerprint {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    }
}

#[cfg(unix)]
fn cached_artifact_match(
    path: &Path,
    expected: &object::ObjectId,
    fingerprint: ArtifactStatFingerprint,
) -> Option<bool> {
    let cache = ARTIFACT_STAT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache
        .get(path)
        .filter(|entry| entry.expected == *expected && entry.fingerprint == fingerprint)
        .map(|entry| entry.matches)
}

#[cfg(unix)]
fn cache_artifact_match(
    path: &Path,
    expected: &object::ObjectId,
    fingerprint: ArtifactStatFingerprint,
    matches: bool,
) {
    let cache = ARTIFACT_STAT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if cache.len() >= ARTIFACT_STAT_CACHE_MAX_ENTRIES && !cache.contains_key(path) {
        cache.clear();
    }
    cache.insert(
        path.to_path_buf(),
        ArtifactStatCacheEntry {
            expected: expected.clone(),
            fingerprint,
            matches,
        },
    );
}

fn artifact_file_matches(path: &Path, expected: &CommitArtifactState) -> Result<Option<bool>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Ok(Some(false));
            }
            if metadata.len() != expected.size() {
                return Ok(Some(false));
            }
            #[cfg(unix)]
            let fingerprint = artifact_stat_fingerprint(&metadata);
            #[cfg(unix)]
            if let Some(matches) = cached_artifact_match(path, expected.content_hash(), fingerprint)
            {
                return Ok(Some(matches));
            }
            let bytes = fs::read(path)?;
            let matches = object::ObjectId::for_bytes(&bytes) == *expected.content_hash();
            #[cfg(unix)]
            if fs::symlink_metadata(path)
                .ok()
                .filter(|metadata| metadata.file_type().is_file())
                .is_some_and(|metadata| artifact_stat_fingerprint(&metadata) == fingerprint)
            {
                cache_artifact_match(path, expected.content_hash(), fingerprint, matches);
            }
            Ok(Some(matches))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn validate_commit_file_state(state: &CommitFileState) -> Result<()> {
    for range in &state.snapshot.ranges {
        let expected_count = (range.start..=range.end).len();
        if range.commits.len() as u64 != expected_count {
            return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "sqlite-snapshot",
                message: format!(
                    "range {:?} {}..={} has {} storage commit hashes; expected {}",
                    range.log,
                    range.start,
                    range.end,
                    range.commits.len(),
                    expected_count
                ),
            }));
        }

        for (commit, expected_lsn) in range.commits.iter().zip((range.start..=range.end).iter()) {
            if commit.lsn != expected_lsn {
                return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                    kind: "sqlite-snapshot",
                    message: format!(
                        "range {:?} {}..={} has storage commit hash for LSN {}; expected {}",
                        range.log, range.start, range.end, commit.lsn, expected_lsn
                    ),
                }));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn repo_snapshot_with_test_hashes(snapshot: &Snapshot) -> RepoSnapshot {
    RepoSnapshot {
        page_count: snapshot.page_count,
        ranges: snapshot
            .iter()
            .map(|range| RepoLogRange {
                log: range.log.clone(),
                start: *range.lsns.start(),
                end: *range.lsns.end(),
                commits: range
                    .lsns
                    .iter()
                    .map(|lsn| RepoStorageCommit {
                        lsn,
                        commit_hash: CommitHash::testonly_random(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn commit_parent_ids(commit: &CommitObject) -> Vec<String> {
    if commit.parents.is_empty() {
        commit.parent.iter().cloned().collect()
    } else {
        commit.parents.clone()
    }
}

fn block_on_remote<T>(
    future: impl std::future::Future<Output = std::result::Result<T, RemoteErr>>,
) -> Result<T> {
    thread_local! {
        static REMOTE_RUNTIME: RefCell<Option<tokio::runtime::Runtime>> = const { RefCell::new(None) };
    }

    REMOTE_RUNTIME.with(|runtime| {
        let mut runtime = runtime.borrow_mut();
        if runtime.is_none() {
            *runtime = Some(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?,
            );
        }
        Ok(runtime
            .as_ref()
            .expect("runtime initialized")
            .block_on(future)?)
    })
}

fn parse_remote_ref(path: &str, bytes: bytes::Bytes) -> Result<String> {
    let raw = String::from_utf8(bytes.to_vec()).map_err(|err| RepoErr::InvalidRemoteObject {
        path: path.to_string(),
        message: err.to_string(),
    })?;
    let target = raw.trim();
    if target.is_empty() {
        return Err(RepoErr::InvalidRemoteObject {
            path: path.to_string(),
            message: "empty ref".to_string(),
        });
    }
    Ok(target.to_string())
}

fn remote_loose_object_id(path: &str) -> Result<Option<object::ObjectId>> {
    let Some(rest) = path.strip_prefix("objects/") else {
        return Ok(None);
    };
    if rest.starts_with("pack/") {
        return Ok(None);
    }
    let Some((fanout, suffix)) = rest.split_once('/') else {
        return Ok(None);
    };
    if fanout.len() != 2 || suffix.len() != 62 || suffix.contains('/') {
        return Ok(None);
    }
    let id = format!("{fanout}{suffix}");
    if !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(None);
    }
    Ok(Some(object::ObjectId::from_str(&id)?))
}

fn fetch_remote_object_pack_indexes(
    remote: &crate::remote::Remote,
) -> Result<Vec<RemoteObjectPackIndex>> {
    let mut indexes = Vec::new();
    for path in block_on_remote(remote.list_raw(DIR_OBJECTS_PACK))? {
        if !path.ends_with(".idx") {
            continue;
        }
        let Some(bytes) = block_on_remote(remote.get_raw(&path))? else {
            continue;
        };
        indexes.push(decode_remote_object_pack_index(&path, &bytes)?);
    }
    Ok(indexes)
}

fn decode_remote_object_pack_index(path: &str, bytes: &[u8]) -> Result<RemoteObjectPackIndex> {
    let index: RemoteObjectPackIndex =
        serde_json::from_slice(bytes).map_err(|err| RepoErr::InvalidRemoteObject {
            path: path.to_string(),
            message: format!("invalid pack index JSON: {err}"),
        })?;
    if index.version != REMOTE_OBJECT_PACK_VERSION {
        return Err(RepoErr::InvalidRemoteObject {
            path: path.to_string(),
            message: format!(
                "unsupported pack index version {}; expected {}",
                index.version, REMOTE_OBJECT_PACK_VERSION
            ),
        });
    }
    if !index.pack.starts_with(&format!("{DIR_OBJECTS_PACK}/")) || !index.pack.ends_with(".pack") {
        return Err(RepoErr::InvalidRemoteObject {
            path: path.to_string(),
            message: format!("pack path `{}` is outside {DIR_OBJECTS_PACK}", index.pack),
        });
    }
    let min_offset = REMOTE_OBJECT_PACK_MAGIC.len() as u64;
    for entry in &index.objects {
        if entry.len == 0 {
            return Err(RepoErr::InvalidRemoteObject {
                path: path.to_string(),
                message: format!("pack entry for object {} is empty", entry.id),
            });
        }
        if entry.offset < min_offset {
            return Err(RepoErr::InvalidRemoteObject {
                path: path.to_string(),
                message: format!(
                    "pack entry for object {} starts inside pack header",
                    entry.id
                ),
            });
        }
        entry
            .offset
            .checked_add(entry.len)
            .ok_or_else(|| RepoErr::InvalidRemoteObject {
                path: path.to_string(),
                message: format!("pack entry for object {} overflows u64 range", entry.id),
            })?;
    }
    Ok(index)
}

fn parse_remote_head_branch(path: &str, bytes: bytes::Bytes) -> Result<Option<String>> {
    let raw = String::from_utf8(bytes.to_vec()).map_err(|err| RepoErr::InvalidRemoteObject {
        path: path.to_string(),
        message: err.to_string(),
    })?;
    let target = raw.trim();
    if target.is_empty() {
        return Err(RepoErr::InvalidRemoteObject {
            path: path.to_string(),
            message: "empty HEAD".to_string(),
        });
    }
    let Some(reference) = target.strip_prefix("ref: ") else {
        return Ok(None);
    };
    let Some(branch) = reference.strip_prefix("refs/heads/") else {
        return Err(RepoErr::InvalidRemoteObject {
            path: path.to_string(),
            message: format!("HEAD points outside refs/heads: {reference}"),
        });
    };
    validate_ref_name(branch)?;
    Ok(Some(branch.to_string()))
}

fn worktree_for_file(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn remove_empty_parent_dirs(mut dir: Option<&Path>, root: &Path) -> Result<()> {
    while let Some(current) = dir {
        if current == root || !current.starts_with(root) {
            break;
        }
        match fs::remove_dir(current) {
            Ok(()) => dir = current.parent(),
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotFound
                ) =>
            {
                break;
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn path_tree_contains_only_file(dir: &Path, allowed_file: &Path) -> Result<bool> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if !path_tree_contains_only_file(&path, allowed_file)? {
                return Ok(false);
            }
        } else if path != allowed_file {
            return Ok(false);
        }
    }
    Ok(true)
}

fn move_path_if_exists(from: PathBuf, to: PathBuf) -> Result<()> {
    if !from.exists() {
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(from, to)?;
    Ok(())
}

fn remove_path_if_exists(path: PathBuf) -> Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)?;
    } else if path.is_file() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn normalize_discovery_start(start: &Path) -> Result<PathBuf> {
    if start.exists() {
        let start = fs::canonicalize(start)?;
        if start.is_file() {
            Ok(start
                .parent()
                .map_or_else(|| PathBuf::from("/"), Path::to_path_buf))
        } else {
            Ok(start)
        }
    } else {
        let base = if start.extension().is_some() {
            worktree_for_file(start)
        } else {
            start
        };
        Ok(fs::canonicalize(base)?)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RevisionOp {
    FirstParent(usize),
    Parent(usize),
}

fn split_revision_ops(rev: &str) -> Result<(&str, Vec<RevisionOp>)> {
    let Some(first_op) = rev.find(['~', '^']) else {
        return Ok((rev, Vec::new()));
    };
    if first_op == 0 {
        return Err(RepoErr::InvalidRevision(rev.to_string()));
    }

    let base = &rev[..first_op];
    let suffix = &rev[first_op..];
    let bytes = suffix.as_bytes();
    let mut ops = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        if op != b'~' && op != b'^' {
            return Err(RepoErr::InvalidRevision(rev.to_string()));
        }
        i += 1;
        let digits_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let value = if digits_start == i {
            1
        } else {
            suffix[digits_start..i]
                .parse()
                .map_err(|_| RepoErr::InvalidRevision(rev.to_string()))?
        };
        ops.push(match op {
            b'~' => RevisionOp::FirstParent(value),
            b'^' => RevisionOp::Parent(value),
            _ => unreachable!("validated op"),
        });
    }

    Ok((base, ops))
}

fn diff_repo_maps(
    from: impl Into<String>,
    to: impl Into<String>,
    from_files: &BTreeMap<String, CommitFileState>,
    to_files: &BTreeMap<String, CommitFileState>,
    from_artifacts: &BTreeMap<String, CommitArtifactState>,
    to_artifacts: &BTreeMap<String, CommitArtifactState>,
    path: Option<&str>,
) -> RepoDiff {
    let path = path.map(normalize_repo_path);
    let mut keys = BTreeMap::<String, ()>::new();
    for key in from_files.keys().chain(to_files.keys()) {
        if repo_path_matches_filter(key, path.as_deref()) {
            keys.insert(key.clone(), ());
        }
    }

    let mut files = Vec::new();
    for key in keys.keys() {
        let before = from_files.get(key).cloned();
        let after = to_files.get(key).cloned();
        let change = match (&before, &after) {
            (None, Some(_)) => Some(RepoFileChange::Added),
            (Some(_), None) => Some(RepoFileChange::Deleted),
            (Some(before), Some(after)) if before != after => Some(RepoFileChange::Modified),
            _ => None,
        };
        if let Some(change) = change {
            files.push(RepoFileDiff {
                path: key.clone(),
                change,
                kind: RepoTrackedPathKind::SqliteDatabase,
                storage: RepoPathStorage::SqliteSnapshot,
                from: before,
                to: after,
                worktree: None,
            });
        }
    }

    let mut artifact_keys = BTreeMap::<String, ()>::new();
    for key in from_artifacts.keys().chain(to_artifacts.keys()) {
        if repo_path_matches_filter(key, path.as_deref()) {
            artifact_keys.insert(key.clone(), ());
        }
    }

    let mut artifacts = Vec::new();
    for key in artifact_keys.keys() {
        let before = from_artifacts.get(key).cloned();
        let after = to_artifacts.get(key).cloned();
        let change = match (&before, &after) {
            (None, Some(_)) => Some(RepoFileChange::Added),
            (Some(_), None) => Some(RepoFileChange::Deleted),
            (Some(before), Some(after)) if before != after => Some(RepoFileChange::Modified),
            _ => None,
        };
        if let Some(change) = change {
            artifacts.push(RepoArtifactDiff {
                path: key.clone(),
                change,
                kind: artifact_diff_kind(before.as_ref(), after.as_ref()),
                storage: artifact_diff_storage(before.as_ref(), after.as_ref()),
                from: before,
                to: after,
            });
        }
    }

    let paths = repo_diff_paths(&files, &artifacts);
    RepoDiff {
        from: from.into(),
        to: to.into(),
        paths,
        files,
        artifacts,
    }
}

fn repo_diff_paths(files: &[RepoFileDiff], artifacts: &[RepoArtifactDiff]) -> Vec<RepoPathDiff> {
    let mut paths = Vec::with_capacity(files.len() + artifacts.len());
    paths.extend(files.iter().map(|file| RepoPathDiff {
        path: file.path.clone(),
        change: file.change,
        kind: file.kind,
        storage: file.storage,
    }));
    paths.extend(artifacts.iter().map(|artifact| RepoPathDiff {
        path: artifact.path.clone(),
        change: artifact.change,
        kind: artifact.kind,
        storage: artifact.storage,
    }));
    paths.sort_by(|left, right| left.path.cmp(&right.path));
    paths
}

fn commit_path_changes(
    from_files: &BTreeMap<String, CommitFileState>,
    to_files: &BTreeMap<String, CommitFileState>,
    from_artifacts: &BTreeMap<String, CommitArtifactState>,
    to_artifacts: &BTreeMap<String, CommitArtifactState>,
) -> Vec<CommitPathChange> {
    let diff = diff_repo_maps(
        "parent",
        "commit",
        from_files,
        to_files,
        from_artifacts,
        to_artifacts,
        None,
    );
    diff.paths
        .into_iter()
        .map(|path| CommitPathChange {
            path: path.path,
            change: path.change,
            kind: path.kind,
            storage: path.storage,
        })
        .collect()
}

fn repo_path_matches_filter(key: &str, path: Option<&str>) -> bool {
    path.is_none_or(|path| {
        path.is_empty()
            || key == path
            || key
                .strip_prefix(path)
                .is_some_and(|suffix| suffix.starts_with('/'))
    })
}

fn validate_remote_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.contains("@{")
        || name == "@"
        || name.starts_with('-')
        || name.ends_with('.')
        || name.ends_with(".lock")
        || name.chars().any(is_invalid_ref_char)
    {
        return Err(RepoErr::InvalidRemoteName(name.to_string()));
    }
    Ok(())
}

fn validate_ref_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('/')
        || name.ends_with('/')
        || name.contains("//")
        || name.contains("..")
        || name.contains("@{")
        || name == "@"
        || name.starts_with('-')
        || name.ends_with('.')
        || name.chars().any(is_invalid_ref_char)
    {
        return Err(RepoErr::InvalidRefName(name.to_string()));
    }

    if name.split('/').any(|part| {
        part == "."
            || part == ".."
            || part.is_empty()
            || part.starts_with('.')
            || part.ends_with(".lock")
    }) {
        return Err(RepoErr::InvalidRefName(name.to_string()));
    }

    Ok(())
}

fn is_invalid_ref_char(ch: char) -> bool {
    ch.is_control() || ch.is_whitespace() || matches!(ch, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
}

fn validate_full_ref(reference: &str) -> Result<()> {
    if let Some(name) = reference.strip_prefix("refs/heads/") {
        validate_ref_name(name)
    } else if let Some(rest) = reference.strip_prefix("refs/remotes/") {
        let (remote, branch) = rest
            .split_once('/')
            .ok_or_else(|| RepoErr::InvalidRefName(reference.to_string()))?;
        validate_remote_name(remote)?;
        validate_ref_name(branch)
    } else if let Some(name) = reference.strip_prefix("refs/tags/") {
        validate_ref_name(name)
    } else {
        Err(RepoErr::InvalidRefName(reference.to_string()))
    }
}

trait WriteAll {
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()>;
}

impl WriteAll for fs::File {
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        Write::write_all(self, buf)
    }
}

#[cfg(test)]
mod tests;
