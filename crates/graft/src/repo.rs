use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Display},
    fs,
    io::Read,
    path::{Path, PathBuf},
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use futures::{StreamExt, TryStreamExt, stream};

pub mod index;
pub mod object;

pub use object::CommitTableSummary;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    core::{
        LogId, VolumeId, commit_hash::CommitHash, lsn::LSN, lsn::LSNRangeExt, page_count::PageCount,
    },
    remote::{RemoteConfig, RemoteErr},
    snapshot::Snapshot,
};

pub const GRAFT_DIR: &str = ".graft";
pub const REPOSITORY_FORMAT_VERSION: u32 = 2;
pub const OBJECT_FORMAT: &str = "blake3";
const NULL_OBJECT_ID: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const REFLOG_ACTOR: &str = "Graft <graft@example.invalid>";

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
const DIR_INDEX: &str = "index";
const DIR_LOCKS: &str = "locks";
const DIR_TMP: &str = "tmp";
const DIR_LOGS_REFS: &str = "logs/refs";
const DIR_LOGS_HEAD: &str = "logs/HEAD";
const SQLITE_DATABASE_MAGIC: &[u8; 16] = b"SQLite format 3\0";
const REMOTE_REF_READ_CONCURRENCY: usize = 5;

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

    #[error("path `{path}` does not exist in revision `{rev}`")]
    PathNotFoundInRevision { path: String, rev: String },

    #[error("path `{0}` is not tracked")]
    PathNotTracked(String),

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

    #[error(transparent)]
    Object(#[from] object::ObjectErr),

    #[error(transparent)]
    Remote(#[from] RemoteErr),
}

pub type Result<T> = std::result::Result<T, RepoErr>;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepoConfig {
    pub core: CoreConfig,

    #[serde(default)]
    pub extensions: ExtensionsConfig,

    #[serde(default)]
    pub remotes: BTreeMap<String, RemoteConfig>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub branches: BTreeMap<String, BranchConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreConfig {
    pub repository_format_version: u32,
    pub default_branch: String,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            repository_format_version: REPOSITORY_FORMAT_VERSION,
            default_branch: "main".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionsConfig {
    pub object_format: String,
}

impl Default for ExtensionsConfig {
    fn default() -> Self {
        Self { object_format: OBJECT_FORMAT.to_string() }
    }
}

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
struct ParsedRefspec {
    source: Option<BranchPattern>,
    destination: Option<BranchPattern>,
    force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BranchPattern {
    Exact(String),
    Wildcard { prefix: String, suffix: String },
}

impl BranchPattern {
    fn is_wildcard(&self) -> bool {
        matches!(self, Self::Wildcard { .. })
    }

    fn exact(&self) -> Option<&str> {
        match self {
            Self::Exact(branch) => Some(branch),
            Self::Wildcard { .. } => None,
        }
    }

    fn capture<'a>(&self, branch: &'a str) -> Result<Option<&'a str>> {
        match self {
            Self::Exact(pattern) => Ok((branch == pattern).then_some("")),
            Self::Wildcard { prefix, suffix } => {
                let Some(rest) = branch.strip_prefix(prefix) else {
                    return Ok(None);
                };
                let Some(capture) = rest.strip_suffix(suffix) else {
                    return Ok(None);
                };
                if capture.is_empty() {
                    return Ok(None);
                }
                validate_ref_name(capture)?;
                Ok(Some(capture))
            }
        }
    }

    fn expand(&self, capture: &str) -> Result<String> {
        match self {
            Self::Exact(branch) => Ok(branch.clone()),
            Self::Wildcard { prefix, suffix } => {
                validate_ref_name(capture)?;
                let branch = format!("{prefix}{capture}{suffix}");
                validate_ref_name(&branch)?;
                Ok(branch)
            }
        }
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tables: Vec<CommitTableSummary>,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub changed_tables: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitFileState {
    pub volume: VolumeId,
    pub snapshot: RepoSnapshot,
}

fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoDiff {
    pub from: String,
    pub to: String,
    pub files: Vec<RepoFileDiff>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoFileDiff {
    pub path: String,
    pub change: RepoFileChange,
    pub from: Option<CommitFileState>,
    pub to: Option<CommitFileState>,
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
pub struct CheckoutPlan {
    pub target: Option<String>,
    pub files: BTreeMap<String, CommitFileState>,
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
    pub unstaged: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unstaged_changes: Vec<RepoWorktreeChange>,
    pub staged: Vec<String>,
    pub conflicted: Vec<String>,
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

    pub fn object_store(&self) -> object::LooseObjectStore {
        object::LooseObjectStore::new(self.graft_dir.join("objects"))
    }

    pub fn config(&self) -> Result<RepoConfig> {
        let raw = fs::read_to_string(self.config_path())?;
        Ok(toml::from_str(&raw)?)
    }

    pub fn write_config(&self, config: &RepoConfig) -> Result<()> {
        let raw = toml::to_string_pretty(config)?;
        fs::write(self.config_path(), raw)?;
        Ok(())
    }

    pub fn head(&self) -> Result<Head> {
        let raw = fs::read_to_string(self.head_path())?;
        Head::parse(&raw)
    }

    pub fn write_head(&self, head: &Head) -> Result<()> {
        self.write_head_with_message(head, "HEAD update")
    }

    fn write_head_with_message(&self, head: &Head, message: &str) -> Result<()> {
        if let Head::Branch { name } = head {
            validate_ref_name(name)?;
        }
        let old = self.current_head_for_reflog()?;
        let old_target = old
            .as_ref()
            .map(|head| self.head_reflog_target(head))
            .transpose()?
            .flatten();
        let new_target = self.head_reflog_target(head)?;
        write_file_atomic(&self.head_path(), head.serialize().as_bytes())?;
        self.append_head_reflog(old_target.as_deref(), new_target.as_deref(), message)?;
        Ok(())
    }

    pub fn status(&self) -> Result<RepoStatus> {
        let config = self.config()?;
        let head = self.head()?;
        let upstream = head
            .branch_name()
            .map(|branch| self.branch_upstream(branch))
            .transpose()?
            .flatten();
        let head_target = self.head_target()?;
        let index = self.read_index()?;
        let branches = self.branches()?;
        let remotes = self.remotes()?;
        let upstream_status = self.upstream_status(head_target.as_deref(), upstream.as_ref())?;
        let ahead = upstream_status.as_ref().map_or(0, |status| status.ahead);
        let behind = upstream_status.as_ref().map_or(0, |status| status.behind);
        let unstaged_changes = self.unstaged_changes_for_index(&index)?;
        let unstaged = unstaged_changes
            .iter()
            .map(|change| change.path.clone())
            .collect();
        let dirty = !unstaged_changes.is_empty();

        Ok(RepoStatus {
            worktree: self.worktree.clone(),
            graft_dir: self.graft_dir.clone(),
            repository_format_version: config.core.repository_format_version,
            head,
            head_target,
            merge_head: self.merge_head()?,
            orig_head: self.orig_head()?,
            dirty,
            unstaged,
            unstaged_changes,
            staged: index.staged_paths(),
            conflicted: index.conflicted_paths(),
            branches,
            remotes,
            upstream,
            upstream_status,
            ahead,
            behind,
        })
    }

    fn upstream_status(
        &self,
        local: Option<&str>,
        upstream: Option<&BranchUpstream>,
    ) -> Result<Option<RepoUpstreamStatus>> {
        let Some(local) = local else {
            return Ok(None);
        };
        let Some(upstream) = upstream else {
            return Ok(None);
        };
        let Some(remote_target) = self.remote_tracking_ref(&upstream.remote, &upstream.branch)?
        else {
            return Ok(None);
        };

        let local_reachable = self.reachable_commits(local)?;
        let remote_reachable = self.reachable_commits(&remote_target)?;
        let ahead = local_reachable.difference(&remote_reachable).count();
        let behind = remote_reachable.difference(&local_reachable).count();
        let state = match (ahead, behind) {
            (0, 0) => RepoUpstreamState::UpToDate,
            (_, 0) => RepoUpstreamState::Ahead,
            (0, _) => RepoUpstreamState::Behind,
            _ => RepoUpstreamState::Diverged,
        };

        Ok(Some(RepoUpstreamStatus {
            remote: upstream.remote.clone(),
            branch: upstream.branch.clone(),
            local: local.to_string(),
            remote_target,
            ahead,
            behind,
            state,
        }))
    }

    fn reachable_commits(&self, start: &str) -> Result<BTreeSet<String>> {
        let mut reachable = BTreeSet::new();
        let mut stack = vec![start.to_string()];
        while let Some(id) = stack.pop() {
            if !reachable.insert(id.clone()) {
                continue;
            }
            for parent in commit_parent_ids(&self.read_commit(&id)?) {
                stack.push(parent);
            }
        }
        Ok(reachable)
    }

    pub fn branches(&self) -> Result<Vec<BranchInfo>> {
        let config = self.config()?;
        let head = self.head()?;
        let current = head.branch_name();
        let mut branches = BTreeMap::<String, Option<String>>::new();

        Self::collect_ref_files(&self.graft_dir.join(DIR_REFS_HEADS), "", &mut branches)?;

        if let Some(current) = current
            && !branches.contains_key(current)
        {
            branches.insert(current.to_string(), None);
        }

        branches
            .into_iter()
            .map(|(name, target)| {
                let upstream = branch_upstream_from_config(&config, &name)?;
                Ok(BranchInfo {
                    current: current == Some(name.as_str()),
                    name,
                    target,
                    upstream,
                })
            })
            .collect::<Result<Vec<_>>>()
    }

    pub fn remote_tracking_branches(&self) -> Result<Vec<RemoteBranchRef>> {
        let mut refs = BTreeMap::<String, Option<String>>::new();
        Self::collect_ref_files(&self.graft_dir.join(DIR_REFS_REMOTES), "", &mut refs)?;

        let mut branches = Vec::new();
        for (name, target) in refs {
            let Some((remote, branch)) = name.split_once('/') else {
                continue;
            };
            validate_remote_name(remote)?;
            validate_ref_name(branch)?;
            let Some(head) = target else {
                continue;
            };
            branches.push(RemoteBranchRef {
                remote: remote.to_string(),
                branch: branch.to_string(),
                head,
            });
        }
        Ok(branches)
    }

    pub fn branch_create(&self, name: &str, start_point: Option<&str>) -> Result<BranchInfo> {
        validate_ref_name(name)?;
        if self.branch_exists(name) {
            return Err(RepoErr::BranchExists(name.to_string()));
        }

        let target = match start_point {
            Some(target) => self.resolve_revision(target)?,
            None => self.head_target()?.ok_or(RepoErr::UnbornHead)?,
        };

        self.write_branch_ref(name, &target, "branch: create")?;
        Ok(BranchInfo {
            name: name.to_string(),
            target: Some(target),
            current: self
                .head()
                .ok()
                .and_then(|head| head.branch_name().map(str::to_string))
                == Some(name.to_string()),
            upstream: self.branch_upstream(name)?,
        })
    }

    pub fn branch_create_unborn(&self, name: &str) -> Result<BranchInfo> {
        validate_ref_name(name)?;
        if self.branch_exists(name) {
            return Err(RepoErr::BranchExists(name.to_string()));
        }
        self.write_ref_update(&format!("refs/heads/{name}"), "", "branch: create unborn")?;
        Ok(BranchInfo {
            name: name.to_string(),
            target: None,
            current: false,
            upstream: self.branch_upstream(name)?,
        })
    }

    pub fn branch_delete(&self, name: &str, force: bool) -> Result<BranchInfo> {
        validate_ref_name(name)?;
        if self.current_branch()?.as_deref() == Some(name) {
            return Err(RepoErr::BranchIsCurrent(name.to_string()));
        }

        if !self.branch_exists(name) {
            return Err(RepoErr::BranchNotFound(name.to_string()));
        }
        let target = self.read_branch_ref(name)?;

        if !force && let Some(target) = &target {
            let merged = if let Some(head) = self.head_target()? {
                self.is_ancestor(target, &head)?
            } else {
                false
            };
            if !merged {
                return Err(RepoErr::BranchNotMerged {
                    branch: name.to_string(),
                    target: target.clone(),
                });
            }
        }

        self.delete_ref(&format!("refs/heads/{name}"))?;
        self.delete_ref_log(&format!("refs/heads/{name}"))?;
        let mut repo_config = self.config()?;
        repo_config.branches.remove(name);
        self.write_config(&repo_config)?;
        Ok(BranchInfo {
            name: name.to_string(),
            target,
            current: false,
            upstream: None,
        })
    }

    pub fn branch_rename(&self, old: &str, new: &str, force: bool) -> Result<BranchInfo> {
        validate_ref_name(old)?;
        validate_ref_name(new)?;

        if old == new {
            return self.branch_info(old);
        }

        let current = self.current_branch()?;
        let old_is_current = current.as_deref() == Some(old);
        let new_is_current = current.as_deref() == Some(new);
        let old_exists = self.branch_exists(old);
        if !old_exists && !old_is_current {
            return Err(RepoErr::BranchNotFound(old.to_string()));
        }
        if new_is_current {
            return Err(RepoErr::BranchIsCurrent(new.to_string()));
        }

        let new_exists = self.branch_exists(new);
        if new_exists && !force {
            return Err(RepoErr::BranchExists(new.to_string()));
        }

        let old_ref = format!("refs/heads/{old}");
        let new_ref = format!("refs/heads/{new}");
        let target = if old_exists {
            self.read_branch_ref(old)?
        } else {
            None
        };
        let target_raw = target.as_deref().unwrap_or("");
        let message = format!("branch: renamed {old} to {new}");

        let mut repo_config = self.config()?;
        let old_branch_config = repo_config.branches.remove(old);
        if force {
            repo_config.branches.remove(new);
        }
        if let Some(old_branch_config) = old_branch_config {
            repo_config
                .branches
                .insert(new.to_string(), old_branch_config);
        }

        Self::ensure_path_namespace_available_for_rename(&self.graft_dir, &old_ref, &new_ref)?;
        let reflog_root = self.graft_dir.join(DIR_LOGS_REFS);
        if reflog_root.join(&old_ref).is_file() {
            Self::ensure_path_namespace_available_for_rename(&reflog_root, &old_ref, &new_ref)?;
        }

        if new_exists {
            self.delete_ref(&new_ref)?;
            self.delete_ref_log(&new_ref)?;
        }
        if old_exists {
            self.delete_ref(&old_ref)?;
        }

        self.ensure_ref_namespace_available(&new_ref)?;
        self.move_ref_log_for_rename(&old_ref, &new_ref)?;
        self.write_ref(&new_ref, target_raw)?;
        self.append_ref_reflog(&new_ref, target.as_deref(), target.as_deref(), &message)?;

        if old_is_current {
            write_file_atomic(&self.head_path(), Head::branch(new).serialize().as_bytes())?;
            self.append_head_reflog(target.as_deref(), target.as_deref(), &message)?;
        }

        self.write_config(&repo_config)?;
        self.branch_info(new)
    }

    pub fn switch_branch(&self, name: &str) -> Result<()> {
        let plan = self.plan_switch_branch(name)?;
        self.apply_switch_branch_plan(name, &plan)
    }

    pub fn plan_switch_branch(&self, name: &str) -> Result<CheckoutPlan> {
        validate_ref_name(name)?;

        let default_branch = self.config()?.core.default_branch;
        let target = self.read_branch_ref(name)?;
        if target.is_none() && name != default_branch && !self.branch_exists(name) {
            return Err(RepoErr::BranchNotFound(name.to_string()));
        }

        self.checkout_plan_for_target(target)
    }

    pub fn apply_switch_branch_plan(&self, name: &str, _plan: &CheckoutPlan) -> Result<()> {
        validate_ref_name(name)?;
        self.write_head_with_message(&Head::branch(name), &format!("checkout: moving to {name}"))
    }

    pub fn switch_new_branch(&self, name: &str, start_point: Option<&str>) -> Result<BranchInfo> {
        let plan = self.plan_switch_new_branch(name, start_point)?;
        self.apply_switch_new_branch_plan(&plan)
    }

    pub fn plan_switch_new_branch(
        &self,
        name: &str,
        start_point: Option<&str>,
    ) -> Result<SwitchNewBranchPlan> {
        validate_ref_name(name)?;
        if self.branch_exists(name) {
            return Err(RepoErr::BranchExists(name.to_string()));
        }
        self.ensure_ref_namespace_available(&format!("refs/heads/{name}"))?;

        let target = match start_point {
            Some(target) => Some(self.resolve_revision(target)?),
            None => self.head_target()?,
        };
        let checkout = self.checkout_plan_for_target(target.clone())?;
        let branch = BranchInfo {
            name: name.to_string(),
            target,
            current: true,
            upstream: self.branch_upstream(name)?,
        };
        Ok(SwitchNewBranchPlan { branch, checkout })
    }

    pub fn apply_switch_new_branch_plan(&self, plan: &SwitchNewBranchPlan) -> Result<BranchInfo> {
        if let Some(target) = &plan.branch.target {
            self.write_branch_ref(&plan.branch.name, target, "branch: create")?;
        } else {
            self.write_ref_update(
                &format!("refs/heads/{}", plan.branch.name),
                "",
                "branch: create unborn",
            )?;
        }
        self.write_head_with_message(
            &Head::branch(plan.branch.name.clone()),
            &format!("checkout: moving to {}", plan.branch.name),
        )?;
        Ok(plan.branch.clone())
    }

    pub fn tags(&self) -> Result<Vec<TagInfo>> {
        let mut tags = BTreeMap::<String, Option<String>>::new();
        Self::collect_ref_files(&self.graft_dir.join(DIR_REFS_TAGS), "", &mut tags)?;
        tags.into_iter()
            .filter_map(|(name, target)| target.map(|target| self.tag_info_from_ref(name, target)))
            .collect()
    }

    pub fn tag_create(&self, name: &str, target: Option<&str>) -> Result<TagInfo> {
        validate_ref_name(name)?;
        if self.tag_exists(name) {
            return Err(RepoErr::TagExists(name.to_string()));
        }

        let target = match target {
            Some(target) => self.resolve_revision(target)?,
            None => self.head_target()?.ok_or(RepoErr::UnbornHead)?,
        };

        self.write_tag_ref(name, &target, "tag: create")?;
        Ok(TagInfo {
            name: name.to_string(),
            object: target.clone(),
            target,
            annotated: false,
            message: None,
        })
    }

    pub fn tag_create_annotated(
        &self,
        name: &str,
        target: Option<&str>,
        message: impl Into<String>,
    ) -> Result<TagInfo> {
        validate_ref_name(name)?;
        if self.tag_exists(name) {
            return Err(RepoErr::TagExists(name.to_string()));
        }

        let target = match target {
            Some(target) => self.resolve_revision(target)?,
            None => self.head_target()?.ok_or(RepoErr::UnbornHead)?,
        };
        let target_id = object::ObjectId::from_str(&target)?;
        let message = message.into();
        let tag_object = object::TagObject {
            object: target_id,
            object_type: object::ObjectKind::Commit,
            name: name.to_string(),
            tagger: object::Signature::new("Graft", "graft@example.invalid", now_ms(), "+0000"),
            message: message.clone(),
        };
        let object = self
            .object_store()
            .write(&object::Object::Tag(tag_object))?;
        let object = object.to_string();

        self.write_tag_ref(name, &object, "tag: create annotated")?;
        Ok(TagInfo {
            name: name.to_string(),
            object,
            target,
            annotated: true,
            message: Some(message),
        })
    }

    pub fn tag_delete(&self, name: &str) -> Result<TagInfo> {
        validate_ref_name(name)?;
        let object = self
            .read_tag_ref(name)?
            .ok_or_else(|| RepoErr::TagNotFound(name.to_string()))?;
        let tag = self.tag_info_from_ref(name.to_string(), object)?;
        self.delete_tag_ref(name)?;
        self.delete_ref_log(&format!("refs/tags/{name}"))?;
        Ok(tag)
    }

    pub fn remote_add(&self, name: &str, config: RemoteConfig) -> Result<RemoteInfo> {
        validate_remote_name(name)?;
        let mut repo_config = self.config()?;
        if repo_config.remotes.contains_key(name) {
            return Err(RepoErr::RemoteExists(name.to_string()));
        }
        repo_config.remotes.insert(name.to_string(), config.clone());
        self.write_config(&repo_config)?;
        fs::create_dir_all(self.graft_dir.join(DIR_REFS_REMOTES).join(name))?;
        Ok(RemoteInfo { name: name.to_string(), config })
    }

    pub fn remote_remove(&self, name: &str) -> Result<RemoteInfo> {
        validate_remote_name(name)?;
        let mut repo_config = self.config()?;
        let Some(config) = repo_config.remotes.remove(name) else {
            return Err(RepoErr::RemoteNotFound(name.to_string()));
        };
        repo_config
            .branches
            .retain(|_, branch| branch.remote.as_deref() != Some(name));
        self.write_config(&repo_config)?;

        remove_path_if_exists(self.graft_dir.join(DIR_REFS_REMOTES).join(name))?;
        remove_path_if_exists(
            self.graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs")
                .join("remotes")
                .join(name),
        )?;
        Ok(RemoteInfo { name: name.to_string(), config })
    }

    pub fn remote_rename(&self, old: &str, new: &str) -> Result<RemoteInfo> {
        validate_remote_name(old)?;
        validate_remote_name(new)?;
        if old == new {
            let config = self
                .config()?
                .remotes
                .remove(old)
                .ok_or_else(|| RepoErr::RemoteNotFound(old.to_string()))?;
            return Ok(RemoteInfo { name: new.to_string(), config });
        }

        let mut repo_config = self.config()?;
        let Some(config) = repo_config.remotes.remove(old) else {
            return Err(RepoErr::RemoteNotFound(old.to_string()));
        };
        if repo_config.remotes.contains_key(new) {
            return Err(RepoErr::RemoteExists(new.to_string()));
        }
        if self.graft_dir.join(DIR_REFS_REMOTES).join(new).exists()
            || self
                .graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs")
                .join("remotes")
                .join(new)
                .exists()
        {
            return Err(RepoErr::RemoteExists(new.to_string()));
        }

        for branch in repo_config.branches.values_mut() {
            if branch.remote.as_deref() == Some(old) {
                branch.remote = Some(new.to_string());
            }
        }
        repo_config.remotes.insert(new.to_string(), config.clone());
        self.write_config(&repo_config)?;

        move_path_if_exists(
            self.graft_dir.join(DIR_REFS_REMOTES).join(old),
            self.graft_dir.join(DIR_REFS_REMOTES).join(new),
        )?;
        move_path_if_exists(
            self.graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs")
                .join("remotes")
                .join(old),
            self.graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs")
                .join("remotes")
                .join(new),
        )?;

        Ok(RemoteInfo { name: new.to_string(), config })
    }

    pub fn remote_get_url(&self, name: &str) -> Result<RemoteInfo> {
        validate_remote_name(name)?;
        let config = self
            .config()?
            .remotes
            .get(name)
            .cloned()
            .ok_or_else(|| RepoErr::RemoteNotFound(name.to_string()))?;
        Ok(RemoteInfo { name: name.to_string(), config })
    }

    pub fn remote_set_url(&self, name: &str, config: RemoteConfig) -> Result<RemoteInfo> {
        validate_remote_name(name)?;
        let mut repo_config = self.config()?;
        let Some(remote_config) = repo_config.remotes.get_mut(name) else {
            return Err(RepoErr::RemoteNotFound(name.to_string()));
        };
        *remote_config = config.clone();
        self.write_config(&repo_config)?;
        Ok(RemoteInfo { name: name.to_string(), config })
    }

    pub fn remotes(&self) -> Result<Vec<RemoteInfo>> {
        Ok(self
            .config()?
            .remotes
            .into_iter()
            .map(|(name, config)| RemoteInfo { name, config })
            .collect())
    }

    pub fn set_remote_tracking_ref(&self, remote: &str, branch: &str, target: &str) -> Result<()> {
        validate_remote_name(remote)?;
        validate_ref_name(branch)?;
        self.write_ref_update(
            &format!("refs/remotes/{remote}/{branch}"),
            target,
            &format!("fetch {remote}/{branch}"),
        )
    }

    pub fn remote_tracking_ref(&self, remote: &str, branch: &str) -> Result<Option<String>> {
        validate_remote_name(remote)?;
        validate_ref_name(branch)?;
        self.read_ref(&format!("refs/remotes/{remote}/{branch}"))
    }

    pub fn remote_default_branch(&self, remote: &str) -> Result<Option<String>> {
        validate_remote_name(remote)?;
        let remote_store = self.remote_store(remote)?;
        let Some(head) = block_on_remote(remote_store.get_raw(HEAD_FILE))? else {
            return Ok(None);
        };
        parse_remote_head_branch(HEAD_FILE, head)
    }

    pub fn remote_branch_refs(&self, remote: &str) -> Result<Vec<RemoteBranchRef>> {
        validate_remote_name(remote)?;
        let remote_store = self.remote_store(remote)?;
        self.remote_branch_refs_from_store(remote, &remote_store)
    }

    pub fn remote_branch_head(&self, remote: &str, branch: &str) -> Result<Option<String>> {
        validate_remote_name(remote)?;
        validate_ref_name(branch)?;
        let remote_store = self.remote_store(remote)?;
        let head_path = format!("refs/heads/{branch}");
        let Some(head) = block_on_remote(remote_store.get_raw(&head_path))? else {
            return Ok(None);
        };
        Ok(Some(parse_remote_ref(&head_path, head)?))
    }

    pub fn remote_prune(&self, remote: &str) -> Result<RemotePruneOutcome> {
        validate_remote_name(remote)?;
        let remote_store = self.remote_store(remote)?;
        let remote_branches = self
            .remote_branch_refs_from_store(remote, &remote_store)?
            .into_iter()
            .map(|reference| reference.branch)
            .collect::<BTreeSet<_>>();
        let mut local_tracking = BTreeMap::<String, Option<String>>::new();
        Self::collect_ref_files(
            &self.graft_dir.join(DIR_REFS_REMOTES).join(remote),
            "",
            &mut local_tracking,
        )?;

        let mut branches = Vec::new();
        for branch in local_tracking.keys() {
            validate_ref_name(branch)?;
            if remote_branches.contains(branch) {
                continue;
            }
            let reference = format!("refs/remotes/{remote}/{branch}");
            self.delete_ref_if_exists(&reference)?;
            self.delete_ref_log(&reference)?;
            branches.push(branch.clone());
        }

        Ok(RemotePruneOutcome { remote: remote.to_string(), branches })
    }

    pub fn current_branch(&self) -> Result<Option<String>> {
        Ok(self.head()?.branch_name().map(ToString::to_string))
    }

    pub fn default_branch(&self) -> Result<String> {
        Ok(self.config()?.core.default_branch)
    }

    pub fn branch_target(&self, branch: &str) -> Result<Option<String>> {
        validate_ref_name(branch)?;
        self.read_branch_ref(branch)
    }

    pub fn branch_upstream(&self, branch: &str) -> Result<Option<BranchUpstream>> {
        validate_ref_name(branch)?;
        branch_upstream_from_config(&self.config()?, branch)
    }

    pub fn set_branch_upstream(
        &self,
        branch: &str,
        remote: &str,
        remote_branch: &str,
    ) -> Result<BranchInfo> {
        self.ensure_local_branch_for_config(branch)?;
        validate_remote_name(remote)?;
        validate_ref_name(remote_branch)?;

        let mut repo_config = self.config()?;
        if !repo_config.remotes.contains_key(remote) {
            return Err(RepoErr::RemoteNotFound(remote.to_string()));
        }

        repo_config.branches.insert(
            branch.to_string(),
            BranchConfig {
                remote: Some(remote.to_string()),
                merge: Some(branch_merge_ref(remote_branch)),
            },
        );
        self.write_config(&repo_config)?;
        self.branch_info(branch)
    }

    pub fn unset_branch_upstream(&self, branch: &str) -> Result<BranchInfo> {
        self.ensure_local_branch_for_config(branch)?;
        let mut repo_config = self.config()?;
        repo_config.branches.remove(branch);
        self.write_config(&repo_config)?;
        self.branch_info(branch)
    }

    pub fn default_remote_branch(
        &self,
        remote: Option<&str>,
        branch: Option<&str>,
    ) -> Result<BranchUpstream> {
        if let Some(remote) = remote {
            validate_remote_name(remote)?;
        }
        if let Some(branch) = branch {
            validate_ref_name(branch)?;
        }

        let current_branch = self.current_branch()?;
        let current_upstream = current_branch
            .as_deref()
            .map(|branch| self.branch_upstream(branch))
            .transpose()?
            .flatten();

        let resolved_remote = remote
            .map(ToString::to_string)
            .or_else(|| {
                current_upstream
                    .as_ref()
                    .map(|upstream| upstream.remote.clone())
            })
            .unwrap_or_else(|| "origin".to_string());
        let resolved_branch = branch
            .map(ToString::to_string)
            .or_else(|| {
                if remote.is_none() {
                    current_upstream
                        .as_ref()
                        .map(|upstream| upstream.branch.clone())
                } else {
                    None
                }
            })
            .or(current_branch)
            .unwrap_or_else(|| self.default_branch().unwrap_or_else(|_| "main".to_string()));

        Ok(BranchUpstream {
            remote: resolved_remote,
            branch: resolved_branch,
        })
    }

    pub fn fetch(&self, remote: &str, branch: &str) -> Result<FetchOutcome> {
        validate_remote_name(remote)?;
        validate_ref_name(branch)?;
        let remote_store = self.remote_store(remote)?;
        let head_path = format!("refs/heads/{branch}");
        let Some(head) = block_on_remote(remote_store.get_raw(&head_path))? else {
            return Err(RepoErr::RemoteBranchNotFound {
                remote: remote.to_string(),
                branch: branch.to_string(),
            });
        };
        let head = parse_remote_ref(&head_path, head)?;
        let commits = self.fetch_commit_chain(&remote_store, &head)?;
        self.set_remote_tracking_ref(remote, branch, &head)?;
        Ok(FetchOutcome {
            remote: remote.to_string(),
            branch: branch.to_string(),
            head,
            commits,
        })
    }

    pub fn fetch_all(&self, remote: &str) -> Result<FetchAllOutcome> {
        validate_remote_name(remote)?;
        let remote_store = self.remote_store(remote)?;
        let remote_refs = self.remote_branch_refs_from_store(remote, &remote_store)?;
        let mut branches = Vec::with_capacity(remote_refs.len());

        for remote_ref in remote_refs {
            let commits = self.fetch_commit_chain(&remote_store, &remote_ref.head)?;
            self.set_remote_tracking_ref(remote, &remote_ref.branch, &remote_ref.head)?;
            branches.push(FetchOutcome {
                remote: remote.to_string(),
                branch: remote_ref.branch,
                head: remote_ref.head,
                commits,
            });
        }

        Ok(FetchAllOutcome { remote: remote.to_string(), branches })
    }

    pub fn fetch_refspec(&self, remote: &str, refspec: &str) -> Result<FetchAllOutcome> {
        validate_remote_name(remote)?;
        let parsed = parse_fetch_refspec(remote, refspec)?;
        let remote_store = self.remote_store(remote)?;
        let branches = self.fetch_refspec_with_store(remote, &remote_store, refspec, &parsed)?;
        Ok(FetchAllOutcome { remote: remote.to_string(), branches })
    }

    fn fetch_refspec_with_store(
        &self,
        remote: &str,
        remote_store: &crate::remote::Remote,
        refspec: &str,
        parsed: &ParsedRefspec,
    ) -> Result<Vec<FetchOutcome>> {
        let source = parsed
            .source
            .as_ref()
            .expect("fetch refspec parser rejects delete refspecs");
        let destination = parsed.destination.as_ref().unwrap_or(source);
        let mut outcomes = Vec::new();

        if let Some(source_branch) = source.exact() {
            let head_path = format!("refs/heads/{source_branch}");
            let Some(head) = block_on_remote(remote_store.get_raw(&head_path))? else {
                return Err(RepoErr::RemoteBranchNotFound {
                    remote: remote.to_string(),
                    branch: source_branch.to_string(),
                });
            };
            let head = parse_remote_ref(&head_path, head)?;
            let destination_branch = destination.expand("")?;
            let commits = self.fetch_commit_chain(remote_store, &head)?;
            self.set_remote_tracking_ref(remote, &destination_branch, &head)?;
            outcomes.push(FetchOutcome {
                remote: remote.to_string(),
                branch: destination_branch,
                head,
                commits,
            });
            return Ok(outcomes);
        }

        let remote_refs = self.remote_branch_refs_from_store(remote, remote_store)?;
        for remote_ref in remote_refs {
            let Some(capture) = source.capture(&remote_ref.branch)? else {
                continue;
            };
            let destination_branch = destination.expand(capture)?;
            let commits = self.fetch_commit_chain(remote_store, &remote_ref.head)?;
            self.set_remote_tracking_ref(remote, &destination_branch, &remote_ref.head)?;
            outcomes.push(FetchOutcome {
                remote: remote.to_string(),
                branch: destination_branch,
                head: remote_ref.head,
                commits,
            });
        }

        if outcomes.is_empty() {
            return Err(RepoErr::InvalidRefspec {
                refspec: refspec.to_string(),
                message: "wildcard matched no remote branches".to_string(),
            });
        }
        Ok(outcomes)
    }

    pub fn push(&self, remote: &str, branch: &str) -> Result<PushOutcome> {
        self.push_branch(remote, branch, branch)
    }

    pub fn push_all(&self, remote: &str) -> Result<PushAllOutcome> {
        self.push_all_with_force(remote, false)
    }

    pub fn push_all_with_force(&self, remote: &str, force: bool) -> Result<PushAllOutcome> {
        validate_remote_name(remote)?;
        let mut branches = Vec::new();

        for branch in self.branches()? {
            if branch.target.is_none() {
                continue;
            }
            branches.push(self.push_branch_with_force(
                remote,
                &branch.name,
                &branch.name,
                force,
            )?);
        }

        Ok(PushAllOutcome { remote: remote.to_string(), branches })
    }

    pub fn push_refspec_with_force(
        &self,
        remote: &str,
        refspec: &str,
        force: bool,
    ) -> Result<PushAllOutcome> {
        validate_remote_name(remote)?;
        let parsed = parse_push_refspec(refspec)?;
        let force = force || parsed.force;
        if parsed.source.is_none() {
            let Some(destination) = &parsed.destination else {
                return Err(RepoErr::InvalidRefspec {
                    refspec: refspec.to_string(),
                    message: "delete refspecs require a destination".to_string(),
                });
            };
            if destination.is_wildcard() {
                return Err(RepoErr::InvalidRefspec {
                    refspec: refspec.to_string(),
                    message: "wildcard delete refspecs are not supported".to_string(),
                });
            }
            let remote_branch = destination.expand("")?;
            let outcome = self.push_delete_branch_with_force(remote, &remote_branch, force)?;
            return Ok(PushAllOutcome {
                remote: remote.to_string(),
                branches: vec![outcome],
            });
        }

        let source = parsed.source.as_ref().expect("handled delete refspec");
        let destination = parsed.destination.as_ref().unwrap_or(source);
        let mut branches = Vec::new();

        if let Some(local_branch) = source.exact() {
            let remote_branch = destination.expand("")?;
            branches.push(self.push_branch_with_force(
                remote,
                local_branch,
                &remote_branch,
                force,
            )?);
            return Ok(PushAllOutcome { remote: remote.to_string(), branches });
        }

        for branch in self.branches()? {
            if branch.target.is_none() {
                continue;
            }
            let Some(capture) = source.capture(&branch.name)? else {
                continue;
            };
            let remote_branch = destination.expand(capture)?;
            branches.push(self.push_branch_with_force(
                remote,
                &branch.name,
                &remote_branch,
                force,
            )?);
        }

        if branches.is_empty() {
            return Err(RepoErr::InvalidRefspec {
                refspec: refspec.to_string(),
                message: "wildcard matched no local branches".to_string(),
            });
        }
        Ok(PushAllOutcome { remote: remote.to_string(), branches })
    }

    pub fn push_refspec_branches(&self, refspec: &str) -> Result<Vec<PushRefspecBranch>> {
        let parsed = parse_push_refspec(refspec)?;
        let Some(source) = parsed.source.as_ref() else {
            return Ok(Vec::new());
        };
        let destination = parsed.destination.as_ref().unwrap_or(source);

        if let Some(local_branch) = source.exact() {
            let remote_branch = destination.expand("")?;
            return Ok(vec![PushRefspecBranch {
                local_branch: local_branch.to_string(),
                remote_branch,
            }]);
        }

        let mut branches = Vec::new();
        for branch in self.branches()? {
            if branch.target.is_none() {
                continue;
            }
            let Some(capture) = source.capture(&branch.name)? else {
                continue;
            };
            let remote_branch = destination.expand(capture)?;
            branches.push(PushRefspecBranch { local_branch: branch.name, remote_branch });
        }

        if branches.is_empty() {
            return Err(RepoErr::InvalidRefspec {
                refspec: refspec.to_string(),
                message: "wildcard matched no local branches".to_string(),
            });
        }
        Ok(branches)
    }

    pub fn push_branch(
        &self,
        remote: &str,
        local_branch: &str,
        remote_branch: &str,
    ) -> Result<PushOutcome> {
        self.push_branch_with_force(remote, local_branch, remote_branch, false)
    }

    pub fn push_branch_with_force(
        &self,
        remote: &str,
        local_branch: &str,
        remote_branch: &str,
        force: bool,
    ) -> Result<PushOutcome> {
        validate_remote_name(remote)?;
        validate_ref_name(local_branch)?;
        validate_ref_name(remote_branch)?;
        let Some(head) = self.branch_target(local_branch)? else {
            return Err(RepoErr::UnbornHead);
        };

        let remote_store = self.remote_store(remote)?;
        let head_path = format!("refs/heads/{remote_branch}");
        let remote_head_raw = block_on_remote(remote_store.get_raw(&head_path))?;
        let remote_head = remote_head_raw
            .as_ref()
            .map(|bytes| parse_remote_ref(&head_path, bytes.clone()))
            .transpose()?;

        if let Some(remote_head) = &remote_head
            && !force
            && !self.is_ancestor(remote_head, &head)?
        {
            return Err(RepoErr::NonFastForward {
                remote: remote.to_string(),
                local_branch: local_branch.to_string(),
                remote_branch: remote_branch.to_string(),
            });
        }

        let commits = self.push_commit_chain(&remote_store, &head, remote_head.as_deref())?;
        match block_on_remote(remote_store.compare_and_swap_raw(
            &head_path,
            remote_head_raw.as_deref(),
            format!("{head}\n"),
        )) {
            Ok(()) => {}
            Err(RepoErr::Remote(RemoteErr::CompareAndSwap { .. } | RemoteErr::LockBusy { .. })) => {
                return Err(RepoErr::RemoteRefChanged {
                    remote: remote.to_string(),
                    branch: remote_branch.to_string(),
                });
            }
            Err(err) => return Err(err),
        }
        self.set_remote_head_if_absent(&remote_store, remote_branch)?;
        self.set_remote_tracking_ref(remote, remote_branch, &head)?;

        Ok(PushOutcome {
            remote: remote.to_string(),
            local_branch: local_branch.to_string(),
            remote_branch: remote_branch.to_string(),
            head,
            commits,
            forced: force,
            deleted: false,
        })
    }

    pub fn push_delete_branch_with_force(
        &self,
        remote: &str,
        remote_branch: &str,
        force: bool,
    ) -> Result<PushOutcome> {
        validate_remote_name(remote)?;
        validate_ref_name(remote_branch)?;

        let remote_store = self.remote_store(remote)?;
        let head_path = format!("refs/heads/{remote_branch}");
        let remote_head_raw =
            block_on_remote(remote_store.get_raw(&head_path))?.ok_or_else(|| {
                RepoErr::RemoteBranchNotFound {
                    remote: remote.to_string(),
                    branch: remote_branch.to_string(),
                }
            })?;
        let remote_head = parse_remote_ref(&head_path, remote_head_raw.clone())?;

        if !force
            && let Some(local_tracking) = self.remote_tracking_ref(remote, remote_branch)?
            && local_tracking != remote_head
        {
            return Err(RepoErr::RemoteRefChanged {
                remote: remote.to_string(),
                branch: remote_branch.to_string(),
            });
        }

        match block_on_remote(
            remote_store.compare_and_delete_raw(&head_path, Some(remote_head_raw.as_ref())),
        ) {
            Ok(()) => {}
            Err(RepoErr::Remote(RemoteErr::CompareAndSwap { .. } | RemoteErr::LockBusy { .. })) => {
                return Err(RepoErr::RemoteRefChanged {
                    remote: remote.to_string(),
                    branch: remote_branch.to_string(),
                });
            }
            Err(err) => return Err(err),
        }

        self.delete_ref_if_exists(&format!("refs/remotes/{remote}/{remote_branch}"))?;
        self.delete_ref_log(&format!("refs/remotes/{remote}/{remote_branch}"))?;

        Ok(PushOutcome {
            remote: remote.to_string(),
            local_branch: String::new(),
            remote_branch: remote_branch.to_string(),
            head: remote_head,
            commits: 0,
            forced: force,
            deleted: true,
        })
    }

    pub fn pull(
        &self,
        remote: &str,
        remote_branch: &str,
        local_branch: &str,
    ) -> Result<PullOutcome> {
        let plan = self.plan_pull(remote, remote_branch, local_branch)?;
        self.apply_pull_plan(&plan)
    }

    pub fn plan_pull(
        &self,
        remote: &str,
        remote_branch: &str,
        local_branch: &str,
    ) -> Result<PullPlan> {
        validate_remote_name(remote)?;
        validate_ref_name(remote_branch)?;
        validate_ref_name(local_branch)?;
        if self.current_branch()?.as_deref() != Some(local_branch) {
            return Err(RepoErr::NotCurrentBranch(local_branch.to_string()));
        }
        if self.merge_head()?.is_some() {
            return Err(RepoErr::MergeInProgress);
        }

        let fetch = self.fetch(remote, remote_branch)?;
        let merge = self.plan_merge_revision(&format!("refs/remotes/{remote}/{remote_branch}"))?;
        Ok(PullPlan {
            remote: remote.to_string(),
            remote_branch: remote_branch.to_string(),
            local_branch: local_branch.to_string(),
            fetch,
            merge,
        })
    }

    pub fn plan_pull_refspec(
        &self,
        remote: &str,
        refspec: &str,
        local_branch: &str,
    ) -> Result<PullPlan> {
        validate_remote_name(remote)?;
        validate_ref_name(local_branch)?;
        if self.current_branch()?.as_deref() != Some(local_branch) {
            return Err(RepoErr::NotCurrentBranch(local_branch.to_string()));
        }
        if self.merge_head()?.is_some() {
            return Err(RepoErr::MergeInProgress);
        }

        let mut fetch = self.fetch_refspec(remote, refspec)?.branches;
        if fetch.len() != 1 {
            return Err(RepoErr::InvalidRefspec {
                refspec: refspec.to_string(),
                message: "pull refspec must update exactly one remote-tracking branch".to_string(),
            });
        }
        let fetch = fetch.pop().expect("length checked");
        let merge = self.plan_merge_revision(&format!("refs/remotes/{remote}/{}", fetch.branch))?;
        Ok(PullPlan {
            remote: remote.to_string(),
            remote_branch: fetch.branch.clone(),
            local_branch: local_branch.to_string(),
            fetch,
            merge,
        })
    }

    pub fn apply_pull_plan(&self, plan: &PullPlan) -> Result<PullOutcome> {
        let merge = self.apply_merge_plan(&plan.merge)?;
        Ok(PullOutcome {
            remote: plan.remote.clone(),
            remote_branch: plan.remote_branch.clone(),
            local_branch: plan.local_branch.clone(),
            head: plan.fetch.head.clone(),
            commits: plan.fetch.commits,
            merge,
        })
    }

    pub fn merge_revision(&self, rev: &str) -> Result<MergeOutcome> {
        let plan = self.plan_merge_revision(rev)?;
        self.apply_merge_plan(&plan)
    }

    pub fn plan_merge_revision(&self, rev: &str) -> Result<MergePlan> {
        if self.merge_head()?.is_some() {
            return Err(RepoErr::MergeInProgress);
        }
        let target = self.resolve_revision(rev)?;
        let checkout = self.checkout_plan_for_target(Some(target.clone()))?;
        let head = self.head_target()?;

        let Some(head) = head else {
            let outcome = MergeOutcome::FastForward { from: None, to: target.clone() };
            return Ok(MergePlan {
                rev: rev.to_string(),
                target,
                checkout,
                outcome,
                index: None,
            });
        };

        if self.is_ancestor(&target, &head)? {
            let outcome = MergeOutcome::AlreadyUpToDate { head };
            return Ok(MergePlan {
                rev: rev.to_string(),
                target,
                checkout,
                outcome,
                index: None,
            });
        }

        if self.is_ancestor(&head, &target)? {
            let outcome = MergeOutcome::FastForward { from: Some(head), to: target.clone() };
            return Ok(MergePlan {
                rev: rev.to_string(),
                target,
                checkout,
                outcome,
                index: None,
            });
        }

        let merge_base = self.merge_base(&head, &target)?;
        let base_files = self.files_for_commit(merge_base.as_deref())?;
        let ours_files = self.files_for_commit(Some(&head))?;
        let theirs_files = self.files_for_commit(Some(&target))?;
        let mut index = self.read_index()?;
        let mut staged = Vec::new();
        let mut conflicted = Vec::new();

        let mut keys = BTreeMap::<String, ()>::new();
        for key in base_files
            .keys()
            .chain(ours_files.keys())
            .chain(theirs_files.keys())
        {
            keys.insert(key.clone(), ());
        }

        for key in keys.keys() {
            let base = base_files.get(key);
            let ours = ours_files.get(key);
            let theirs = theirs_files.get(key);

            if ours == theirs || base == theirs {
                continue;
            }

            if base == ours {
                index.remove_path(key);
                if let Some(theirs) = theirs {
                    index.stage(self.index_entry_for_state(
                        key.clone(),
                        index::IndexStage::Normal,
                        theirs.clone(),
                    )?);
                } else {
                    index.stage(index::IndexEntry {
                        path: key.clone(),
                        mode: None,
                        oid: None,
                        stage: index::IndexStage::Normal,
                        file: None,
                    });
                }
                staged.push(key.clone());
                continue;
            }

            self.stage_merge_conflict(key, base, ours, theirs, &mut index)?;
            conflicted.push(key.clone());
        }

        let outcome = MergeOutcome::Merged {
            head,
            target: target.clone(),
            merge_base,
            staged,
            conflicted,
        };
        Ok(MergePlan {
            rev: rev.to_string(),
            target,
            checkout,
            outcome,
            index: Some(index),
        })
    }

    pub fn apply_merge_plan(&self, plan: &MergePlan) -> Result<MergeOutcome> {
        if self.merge_head()?.is_some() {
            return Err(RepoErr::MergeInProgress);
        }

        match &plan.outcome {
            MergeOutcome::FastForward { to, .. } => {
                self.move_head_to(to, &format!("merge {}: fast-forward", plan.rev))?;
            }
            MergeOutcome::AlreadyUpToDate { .. } => {}
            MergeOutcome::Merged { head, target, .. } => {
                let index = plan.index.as_ref().ok_or(RepoErr::UnresolvedConflicts)?;
                self.write_index(index)?;
                self.write_merge_state(head, target)?;
            }
        }

        Ok(plan.outcome.clone())
    }

    pub fn merge_abort(&self) -> Result<String> {
        let plan = self.plan_merge_abort()?;
        self.apply_merge_abort_plan(&plan)
    }

    pub fn plan_merge_abort(&self) -> Result<MergeAbortPlan> {
        let target = self.orig_head()?.ok_or(RepoErr::NoMergeInProgress)?;
        let checkout = self.checkout_plan_for_target(Some(target.clone()))?;
        Ok(MergeAbortPlan { target, checkout })
    }

    pub fn apply_merge_abort_plan(&self, plan: &MergeAbortPlan) -> Result<String> {
        if self.orig_head()?.is_none() && self.merge_head()?.is_none() {
            return Err(RepoErr::NoMergeInProgress);
        }
        self.move_head_to(&plan.target, "merge: abort")?;
        self.clear_index()?;
        self.clear_dirty()?;
        self.clear_merge_state()?;
        Ok(plan.target.clone())
    }

    pub fn commit(&self, message: impl Into<String>) -> Result<CommitObject> {
        let commit = self.commit_with_files(message, BTreeMap::new(), Vec::new())?;
        self.clear_dirty()?;
        Ok(commit)
    }

    #[cfg(test)]
    fn stage_file(
        &self,
        path: impl AsRef<Path>,
        volume: VolumeId,
        snapshot: &Snapshot,
    ) -> Result<index::IndexEntry> {
        let key = self.file_key(path)?;
        let file = CommitFileState {
            volume,
            snapshot: repo_snapshot_with_test_hashes(snapshot),
        };
        self.stage_file_state(key, file)
    }

    fn stage_file_state(&self, key: String, file: CommitFileState) -> Result<index::IndexEntry> {
        let entry = self.index_entry_for_state(key.clone(), index::IndexStage::Normal, file)?;
        let mut index = self.read_index()?;
        index.stage(entry.clone());
        self.write_index(&index)?;
        self.clear_dirty_key(&key)?;
        Ok(entry)
    }

    pub fn stage_file_state_path(
        &self,
        path: impl AsRef<Path>,
        file: CommitFileState,
    ) -> Result<index::IndexEntry> {
        validate_commit_file_state(&file)?;
        let key = self.file_key(path)?;
        self.stage_file_state(key, file)
    }

    pub fn stage_file_removal(&self, path: impl AsRef<Path>) -> Result<index::IndexEntry> {
        let key = self.file_key(path)?;
        if !self.head_files()?.contains_key(&key) {
            return Err(RepoErr::PathNotTracked(key));
        }
        let entry = index::IndexEntry {
            path: key,
            mode: None,
            oid: None,
            stage: index::IndexStage::Normal,
            file: None,
        };
        let mut index = self.read_index()?;
        index.stage(entry.clone());
        self.write_index(&index)?;
        self.clear_dirty_key(&entry.path)?;
        Ok(entry)
    }

    pub fn resolve_file_conflict(
        &self,
        path: impl AsRef<Path>,
        file: Option<CommitFileState>,
    ) -> Result<index::IndexEntry> {
        let key = self.file_key(path)?;
        let mut index = self.read_index()?;
        if !index.conflicted_paths().iter().any(|path| path == &key) {
            return Err(RepoErr::PathNotConflicted(key));
        }

        let entry = if let Some(file) = file {
            self.index_entry_for_state(key.clone(), index::IndexStage::Normal, file)?
        } else {
            index::IndexEntry {
                path: key.clone(),
                mode: None,
                oid: None,
                stage: index::IndexStage::Normal,
                file: None,
            }
        };
        index.stage(entry.clone());
        self.write_index(&index)?;
        self.clear_dirty_key(&key)?;
        Ok(entry)
    }

    fn index_entry_for_state(
        &self,
        key: String,
        stage: index::IndexStage,
        file: CommitFileState,
    ) -> Result<index::IndexEntry> {
        let blob = object::Object::Blob(object::BlobObject::SqliteSnapshot(sqlite_snapshot_blob(
            &file,
        )));
        let oid = self.object_store().write(&blob)?;
        Ok(index::IndexEntry {
            path: key,
            mode: Some(object::TreeEntryMode::SqliteDatabase),
            oid: Some(oid),
            stage,
            file: Some(file),
        })
    }

    pub fn commit_staged(&self, message: impl Into<String>) -> Result<CommitObject> {
        self.commit_staged_with_table_summary(message, Vec::new())
    }

    pub fn commit_staged_with_table_summary(
        &self,
        message: impl Into<String>,
        tables: Vec<CommitTableSummary>,
    ) -> Result<CommitObject> {
        let index = self.read_index()?;
        if index.has_conflicts() {
            return Err(RepoErr::UnresolvedConflicts);
        }
        if !index.has_staged_changes() && self.merge_head()?.is_none() {
            return Err(RepoErr::NoStagedChanges);
        }

        let mut files = self.head_files()?;
        for entry in index.stage0_entries() {
            if let Some(file) = &entry.file {
                files.insert(entry.path.clone(), file.clone());
            } else {
                files.remove(&entry.path);
            }
        }
        let commit = self.commit_with_files(message, files, tables)?;
        self.clear_index()?;
        Ok(commit)
    }

    #[cfg(test)]
    fn commit_file(
        &self,
        path: impl AsRef<Path>,
        message: impl Into<String>,
        volume: VolumeId,
        snapshot: &Snapshot,
    ) -> Result<CommitObject> {
        self.stage_file(path, volume, snapshot)?;
        self.commit_staged(message)
    }

    fn commit_with_files(
        &self,
        message: impl Into<String>,
        files: BTreeMap<String, CommitFileState>,
        tables: Vec<CommitTableSummary>,
    ) -> Result<CommitObject> {
        let head = self.head()?;
        let parents = self.commit_parents()?;
        let parent = parents.first().cloned();
        let timestamp_ms = now_ms();
        let message = message.into();
        let tables = normalize_commit_table_summary(tables);
        let changed_tables = tables.len();
        let object_store = self.object_store();
        let tree = self.write_tree_object(&object_store, &files)?;
        let commit_object = self.canonical_commit_object(
            tree.clone(),
            &parents,
            &message,
            timestamp_ms,
            tables.clone(),
        )?;
        let id = object_store.write(&object::Object::Commit(commit_object))?;
        let commit = CommitObject {
            id: id.to_string(),
            parent,
            parents,
            tree: Some(tree.to_string()),
            message,
            timestamp_ms,
            files,
            tables,
            changed_tables,
        };

        match head {
            Head::Branch { name } => {
                self.write_branch_ref(&name, &commit.id, &format!("commit: {}", commit.message))?
            }
            Head::Detached { .. } => self.write_head_with_message(
                &Head::Detached { commit: commit.id.clone() },
                &format!("commit: {}", commit.message),
            )?,
        }

        self.clear_merge_state()?;
        Ok(commit)
    }

    pub fn log(&self) -> Result<Vec<CommitObject>> {
        let mut commits = vec![];
        let mut stack = self.head_target()?.into_iter().collect::<Vec<_>>();
        let mut seen = BTreeMap::<String, ()>::new();

        while let Some(id) = stack.pop() {
            if seen.insert(id.clone(), ()).is_some() {
                continue;
            }
            let commit = self.read_commit(&id)?;
            for parent in commit_parent_ids(&commit).into_iter().rev() {
                stack.push(parent);
            }
            commits.push(commit);
        }

        Ok(commits)
    }

    pub fn resolve_revision(&self, rev: &str) -> Result<String> {
        let rev = rev.trim();
        if rev.is_empty() {
            return Err(RepoErr::InvalidRevision(rev.to_string()));
        }

        let (base, ops) = split_revision_ops(rev)?;
        let mut id = self.resolve_revision_base(base)?;
        for op in ops {
            id = self.apply_revision_op(&id, op, rev)?;
        }
        Ok(id)
    }

    pub fn diff_revisions(&self, from: &str, to: &str, path: Option<&str>) -> Result<RepoDiff> {
        let from_id = self.resolve_revision(from)?;
        let to_id = self.resolve_revision(to)?;
        let from_commit = self.read_commit(&from_id)?;
        let to_commit = self.read_commit(&to_id)?;

        Ok(diff_file_maps(
            from_id,
            to_id,
            &from_commit.files,
            &to_commit.files,
            path,
        ))
    }

    pub fn diff_staged(&self, path: Option<&str>) -> Result<RepoDiff> {
        let from = self.head_target()?.unwrap_or_else(|| "HEAD".to_string());
        let head_files = self.head_files()?;
        let index_files = self.index_files()?;
        Ok(diff_file_maps(
            from,
            "index",
            &head_files,
            &index_files,
            path,
        ))
    }

    pub fn diff_worktree_file(
        &self,
        path: impl AsRef<Path>,
        state: CommitFileState,
        filter: Option<&str>,
    ) -> Result<RepoDiff> {
        let mut worktree_files = self.index_files()?;
        worktree_files.insert(self.file_key(path)?, state);
        Ok(diff_file_maps(
            "index",
            "worktree",
            &self.index_files()?,
            &worktree_files,
            filter,
        ))
    }

    pub fn diff_worktree_file_removal(
        &self,
        path: impl AsRef<Path>,
        filter: Option<&str>,
    ) -> Result<RepoDiff> {
        let mut worktree_files = self.index_files()?;
        worktree_files.remove(&self.file_key(path)?);
        Ok(diff_file_maps(
            "index",
            "worktree",
            &self.index_files()?,
            &worktree_files,
            filter,
        ))
    }

    pub fn diff_revision_to_worktree_file(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
        state: CommitFileState,
        filter: Option<&str>,
    ) -> Result<RepoDiff> {
        let from_id = self.resolve_revision(rev)?;
        let from_files = self.read_commit(&from_id)?.files;
        let mut worktree_files = from_files.clone();
        worktree_files.insert(self.file_key(path)?, state);
        Ok(diff_file_maps(
            from_id,
            "worktree",
            &from_files,
            &worktree_files,
            filter,
        ))
    }

    pub fn diff_revision_to_worktree_file_removal(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
        filter: Option<&str>,
    ) -> Result<RepoDiff> {
        let from_id = self.resolve_revision(rev)?;
        let from_files = self.read_commit(&from_id)?.files;
        let mut worktree_files = from_files.clone();
        worktree_files.remove(&self.file_key(path)?);
        Ok(diff_file_maps(
            from_id,
            "worktree",
            &from_files,
            &worktree_files,
            filter,
        ))
    }

    pub fn show_revision(&self, rev: &str) -> Result<CommitObject> {
        let id = self.resolve_revision(rev)?;
        self.read_commit(&id)
    }

    pub fn detach(&self, rev: &str) -> Result<String> {
        let plan = self.plan_detach(rev)?;
        self.apply_detach_plan(rev, &plan)
    }

    pub fn plan_detach(&self, rev: &str) -> Result<CheckoutPlan> {
        self.plan_revision_checkout(rev)
    }

    pub fn plan_revision_checkout(&self, rev: &str) -> Result<CheckoutPlan> {
        let id = self.resolve_revision(rev)?;
        self.checkout_plan_for_target(Some(id))
    }

    pub fn apply_detach_plan(&self, rev: &str, plan: &CheckoutPlan) -> Result<String> {
        let id = plan.target.clone().ok_or(RepoErr::UnbornHead)?;
        self.write_head_with_message(
            &Head::Detached { commit: id.clone() },
            &format!("checkout: moving to {rev}"),
        )?;
        Ok(id)
    }

    pub fn checkout_file_from_revision(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
    ) -> Result<CheckoutFileOutcome> {
        let path = self.file_key(path)?;
        self.checkout_file_key_from_revision(rev, path)
    }

    pub fn checkout_file_key_from_revision(
        &self,
        rev: &str,
        path: impl Into<String>,
    ) -> Result<CheckoutFileOutcome> {
        let plan = self.plan_checkout_file_key_from_revision(rev, path)?;
        self.apply_checkout_file_plan(&plan)
    }

    pub fn plan_checkout_file_from_revision(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
    ) -> Result<CheckoutFilePlan> {
        let path = self.file_key(path)?;
        self.plan_checkout_file_key_from_revision(rev, path)
    }

    pub fn plan_checkout_file_key_from_revision(
        &self,
        rev: &str,
        path: impl Into<String>,
    ) -> Result<CheckoutFilePlan> {
        let target = self.resolve_revision(rev)?;
        let path = normalize_repo_path(&path.into());
        let commit = self.read_commit(&target)?;
        let state =
            commit
                .files
                .get(&path)
                .cloned()
                .ok_or_else(|| RepoErr::PathNotFoundInRevision {
                    path: path.clone(),
                    rev: rev.to_string(),
                })?;
        let entry =
            self.index_entry_for_state(path.clone(), index::IndexStage::Normal, state.clone())?;
        Ok(CheckoutFilePlan { target, path, state, entry })
    }

    pub fn apply_checkout_file_plan(&self, plan: &CheckoutFilePlan) -> Result<CheckoutFileOutcome> {
        let mut index = self.read_index()?;
        index.stage(plan.entry.clone());
        self.write_index(&index)?;
        self.clear_dirty_key(&plan.path)?;
        Ok(CheckoutFileOutcome {
            target: plan.target.clone(),
            path: plan.path.clone(),
            state: plan.state.clone(),
        })
    }

    pub fn reset(&self, rev: &str, mode: ResetMode) -> Result<ResetOutcome> {
        let plan = self.plan_reset(rev, mode)?;
        self.apply_reset_plan(&plan)
    }

    pub fn plan_reset(&self, rev: &str, mode: ResetMode) -> Result<ResetPlan> {
        let target = self.resolve_revision(rev)?;
        let checkout = self.checkout_plan_for_target(Some(target.clone()))?;
        Ok(ResetPlan {
            rev: rev.to_string(),
            target,
            mode,
            checkout,
        })
    }

    pub fn apply_reset_plan(&self, plan: &ResetPlan) -> Result<ResetOutcome> {
        self.move_head_to(&plan.target, &format!("reset: moving to {}", plan.rev))?;
        match plan.mode {
            ResetMode::Soft => {}
            ResetMode::Mixed => self.clear_index()?,
            ResetMode::Hard => {
                self.clear_index()?;
                self.clear_dirty()?;
            }
        }
        self.clear_merge_state()?;
        Ok(ResetOutcome {
            target: plan.target.clone(),
            mode: plan.mode,
        })
    }

    pub fn mark_dirty_path(&self, path: impl AsRef<Path>) -> Result<()> {
        let key = self.file_key(path)?;
        self.mark_dirty_key(key)
    }

    fn mark_dirty_key(&self, key: String) -> Result<()> {
        let mut state = self.read_worktree_state()?;
        let mut dirty = state.dirty.into_iter().collect::<BTreeSet<_>>();
        dirty.insert(key.clone());
        state.dirty = dirty.into_iter().collect();
        state.deleted.retain(|path| path != &key);
        self.write_worktree_state(&state)
    }

    pub fn mark_deleted_path(&self, path: impl AsRef<Path>) -> Result<()> {
        let key = self.file_key(path)?;
        let mut state = self.read_worktree_state()?;
        state.dirty.retain(|path| path != &key);
        let mut deleted = state.deleted.into_iter().collect::<BTreeSet<_>>();
        deleted.insert(key);
        state.deleted = deleted.into_iter().collect();
        self.write_worktree_state(&state)
    }

    pub fn clear_dirty_path(&self, path: impl AsRef<Path>) -> Result<()> {
        let key = self.file_key(path)?;
        self.clear_dirty_key(&key)
    }

    fn clear_dirty_key(&self, key: &str) -> Result<()> {
        let mut state = self.read_worktree_state()?;
        state.dirty.retain(|path| path != key);
        state.deleted.retain(|path| path != key);
        self.write_worktree_state(&state)
    }

    pub fn clear_dirty(&self) -> Result<()> {
        let path = self.worktree_state_path();
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    pub fn dirty_paths(&self) -> Result<Vec<String>> {
        let state = self.read_worktree_state()?;
        let mut paths = state.dirty.into_iter().collect::<BTreeSet<_>>();
        paths.extend(state.deleted);
        Ok(paths.into_iter().collect())
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty_paths()
            .map(|paths| !paths.is_empty())
            .unwrap_or(false)
    }

    pub fn has_staged_changes(&self) -> Result<bool> {
        Ok(self.read_index()?.has_staged_changes())
    }

    pub fn has_work_in_progress(&self) -> Result<bool> {
        let index = self.read_index()?;
        Ok(!self.dirty_paths()?.is_empty()
            || index.has_staged_changes()
            || index.has_conflicts()
            || self.merge_head()?.is_some())
    }

    pub fn discard_work_in_progress(&self) -> Result<()> {
        self.clear_index()?;
        self.clear_dirty()?;
        self.clear_merge_state()
    }

    pub fn head_file(&self, path: impl AsRef<Path>) -> Result<Option<CommitFileState>> {
        let key = self.file_key(path)?;
        Ok(self
            .head_target()?
            .map(|commit| self.read_commit(&commit))
            .transpose()?
            .and_then(|commit| commit.files.get(&key).cloned()))
    }

    pub fn index_file(&self, path: impl AsRef<Path>) -> Result<Option<CommitFileState>> {
        let key = self.file_key(path)?;
        Ok(self.index_files()?.remove(&key))
    }

    pub fn index_has_entry(&self, path: impl AsRef<Path>) -> Result<bool> {
        let key = self.file_key(path)?;
        Ok(self
            .read_index()?
            .stage0_entries()
            .any(|entry| entry.path == key))
    }

    pub fn restore_index_path_from_head(&self, path: impl AsRef<Path>) -> Result<String> {
        let key = self.file_key(path)?;
        let mut index = self.read_index()?;
        if index.conflicted_paths().iter().any(|path| path == &key) {
            return Err(RepoErr::UnresolvedConflicts);
        }
        let had_index_entry = index.entries.iter().any(|entry| entry.path == key);
        let is_tracked_at_head = self.head_files()?.contains_key(&key);
        if !had_index_entry && !is_tracked_at_head {
            return Err(RepoErr::PathNotTracked(key));
        }
        index.remove_path(&key);
        self.write_index(&index)?;
        Ok(key)
    }

    pub fn restore_index_path_from_revision(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
    ) -> Result<String> {
        let key = self.file_key(path)?;
        let target = self.resolve_revision(rev)?;
        let source_files = self.read_commit(&target)?.files;
        let source_state = source_files.get(&key).cloned();
        let head_files = self.head_files()?;
        let head_state = head_files.get(&key);
        let head_has_path = head_state.is_some();
        let mut index = self.read_index()?;
        if index.conflicted_paths().iter().any(|path| path == &key) {
            return Err(RepoErr::UnresolvedConflicts);
        }
        let had_index_entry = index.entries.iter().any(|entry| entry.path == key);

        if source_state.is_none() && !head_has_path && !had_index_entry {
            return Err(RepoErr::PathNotFoundInRevision { path: key, rev: rev.to_string() });
        }

        index.remove_path(&key);
        if source_state.as_ref() == head_state {
            // Resetting the index to HEAD is represented by the absence of an index entry.
        } else if let Some(file) = source_state {
            index.stage(self.index_entry_for_state(
                key.clone(),
                index::IndexStage::Normal,
                file,
            )?);
        } else if head_has_path {
            index.stage(index::IndexEntry {
                path: key.clone(),
                mode: None,
                oid: None,
                stage: index::IndexStage::Normal,
                file: None,
            });
        }
        self.write_index(&index)?;
        Ok(key)
    }

    pub fn file_from_revision(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
    ) -> Result<Option<CommitFileState>> {
        let target = self.resolve_revision(rev)?;
        let key = self.file_key(path)?;
        Ok(self.read_commit(&target)?.files.get(&key).cloned())
    }

    pub fn file_key(&self, path: impl AsRef<Path>) -> Result<String> {
        let path = path.as_ref();
        let parent = worktree_for_file(path);
        let parent = fs::canonicalize(parent)?;
        let Some(file_name) = path.file_name() else {
            return Err(RepoErr::PathOutsideWorktree {
                path: path.to_path_buf(),
                worktree: self.worktree.clone(),
            });
        };
        let absolute = parent.join(file_name);
        let relative =
            absolute
                .strip_prefix(&self.worktree)
                .map_err(|_| RepoErr::PathOutsideWorktree {
                    path: absolute.clone(),
                    worktree: self.worktree.clone(),
                })?;
        relative
            .to_str()
            .map(|path| path.replace('\\', "/"))
            .ok_or_else(|| RepoErr::NonUtf8Path(relative.to_path_buf()))
    }

    fn create_layout(&self) -> Result<()> {
        for dir in [
            DIR_REFS_HEADS,
            DIR_REFS_REMOTES,
            DIR_REFS_TAGS,
            DIR_OBJECTS,
            DIR_OBJECTS_PACK,
            DIR_STORE_FJALL,
            DIR_INDEX,
            DIR_LOCKS,
            DIR_TMP,
            DIR_LOGS_REFS,
            DIR_LOGS_HEAD,
        ] {
            fs::create_dir_all(self.graft_dir.join(dir))?;
        }
        Ok(())
    }

    fn ensure_supported_format(&self) -> Result<()> {
        let config = self.config()?;
        let actual = config.core.repository_format_version;
        if actual != REPOSITORY_FORMAT_VERSION {
            return Err(RepoErr::UnsupportedFormat {
                expected: REPOSITORY_FORMAT_VERSION,
                actual,
            });
        }
        let actual = config.extensions.object_format;
        if actual != OBJECT_FORMAT {
            return Err(RepoErr::UnsupportedObjectFormat { expected: OBJECT_FORMAT, actual });
        }
        Ok(())
    }

    fn config_path(&self) -> PathBuf {
        self.graft_dir.join(CONFIG_FILE)
    }

    fn head_path(&self) -> PathBuf {
        self.graft_dir.join(HEAD_FILE)
    }

    fn current_head_for_reflog(&self) -> Result<Option<Head>> {
        if !self.head_path().is_file() {
            return Ok(None);
        }
        self.head().map(Some)
    }

    fn head_reflog_target(&self, head: &Head) -> Result<Option<String>> {
        match head {
            Head::Branch { name } => self.read_branch_ref(name),
            Head::Detached { commit } => Ok(Some(commit.clone())),
        }
    }

    fn merge_head_path(&self) -> PathBuf {
        self.graft_dir.join(MERGE_HEAD_FILE)
    }

    fn orig_head_path(&self) -> PathBuf {
        self.graft_dir.join(ORIG_HEAD_FILE)
    }

    fn worktree_state_path(&self) -> PathBuf {
        self.graft_dir.join(DIR_INDEX).join("worktree.toml")
    }

    fn index_path(&self) -> PathBuf {
        self.graft_dir.join(DIR_INDEX).join("state.toml")
    }

    fn head_target(&self) -> Result<Option<String>> {
        match self.head()? {
            Head::Branch { name } => self.read_branch_ref(&name),
            Head::Detached { commit } => Ok(Some(commit)),
        }
    }

    fn move_head_to(&self, id: &str, message: &str) -> Result<()> {
        match self.head()? {
            Head::Branch { name } => self.write_branch_ref(&name, id, message)?,
            Head::Detached { .. } => {
                self.write_head_with_message(&Head::Detached { commit: id.to_string() }, message)?
            }
        }
        Ok(())
    }

    fn merge_head(&self) -> Result<Option<String>> {
        let path = self.merge_head_path();
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(path)?;
        let target = raw.trim();
        if target.is_empty() {
            return Ok(None);
        }
        Ok(Some(target.to_string()))
    }

    fn orig_head(&self) -> Result<Option<String>> {
        let path = self.orig_head_path();
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(path)?;
        let target = raw.trim();
        if target.is_empty() {
            return Ok(None);
        }
        Ok(Some(target.to_string()))
    }

    fn write_merge_state(&self, orig_head: &str, merge_head: &str) -> Result<()> {
        fs::write(self.orig_head_path(), format!("{orig_head}\n"))?;
        fs::write(self.merge_head_path(), format!("{merge_head}\n"))?;
        Ok(())
    }

    fn clear_merge_state(&self) -> Result<()> {
        for path in [self.merge_head_path(), self.orig_head_path()] {
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    fn commit_parents(&self) -> Result<Vec<String>> {
        let mut parents = Vec::new();
        if let Some(head) = self.head_target()? {
            parents.push(head);
        }
        if let Some(merge_head) = self.merge_head()?
            && !parents.iter().any(|parent| parent == &merge_head)
        {
            parents.push(merge_head);
        }
        Ok(parents)
    }

    pub fn read_commit(&self, id: &str) -> Result<CommitObject> {
        let id = object::ObjectId::from_str(id)?;
        let commit = self
            .read_commit_object(&id)?
            .ok_or_else(|| RepoErr::CommitNotFound(id.to_string()))?;
        self.commit_from_object(&id, commit)
    }

    fn read_commit_object(&self, id: &object::ObjectId) -> Result<Option<object::CommitObject>> {
        let Some(bytes) = self.object_store().read_raw(id)? else {
            return Ok(None);
        };
        let object = object::Object::decode(&bytes)?;
        let actual = object.id();
        if actual != *id {
            return Err(RepoErr::Object(object::ObjectErr::ObjectIdMismatch {
                expected: id.clone(),
                actual,
            }));
        }
        match object {
            object::Object::Commit(commit) => Ok(Some(commit)),
            object => Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "commit",
                message: format!("object {id} is a {}", object.kind()),
            })),
        }
    }

    fn commit_from_object(
        &self,
        id: &object::ObjectId,
        commit: object::CommitObject,
    ) -> Result<CommitObject> {
        let parents = commit
            .parents
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let tables = commit.tables;
        let changed_tables = tables.len();
        Ok(CommitObject {
            id: id.to_string(),
            parent: parents.first().cloned(),
            parents,
            tree: Some(commit.tree.to_string()),
            message: commit.message,
            timestamp_ms: commit.committer.timestamp_ms,
            files: self.files_from_tree_object(&commit.tree)?,
            tables,
            changed_tables,
        })
    }

    fn files_from_tree_object(
        &self,
        id: &object::ObjectId,
    ) -> Result<BTreeMap<String, CommitFileState>> {
        let object = self.object_store().read(id)?;
        let object::Object::Tree(tree) = object else {
            return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "tree",
                message: format!("object {id} is not a tree"),
            }));
        };

        let mut files = BTreeMap::new();
        for entry in tree.entries {
            if entry.mode != object::TreeEntryMode::SqliteDatabase {
                return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                    kind: "tree",
                    message: format!("entry `{}` has unsupported mode {}", entry.path, entry.mode),
                }));
            }

            let object = self.object_store().read(&entry.oid)?;
            let object::Object::Blob(object::BlobObject::SqliteSnapshot(blob)) = object else {
                return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                    kind: "blob",
                    message: format!("tree entry `{}` is not a sqlite snapshot", entry.path),
                }));
            };
            files.insert(entry.path, file_state_from_sqlite_snapshot_blob(blob));
        }
        Ok(files)
    }

    pub fn read_index(&self) -> Result<index::Index> {
        let path = self.index_path();
        if !path.exists() {
            return Ok(index::Index::default());
        }
        let raw = fs::read_to_string(path)?;
        Ok(toml::from_str(&raw)?)
    }

    pub fn index_files(&self) -> Result<BTreeMap<String, CommitFileState>> {
        let index = self.read_index()?;
        if index.has_conflicts() {
            return Err(RepoErr::UnresolvedConflicts);
        }

        let mut files = self.head_files()?;
        for entry in index.stage0_entries() {
            if let Some(file) = &entry.file {
                files.insert(entry.path.clone(), file.clone());
            } else {
                files.remove(&entry.path);
            }
        }
        Ok(files)
    }

    fn files_for_worktree_status(
        &self,
        index: &index::Index,
    ) -> Result<BTreeMap<String, CommitFileState>> {
        let mut files = self.head_files()?;
        for entry in index.stage0_entries() {
            if let Some(file) = &entry.file {
                files.insert(entry.path.clone(), file.clone());
            } else {
                files.remove(&entry.path);
            }
        }
        Ok(files)
    }

    fn unstaged_changes_for_index(&self, index: &index::Index) -> Result<Vec<RepoWorktreeChange>> {
        let tracked = self.files_for_worktree_status(index)?;
        let state = self.read_worktree_state()?;
        let mut changes = BTreeMap::new();
        for path in state.dirty {
            let change = if tracked.contains_key(&path) {
                RepoWorktreeChangeKind::Modified
            } else {
                RepoWorktreeChangeKind::Untracked
            };
            changes.insert(path, change);
        }
        for path in state.deleted {
            if tracked.contains_key(&path) {
                changes.insert(path, RepoWorktreeChangeKind::Deleted);
            }
        }
        for path in self.scan_untracked_sqlite_files()? {
            if !tracked.contains_key(&path) {
                changes
                    .entry(path)
                    .or_insert(RepoWorktreeChangeKind::Untracked);
            }
        }
        Ok(changes
            .into_iter()
            .map(|(path, change)| RepoWorktreeChange { path, change })
            .collect())
    }

    fn scan_untracked_sqlite_files(&self) -> Result<Vec<String>> {
        let mut paths = BTreeSet::new();
        self.collect_sqlite_worktree_files(&self.worktree, &mut paths)?;
        Ok(paths.into_iter().collect())
    }

    fn collect_sqlite_worktree_files(&self, dir: &Path, out: &mut BTreeSet<String>) -> Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                if entry.file_name() == GRAFT_DIR {
                    continue;
                }
                self.collect_sqlite_worktree_files(&path, out)?;
            } else if file_type.is_file() && is_sqlite_database_file(&path)? {
                let relative = path.strip_prefix(&self.worktree).map_err(|_| {
                    RepoErr::PathOutsideWorktree {
                        path: path.clone(),
                        worktree: self.worktree.clone(),
                    }
                })?;
                let key = relative
                    .to_str()
                    .map(|path| path.replace('\\', "/"))
                    .ok_or_else(|| RepoErr::NonUtf8Path(relative.to_path_buf()))?;
                out.insert(key);
            }
        }
        Ok(())
    }

    fn files_for_commit(&self, id: Option<&str>) -> Result<BTreeMap<String, CommitFileState>> {
        id.map(|id| self.read_commit(id).map(|commit| commit.files))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    fn checkout_plan_for_target(&self, target: Option<String>) -> Result<CheckoutPlan> {
        let files = self.files_for_commit(target.as_deref())?;
        Ok(CheckoutPlan { target, files })
    }

    fn stage_merge_conflict(
        &self,
        key: &str,
        base: Option<&CommitFileState>,
        ours: Option<&CommitFileState>,
        theirs: Option<&CommitFileState>,
        index: &mut index::Index,
    ) -> Result<()> {
        index.remove_path(key);
        for (stage, state) in [
            (index::IndexStage::Base, base),
            (index::IndexStage::Ours, ours),
            (index::IndexStage::Theirs, theirs),
        ] {
            if let Some(state) = state {
                index.stage(self.index_entry_for_state(key.to_string(), stage, state.clone())?);
            }
        }
        Ok(())
    }

    fn write_index(&self, index: &index::Index) -> Result<()> {
        let path = self.index_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, toml::to_string_pretty(index)?)?;
        Ok(())
    }

    fn clear_index(&self) -> Result<()> {
        let path = self.index_path();
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    fn read_worktree_state(&self) -> Result<WorktreeState> {
        let path = self.worktree_state_path();
        if !path.exists() {
            return Ok(WorktreeState::default());
        }
        let raw = fs::read_to_string(path)?;
        let mut state: WorktreeState = toml::from_str(&raw)?;
        let dirty = state.dirty.into_iter().collect::<BTreeSet<_>>();
        state.dirty = dirty.into_iter().collect();
        let deleted = state.deleted.into_iter().collect::<BTreeSet<_>>();
        state.deleted = deleted.into_iter().collect();
        Ok(state)
    }

    fn write_worktree_state(&self, state: &WorktreeState) -> Result<()> {
        let path = self.worktree_state_path();
        if state.dirty.is_empty() && state.deleted.is_empty() {
            if path.exists() {
                fs::remove_file(path)?;
            }
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_file_atomic(&path, toml::to_string_pretty(state)?.as_bytes())
    }

    pub fn read_object(&self, id: &str) -> Result<object::Object> {
        let id = object::ObjectId::from_str(id)?;
        Ok(self.object_store().read(&id)?)
    }

    pub fn remote_store(&self, remote: &str) -> Result<crate::remote::Remote> {
        validate_remote_name(remote)?;
        let config = self
            .config()?
            .remotes
            .get(remote)
            .cloned()
            .ok_or_else(|| RepoErr::RemoteNotFound(remote.to_string()))?;
        Ok(config.build()?)
    }

    fn remote_branch_refs_from_store(
        &self,
        remote: &str,
        remote_store: &crate::remote::Remote,
    ) -> Result<Vec<RemoteBranchRef>> {
        validate_remote_name(remote)?;
        let prefix = "refs/heads/";
        let mut refs = BTreeMap::<String, String>::new();
        let mut paths = Vec::new();
        for path in block_on_remote(remote_store.list_raw(prefix))? {
            if path == prefix || path.ends_with('/') {
                continue;
            }
            let Some(branch) = path.strip_prefix(prefix) else {
                continue;
            };
            validate_ref_name(branch)?;
            let branch = branch.to_string();
            paths.push((path, branch));
        }

        let remote_refs = block_on_remote(async {
            stream::iter(paths)
                .map(|(path, branch)| async move {
                    let bytes = remote_store.get_raw(&path).await?;
                    Ok::<_, RemoteErr>((path, branch, bytes))
                })
                .buffer_unordered(REMOTE_REF_READ_CONCURRENCY)
                .try_collect::<Vec<_>>()
                .await
        })?;

        for (path, branch, bytes) in remote_refs {
            let Some(bytes) = bytes else {
                continue;
            };
            refs.insert(branch, parse_remote_ref(&path, bytes)?);
        }

        Ok(refs
            .into_iter()
            .map(|(branch, head)| RemoteBranchRef { remote: remote.to_string(), branch, head })
            .collect())
    }

    fn set_remote_head_if_absent(
        &self,
        remote_store: &crate::remote::Remote,
        branch: &str,
    ) -> Result<()> {
        if branch != self.default_branch()? {
            return Ok(());
        }

        match block_on_remote(remote_store.compare_and_swap_raw(
            HEAD_FILE,
            None,
            Head::branch(branch).serialize(),
        )) {
            Ok(()) => Ok(()),
            Err(RepoErr::Remote(RemoteErr::CompareAndSwap { .. } | RemoteErr::LockBusy { .. })) => {
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    fn fetch_commit_chain(&self, remote: &crate::remote::Remote, head: &str) -> Result<usize> {
        let mut count = 0;
        let mut stack = vec![head.to_string()];
        let mut seen = BTreeMap::<String, ()>::new();
        while let Some(id) = stack.pop() {
            if seen.insert(id.clone(), ()).is_some() {
                continue;
            }

            let object_id = object::ObjectId::from_str(&id)?;
            let commit = match self.read_commit_object(&object_id)? {
                Some(commit) => commit,
                None => {
                    let object = self.fetch_loose_object(remote, &object_id)?;
                    let object::Object::Commit(commit) = object else {
                        return Err(RepoErr::InvalidRemoteObject {
                            path: object::LooseObjectStore::relative_path(&object_id),
                            message: "expected commit object".to_string(),
                        });
                    };
                    count += 1;
                    commit
                }
            };

            self.fetch_object_graph(remote, &commit.tree)?;
            for parent in commit.parents {
                stack.push(parent.to_string());
            }
        }
        Ok(count)
    }

    fn push_commit_chain(
        &self,
        remote: &crate::remote::Remote,
        head: &str,
        stop_at: Option<&str>,
    ) -> Result<usize> {
        let mut commits = vec![];
        let mut stack = vec![head.to_string()];
        let mut seen = BTreeMap::<String, ()>::new();
        while let Some(id) = stack.pop() {
            if seen.insert(id.clone(), ()).is_some() {
                continue;
            }
            if stop_at == Some(id.as_str()) {
                continue;
            }
            let object_id = object::ObjectId::from_str(&id)?;
            let path = object::LooseObjectStore::relative_path(&object_id);
            if block_on_remote(remote.get_raw(&path))?.is_some() {
                continue;
            }

            let commit = self
                .read_commit_object(&object_id)?
                .ok_or_else(|| RepoErr::CommitNotFound(id.clone()))?;
            for parent in &commit.parents {
                stack.push(parent.to_string());
            }
            commits.push(object_id);
        }

        let count = commits.len();
        for id in commits.into_iter().rev() {
            self.push_object_graph(remote, &id)?;
        }
        Ok(count)
    }

    fn fetch_object_graph(
        &self,
        remote: &crate::remote::Remote,
        id: &object::ObjectId,
    ) -> Result<()> {
        let object = match self.object_store().read_raw(id)? {
            Some(bytes) => {
                let object = object::Object::decode(&bytes)?;
                let actual = object.id();
                if actual != *id {
                    return Err(RepoErr::Object(object::ObjectErr::ObjectIdMismatch {
                        expected: id.clone(),
                        actual,
                    }));
                }
                object
            }
            None => self.fetch_loose_object(remote, id)?,
        };

        match object {
            object::Object::Commit(commit) => {
                self.fetch_object_graph(remote, &commit.tree)?;
                for parent in commit.parents {
                    self.fetch_object_graph(remote, &parent)?;
                }
            }
            object::Object::Tree(tree) => {
                for entry in tree.entries {
                    self.fetch_object_graph(remote, &entry.oid)?;
                }
            }
            object::Object::Blob(_) | object::Object::Tag(_) => {}
        }
        Ok(())
    }

    fn fetch_loose_object(
        &self,
        remote: &crate::remote::Remote,
        id: &object::ObjectId,
    ) -> Result<object::Object> {
        let path = object::LooseObjectStore::relative_path(id);
        let Some(bytes) = block_on_remote(remote.get_raw(&path))? else {
            return Err(RepoErr::InvalidRemoteObject {
                path,
                message: "missing object".to_string(),
            });
        };
        Ok(self.object_store().write_raw_validated(id, &bytes)?)
    }

    fn push_object_graph(
        &self,
        remote: &crate::remote::Remote,
        id: &object::ObjectId,
    ) -> Result<()> {
        let Some(bytes) = self.object_store().read_raw(id)? else {
            return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "object",
                message: format!("missing local object {id}"),
            }));
        };
        let object = object::Object::decode(&bytes)?;
        let actual = object.id();
        if actual != *id {
            return Err(RepoErr::Object(object::ObjectErr::ObjectIdMismatch {
                expected: id.clone(),
                actual,
            }));
        }

        let path = object::LooseObjectStore::relative_path(id);
        if matches!(object, object::Object::Commit(_) | object::Object::Tree(_))
            && block_on_remote(remote.get_raw(&path))?.is_some()
        {
            return Ok(());
        }

        match &object {
            object::Object::Commit(commit) => {
                self.push_object_graph(remote, &commit.tree)?;
                for parent in &commit.parents {
                    self.push_object_graph(remote, parent)?;
                }
            }
            object::Object::Tree(tree) => {
                for entry in &tree.entries {
                    self.push_object_graph(remote, &entry.oid)?;
                }
            }
            object::Object::Blob(_) | object::Object::Tag(_) => {}
        }

        match block_on_remote(remote.put_raw_if_not_exists(&path, bytes)) {
            Ok(()) => {}
            Err(RepoErr::Remote(err)) if err.precondition_failed() => {}
            Err(err) => return Err(err),
        }
        Ok(())
    }

    fn write_tree_object(
        &self,
        object_store: &object::LooseObjectStore,
        files: &BTreeMap<String, CommitFileState>,
    ) -> Result<object::ObjectId> {
        let mut entries = Vec::with_capacity(files.len());
        for (path, state) in files {
            let blob = object::Object::Blob(object::BlobObject::SqliteSnapshot(
                sqlite_snapshot_blob(state),
            ));
            let oid = object_store.write(&blob)?;
            entries.push(object::TreeEntry {
                mode: object::TreeEntryMode::SqliteDatabase,
                oid,
                path: path.clone(),
            });
        }
        let tree = object::TreeObject::new(entries)?;
        Ok(object_store.write(&object::Object::Tree(tree))?)
    }

    fn canonical_commit_object(
        &self,
        tree: object::ObjectId,
        parents: &[String],
        message: &str,
        timestamp_ms: u64,
        tables: Vec<CommitTableSummary>,
    ) -> Result<object::CommitObject> {
        let parents = parents
            .iter()
            .map(|parent| object::ObjectId::from_str(parent))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let signature =
            object::Signature::new("Graft", "graft@example.invalid", timestamp_ms, "+0000");
        Ok(object::CommitObject {
            tree,
            parents,
            author: signature.clone(),
            committer: signature,
            repo_format_version: REPOSITORY_FORMAT_VERSION,
            tables,
            message: message.to_string(),
        })
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let mut stack = vec![descendant.to_string()];
        let mut seen = BTreeMap::<String, ()>::new();
        while let Some(id) = stack.pop() {
            if seen.insert(id.clone(), ()).is_some() {
                continue;
            }
            if id == ancestor {
                return Ok(true);
            }
            for parent in commit_parent_ids(&self.read_commit(&id)?) {
                stack.push(parent);
            }
        }
        Ok(false)
    }

    fn merge_base(&self, left: &str, right: &str) -> Result<Option<String>> {
        let mut left_ancestors = BTreeMap::<String, ()>::new();
        let mut stack = vec![left.to_string()];
        while let Some(id) = stack.pop() {
            if left_ancestors.insert(id.clone(), ()).is_some() {
                continue;
            }
            for parent in commit_parent_ids(&self.read_commit(&id)?) {
                stack.push(parent);
            }
        }

        let mut stack = vec![right.to_string()];
        let mut seen = BTreeMap::<String, ()>::new();
        while let Some(id) = stack.pop() {
            if seen.insert(id.clone(), ()).is_some() {
                continue;
            }
            if left_ancestors.contains_key(&id) {
                return Ok(Some(id));
            }
            for parent in commit_parent_ids(&self.read_commit(&id)?) {
                stack.push(parent);
            }
        }

        Ok(None)
    }

    fn head_files(&self) -> Result<BTreeMap<String, CommitFileState>> {
        Ok(self
            .head_target()?
            .map(|commit| self.read_commit(&commit))
            .transpose()?
            .map(|commit| commit.files)
            .unwrap_or_default())
    }

    fn read_branch_ref(&self, name: &str) -> Result<Option<String>> {
        self.read_ref(&format!("refs/heads/{name}"))
    }

    fn branch_info(&self, name: &str) -> Result<BranchInfo> {
        self.ensure_local_branch_for_config(name)?;
        let current = self.current_branch()?.as_deref() == Some(name);
        Ok(BranchInfo {
            name: name.to_string(),
            target: self.read_branch_ref(name)?,
            current,
            upstream: self.branch_upstream(name)?,
        })
    }

    fn resolve_revision_base(&self, rev: &str) -> Result<String> {
        match rev {
            "HEAD" | "@" => return self.head_target()?.ok_or(RepoErr::UnbornHead),
            _ => {}
        }

        if let Some(target) = self.resolve_refish(rev)? {
            return Ok(target);
        }

        self.resolve_commit_prefix(rev)
    }

    fn apply_revision_op(&self, id: &str, op: RevisionOp, rev: &str) -> Result<String> {
        match op {
            RevisionOp::FirstParent(ancestors) => {
                let mut id = id.to_string();
                for _ in 0..ancestors {
                    let parents = commit_parent_ids(&self.read_commit(&id)?);
                    id = parents
                        .into_iter()
                        .next()
                        .ok_or_else(|| RepoErr::UnknownRevision(rev.to_string()))?;
                }
                Ok(id)
            }
            RevisionOp::Parent(parent) => {
                if parent == 0 {
                    return Ok(id.to_string());
                }
                let parents = commit_parent_ids(&self.read_commit(id)?);
                parents
                    .get(parent - 1)
                    .cloned()
                    .ok_or_else(|| RepoErr::UnknownRevision(rev.to_string()))
            }
        }
    }

    fn resolve_refish(&self, rev: &str) -> Result<Option<String>> {
        if rev.starts_with("refs/") {
            return self
                .read_ref(rev)?
                .map(|target| {
                    if rev.starts_with("refs/tags/") {
                        self.peel_object_to_commit(&target, rev)
                    } else {
                        Ok(target)
                    }
                })
                .transpose();
        }

        if let Some(target) = self.read_ref(&format!("refs/heads/{rev}"))? {
            return Ok(Some(target));
        }

        if let Some(target) = self.read_ref(&format!("refs/tags/{rev}"))? {
            return Ok(Some(self.peel_object_to_commit(&target, rev)?));
        }

        if let Some((remote, branch)) = rev.split_once('/')
            && validate_remote_name(remote).is_ok()
            && validate_ref_name(branch).is_ok()
            && let Some(target) = self.read_ref(&format!("refs/remotes/{remote}/{branch}"))?
        {
            return Ok(Some(target));
        }

        Ok(None)
    }

    fn resolve_commit_prefix(&self, rev: &str) -> Result<String> {
        if rev.len() < 4 || rev.len() > 64 || !rev.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(RepoErr::UnknownRevision(rev.to_string()));
        }

        if rev.len() == 64 {
            let id = object::ObjectId::from_str(rev)?;
            return self.peel_object_id_to_commit(&id, rev);
        }

        let mut matches = self.commitish_object_ids_with_prefix(rev)?;

        match matches.len() {
            0 => Err(RepoErr::UnknownRevision(rev.to_string())),
            1 => {
                let id = object::ObjectId::from_str(&matches.pop().expect("one match"))?;
                self.peel_object_id_to_commit(&id, rev)
            }
            _ => Err(RepoErr::AmbiguousRevision(rev.to_string())),
        }
    }

    fn commitish_object_ids_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let mut matches = Vec::new();
        let root = self.object_store().root().to_path_buf();
        if !root.exists() {
            return Ok(matches);
        }

        for dir in fs::read_dir(root)? {
            let dir = dir?;
            if !dir.file_type()?.is_dir() {
                continue;
            }
            let fanout = dir.file_name().to_string_lossy().into_owned();
            if fanout.len() != 2 || !fanout.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }

            for file in fs::read_dir(dir.path())? {
                let file = file?;
                if !file.file_type()?.is_file() {
                    continue;
                }
                let suffix = file.file_name().to_string_lossy().into_owned();
                if suffix.len() != 62 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    continue;
                }

                let id = format!("{fanout}{suffix}");
                if !id.starts_with(prefix) {
                    continue;
                }

                let object_id = object::ObjectId::from_str(&id)?;
                let Some(bytes) = self.object_store().read_raw(&object_id)? else {
                    continue;
                };
                let object = object::Object::decode(&bytes)?;
                let actual = object.id();
                if actual != object_id {
                    return Err(RepoErr::Object(object::ObjectErr::ObjectIdMismatch {
                        expected: object_id,
                        actual,
                    }));
                }
                if matches!(object, object::Object::Commit(_) | object::Object::Tag(_)) {
                    matches.push(id);
                }
            }
        }

        matches.sort();
        Ok(matches)
    }

    fn peel_object_to_commit(&self, id: &str, rev: &str) -> Result<String> {
        let id = object::ObjectId::from_str(id)?;
        self.peel_object_id_to_commit(&id, rev)
    }

    fn peel_object_id_to_commit(&self, id: &object::ObjectId, rev: &str) -> Result<String> {
        let mut current = id.clone();
        let mut seen = BTreeMap::<String, ()>::new();

        loop {
            if seen.insert(current.to_string(), ()).is_some() {
                return Err(RepoErr::UnknownRevision(rev.to_string()));
            }

            let Some(bytes) = self.object_store().read_raw(&current)? else {
                return Err(RepoErr::UnknownRevision(rev.to_string()));
            };
            let object = object::Object::decode(&bytes)?;
            let actual = object.id();
            if actual != current {
                return Err(RepoErr::Object(object::ObjectErr::ObjectIdMismatch {
                    expected: current,
                    actual,
                }));
            }

            match object {
                object::Object::Commit(_) => return Ok(current.to_string()),
                object::Object::Tag(tag) => {
                    if !matches!(
                        tag.object_type,
                        object::ObjectKind::Commit | object::ObjectKind::Tag
                    ) {
                        return Err(RepoErr::UnknownRevision(rev.to_string()));
                    }
                    current = tag.object;
                }
                _ => return Err(RepoErr::UnknownRevision(rev.to_string())),
            }
        }
    }

    fn branch_exists(&self, name: &str) -> bool {
        self.graft_dir.join(DIR_REFS_HEADS).join(name).is_file()
    }

    fn ensure_local_branch_for_config(&self, name: &str) -> Result<()> {
        validate_ref_name(name)?;
        if self.branch_exists(name) || self.current_branch()?.as_deref() == Some(name) {
            Ok(())
        } else {
            Err(RepoErr::BranchNotFound(name.to_string()))
        }
    }

    fn tag_exists(&self, name: &str) -> bool {
        self.graft_dir.join(DIR_REFS_TAGS).join(name).is_file()
    }

    fn write_branch_ref(&self, name: &str, target: &str, message: &str) -> Result<()> {
        self.write_ref_update(&format!("refs/heads/{name}"), target, message)
    }

    fn read_tag_ref(&self, name: &str) -> Result<Option<String>> {
        validate_ref_name(name)?;
        self.read_ref(&format!("refs/tags/{name}"))
    }

    fn tag_info_from_ref(&self, name: String, object: String) -> Result<TagInfo> {
        let object_id = object::ObjectId::from_str(&object)?;
        match self.object_store().read(&object_id)? {
            object::Object::Commit(_) => Ok(TagInfo {
                name,
                object: object.clone(),
                target: object,
                annotated: false,
                message: None,
            }),
            object::Object::Tag(tag) => {
                let target = self.peel_object_id_to_commit(&tag.object, &name)?;
                Ok(TagInfo {
                    name,
                    object,
                    target,
                    annotated: true,
                    message: Some(tag.message),
                })
            }
            object => Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "tag",
                message: format!("tag ref `{name}` points at a {}", object.kind()),
            })),
        }
    }

    fn write_tag_ref(&self, name: &str, target: &str, message: &str) -> Result<()> {
        self.write_ref_update(&format!("refs/tags/{name}"), target, message)
    }

    fn delete_tag_ref(&self, name: &str) -> Result<()> {
        validate_ref_name(name)?;
        let path = self.graft_dir.join(DIR_REFS_TAGS).join(name);
        if !path.is_file() {
            return Err(RepoErr::TagNotFound(name.to_string()));
        }
        fs::remove_file(&path)?;
        remove_empty_parent_dirs(path.parent(), &self.graft_dir.join(DIR_REFS_TAGS))?;
        Ok(())
    }

    fn read_ref(&self, reference: &str) -> Result<Option<String>> {
        validate_full_ref(reference)?;
        let path = self.graft_dir.join(reference);
        if !path.exists() {
            return Ok(None);
        }
        if !path.is_file() {
            return Err(RepoErr::BranchNotFound(reference.to_string()));
        }

        let raw = fs::read_to_string(path)?;
        let target = raw.trim();
        if target.is_empty() {
            Ok(None)
        } else {
            Ok(Some(target.to_string()))
        }
    }

    fn write_ref_update(&self, reference: &str, target: &str, message: &str) -> Result<()> {
        validate_full_ref(reference)?;
        self.ensure_ref_namespace_available(reference)?;
        let old = self.read_ref(reference)?;
        self.write_ref(reference, target)?;
        self.append_ref_reflog(reference, old.as_deref(), Some(target), message)?;
        Ok(())
    }

    fn write_ref(&self, reference: &str, target: &str) -> Result<()> {
        validate_full_ref(reference)?;
        self.ensure_ref_namespace_available(reference)?;
        let path = self.graft_dir.join(reference);
        write_file_atomic(&path, format!("{target}\n").as_bytes())?;
        Ok(())
    }

    fn ensure_ref_namespace_available(&self, reference: &str) -> Result<()> {
        validate_full_ref(reference)?;
        let path = self.graft_dir.join(reference);
        if path.is_dir() {
            return Err(RepoErr::RefNameConflict {
                reference: reference.to_string(),
                existing: reference.to_string(),
            });
        }

        let mut current = path.parent();
        while let Some(parent) = current {
            if parent == self.graft_dir {
                break;
            }
            if parent.is_file() {
                let existing = parent.strip_prefix(&self.graft_dir).map_or_else(
                    |_| parent.display().to_string(),
                    |path| path.to_string_lossy().replace('\\', "/"),
                );
                return Err(RepoErr::RefNameConflict {
                    reference: reference.to_string(),
                    existing,
                });
            }
            current = parent.parent();
        }

        Ok(())
    }

    fn ensure_path_namespace_available_for_rename(
        root: &Path,
        old_reference: &str,
        new_reference: &str,
    ) -> Result<()> {
        validate_full_ref(old_reference)?;
        validate_full_ref(new_reference)?;

        let old_path = root.join(old_reference);
        let new_path = root.join(new_reference);
        if new_path.is_dir() && !path_tree_contains_only_file(&new_path, &old_path)? {
            return Err(RepoErr::RefNameConflict {
                reference: new_reference.to_string(),
                existing: new_reference.to_string(),
            });
        }

        let mut current = new_path.parent();
        while let Some(parent) = current {
            if parent == root {
                break;
            }
            if parent.is_file() && parent != old_path {
                let existing = parent.strip_prefix(root).map_or_else(
                    |_| parent.display().to_string(),
                    |path| path.to_string_lossy().replace('\\', "/"),
                );
                return Err(RepoErr::RefNameConflict {
                    reference: new_reference.to_string(),
                    existing,
                });
            }
            current = parent.parent();
        }

        Ok(())
    }

    fn delete_ref(&self, reference: &str) -> Result<()> {
        validate_full_ref(reference)?;
        let path = self.graft_dir.join(reference);
        if !path.is_file() {
            return Err(RepoErr::BranchNotFound(reference.to_string()));
        }
        fs::remove_file(&path)?;
        remove_empty_parent_dirs(path.parent(), &self.graft_dir.join(DIR_REFS_HEADS))?;
        Ok(())
    }

    fn delete_ref_if_exists(&self, reference: &str) -> Result<()> {
        validate_full_ref(reference)?;
        let path = self.graft_dir.join(reference);
        if path.is_file() {
            fs::remove_file(&path)?;
            remove_empty_parent_dirs(path.parent(), &self.graft_dir.join("refs"))?;
        }
        Ok(())
    }

    fn collect_ref_files(
        dir: &Path,
        prefix: &str,
        out: &mut BTreeMap<String, Option<String>>,
    ) -> Result<()> {
        if !dir.exists() {
            return Ok(());
        }

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let name = if prefix.is_empty() {
                file_name
            } else {
                format!("{prefix}/{file_name}")
            };

            if entry.file_type()?.is_dir() {
                Self::collect_ref_files(&entry.path(), &name, out)?;
            } else {
                let raw = fs::read_to_string(entry.path())?;
                let target = raw.trim();
                out.insert(
                    name,
                    if target.is_empty() {
                        None
                    } else {
                        Some(target.to_string())
                    },
                );
            }
        }

        Ok(())
    }

    fn delete_ref_log(&self, reference: &str) -> Result<()> {
        validate_full_ref(reference)?;
        let path = self.graft_dir.join(DIR_LOGS_REFS).join(reference);
        if path.is_file() {
            fs::remove_file(&path)?;
            remove_empty_parent_dirs(path.parent(), &self.graft_dir.join(DIR_LOGS_REFS))?;
        }
        Ok(())
    }

    fn move_ref_log_for_rename(&self, old_reference: &str, new_reference: &str) -> Result<()> {
        validate_full_ref(old_reference)?;
        validate_full_ref(new_reference)?;

        let root = self.graft_dir.join(DIR_LOGS_REFS);
        let old_path = root.join(old_reference);
        if !old_path.is_file() {
            return Ok(());
        }

        let bytes = fs::read(&old_path)?;
        fs::remove_file(&old_path)?;
        remove_empty_parent_dirs(old_path.parent(), &root)?;

        let new_path = root.join(new_reference);
        write_file_atomic(&new_path, &bytes)?;
        Ok(())
    }

    fn append_head_reflog(
        &self,
        old: Option<&str>,
        new: Option<&str>,
        message: &str,
    ) -> Result<()> {
        fs::create_dir_all(self.graft_dir.join(DIR_LOGS_HEAD))?;
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.graft_dir.join(DIR_LOGS_HEAD).join("HEAD"))?
            .write_all(reflog_line(old, new, message).as_bytes())?;
        Ok(())
    }

    fn append_ref_reflog(
        &self,
        reference: &str,
        old: Option<&str>,
        new: Option<&str>,
        message: &str,
    ) -> Result<()> {
        validate_full_ref(reference)?;
        let path = self.graft_dir.join(DIR_LOGS_REFS).join(reference);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(reflog_line(old, new, message).as_bytes())?;
        Ok(())
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

fn parse_fetch_refspec(remote: &str, refspec: &str) -> Result<ParsedRefspec> {
    let parsed = parse_refspec(refspec, RefspecSide::FetchSource, |dst| {
        parse_fetch_destination(remote, dst)
    })?;
    if parsed.source.is_none() {
        return invalid_refspec(refspec, "fetch refspecs require a source");
    }
    validate_refspec_shape(refspec, &parsed)?;
    Ok(parsed)
}

fn parse_push_refspec(refspec: &str) -> Result<ParsedRefspec> {
    let parsed = parse_refspec(refspec, RefspecSide::PushSource, |dst| {
        parse_branch_pattern_ref(dst, RefspecSide::PushDestination)
    })?;
    validate_refspec_shape(refspec, &parsed)?;
    Ok(parsed)
}

fn parse_refspec(
    refspec: &str,
    source_side: RefspecSide,
    parse_destination: impl FnOnce(&str) -> Result<BranchPattern>,
) -> Result<ParsedRefspec> {
    let refspec = refspec.trim();
    if refspec.is_empty() {
        return invalid_refspec(refspec, "empty refspec");
    }

    let (force, body) = if let Some(body) = refspec.strip_prefix('+') {
        (true, body)
    } else {
        (false, refspec)
    };
    if body.is_empty() {
        return invalid_refspec(refspec, "missing source ref");
    }
    if body.matches(':').count() > 1 {
        return invalid_refspec(refspec, "too many `:` separators");
    }

    let (source, destination) = match body.split_once(':') {
        Some((source, destination)) => {
            if destination.is_empty() {
                return invalid_refspec(refspec, "empty destination refs are not supported");
            }
            (
                if source.is_empty() {
                    None
                } else {
                    Some(parse_branch_pattern_ref(source, source_side)?)
                },
                Some(parse_destination(destination)?),
            )
        }
        None => (Some(parse_branch_pattern_ref(body, source_side)?), None),
    };

    Ok(ParsedRefspec { source, destination, force })
}

fn validate_refspec_shape(refspec: &str, parsed: &ParsedRefspec) -> Result<()> {
    let Some(source) = &parsed.source else {
        if parsed
            .destination
            .as_ref()
            .is_some_and(BranchPattern::is_wildcard)
        {
            return invalid_refspec(refspec, "wildcard delete refspecs are not supported");
        }
        return Ok(());
    };
    let destination = parsed.destination.as_ref().unwrap_or(source);
    if source.is_wildcard() != destination.is_wildcard() {
        return invalid_refspec(
            refspec,
            "wildcard refspecs must use `*` on both source and destination",
        );
    }
    if source.is_wildcard() && parsed.destination.is_none() {
        return invalid_refspec(
            refspec,
            "wildcard refspecs must include an explicit destination",
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum RefspecSide {
    FetchSource,
    FetchDestination,
    PushSource,
    PushDestination,
}

fn parse_fetch_destination(remote: &str, destination: &str) -> Result<BranchPattern> {
    if let Some(rest) = destination.strip_prefix("refs/remotes/") {
        let (destination_remote, branch) =
            rest.split_once('/')
                .ok_or_else(|| RepoErr::InvalidRefspec {
                    refspec: destination.to_string(),
                    message: "fetch destination must be under `refs/remotes/<remote>/`".to_string(),
                })?;
        validate_remote_name(destination_remote)?;
        if destination_remote != remote {
            return invalid_refspec(
                destination,
                "fetch destination remote must match the selected remote",
            );
        }
        return parse_branch_pattern(branch, RefspecSide::FetchDestination);
    }
    if destination.starts_with("refs/") {
        return invalid_refspec(
            destination,
            "fetch destination must be a branch name or `refs/remotes/<remote>/<branch>`",
        );
    }
    parse_branch_pattern(destination, RefspecSide::FetchDestination)
}

fn parse_branch_pattern_ref(value: &str, side: RefspecSide) -> Result<BranchPattern> {
    let branch = if let Some(branch) = value.strip_prefix("refs/heads/") {
        branch
    } else if value.starts_with("refs/") {
        return invalid_refspec(value, refspec_side_message(side));
    } else {
        value
    };
    parse_branch_pattern(branch, side)
}

fn parse_branch_pattern(value: &str, _side: RefspecSide) -> Result<BranchPattern> {
    if value.matches('*').count() > 1 {
        return invalid_refspec(value, "only one `*` wildcard is supported");
    }
    if let Some((prefix, suffix)) = value.split_once('*') {
        let sample = format!("{prefix}x{suffix}");
        validate_ref_name(&sample)?;
        Ok(BranchPattern::Wildcard {
            prefix: prefix.to_string(),
            suffix: suffix.to_string(),
        })
    } else {
        validate_ref_name(value)?;
        Ok(BranchPattern::Exact(value.to_string()))
    }
}

fn refspec_side_message(side: RefspecSide) -> &'static str {
    match side {
        RefspecSide::FetchSource | RefspecSide::PushSource => {
            "source must be a branch name or `refs/heads/<branch>`"
        }
        RefspecSide::FetchDestination => {
            "fetch destination must be a branch name or `refs/remotes/<remote>/<branch>`"
        }
        RefspecSide::PushDestination => {
            "push destination must be a branch name or `refs/heads/<branch>`"
        }
    }
}

fn invalid_refspec<T>(refspec: &str, message: impl Into<String>) -> Result<T> {
    Err(RepoErr::InvalidRefspec {
        refspec: refspec.to_string(),
        message: message.into(),
    })
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

fn normalize_repo_path(path: &str) -> String {
    path.trim().trim_start_matches("./").replace('\\', "/")
}

fn is_sqlite_database_file(path: &Path) -> Result<bool> {
    let mut file = fs::File::open(path)?;
    let mut magic = [0; SQLITE_DATABASE_MAGIC.len()];
    match file.read_exact(&mut magic) {
        Ok(()) => Ok(&magic == SQLITE_DATABASE_MAGIC),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn diff_file_maps(
    from: impl Into<String>,
    to: impl Into<String>,
    from_files: &BTreeMap<String, CommitFileState>,
    to_files: &BTreeMap<String, CommitFileState>,
    path: Option<&str>,
) -> RepoDiff {
    let path = path.map(normalize_repo_path);
    let mut keys = BTreeMap::<String, ()>::new();
    for key in from_files.keys().chain(to_files.keys()) {
        if path.as_ref().is_none_or(|path| path == key) {
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
                from: before,
                to: after,
            });
        }
    }

    RepoDiff { from: from.into(), to: to.into(), files }
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
mod tests {
    use super::*;

    #[test]
    fn init_creates_repo_layout_and_unborn_main() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        assert!(repo.graft_dir().join(CONFIG_FILE).exists());
        assert!(repo.graft_dir().join(DIR_OBJECTS).is_dir());
        assert!(repo.graft_dir().join(DIR_OBJECTS_PACK).is_dir());
        assert!(!repo.graft_dir().join("objects/commits").exists());
        assert!(repo.graft_dir().join(DIR_STORE_FJALL).is_dir());
        assert_eq!(
            repo.config().unwrap().extensions.object_format,
            OBJECT_FORMAT
        );
        assert_eq!(
            fs::read_to_string(repo.graft_dir().join(HEAD_FILE)).unwrap(),
            "ref: refs/heads/main\n"
        );

        let status = repo.status().unwrap();
        assert_eq!(status.repository_format_version, REPOSITORY_FORMAT_VERSION);
        assert_eq!(status.head, Head::branch("main"));
        assert_eq!(status.head_target, None);
        assert!(!status.dirty);
        assert_eq!(status.branches.len(), 1);
        assert_eq!(status.branches[0].name, "main");
        assert_eq!(status.branches[0].target, None);
        assert!(status.branches[0].current);
    }

    #[test]
    fn open_rejects_unsupported_object_format() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let mut config = repo.config().unwrap();
        config.extensions.object_format = "sha1".to_string();
        repo.write_config(&config).unwrap();

        assert!(matches!(
            Repository::open(tmp.path()),
            Err(RepoErr::UnsupportedObjectFormat { expected, actual })
                if expected == OBJECT_FORMAT && actual == "sha1"
        ));
    }

    #[test]
    fn commit_updates_current_branch_and_log() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let app = tmp.path().join("app.db");

        repo.mark_dirty_path(&app).unwrap();
        let status = repo.status().unwrap();
        assert!(status.dirty);
        assert_eq!(status.unstaged, vec!["app.db".to_string()]);

        let first = repo.commit("initial database").unwrap();
        assert!(!repo.status().unwrap().dirty);
        assert_eq!(repo.status().unwrap().head_target, Some(first.id.clone()));

        repo.mark_dirty_path(&app).unwrap();
        let second = repo.commit("add table").unwrap();

        let log = repo.log().unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0], second);
        assert_eq!(log[1], first);
    }

    #[test]
    fn status_scans_physical_sqlite_files_as_untracked() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let nested = tmp.path().join("nested");
        fs::create_dir_all(&nested).unwrap();

        write_sqlite_magic(tmp.path().join("app.db"));
        fs::write(tmp.path().join("notes.txt"), b"not sqlite").unwrap();
        write_sqlite_magic(repo.graft_dir().join("ignored.db"));
        write_sqlite_magic(nested.join("data.sqlite"));

        let status = repo.status().unwrap();

        assert_eq!(
            status.unstaged_changes,
            vec![
                RepoWorktreeChange {
                    path: "app.db".to_string(),
                    change: RepoWorktreeChangeKind::Untracked,
                },
                RepoWorktreeChange {
                    path: "nested/data.sqlite".to_string(),
                    change: RepoWorktreeChangeKind::Untracked,
                },
            ]
        );
    }

    #[test]
    fn status_classifies_unstaged_modified_deleted_and_untracked_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(3), PageCount::new(7));
        let app = tmp.path().join("app.db");
        let notes = tmp.path().join("notes.db");

        fs::write(&app, b"tracked database").unwrap();
        repo.commit_file(&app, "initial database", volume, &snapshot)
            .unwrap();

        fs::write(&app, b"modified database").unwrap();
        repo.mark_dirty_path(&app).unwrap();
        let status = repo.status().unwrap();
        assert_eq!(
            status.unstaged_changes,
            vec![RepoWorktreeChange {
                path: "app.db".to_string(),
                change: RepoWorktreeChangeKind::Modified,
            }]
        );

        fs::remove_file(&app).unwrap();
        repo.mark_deleted_path(&app).unwrap();
        let status = repo.status().unwrap();
        assert_eq!(
            status.unstaged_changes,
            vec![RepoWorktreeChange {
                path: "app.db".to_string(),
                change: RepoWorktreeChangeKind::Deleted,
            }]
        );

        repo.clear_dirty().unwrap();
        fs::write(&notes, b"new database").unwrap();
        repo.mark_dirty_path(&notes).unwrap();
        let status = repo.status().unwrap();
        assert_eq!(
            status.unstaged_changes,
            vec![RepoWorktreeChange {
                path: "notes.db".to_string(),
                change: RepoWorktreeChangeKind::Untracked,
            }]
        );
        assert_eq!(status.unstaged, vec!["notes.db".to_string()]);
    }

    #[test]
    fn branch_reflog_records_old_and_new_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        let first = repo.commit("initial database").unwrap();
        let second = repo.commit("add table").unwrap();

        let reflog =
            fs::read_to_string(repo.graft_dir().join(DIR_LOGS_REFS).join("refs/heads/main"))
                .unwrap();
        let lines = reflog.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with(&format!("{NULL_OBJECT_ID} {}", first.id)));
        assert!(lines[0].contains("\tcommit: initial database"));
        assert!(lines[1].starts_with(&format!("{} {}", first.id, second.id)));
        assert!(lines[1].contains("\tcommit: add table"));
    }

    #[test]
    fn head_reflog_records_branch_switch_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        let main = repo.commit("initial database").unwrap();
        repo.switch_new_branch("feature/search", None).unwrap();
        let feature = repo.commit("feature work").unwrap();
        repo.switch_branch("main").unwrap();

        let reflog = fs::read_to_string(repo.graft_dir().join(DIR_LOGS_HEAD).join("HEAD")).unwrap();
        let last = reflog.lines().last().unwrap();
        assert!(last.starts_with(&format!("{} {}", feature.id, main.id)));
        assert!(last.contains("\tcheckout: moving to main"));
    }

    #[test]
    fn commit_file_records_snapshot_state() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(3), PageCount::new(7));

        let commit = repo
            .commit_file(
                tmp.path().join("app.db"),
                "initial database",
                volume.clone(),
                &snapshot,
            )
            .unwrap();

        let file = commit.files.get("app.db").unwrap();
        assert_eq!(file.volume, volume);
        assert_eq!(file.snapshot.to_snapshot().head(), snapshot.head());
        assert_eq!(
            repo.head_file(tmp.path().join("app.db")).unwrap(),
            Some(file.clone())
        );
    }

    #[test]
    fn stage_file_updates_index_and_commit_clears_it() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(3), PageCount::new(7));

        repo.mark_dirty_path(tmp.path().join("app.db")).unwrap();
        let entry = repo
            .stage_file(tmp.path().join("app.db"), volume, &snapshot)
            .unwrap();
        assert_eq!(entry.path, "app.db");
        assert!(!repo.is_dirty());
        assert!(repo.has_staged_changes().unwrap());

        let status = repo.status().unwrap();
        assert_eq!(status.staged, vec!["app.db".to_string()]);
        assert!(status.conflicted.is_empty());

        let commit = repo.commit_staged("initial database").unwrap();
        assert_eq!(
            repo.head_file(tmp.path().join("app.db")).unwrap(),
            entry.file
        );
        assert!(!repo.has_staged_changes().unwrap());
        assert!(repo.read_index().unwrap().is_empty());
        assert_eq!(repo.status().unwrap().head_target, Some(commit.id));
    }

    #[test]
    fn staging_one_file_preserves_other_unstaged_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(3), PageCount::new(7));
        let app = tmp.path().join("app.db");
        let notes = tmp.path().join("notes.db");

        repo.mark_dirty_path(&notes).unwrap();
        repo.mark_dirty_path(&app).unwrap();

        let status = repo.status().unwrap();
        assert_eq!(
            status.unstaged,
            vec!["app.db".to_string(), "notes.db".to_string()]
        );

        repo.stage_file(&app, volume, &snapshot).unwrap();
        let status = repo.status().unwrap();
        assert_eq!(status.staged, vec!["app.db".to_string()]);
        assert_eq!(status.unstaged, vec!["notes.db".to_string()]);

        repo.commit_staged("stage app only").unwrap();
        let status = repo.status().unwrap();
        assert!(status.staged.is_empty());
        assert_eq!(status.unstaged, vec!["notes.db".to_string()]);
        assert!(status.dirty);
    }

    #[test]
    fn stage_file_removal_commits_deleted_path() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let app = tmp.path().join("app.db");
        let notes = tmp.path().join("notes.db");
        let app_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let notes_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(3), PageCount::new(4));

        repo.stage_file(&app, volume.clone(), &app_snapshot)
            .unwrap();
        repo.stage_file(&notes, volume, &notes_snapshot).unwrap();
        let base = repo.commit_staged("base").unwrap();

        let removal = repo.stage_file_removal(&notes).unwrap();

        assert_eq!(removal.path, "notes.db");
        assert!(removal.file.is_none());
        let staged = repo.diff_staged(None).unwrap();
        assert_eq!(staged.from, base.id);
        assert_eq!(staged.files.len(), 1);
        assert_eq!(staged.files[0].path, "notes.db");
        assert_eq!(staged.files[0].change, RepoFileChange::Deleted);
        assert!(staged.files[0].to.is_none());

        let commit = repo.commit_staged("remove notes").unwrap();

        assert!(repo.head_file(&app).unwrap().is_some());
        assert!(repo.head_file(&notes).unwrap().is_none());
        assert!(
            !repo
                .read_commit(&commit.id)
                .unwrap()
                .files
                .contains_key("notes.db")
        );
        assert!(repo.read_index().unwrap().is_empty());
    }

    #[test]
    fn commit_staged_rejects_empty_index() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        assert!(matches!(
            repo.commit_staged("nothing to commit"),
            Err(RepoErr::NoStagedChanges)
        ));
    }

    #[test]
    fn stage_file_state_path_requires_storage_commit_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let log = LogId::random();
        let state = CommitFileState {
            volume: VolumeId::random(),
            snapshot: RepoSnapshot {
                page_count: PageCount::new(3),
                ranges: vec![RepoLogRange {
                    log,
                    start: LSN::FIRST,
                    end: LSN::new(2),
                    commits: vec![],
                }],
            },
        };

        let err = repo
            .stage_file_state_path(tmp.path().join("app.db"), state)
            .expect_err("missing storage commit hashes should be rejected");
        assert!(matches!(
            err,
            RepoErr::Object(object::ObjectErr::InvalidObject { kind: "sqlite-snapshot", .. })
        ));
    }

    #[test]
    fn commit_file_writes_content_addressed_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(3), PageCount::new(7));

        let commit = repo
            .commit_file(
                tmp.path().join("app.db"),
                "initial database",
                volume.clone(),
                &snapshot,
            )
            .unwrap();

        assert!(!repo.graft_dir().join("objects/commits").exists());
        let object::Object::Commit(commit_object) = repo.read_object(&commit.id).unwrap() else {
            panic!("repo commit id should point at a commit object");
        };
        assert_eq!(commit.tree.as_deref(), Some(commit_object.tree.as_str()));
        assert!(commit_object.parents.is_empty());

        let object::Object::Tree(tree) = repo.read_object(commit_object.tree.as_str()).unwrap()
        else {
            panic!("commit tree should point at a tree object");
        };
        assert_eq!(tree.entries.len(), 1);
        assert_eq!(tree.entries[0].path, "app.db");
        assert_eq!(tree.entries[0].mode, object::TreeEntryMode::SqliteDatabase);

        let object::Object::Blob(object::BlobObject::SqliteSnapshot(blob)) =
            repo.read_object(tree.entries[0].oid.as_str()).unwrap()
        else {
            panic!("tree entry should point at a sqlite snapshot blob");
        };
        assert_eq!(blob.volume, volume);
        assert_eq!(blob.page_count, PageCount::new(7));
        assert_eq!(blob.ranges.len(), 1);
        assert_eq!(blob.ranges[0].log, log);
        assert_eq!(blob.ranges[0].start, LSN::FIRST);
        assert_eq!(blob.ranges[0].end, LSN::new(3));

        let reconstructed = repo.read_commit(&commit.id).unwrap();
        assert_eq!(reconstructed.id, commit.id);
        assert_eq!(reconstructed.tree, commit.tree);
        assert_eq!(reconstructed.files, commit.files);
    }

    #[test]
    fn tree_id_changes_when_sqlite_snapshot_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let first_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let second_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(4));

        let first = repo
            .commit_file(
                tmp.path().join("app.db"),
                "first",
                volume.clone(),
                &first_snapshot,
            )
            .unwrap();
        let second = repo
            .commit_file(
                tmp.path().join("app.db"),
                "second",
                volume,
                &second_snapshot,
            )
            .unwrap();

        assert_ne!(first.tree, second.tree);
        assert_ne!(first.id, second.id);
    }

    #[test]
    fn resolve_revision_supports_head_branch_parent_and_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let first_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let second_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(4));

        let first = repo
            .commit_file(
                tmp.path().join("app.db"),
                "first",
                volume.clone(),
                &first_snapshot,
            )
            .unwrap();
        let second = repo
            .commit_file(
                tmp.path().join("app.db"),
                "second",
                volume,
                &second_snapshot,
            )
            .unwrap();
        let prefix = &second.id[..12];

        assert_eq!(repo.resolve_revision("HEAD").unwrap(), second.id);
        assert_eq!(repo.resolve_revision("@").unwrap(), second.id);
        assert_eq!(repo.resolve_revision("main").unwrap(), second.id);
        assert_eq!(repo.resolve_revision("HEAD~1").unwrap(), first.id);
        assert_eq!(repo.resolve_revision("HEAD^").unwrap(), first.id);
        assert_eq!(repo.resolve_revision("HEAD^1").unwrap(), first.id);
        assert_eq!(repo.resolve_revision("HEAD^0").unwrap(), second.id);
        assert_eq!(repo.resolve_revision(prefix).unwrap(), second.id);
    }

    #[test]
    fn tags_create_list_resolve_and_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let first_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let second_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(4));

        let first = repo
            .commit_file(
                tmp.path().join("app.db"),
                "first",
                volume.clone(),
                &first_snapshot,
            )
            .unwrap();
        let second = repo
            .commit_file(
                tmp.path().join("app.db"),
                "second",
                volume,
                &second_snapshot,
            )
            .unwrap();

        let tag = repo.tag_create("v1.0", Some("HEAD~1")).unwrap();
        assert_eq!(tag.name, "v1.0");
        assert_eq!(tag.target, first.id);
        assert_eq!(repo.resolve_revision("v1.0").unwrap(), tag.target);

        let latest = repo.tag_create("latest", None).unwrap();
        assert_eq!(latest.target, second.id);
        assert_eq!(repo.resolve_revision("latest").unwrap(), latest.target);
        assert!(repo.tags().unwrap().iter().any(|tag| tag.name == "v1.0"));
        assert!(matches!(
            repo.tag_create("latest", None),
            Err(RepoErr::TagExists(name)) if name == "latest"
        ));

        let deleted = repo.tag_delete("v1.0").unwrap();
        assert_eq!(deleted.name, "v1.0");
        assert!(repo.tags().unwrap().iter().all(|tag| tag.name != "v1.0"));
        assert!(matches!(
            repo.resolve_revision("v1.0"),
            Err(RepoErr::UnknownRevision(rev)) if rev == "v1.0"
        ));
        assert!(matches!(
            repo.tag_delete("v1.0"),
            Err(RepoErr::TagNotFound(name)) if name == "v1.0"
        ));
    }

    #[test]
    fn annotated_tags_point_refs_at_tag_objects_and_peel_to_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let commit = repo.commit("initial database").unwrap();

        let tag = repo
            .tag_create_annotated("v1.0", None, "release 1.0")
            .unwrap();
        assert_eq!(tag.name, "v1.0");
        assert_eq!(tag.target, commit.id);
        assert_ne!(tag.object, tag.target);
        assert!(tag.annotated);
        assert_eq!(tag.message.as_deref(), Some("release 1.0"));

        let tag_object = repo.read_object(&tag.object).unwrap();
        let object::Object::Tag(tag_object) = tag_object else {
            panic!("expected tag object");
        };
        assert_eq!(tag_object.name, "v1.0");
        assert_eq!(tag_object.message, "release 1.0");
        assert_eq!(tag_object.object.to_string(), commit.id);

        assert_eq!(repo.resolve_revision("v1.0").unwrap(), commit.id);
        assert_eq!(
            repo.resolve_revision(&format!("refs/tags/{}", tag.name))
                .unwrap(),
            commit.id
        );
        assert_eq!(repo.resolve_revision(&tag.object).unwrap(), commit.id);

        let listed = repo.tags().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0], tag);
    }

    #[test]
    fn diff_revisions_reports_changed_sqlite_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let first_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let second_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(4));

        let first = repo
            .commit_file(
                tmp.path().join("app.db"),
                "first",
                volume.clone(),
                &first_snapshot,
            )
            .unwrap();
        let second = repo
            .commit_file(
                tmp.path().join("app.db"),
                "second",
                volume,
                &second_snapshot,
            )
            .unwrap();

        let diff = repo.diff_revisions(&first.id, &second.id, None).unwrap();
        assert_eq!(diff.from, first.id);
        assert_eq!(diff.to, second.id);
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].path, "app.db");
        assert_eq!(diff.files[0].change, RepoFileChange::Modified);

        let empty = repo
            .diff_revisions("HEAD~1", "HEAD", Some("missing.db"))
            .unwrap();
        assert!(empty.files.is_empty());
    }

    #[test]
    fn diff_staged_and_worktree_file_reports_git_like_states() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let first_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let staged_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(4), PageCount::new(4));
        let worktree_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(6), PageCount::new(5));
        let db = tmp.path().join("app.db");

        let first = repo
            .commit_file(&db, "first", volume.clone(), &first_snapshot)
            .unwrap();
        let staged = repo
            .stage_file(&db, volume.clone(), &staged_snapshot)
            .unwrap();
        let worktree = CommitFileState {
            volume,
            snapshot: RepoSnapshot::from_snapshot(&worktree_snapshot),
        };

        let staged_diff = repo.diff_staged(None).unwrap();
        assert_eq!(staged_diff.from, first.id);
        assert_eq!(staged_diff.to, "index");
        assert_eq!(staged_diff.files.len(), 1);
        assert_eq!(staged_diff.files[0].change, RepoFileChange::Modified);
        assert_eq!(staged_diff.files[0].to, staged.file);

        let worktree_diff = repo
            .diff_worktree_file(&db, worktree.clone(), Some("app.db"))
            .unwrap();
        assert_eq!(worktree_diff.from, "index");
        assert_eq!(worktree_diff.to, "worktree");
        assert_eq!(worktree_diff.files.len(), 1);
        assert_eq!(worktree_diff.files[0].change, RepoFileChange::Modified);
        assert_eq!(worktree_diff.files[0].to, Some(worktree.clone()));

        let rev_worktree_diff = repo
            .diff_revision_to_worktree_file("HEAD", &db, worktree, None)
            .unwrap();
        assert_eq!(rev_worktree_diff.from, first.id);
        assert_eq!(rev_worktree_diff.to, "worktree");
        assert_eq!(rev_worktree_diff.files.len(), 1);
        assert_eq!(rev_worktree_diff.files[0].change, RepoFileChange::Modified);
    }

    #[test]
    fn detach_moves_head_to_resolved_revision() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let first_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let second_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(4));

        let first = repo
            .commit_file(
                tmp.path().join("app.db"),
                "first",
                volume.clone(),
                &first_snapshot,
            )
            .unwrap();
        let second = repo
            .commit_file(
                tmp.path().join("app.db"),
                "second",
                volume,
                &second_snapshot,
            )
            .unwrap();

        let detached = repo.detach("HEAD~1").unwrap();
        assert_eq!(detached, first.id);
        assert_eq!(
            repo.head().unwrap(),
            Head::Detached { commit: first.id.clone() }
        );
        assert_eq!(repo.resolve_revision("HEAD").unwrap(), first.id);
        assert_eq!(repo.resolve_revision(&second.id[..12]).unwrap(), second.id);
    }

    #[test]
    fn checkout_plan_freezes_target_files_before_refs_move() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let first_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let second_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(4), PageCount::new(4));
        let third_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(6), PageCount::new(5));
        let db = tmp.path().join("app.db");

        repo.commit_file(&db, "first", volume.clone(), &first_snapshot)
            .unwrap();
        let second = repo
            .commit_file(&db, "second", volume.clone(), &second_snapshot)
            .unwrap();
        let plan = repo.plan_revision_checkout("HEAD").unwrap();
        let third = repo
            .commit_file(&db, "third", volume, &third_snapshot)
            .unwrap();

        assert_eq!(plan.target, Some(second.id));
        assert_eq!(
            plan.files
                .get("app.db")
                .expect("planned app.db")
                .snapshot
                .to_snapshot()
                .head(),
            second_snapshot.head()
        );
        assert_eq!(repo.status().unwrap().head_target, Some(third.id));
    }

    #[test]
    fn checkout_file_from_revision_stages_path_without_moving_head() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let first_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let second_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(4));
        let db = tmp.path().join("app.db");

        let first = repo
            .commit_file(&db, "first", volume.clone(), &first_snapshot)
            .unwrap();
        let second = repo
            .commit_file(&db, "second", volume, &second_snapshot)
            .unwrap();

        let outcome = repo.checkout_file_from_revision("HEAD~1", &db).unwrap();

        assert_eq!(outcome.target, first.id);
        assert_eq!(outcome.path, "app.db");
        assert_eq!(
            outcome.state.snapshot.to_snapshot().head(),
            first_snapshot.head()
        );
        assert_eq!(repo.status().unwrap().head_target, Some(second.id));
        let index = repo.read_index().unwrap();
        let staged: Vec<_> = index.stage0_entries().collect();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].path, "app.db");
        assert_eq!(staged[0].file, Some(outcome.state));
    }

    #[test]
    fn checkout_file_plan_freezes_target_before_branch_moves() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let base_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let feature_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(4), PageCount::new(4));
        let later_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(6), PageCount::new(5));
        let db = tmp.path().join("app.db");

        repo.commit_file(&db, "base", volume.clone(), &base_snapshot)
            .unwrap();
        repo.switch_new_branch("feature/search", None).unwrap();
        let feature = repo
            .commit_file(&db, "feature", volume.clone(), &feature_snapshot)
            .unwrap();
        repo.switch_branch("main").unwrap();

        let plan = repo
            .plan_checkout_file_from_revision("feature/search", &db)
            .unwrap();
        assert_eq!(plan.target, feature.id);
        assert_eq!(plan.path, "app.db");
        assert!(repo.read_index().unwrap().is_empty());

        repo.switch_branch("feature/search").unwrap();
        let later = repo
            .commit_file(&db, "feature later", volume, &later_snapshot)
            .unwrap();
        repo.switch_branch("main").unwrap();

        let outcome = repo.apply_checkout_file_plan(&plan).unwrap();

        assert_eq!(outcome.target, feature.id);
        assert_eq!(
            outcome.state.snapshot.to_snapshot().head(),
            feature_snapshot.head()
        );
        assert_eq!(
            repo.branch_target("feature/search").unwrap(),
            Some(later.id)
        );
        let staged = repo.diff_staged(Some("app.db")).unwrap();
        assert_eq!(staged.to, "index");
        assert_eq!(staged.files.len(), 1);
        assert_eq!(
            staged.files[0]
                .to
                .as_ref()
                .unwrap()
                .snapshot
                .to_snapshot()
                .head(),
            feature_snapshot.head()
        );
    }

    #[test]
    fn reset_modes_update_head_index_and_dirty_state() {
        for (mode, expect_staged, expect_dirty) in [
            (ResetMode::Soft, true, true),
            (ResetMode::Mixed, false, true),
            (ResetMode::Hard, false, false),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let repo = Repository::init(tmp.path()).unwrap();
            let volume = VolumeId::random();
            let log = LogId::random();
            let first_snapshot =
                Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
            let second_snapshot =
                Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(4), PageCount::new(4));
            let staged_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(6), PageCount::new(5));

            let first = repo
                .commit_file(
                    tmp.path().join("app.db"),
                    "first",
                    volume.clone(),
                    &first_snapshot,
                )
                .unwrap();
            repo.commit_file(
                tmp.path().join("app.db"),
                "second",
                volume.clone(),
                &second_snapshot,
            )
            .unwrap();
            repo.stage_file(tmp.path().join("app.db"), volume, &staged_snapshot)
                .unwrap();
            repo.mark_dirty_path(tmp.path().join("app.db")).unwrap();

            let outcome = repo.reset("HEAD~1", mode).unwrap();

            assert_eq!(outcome.target, first.id);
            assert_eq!(outcome.mode, mode);
            assert_eq!(repo.status().unwrap().head_target, Some(outcome.target));
            assert_eq!(repo.has_staged_changes().unwrap(), expect_staged);
            assert_eq!(repo.is_dirty(), expect_dirty);
        }
    }

    #[test]
    fn reset_plan_freezes_target_before_branch_moves() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let app = tmp.path().join("app.db");
        let base_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let feature_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(4), PageCount::new(4));
        let later_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(6), PageCount::new(5));

        let base = repo
            .commit_file(&app, "base", volume.clone(), &base_snapshot)
            .unwrap();
        repo.switch_new_branch("feature/search", None).unwrap();
        let feature = repo
            .commit_file(&app, "feature", volume.clone(), &feature_snapshot)
            .unwrap();
        repo.switch_branch("main").unwrap();

        let plan = repo.plan_reset("feature/search", ResetMode::Hard).unwrap();
        assert_eq!(plan.target, feature.id);
        assert_eq!(plan.checkout.target, Some(feature.id.clone()));

        repo.switch_branch("feature/search").unwrap();
        let later = repo
            .commit_file(&app, "feature later", volume, &later_snapshot)
            .unwrap();
        repo.switch_branch("main").unwrap();

        let outcome = repo.apply_reset_plan(&plan).unwrap();
        assert_eq!(outcome.target, feature.id);
        assert_eq!(repo.branch_target("main").unwrap(), Some(feature.id));
        assert_eq!(
            repo.branch_target("feature/search").unwrap(),
            Some(later.id)
        );
        assert_ne!(repo.branch_target("main").unwrap(), Some(base.id));
    }

    #[test]
    fn branch_switch_and_remote_tracking_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let first = repo.commit("initial database").unwrap();

        let branch = repo.switch_new_branch("feature/search", None).unwrap();
        assert!(branch.current);
        assert_eq!(branch.target, Some(first.id.clone()));
        assert_eq!(repo.head().unwrap(), Head::branch("feature/search"));

        repo.switch_branch("main").unwrap();
        assert_eq!(repo.head().unwrap(), Head::branch("main"));

        repo.remote_add(
            "origin",
            RemoteConfig::Fs {
                root: tmp.path().join("remote").to_string_lossy().into_owned(),
            },
        )
        .unwrap();
        repo.set_remote_tracking_ref("origin", "main", &first.id)
            .unwrap();

        assert_eq!(
            repo.remote_tracking_ref("origin", "main").unwrap(),
            Some(first.id)
        );
        assert_eq!(repo.remotes().unwrap().len(), 1);
    }

    #[test]
    fn branch_upstream_config_drives_default_remote_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        repo.commit("initial database").unwrap();
        repo.remote_add(
            "origin",
            RemoteConfig::Fs {
                root: tmp.path().join("remote").to_string_lossy().into_owned(),
            },
        )
        .unwrap();

        let branch = repo.set_branch_upstream("main", "origin", "trunk").unwrap();
        assert_eq!(
            branch.upstream,
            Some(BranchUpstream {
                remote: "origin".to_string(),
                branch: "trunk".to_string(),
            })
        );
        assert_eq!(repo.status().unwrap().upstream, branch.upstream);
        assert_eq!(
            repo.default_remote_branch(None, None).unwrap(),
            BranchUpstream {
                remote: "origin".to_string(),
                branch: "trunk".to_string(),
            }
        );
        assert_eq!(
            repo.default_remote_branch(Some("origin"), None).unwrap(),
            BranchUpstream {
                remote: "origin".to_string(),
                branch: "main".to_string(),
            }
        );

        let config = repo.config().unwrap();
        assert_eq!(
            config.branches["main"].merge.as_deref(),
            Some("refs/heads/trunk")
        );

        let branch = repo.unset_branch_upstream("main").unwrap();
        assert_eq!(branch.upstream, None);
        assert_eq!(
            repo.default_remote_branch(None, None).unwrap(),
            BranchUpstream {
                remote: "origin".to_string(),
                branch: "main".to_string(),
            }
        );
    }

    #[test]
    fn branch_create_resolves_start_point_and_rejects_existing_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let first = repo.commit("initial database").unwrap();
        let second = repo.commit("second database").unwrap();

        let branch = repo.branch_create("release/1.0", Some("HEAD~1")).unwrap();
        assert_eq!(branch.name, "release/1.0");
        assert_eq!(branch.target, Some(first.id.clone()));
        assert_eq!(repo.resolve_revision("release/1.0").unwrap(), first.id);
        assert_eq!(repo.resolve_revision("HEAD").unwrap(), second.id);
        assert!(matches!(
            repo.branch_create("release/1.0", None),
            Err(RepoErr::BranchExists(name)) if name == "release/1.0"
        ));
    }

    #[test]
    fn branch_rename_moves_current_branch_config_and_reflog() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let first = repo.commit("initial database").unwrap();

        repo.branch_create("feature/search", None).unwrap();
        repo.remote_add(
            "origin",
            RemoteConfig::Fs {
                root: tmp.path().join("remote").to_string_lossy().into_owned(),
            },
        )
        .unwrap();
        repo.set_branch_upstream("feature/search", "origin", "feature/search")
            .unwrap();
        repo.switch_branch("feature/search").unwrap();

        let renamed = repo
            .branch_rename("feature/search", "topic/search", false)
            .unwrap();
        assert!(renamed.current);
        assert_eq!(renamed.name, "topic/search");
        assert_eq!(renamed.target, Some(first.id.clone()));
        assert_eq!(
            renamed.upstream,
            Some(BranchUpstream {
                remote: "origin".to_string(),
                branch: "feature/search".to_string(),
            })
        );
        assert_eq!(repo.head().unwrap(), Head::branch("topic/search"));
        assert_eq!(repo.branch_target("topic/search").unwrap(), Some(first.id));
        assert!(
            !repo
                .graft_dir
                .join(DIR_REFS_HEADS)
                .join("feature/search")
                .exists()
        );
        assert!(
            repo.graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs/heads/topic/search")
                .is_file()
        );
        assert!(
            !repo
                .graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs/heads/feature/search")
                .exists()
        );
    }

    #[test]
    fn branch_rename_force_overwrites_existing_branch_and_ref_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let first = repo.commit("initial database").unwrap();
        let second = repo.commit("second database").unwrap();

        repo.branch_create("feature", Some(&first.id)).unwrap();
        repo.branch_rename("feature", "feature/search", false)
            .unwrap();
        assert_eq!(
            repo.branch_target("feature/search").unwrap(),
            Some(first.id.clone())
        );

        repo.branch_create("release/next", Some(&second.id))
            .unwrap();
        repo.branch_rename("feature/search", "release/next", true)
            .unwrap();
        assert_eq!(repo.branch_target("release/next").unwrap(), Some(first.id));
        assert!(!repo.graft_dir.join(DIR_REFS_HEADS).join("feature").exists());
        assert!(
            !repo
                .graft_dir
                .join(DIR_REFS_HEADS)
                .join("feature/search")
                .exists()
        );
    }

    #[test]
    fn branch_rename_current_unborn_branch_updates_head() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        let renamed = repo.branch_rename("main", "trunk", false).unwrap();
        assert_eq!(renamed.name, "trunk");
        assert!(renamed.current);
        assert_eq!(renamed.target, None);
        assert_eq!(repo.head().unwrap(), Head::branch("trunk"));
    }

    #[test]
    fn refs_reject_file_directory_namespace_conflicts() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let first = repo.commit("initial database").unwrap();

        repo.branch_create("feature/search", None).unwrap();
        assert!(matches!(
            repo.branch_create("feature", None),
            Err(RepoErr::RefNameConflict { reference, existing })
                if reference == "refs/heads/feature" && existing == "refs/heads/feature"
        ));

        repo.branch_create("release", None).unwrap();
        assert!(matches!(
            repo.branch_create("release/1.0", None),
            Err(RepoErr::RefNameConflict { reference, existing })
                if reference == "refs/heads/release/1.0" && existing == "refs/heads/release"
        ));

        repo.tag_create("v1/rc1", None).unwrap();
        assert!(matches!(
            repo.tag_create("v1", None),
            Err(RepoErr::RefNameConflict { reference, existing })
                if reference == "refs/tags/v1" && existing == "refs/tags/v1"
        ));

        repo.tag_create("stable", None).unwrap();
        assert!(matches!(
            repo.tag_create("stable/rc1", None),
            Err(RepoErr::RefNameConflict { reference, existing })
                if reference == "refs/tags/stable/rc1" && existing == "refs/tags/stable"
        ));

        repo.set_remote_tracking_ref("origin", "topic/search", &first.id)
            .unwrap();
        assert!(matches!(
            repo.set_remote_tracking_ref("origin", "topic", &first.id),
            Err(RepoErr::RefNameConflict { reference, existing })
                if reference == "refs/remotes/origin/topic" && existing == "refs/remotes/origin/topic"
        ));

        repo.set_remote_tracking_ref("origin", "main", &first.id)
            .unwrap();
        assert!(matches!(
            repo.set_remote_tracking_ref("origin", "main/v2", &first.id),
            Err(RepoErr::RefNameConflict { reference, existing })
                if reference == "refs/remotes/origin/main/v2" && existing == "refs/remotes/origin/main"
        ));
    }

    #[test]
    fn remote_remove_deletes_config_and_tracking_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let first = repo.commit("initial database").unwrap();

        let config = RemoteConfig::Fs {
            root: tmp.path().join("remote").to_string_lossy().into_owned(),
        };
        repo.remote_add("origin", config).unwrap();
        repo.set_branch_upstream("main", "origin", "main").unwrap();
        assert!(repo.branch_upstream("main").unwrap().is_some());
        repo.set_remote_tracking_ref("origin", "main", &first.id)
            .unwrap();
        assert_eq!(
            repo.remote_tracking_ref("origin", "main").unwrap(),
            Some(first.id)
        );

        let removed = repo.remote_remove("origin").unwrap();
        assert_eq!(removed.name, "origin");
        assert!(matches!(removed.config, RemoteConfig::Fs { .. }));
        assert!(repo.remotes().unwrap().is_empty());
        assert_eq!(repo.branch_upstream("main").unwrap(), None);
        assert_eq!(repo.remote_tracking_ref("origin", "main").unwrap(), None);
        assert!(matches!(
            repo.remote_store("origin"),
            Err(RepoErr::RemoteNotFound(name)) if name == "origin"
        ));
        assert!(matches!(
            repo.remote_remove("origin"),
            Err(RepoErr::RemoteNotFound(name)) if name == "origin"
        ));
    }

    #[test]
    fn remote_rename_moves_config_tracking_refs_reflogs_and_upstreams() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let first = repo.commit("initial database").unwrap();

        let config = RemoteConfig::Fs {
            root: tmp.path().join("remote").to_string_lossy().into_owned(),
        };
        repo.remote_add("origin", config).unwrap();
        repo.set_branch_upstream("main", "origin", "main").unwrap();
        repo.set_remote_tracking_ref("origin", "main", &first.id)
            .unwrap();
        assert!(
            repo.graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs/remotes/origin/main")
                .is_file()
        );

        let renamed = repo.remote_rename("origin", "upstream").unwrap();
        assert_eq!(renamed.name, "upstream");
        assert!(matches!(renamed.config, RemoteConfig::Fs { .. }));
        assert!(repo.remote_store("upstream").is_ok());
        assert!(matches!(
            repo.remote_store("origin"),
            Err(RepoErr::RemoteNotFound(name)) if name == "origin"
        ));
        assert_eq!(
            repo.branch_upstream("main").unwrap(),
            Some(BranchUpstream {
                remote: "upstream".to_string(),
                branch: "main".to_string(),
            })
        );
        assert_eq!(
            repo.remote_tracking_ref("upstream", "main").unwrap(),
            Some(first.id)
        );
        assert_eq!(repo.remote_tracking_ref("origin", "main").unwrap(), None);
        assert!(
            repo.graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs/remotes/upstream/main")
                .is_file()
        );
        assert!(
            !repo
                .graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs/remotes/origin")
                .exists()
        );
        assert!(matches!(
            repo.remote_rename("upstream", "upstream"),
            Ok(RemoteInfo { name, .. }) if name == "upstream"
        ));
        repo.remote_add("backup", RemoteConfig::Memory).unwrap();
        assert!(matches!(
            repo.remote_rename("upstream", "backup"),
            Err(RepoErr::RemoteExists(name)) if name == "backup"
        ));
    }

    #[test]
    fn remote_set_url_updates_config_without_touching_refs_or_upstreams() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let first = repo.commit("initial database").unwrap();

        let original = RemoteConfig::Fs {
            root: tmp.path().join("remote-a").to_string_lossy().into_owned(),
        };
        let updated = RemoteConfig::Fs {
            root: tmp.path().join("remote-b").to_string_lossy().into_owned(),
        };
        repo.remote_add("origin", original.clone()).unwrap();
        repo.set_branch_upstream("main", "origin", "main").unwrap();
        repo.set_remote_tracking_ref("origin", "main", &first.id)
            .unwrap();

        assert_eq!(repo.remote_get_url("origin").unwrap().config, original);
        let info = repo.remote_set_url("origin", updated.clone()).unwrap();
        assert_eq!(info.name, "origin");
        assert_eq!(info.config, updated);
        assert_eq!(repo.remote_get_url("origin").unwrap().config, updated);
        assert_eq!(
            repo.branch_upstream("main").unwrap(),
            Some(BranchUpstream {
                remote: "origin".to_string(),
                branch: "main".to_string(),
            })
        );
        assert_eq!(
            repo.remote_tracking_ref("origin", "main").unwrap(),
            Some(first.id)
        );
        assert!(matches!(
            repo.remote_set_url("missing", RemoteConfig::Memory),
            Err(RepoErr::RemoteNotFound(name)) if name == "missing"
        ));
        assert!(matches!(
            repo.remote_get_url("../origin"),
            Err(RepoErr::InvalidRemoteName(name)) if name == "../origin"
        ));
        assert!(matches!(
            repo.remote_store("../origin"),
            Err(RepoErr::InvalidRemoteName(name)) if name == "../origin"
        ));
    }

    #[test]
    fn branch_delete_removes_merged_branch_and_rejects_current_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let first = repo.commit("initial database").unwrap();

        repo.branch_create("feature/search", None).unwrap();
        assert!(repo.branch_exists("feature/search"));

        assert!(matches!(
            repo.branch_delete("main", false),
            Err(RepoErr::BranchIsCurrent(name)) if name == "main"
        ));

        let deleted = repo.branch_delete("feature/search", false).unwrap();
        assert_eq!(deleted.name, "feature/search");
        assert_eq!(deleted.target, Some(first.id));
        assert!(!repo.branch_exists("feature/search"));
        assert!(matches!(
            repo.switch_branch("feature/search"),
            Err(RepoErr::BranchNotFound(name)) if name == "feature/search"
        ));
        assert!(matches!(
            repo.switch_branch("feature"),
            Err(RepoErr::BranchNotFound(name)) if name == "feature"
        ));
    }

    #[test]
    fn branch_delete_requires_force_for_unmerged_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let app = tmp.path().join("app.db");
        let base_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let feature_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(3), PageCount::new(4));

        repo.commit_file(&app, "base", volume.clone(), &base_snapshot)
            .unwrap();
        repo.switch_new_branch("feature/search", None).unwrap();
        let feature = repo
            .commit_file(&app, "feature", volume, &feature_snapshot)
            .unwrap();
        repo.switch_branch("main").unwrap();

        assert!(matches!(
            repo.branch_delete("feature/search", false),
            Err(RepoErr::BranchNotMerged { branch, target })
                if branch == "feature/search" && target == feature.id
        ));

        let deleted = repo.branch_delete("feature/search", true).unwrap();
        assert_eq!(deleted.target, Some(feature.id));
        assert!(!repo.branch_exists("feature/search"));
    }

    #[test]
    fn ref_names_reject_git_revision_syntax_and_path_hazards() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        for name in [
            "-topic",
            ".topic",
            "topic.",
            "topic.lock",
            "feature/.hidden",
            "feature/topic.lock",
            "feature//topic",
            "feature/../topic",
            "topic..next",
            "topic name",
            "topic\tname",
            "topic~1",
            "topic^1",
            "topic:bad",
            "topic?bad",
            "topic*bad",
            "topic[bad",
            "topic\\bad",
            "topic@{1}",
            "@",
        ] {
            assert!(
                matches!(repo.branch_create_unborn(name), Err(RepoErr::InvalidRefName(actual)) if actual == name),
                "expected invalid branch name `{name}`"
            );
        }
    }

    #[test]
    fn remote_names_reject_ref_path_and_revision_hazards() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        for name in [
            "-origin",
            "origin.",
            "origin.lock",
            "up/stream",
            "up\\stream",
            "up..stream",
            "up stream",
            "up\tstream",
            "origin~1",
            "origin^1",
            "origin:bad",
            "origin?bad",
            "origin*bad",
            "origin[bad",
            "origin@{1}",
            "@",
        ] {
            assert!(
                matches!(
                    repo.remote_add(name, RemoteConfig::Memory),
                    Err(RepoErr::InvalidRemoteName(actual)) if actual == name
                ),
                "expected invalid remote name `{name}`"
            );
        }
    }

    #[test]
    fn merge_revision_stages_clean_three_way_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let app = tmp.path().join("app.db");
        let notes = tmp.path().join("notes.db");
        let app_base = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let app_main = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(3), PageCount::new(4));
        let notes_base = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(4), PageCount::new(5));
        let notes_feature = Snapshot::new(log, LSN::FIRST..=LSN::new(5), PageCount::new(6));

        repo.stage_file(&app, volume.clone(), &app_base).unwrap();
        repo.stage_file(&notes, volume.clone(), &notes_base)
            .unwrap();
        let base = repo.commit_staged("base").unwrap();
        repo.switch_new_branch("feature/search", None).unwrap();
        let feature = repo
            .commit_file(&notes, "feature notes", volume.clone(), &notes_feature)
            .unwrap();
        repo.switch_branch("main").unwrap();
        let main = repo
            .commit_file(&app, "main app", volume, &app_main)
            .unwrap();

        let outcome = repo.merge_revision("feature/search").unwrap();

        assert_eq!(
            outcome,
            MergeOutcome::Merged {
                head: main.id,
                target: feature.id,
                merge_base: Some(base.id),
                staged: vec!["notes.db".to_string()],
                conflicted: vec![],
            }
        );
        let status = repo.status().unwrap();
        assert_eq!(status.staged, vec!["notes.db".to_string()]);
        assert!(status.conflicted.is_empty());
        assert!(!repo.read_index().unwrap().has_conflicts());
    }

    #[test]
    fn merge_plan_freezes_target_before_branch_moves() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let app = tmp.path().join("app.db");
        let base_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let feature_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(4), PageCount::new(4));
        let later_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(6), PageCount::new(5));

        let base = repo
            .commit_file(&app, "base", volume.clone(), &base_snapshot)
            .unwrap();
        repo.switch_new_branch("feature/search", None).unwrap();
        let feature = repo
            .commit_file(&app, "feature", volume.clone(), &feature_snapshot)
            .unwrap();
        repo.switch_branch("main").unwrap();

        let plan = repo.plan_merge_revision("feature/search").unwrap();
        assert_eq!(plan.target, feature.id);
        assert_eq!(
            plan.outcome,
            MergeOutcome::FastForward {
                from: Some(base.id.clone()),
                to: feature.id.clone()
            }
        );

        repo.switch_branch("feature/search").unwrap();
        let later = repo
            .commit_file(&app, "feature later", volume, &later_snapshot)
            .unwrap();
        repo.switch_branch("main").unwrap();

        let outcome = repo.apply_merge_plan(&plan).unwrap();
        assert_eq!(
            outcome,
            MergeOutcome::FastForward {
                from: Some(base.id),
                to: feature.id.clone()
            }
        );
        assert_eq!(repo.branch_target("main").unwrap(), Some(feature.id));
        assert_eq!(
            repo.branch_target("feature/search").unwrap(),
            Some(later.id)
        );
    }

    #[test]
    fn merge_revision_stages_clean_delete_from_theirs() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let app = tmp.path().join("app.db");
        let notes = tmp.path().join("notes.db");
        let app_base = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let notes_base = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(3), PageCount::new(4));
        let app_main = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(5));

        repo.stage_file(&app, volume.clone(), &app_base).unwrap();
        repo.stage_file(&notes, volume.clone(), &notes_base)
            .unwrap();
        let base = repo.commit_staged("base").unwrap();
        repo.switch_new_branch("feature/delete-notes", None)
            .unwrap();
        let removal = repo.stage_file_removal(&notes).unwrap();
        assert!(removal.file.is_none());
        let feature = repo.commit_staged("remove notes").unwrap();
        repo.switch_branch("main").unwrap();
        let main = repo
            .commit_file(&app, "main app", volume, &app_main)
            .unwrap();

        let outcome = repo.merge_revision("feature/delete-notes").unwrap();

        assert_eq!(
            outcome,
            MergeOutcome::Merged {
                head: main.id,
                target: feature.id,
                merge_base: Some(base.id),
                staged: vec!["notes.db".to_string()],
                conflicted: vec![],
            }
        );
        let staged = repo.diff_staged(Some("notes.db")).unwrap();
        assert_eq!(staged.files.len(), 1);
        assert_eq!(staged.files[0].change, RepoFileChange::Deleted);
        assert!(staged.files[0].to.is_none());
    }

    #[test]
    fn merge_revision_writes_conflict_stages_and_blocks_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let app = tmp.path().join("app.db");
        let base = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let ours = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(3), PageCount::new(4));
        let theirs = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(5));

        let base_commit = repo
            .commit_file(&app, "base", volume.clone(), &base)
            .unwrap();
        repo.switch_new_branch("feature/search", None).unwrap();
        let feature = repo
            .commit_file(&app, "feature", volume.clone(), &theirs)
            .unwrap();
        repo.switch_branch("main").unwrap();
        let main = repo.commit_file(&app, "main", volume, &ours).unwrap();

        let outcome = repo.merge_revision("feature/search").unwrap();

        assert_eq!(
            outcome,
            MergeOutcome::Merged {
                head: main.id.clone(),
                target: feature.id.clone(),
                merge_base: Some(base_commit.id.clone()),
                staged: vec![],
                conflicted: vec!["app.db".to_string()],
            }
        );
        let index = repo.read_index().unwrap();
        assert!(index.has_conflicts());
        assert_eq!(index.conflicted_paths(), vec!["app.db".to_string()]);
        let stages: Vec<_> = index.entries.iter().map(|entry| entry.stage).collect();
        assert_eq!(
            stages,
            vec![
                index::IndexStage::Base,
                index::IndexStage::Ours,
                index::IndexStage::Theirs,
            ]
        );
        assert!(matches!(
            repo.commit_staged("merge feature"),
            Err(RepoErr::UnresolvedConflicts)
        ));

        repo.stage_file(&app, VolumeId::random(), &theirs).unwrap();
        assert!(!repo.read_index().unwrap().has_conflicts());
        let merge_commit = repo.commit_staged("merge feature").unwrap();
        assert_eq!(merge_commit.parent, Some(main.id.clone()));
        assert_eq!(
            merge_commit.parents,
            vec![main.id.clone(), feature.id.clone()]
        );
        assert_eq!(repo.resolve_revision("HEAD^").unwrap(), main.id);
        assert_eq!(repo.resolve_revision("HEAD^1").unwrap(), main.id);
        assert_eq!(repo.resolve_revision("HEAD^2").unwrap(), feature.id);
        assert_eq!(repo.resolve_revision("HEAD^0").unwrap(), merge_commit.id);
        let log_ids: Vec<_> = repo
            .log()
            .unwrap()
            .into_iter()
            .map(|commit| commit.id)
            .collect();
        assert!(log_ids.contains(&merge_commit.id));
        assert!(log_ids.contains(&main.id));
        assert!(log_ids.contains(&feature.id));
        assert!(log_ids.contains(&base_commit.id));
        let object::Object::Commit(commit_object) = repo.read_object(&merge_commit.id).unwrap()
        else {
            panic!("merge commit id should point at a commit object");
        };
        assert_eq!(commit_object.parents.len(), 2);
        assert!(repo.merge_head().unwrap().is_none());
    }

    #[test]
    fn merge_abort_restores_orig_head_and_clears_index() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let app = tmp.path().join("app.db");
        let base = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let ours = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(3), PageCount::new(4));
        let theirs = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(5));

        repo.commit_file(&app, "base", volume.clone(), &base)
            .unwrap();
        repo.switch_new_branch("feature/search", None).unwrap();
        repo.commit_file(&app, "feature", volume.clone(), &theirs)
            .unwrap();
        repo.switch_branch("main").unwrap();
        let main = repo.commit_file(&app, "main", volume, &ours).unwrap();

        repo.merge_revision("feature/search").unwrap();
        assert!(repo.read_index().unwrap().has_conflicts());

        let restored = repo.merge_abort().unwrap();

        assert_eq!(restored, main.id);
        assert_eq!(repo.status().unwrap().head_target, Some(restored));
        assert!(repo.read_index().unwrap().is_empty());
        assert!(repo.merge_head().unwrap().is_none());
    }

    #[test]
    fn merge_abort_plan_freezes_orig_head_before_merge_state_moves() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let app = tmp.path().join("app.db");
        let base = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let ours = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(3), PageCount::new(4));
        let theirs = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(5));

        let base_commit = repo
            .commit_file(&app, "base", volume.clone(), &base)
            .unwrap();
        repo.switch_new_branch("feature/search", None).unwrap();
        let feature = repo
            .commit_file(&app, "feature", volume.clone(), &theirs)
            .unwrap();
        repo.switch_branch("main").unwrap();
        let main = repo.commit_file(&app, "main", volume, &ours).unwrap();

        repo.merge_revision("feature/search").unwrap();
        let plan = repo.plan_merge_abort().unwrap();
        assert_eq!(plan.target, main.id);
        assert_eq!(
            plan.checkout
                .files
                .get("app.db")
                .expect("planned app.db")
                .snapshot
                .to_snapshot()
                .head(),
            ours.head()
        );

        repo.write_merge_state(&feature.id, &base_commit.id)
            .unwrap();
        let restored = repo.apply_merge_abort_plan(&plan).unwrap();

        assert_eq!(restored, main.id);
        assert_eq!(repo.status().unwrap().head_target, Some(restored));
        assert!(repo.read_index().unwrap().is_empty());
        assert!(repo.merge_head().unwrap().is_none());
    }

    #[test]
    fn push_and_fetch_roundtrip_named_remote_refs() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote.clone()).unwrap();
        let first = source.commit("initial database").unwrap();
        let second = source.commit("add table").unwrap();

        let push = source.push("origin", "main").unwrap();
        assert_eq!(push.head, second.id);
        assert_eq!(push.commits, 2);
        assert_eq!(
            source.remote_tracking_ref("origin", "main").unwrap(),
            Some(second.id.clone())
        );
        assert_eq!(
            fs::read_to_string(remote_dir.path().join("HEAD")).unwrap(),
            "ref: refs/heads/main\n"
        );
        assert_eq!(
            source.remote_default_branch("origin").unwrap().as_deref(),
            Some("main")
        );
        let second_oid = object::ObjectId::from_str(&second.id).unwrap();
        assert!(
            remote_dir
                .path()
                .join(object::LooseObjectStore::relative_path(&second_oid))
                .is_file()
        );
        assert!(!remote_dir.path().join("objects/commits").exists());

        let clone_dir = tempfile::tempdir().unwrap();
        let clone = Repository::init(clone_dir.path()).unwrap();
        clone.remote_add("origin", remote).unwrap();

        let fetch = clone.fetch("origin", "main").unwrap();
        assert_eq!(fetch.head, second.id);
        assert_eq!(fetch.commits, 2);
        assert_eq!(
            clone.remote_tracking_ref("origin", "main").unwrap(),
            Some(second.id.clone())
        );
        assert_eq!(
            clone.read_commit(&first.id).unwrap().message,
            "initial database"
        );
        assert_eq!(
            clone.read_commit(&second.id).unwrap().parent,
            Some(first.id)
        );
        let object::Object::Commit(commit_object) = clone.read_object(&second.id).unwrap() else {
            panic!("fetch should hydrate canonical commit object");
        };
        let object::Object::Tree(_) = clone.read_object(commit_object.tree.as_str()).unwrap()
        else {
            panic!("fetch should hydrate canonical tree object");
        };
        assert!(!clone.graft_dir().join("objects/commits").exists());
    }

    #[test]
    fn force_push_overwrites_non_fast_forward_remote_ref() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote.clone()).unwrap();
        let base = source.commit("base").unwrap();
        source.push("origin", "main").unwrap();

        let other_dir = tempfile::tempdir().unwrap();
        let other = Repository::init(other_dir.path()).unwrap();
        other.remote_add("origin", remote).unwrap();
        other.fetch("origin", "main").unwrap();
        other.switch_branch("main").unwrap();
        other.reset(&base.id, ResetMode::Hard).unwrap();
        let remote_tip = other.commit("remote work").unwrap();
        other.push("origin", "main").unwrap();

        let local_tip = source.commit("local rewrite").unwrap();
        assert!(matches!(
            source.push("origin", "main"),
            Err(RepoErr::NonFastForward {
                remote,
                local_branch,
                remote_branch,
            }) if remote == "origin" && local_branch == "main" && remote_branch == "main"
        ));

        let push = source
            .push_branch_with_force("origin", "main", "main", true)
            .unwrap();

        assert!(push.forced);
        assert_eq!(push.head, local_tip.id);
        assert_eq!(
            source.remote_tracking_ref("origin", "main").unwrap(),
            Some(local_tip.id.clone())
        );
        assert_eq!(
            fs::read_to_string(remote_dir.path().join("refs/heads/main"))
                .unwrap()
                .trim(),
            local_tip.id
        );
        assert_ne!(remote_tip.id, local_tip.id);
    }

    #[test]
    fn push_rejects_when_remote_ref_lock_is_held() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote).unwrap();
        source.commit("initial database").unwrap();

        let lock_path = remote_dir.path().join("locks/refs/heads/main.lock");
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        fs::write(&lock_path, "held\n").unwrap();

        let err = source.push("origin", "main").unwrap_err();

        assert!(matches!(
            err,
            RepoErr::RemoteRefChanged { remote, branch }
                if remote == "origin" && branch == "main"
        ));
        assert_eq!(source.remote_tracking_ref("origin", "main").unwrap(), None);
        assert!(!remote_dir.path().join("refs/heads/main").exists());
    }

    #[test]
    fn push_delete_refspec_deletes_remote_branch_and_tracking_ref() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote).unwrap();
        let main = source.commit("initial database").unwrap();
        source.push("origin", "main").unwrap();
        assert_eq!(
            source.remote_tracking_ref("origin", "main").unwrap(),
            Some(main.id.clone())
        );
        assert!(remote_dir.path().join("refs/heads/main").is_file());

        let deleted = source
            .push_refspec_with_force("origin", ":main", false)
            .unwrap();
        assert_eq!(deleted.branches.len(), 1);
        let outcome = &deleted.branches[0];
        assert!(outcome.deleted);
        assert_eq!(outcome.remote_branch, "main");
        assert_eq!(outcome.head, main.id);
        assert_eq!(outcome.commits, 0);
        assert_eq!(source.remote_tracking_ref("origin", "main").unwrap(), None);
        assert!(!remote_dir.path().join("refs/heads/main").exists());
        assert!(matches!(
            source.push_refspec_with_force("origin", ":main", false),
            Err(RepoErr::RemoteBranchNotFound { remote, branch })
                if remote == "origin" && branch == "main"
        ));
    }

    #[test]
    fn push_all_and_fetch_all_sync_default_branch_refspecs() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote.clone()).unwrap();
        let base = source.commit("base").unwrap();
        source.switch_new_branch("feature/search", None).unwrap();
        let feature = source.commit("feature").unwrap();
        source.switch_branch("main").unwrap();
        let main = source.commit("main").unwrap();

        let push = source.push_all("origin").unwrap();
        assert_eq!(
            push.branches
                .iter()
                .map(|outcome| outcome.remote_branch.as_str())
                .collect::<Vec<_>>(),
            vec!["feature/search", "main"]
        );
        assert_eq!(
            source
                .remote_branch_refs("origin")
                .unwrap()
                .iter()
                .map(|reference| (reference.branch.as_str(), reference.head.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("feature/search", feature.id.as_str()),
                ("main", main.id.as_str())
            ]
        );

        let clone_dir = tempfile::tempdir().unwrap();
        let clone = Repository::init(clone_dir.path()).unwrap();
        clone.remote_add("origin", remote).unwrap();
        let fetch = clone.fetch_all("origin").unwrap();

        assert_eq!(
            fetch
                .branches
                .iter()
                .map(|outcome| outcome.branch.as_str())
                .collect::<Vec<_>>(),
            vec!["feature/search", "main"]
        );
        assert_eq!(
            clone
                .remote_tracking_ref("origin", "feature/search")
                .unwrap(),
            Some(feature.id.clone())
        );
        assert_eq!(
            clone.remote_tracking_ref("origin", "main").unwrap(),
            Some(main.id.clone())
        );
        assert_eq!(
            clone
                .remote_tracking_branches()
                .unwrap()
                .into_iter()
                .map(|reference| (reference.remote, reference.branch, reference.head))
                .collect::<Vec<_>>(),
            vec![
                (
                    "origin".to_string(),
                    "feature/search".to_string(),
                    feature.id.clone()
                ),
                ("origin".to_string(), "main".to_string(), main.id)
            ]
        );
        assert_eq!(clone.read_commit(&base.id).unwrap().message, "base");
        assert_eq!(
            clone.read_commit(&feature.id).unwrap().parent,
            Some(base.id)
        );
    }

    #[test]
    fn explicit_refspecs_map_push_and_fetch_branch_names() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote.clone()).unwrap();
        source.commit("base").unwrap();
        source.switch_new_branch("feature/search", None).unwrap();
        let feature = source.commit("feature").unwrap();

        let push = source
            .push_refspec_with_force(
                "origin",
                "refs/heads/feature/search:refs/heads/review/search",
                false,
            )
            .unwrap();

        assert_eq!(push.branches.len(), 1);
        assert_eq!(push.branches[0].local_branch, "feature/search");
        assert_eq!(push.branches[0].remote_branch, "review/search");
        assert_eq!(
            fs::read_to_string(remote_dir.path().join("refs/heads/review/search"))
                .unwrap()
                .trim(),
            feature.id
        );

        let clone_dir = tempfile::tempdir().unwrap();
        let clone = Repository::init(clone_dir.path()).unwrap();
        clone.remote_add("origin", remote).unwrap();
        let fetch = clone
            .fetch_refspec(
                "origin",
                "refs/heads/review/search:refs/remotes/origin/local/search",
            )
            .unwrap();

        assert_eq!(fetch.branches.len(), 1);
        assert_eq!(fetch.branches[0].branch, "local/search");
        assert_eq!(
            clone.remote_tracking_ref("origin", "local/search").unwrap(),
            Some(feature.id)
        );
        assert_eq!(
            clone
                .remote_tracking_ref("origin", "review/search")
                .unwrap(),
            None
        );
    }

    #[test]
    fn remote_prune_deletes_stale_tracking_refs() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote.clone()).unwrap();
        source.commit("base").unwrap();
        source.switch_new_branch("feature/prune", None).unwrap();
        let feature = source.commit("feature").unwrap();
        source.switch_branch("main").unwrap();
        let main = source.commit("main").unwrap();
        source.push_all("origin").unwrap();

        let clone_dir = tempfile::tempdir().unwrap();
        let clone = Repository::init(clone_dir.path()).unwrap();
        clone.remote_add("origin", remote).unwrap();
        clone.fetch_all("origin").unwrap();
        assert_eq!(
            clone
                .remote_tracking_ref("origin", "feature/prune")
                .unwrap(),
            Some(feature.id.clone())
        );
        assert_eq!(
            clone.remote_tracking_ref("origin", "main").unwrap(),
            Some(main.id.clone())
        );

        let deleted = source
            .push_refspec_with_force("origin", ":feature/prune", false)
            .unwrap();
        assert_eq!(deleted.branches[0].remote_branch, "feature/prune");
        assert!(deleted.branches[0].deleted);
        assert_eq!(
            clone
                .remote_tracking_ref("origin", "feature/prune")
                .unwrap(),
            Some(feature.id)
        );

        let pruned = clone.remote_prune("origin").unwrap();
        assert_eq!(pruned.remote, "origin");
        assert_eq!(pruned.branches, vec!["feature/prune"]);
        assert_eq!(
            clone
                .remote_tracking_ref("origin", "feature/prune")
                .unwrap(),
            None
        );
        assert_eq!(
            clone.remote_tracking_ref("origin", "main").unwrap(),
            Some(main.id)
        );

        let pruned_again = clone.remote_prune("origin").unwrap();
        assert_eq!(pruned_again.branches, Vec::<String>::new());
    }

    #[test]
    fn wildcard_refspecs_map_branch_captures() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote.clone()).unwrap();
        source.commit("base").unwrap();
        source.switch_new_branch("feature/search", None).unwrap();
        let search = source.commit("search").unwrap();
        source
            .switch_new_branch("feature/reporting", Some("main"))
            .unwrap();
        let reporting = source.commit("reporting").unwrap();
        source.switch_branch("main").unwrap();
        let main = source.commit("main").unwrap();

        let push = source
            .push_refspec_with_force("origin", "refs/heads/feature/*:refs/heads/review/*", false)
            .unwrap();

        assert_eq!(
            push.branches
                .iter()
                .map(|outcome| outcome.remote_branch.as_str())
                .collect::<Vec<_>>(),
            vec!["review/reporting", "review/search"]
        );
        assert!(!remote_dir.path().join("refs/heads/main").exists());

        let clone_dir = tempfile::tempdir().unwrap();
        let clone = Repository::init(clone_dir.path()).unwrap();
        clone.remote_add("origin", remote).unwrap();
        let fetch = clone
            .fetch_refspec(
                "origin",
                "refs/heads/review/*:refs/remotes/origin/reviewed/*",
            )
            .unwrap();

        assert_eq!(
            fetch
                .branches
                .iter()
                .map(|outcome| outcome.branch.as_str())
                .collect::<Vec<_>>(),
            vec!["reviewed/reporting", "reviewed/search"]
        );
        assert_eq!(
            clone
                .remote_tracking_ref("origin", "reviewed/search")
                .unwrap(),
            Some(search.id)
        );
        assert_eq!(
            clone
                .remote_tracking_ref("origin", "reviewed/reporting")
                .unwrap(),
            Some(reporting.id)
        );
        assert_eq!(clone.remote_tracking_ref("origin", "main").unwrap(), None);
        assert_eq!(source.branch_target("main").unwrap(), Some(main.id));
    }

    #[test]
    fn pull_merges_diverged_remote_branch_and_push_sees_second_parent() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };
        let volume = VolumeId::random();
        let log = LogId::random();

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote.clone()).unwrap();
        let source_app = source_dir.path().join("app.db");
        let source_notes = source_dir.path().join("notes.db");
        let app_base = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let notes_base = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(3), PageCount::new(4));
        let notes_remote = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(4), PageCount::new(5));
        let app_local = Snapshot::new(log, LSN::FIRST..=LSN::new(5), PageCount::new(6));

        source
            .stage_file(&source_app, volume.clone(), &app_base)
            .unwrap();
        source
            .stage_file(&source_notes, volume.clone(), &notes_base)
            .unwrap();
        let base = source.commit_staged("base").unwrap();
        source.push("origin", "main").unwrap();

        let clone_dir = tempfile::tempdir().unwrap();
        let clone = Repository::init(clone_dir.path()).unwrap();
        clone.remote_add("origin", remote).unwrap();
        let initial_pull = clone.pull("origin", "main", "main").unwrap();
        assert_eq!(
            initial_pull.merge,
            MergeOutcome::FastForward { from: None, to: base.id.clone() }
        );
        assert_eq!(clone.head_target().unwrap(), Some(base.id.clone()));

        let remote_commit = source
            .commit_file(&source_notes, "remote notes", volume.clone(), &notes_remote)
            .unwrap();
        source.push("origin", "main").unwrap();

        let local_commit = clone
            .commit_file(
                clone_dir.path().join("app.db"),
                "local app",
                volume,
                &app_local,
            )
            .unwrap();

        let pull = clone.pull("origin", "main", "main").unwrap();

        assert_eq!(
            pull.merge,
            MergeOutcome::Merged {
                head: local_commit.id.clone(),
                target: remote_commit.id.clone(),
                merge_base: Some(base.id),
                staged: vec!["notes.db".to_string()],
                conflicted: vec![],
            }
        );
        assert_eq!(clone.head_target().unwrap(), Some(local_commit.id.clone()));
        assert_eq!(clone.merge_head().unwrap(), Some(remote_commit.id.clone()));
        assert_eq!(
            clone.read_index().unwrap().staged_paths(),
            vec!["notes.db".to_string()]
        );

        let merge_commit = clone.commit_staged("merge origin/main").unwrap();
        assert_eq!(
            merge_commit.parents,
            vec![local_commit.id.clone(), remote_commit.id.clone()]
        );
        assert!(
            clone
                .is_ancestor(&local_commit.id, &merge_commit.id)
                .unwrap()
        );
        assert!(
            clone
                .is_ancestor(&remote_commit.id, &merge_commit.id)
                .unwrap()
        );

        let push = clone.push("origin", "main").unwrap();
        assert_eq!(push.head, merge_commit.id);
        assert_eq!(push.commits, 2);
    }

    #[test]
    fn pull_plan_freezes_fetched_target_before_tracking_ref_moves() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };
        let volume = VolumeId::random();
        let log = LogId::random();

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote.clone()).unwrap();
        let source_app = source_dir.path().join("app.db");
        let base_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let next_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(4));
        let base = source
            .commit_file(&source_app, "base", volume.clone(), &base_snapshot)
            .unwrap();
        source.push("origin", "main").unwrap();

        let clone_dir = tempfile::tempdir().unwrap();
        let clone = Repository::init(clone_dir.path()).unwrap();
        clone.remote_add("origin", remote).unwrap();
        let plan = clone.plan_pull("origin", "main", "main").unwrap();
        assert_eq!(plan.merge.checkout.target, Some(base.id.clone()));

        let next = source
            .commit_file(&source_app, "next", volume, &next_snapshot)
            .unwrap();
        source.push("origin", "main").unwrap();
        clone.fetch("origin", "main").unwrap();
        assert_eq!(
            clone.remote_tracking_ref("origin", "main").unwrap(),
            Some(next.id)
        );

        let outcome = clone.apply_pull_plan(&plan).unwrap();
        assert_eq!(
            outcome.merge,
            MergeOutcome::FastForward { from: None, to: base.id.clone() }
        );
        assert_eq!(clone.branch_target("main").unwrap(), Some(base.id.clone()));
        assert_eq!(
            clone
                .read_commit(&base.id)
                .unwrap()
                .files
                .get("app.db")
                .expect("planned app.db")
                .snapshot
                .to_snapshot()
                .head(),
            base_snapshot.head()
        );
    }

    #[test]
    fn discover_from_nested_path() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let nested = tmp.path().join("a/b/c");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(Repository::discover(&nested).unwrap(), repo);
        assert_eq!(
            Repository::discover_for_file(nested.join("app.db")).unwrap(),
            repo
        );
    }

    fn write_sqlite_magic(path: impl AsRef<Path>) {
        fs::write(path, SQLITE_DATABASE_MAGIC).unwrap();
    }
}
