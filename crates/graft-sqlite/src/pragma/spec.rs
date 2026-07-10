use super::*;

/// Diff granularity mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    /// Default: page-level + table-level
    Default,
    /// Row-level: detailed comparison of each row
    Rows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonLogMode {
    LegacyArray,
    WithStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonConfigListMode {
    LegacyArray,
    WithStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonTagsMode {
    LegacyArray,
    WithStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonFetchAsyncMode {
    LegacyId,
    WithStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoDiffSpec {
    pub(super) mode: DiffMode,
    pub(super) kind: Option<RepoTrackedPathKind>,
    pub(super) target: RepoDiffTarget,
    pub(super) content: Option<RepoTextContentSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RepoTextContentSpec {
    pub(super) max_bytes: ByteUnit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RepoDiffTarget {
    Worktree {
        path: Option<String>,
    },
    Staged {
        path: Option<String>,
    },
    RevisionToWorktree {
        rev: String,
        path: Option<String>,
    },
    Revisions {
        from: String,
        to: String,
        path: Option<String>,
    },
    Root {
        to: String,
        path: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoAddSpec {
    pub(super) path: Option<PathBuf>,
    pub(super) force: bool,
    pub(super) all: bool,
    pub(super) kind: Option<RepoTrackedPathKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoRemoveSpec {
    pub(super) path: Option<PathBuf>,
    pub(super) cached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoAuditSpec {
    pub(super) repair: bool,
    pub(super) remote: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LargeFilePruneSpec {
    pub(super) dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LargeFileFetchSpec {
    pub(super) remote: Option<String>,
    pub(super) rev: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LargeFileStatusSpec {
    pub(super) rev: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RepoCheckoutSpec {
    Detach { rev: String, force: bool },
    Path { rev: String, path: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoRestoreSpec {
    pub(super) source: Option<String>,
    pub(super) expected_head: Option<String>,
    pub(super) require_clean: bool,
    pub(super) staged: bool,
    pub(super) all: bool,
    pub(super) kind: Option<RepoTrackedPathKind>,
    pub(super) path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoExportSpec {
    pub(super) source: Option<String>,
    pub(super) path: Option<PathBuf>,
    pub(super) output: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoCloneSpec {
    pub(super) config: RemoteConfig,
    pub(super) branch: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolveSide {
    Ours,
    Theirs,
    Manual,
}

impl ResolveSide {
    pub(super) fn index_stage(self) -> Option<graft::repo::index::IndexStage> {
        match self {
            Self::Ours => Some(graft::repo::index::IndexStage::Ours),
            Self::Theirs => Some(graft::repo::index::IndexStage::Theirs),
            Self::Manual => None,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Ours => "ours",
            Self::Theirs => "theirs",
            Self::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoResolveSpec {
    pub(super) side: ResolveSide,
    pub(super) path: Option<PathBuf>,
    pub(super) row: Option<RepoResolveRowSpec>,
}

pub(super) enum RepoConflictSideState {
    SqliteDatabase(CommitFileState),
    Artifact(CommitArtifactState),
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoResolveRowSpec {
    pub(super) table: String,
    pub(super) rowid: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct RowConflictResolutionState {
    pub(super) merge_head: Option<String>,
    pub(super) rows: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BranchListMode {
    Local,
    Remote,
    All,
}

impl BranchListMode {
    pub(super) fn includes_remote(self) -> bool {
        matches!(self, Self::Remote | Self::All)
    }
}
