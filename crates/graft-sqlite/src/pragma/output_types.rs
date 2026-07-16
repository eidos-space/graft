use super::*;

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonBranchList {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) branches: Vec<BranchInfo>,
    pub(super) remote_branches: Vec<RemoteBranchRef>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonFetchCommandOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) remote: String,
    pub(super) branches: Vec<FetchOutcome>,
    pub(super) commits: usize,
}

#[derive(Debug, Clone)]
pub(super) enum FetchCommandOutcome {
    One(FetchOutcome),
    Many(FetchAllOutcome),
}

impl FetchCommandOutcome {
    pub(super) fn remote(&self) -> String {
        match self {
            Self::One(outcome) => outcome.remote.clone(),
            Self::Many(outcome) => outcome.remote.clone(),
        }
    }

    pub(super) fn branches(&self) -> Vec<FetchOutcome> {
        match self {
            Self::One(outcome) => vec![outcome.clone()],
            Self::Many(outcome) => outcome.branches.clone(),
        }
    }

    pub(super) fn commits(&self) -> usize {
        self.branches().iter().map(|branch| branch.commits).sum()
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonPushCommandOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) remote: String,
    pub(super) branches: Vec<PushOutcome>,
    pub(super) commits: usize,
    pub(super) forced: bool,
}

#[derive(Debug, Clone)]
pub(super) enum PushCommandOutcome {
    One(PushOutcome),
    Many(PushAllOutcome),
}

impl PushCommandOutcome {
    pub(super) fn remote(&self) -> String {
        match self {
            Self::One(outcome) => outcome.remote.clone(),
            Self::Many(outcome) => outcome.remote.clone(),
        }
    }

    pub(super) fn branches(&self) -> Vec<PushOutcome> {
        match self {
            Self::One(outcome) => vec![outcome.clone()],
            Self::Many(outcome) => outcome.branches.clone(),
        }
    }

    pub(super) fn commits(&self) -> usize {
        self.branches().iter().map(|branch| branch.commits).sum()
    }

    pub(super) fn forced(&self) -> bool {
        self.branches().iter().any(|branch| branch.forced)
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonPullCommandOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    pub(super) current_branch: Option<String>,
    #[serde(flatten)]
    pub(super) outcome: PullOutcome,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) paths: Vec<JsonPathAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) conflict_analysis: Option<JsonRowMergeAnalysis>,
}

#[derive(Debug, Clone)]
pub(super) struct RepoPullCommandOutcome {
    pub(super) outcome: PullOutcome,
    pub(super) current_head: Option<String>,
    pub(super) current_branch: Option<String>,
    pub(super) paths: Vec<JsonPathAction>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonMergeCommandOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) head: Option<String>,
    pub(super) branch: Option<String>,
    #[serde(flatten)]
    pub(super) outcome: MergeOutcome,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) paths: Vec<JsonPathAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) conflict_analysis: Option<JsonRowMergeAnalysis>,
}

#[derive(Debug)]
pub(super) struct RepoMergeCommandOutcome {
    pub(super) outcome: MergeOutcome,
    pub(super) branch: Option<String>,
    pub(super) paths: Vec<JsonPathAction>,
    pub(super) row_auto_merge: Option<RowAutoMergeResult>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonMergeAbortCommandOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) head: String,
    pub(super) branch: Option<String>,
    pub(super) target: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) paths: Vec<JsonPathAction>,
}

#[derive(Debug)]
pub(super) struct RepoMergeAbortCommandOutcome {
    pub(super) target: String,
    pub(super) branch: Option<String>,
    pub(super) paths: Vec<JsonPathAction>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonMergeContinueCommandOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) head: String,
    pub(super) branch: Option<String>,
    pub(super) commit: JsonCommitSummary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) paths: Vec<crate::json::JsonRepoPathDiff>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) materialized: Vec<JsonPathAction>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonCommitOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) head: String,
    pub(super) branch: Option<String>,
    pub(super) commit: JsonCommitSummary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) paths: Vec<crate::json::JsonRepoPathDiff>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) materialized: Vec<JsonPathAction>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonCommitSummary {
    pub(super) id: String,
    pub(super) message: String,
    pub(super) parents: Vec<String>,
}

#[derive(Debug, Clone)]
pub(super) struct RepoCommitOutcome {
    pub(super) commit: CommitObject,
    pub(super) branch: Option<String>,
    pub(super) materialized: Vec<JsonPathAction>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonRepoStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) kind: Option<&'static str>,
    #[serde(flatten)]
    pub(super) status: RepoStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) conflict_analysis: Option<JsonRowMergeAnalysis>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StatusSpec {
    pub(crate) kind: Option<RepoTrackedPathKind>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonLsFilesOutcome<T> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) stage: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(super) details: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(super) others: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) kind: Option<&'static str>,
    pub(super) paths: Vec<T>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LsFilesSpec {
    pub(crate) stage: bool,
    pub(crate) details: bool,
    pub(crate) others: bool,
    pub(crate) kind: Option<RepoTrackedPathKind>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonInitOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) graft_dir: String,
    pub(super) worktree: String,
    pub(super) path: String,
    pub(super) kind: &'static str,
    pub(super) preserved_contents: bool,
}

#[derive(Debug, Clone)]
pub(super) struct RepoInitOutcome {
    pub(super) graft_dir: PathBuf,
    pub(super) worktree: PathBuf,
    pub(super) path: String,
    pub(super) preserved_contents: bool,
    pub(super) current_head: Option<String>,
    pub(super) current_branch: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonAddOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) kind: Option<&'static str>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) paths: Vec<crate::json::JsonRepoPathDiff>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) status: Option<JsonRepoStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonRemoveOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) cached: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) paths: Vec<JsonPathAction>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonSwitchOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) head: Option<String>,
    pub(super) branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) target: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) paths: Vec<JsonPathAction>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonBranchMutationOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) branch: BranchInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) old_branch: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonTagMutationOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) tag: TagInfo,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonTagListOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) tags: Vec<TagInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonRemoteInfo {
    pub(super) name: String,
    pub(super) config: RemoteConfig,
    pub(super) url: String,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonRemoteList {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) remotes: Vec<JsonRemoteInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonRemoteMutationOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) remote: JsonRemoteInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) old_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonCloneOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) remote: JsonRemoteInfo,
    pub(super) branch: String,
    pub(super) head: String,
    pub(super) commits: usize,
    pub(super) graft_dir: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) paths: Vec<JsonPathAction>,
}

#[derive(Debug)]
pub(super) struct RepoCloneOutcome {
    pub(super) remote: RemoteInfo,
    pub(super) current_head: Option<String>,
    pub(super) current_branch: Option<String>,
    pub(super) branch: String,
    pub(super) head: String,
    pub(super) commits: usize,
    pub(super) graft_dir: PathBuf,
    pub(super) paths: Vec<JsonPathAction>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonRemotePruneCommandOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    #[serde(flatten)]
    pub(super) outcome: RemotePruneOutcome,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonConfigEntryOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    #[serde(flatten)]
    pub(super) entry: RepoConfigEntry,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonConfigListOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) entries: Vec<RepoConfigEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonConfigMutationOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) entry: RepoConfigEntry,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonLsRemoteOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) remote: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) default_branch: Option<String>,
    pub(super) refs: Vec<RemoteBranchRef>,
}

#[derive(Debug)]
pub(super) struct RepoSwitchOutcome {
    pub(super) branch: String,
    pub(super) target: Option<String>,
    pub(super) paths: Vec<JsonPathAction>,
}

#[derive(Debug)]
pub(super) struct RepoSwitchCreateOutcome {
    pub(super) branch: BranchInfo,
    pub(super) paths: Vec<JsonPathAction>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonCheckoutOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) head: Option<String>,
    pub(super) branch: Option<String>,
    pub(super) target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) path_details: Vec<JsonPathDetail>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonPathDetail {
    pub(super) path: String,
    pub(super) kind: &'static str,
    pub(super) storage: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonRestoreOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) source: Option<String>,
    pub(super) staged: bool,
    pub(super) all: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) path_details: Vec<JsonPathDetail>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonExportOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) source: Option<String>,
    pub(super) path: String,
    pub(super) kind: &'static str,
    pub(super) output: String,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonResetCommandOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) head: String,
    pub(super) branch: Option<String>,
    #[serde(flatten)]
    pub(super) outcome: ResetOutcome,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) paths: Vec<JsonPathAction>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonPathAction {
    pub(super) path: String,
    pub(super) kind: &'static str,
    pub(super) storage: &'static str,
    pub(super) action: &'static str,
}

#[derive(Debug, Clone)]
pub(super) struct RepoResetCommandOutcome {
    pub(super) outcome: ResetOutcome,
    pub(super) branch: Option<String>,
    pub(super) paths: Vec<JsonPathAction>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonRowMergeAnalysis {
    pub(super) path: String,
    pub(super) available: bool,
    pub(super) can_auto_merge: bool,
    pub(super) ours_changes: usize,
    pub(super) theirs_changes: usize,
    pub(super) apply_changes: usize,
    pub(super) opaque_changes: usize,
    pub(super) resolved_opaque_changes: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) resolved_opaque_change_details: Vec<JsonResolvedOpaqueChange>,
    pub(super) apply_policy: JsonRowMergeApplyPolicy,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) limitations: Vec<crate::json::JsonDiffLimitation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) blocked_reasons: Vec<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) row_conflicts: Vec<JsonRowMergeConflict>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) schema_conflicts: Vec<JsonSchemaMergeConflict>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonResolvedOpaqueChange {
    pub(super) name: String,
    pub(super) reason: &'static str,
    pub(super) resolver: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonRowMergeApplyPolicy {
    pub(super) foreign_keys: &'static str,
    pub(super) triggers: &'static str,
    pub(super) validation: Vec<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) default_semantic_keys: Vec<String>,
    pub(super) internal_resolvers: Vec<JsonRowMergeInternalResolver>,
    pub(super) schema_resolvers: Vec<JsonRowMergeSchemaResolver>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) generated_columns: Vec<JsonRowMergeGeneratedColumns>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonRowMergeInternalResolver {
    pub(super) table: String,
    pub(super) resolver: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonRowMergeSchemaResolver {
    pub(super) operation: String,
    pub(super) resolver: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonRowMergeGeneratedColumns {
    pub(super) table: String,
    pub(super) columns: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonRowMergeConflict {
    pub(super) reason: &'static str,
    pub(super) table: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) columns: Vec<String>,
    pub(super) rowid: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) ours_rowid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) theirs_rowid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) semantic_key: Option<Vec<String>>,
    pub(super) ours: &'static str,
    pub(super) theirs: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) base_row: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) ours_row: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) theirs_row: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonSchemaMergeConflict {
    pub(super) reason: &'static str,
    pub(super) name: String,
    pub(super) entry_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) ours: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) theirs: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) column_changes: Vec<JsonSchemaColumnChange>,
    pub(super) message: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonSchemaColumnChange {
    pub(super) side: &'static str,
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) to: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonConflictList {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) merge_head: Option<String>,
    pub(super) paths: Vec<JsonConflictPath>,
    pub(super) conflicts: Vec<JsonConflictArtifact>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonConflictPath {
    pub(super) path: String,
    pub(super) kind: &'static str,
    pub(super) storage: &'static str,
    pub(super) status: &'static str,
    pub(super) total: usize,
    pub(super) unresolved: usize,
    pub(super) resolved: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonConflictArtifact {
    pub(super) id: String,
    pub(super) path: String,
    pub(super) path_kind: &'static str,
    pub(super) storage: &'static str,
    pub(super) kind: &'static str,
    pub(super) reason: &'static str,
    pub(super) status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) resolution: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) table: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) columns: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) rowid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) ours_rowid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) theirs_rowid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) semantic_key: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) entry_type: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) column_changes: Vec<JsonSchemaColumnChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) change: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) ours_op: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) theirs_op: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) base_row: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) ours_row: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) theirs_row: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonResolveConflictOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) path: String,
    pub(super) path_kind: &'static str,
    pub(super) storage: &'static str,
    pub(super) resolution: &'static str,
    pub(super) remaining_conflicts: usize,
}

#[derive(Debug, Clone)]
pub(super) struct RepoResolveConflictOutcome {
    pub(super) path: String,
    pub(super) path_kind: RepoTrackedPathKind,
    pub(super) path_storage: RepoPathStorage,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonVolumeListEntry {
    pub(super) id: String,
    pub(super) local: String,
    pub(super) remote: String,
    pub(super) status: String,
    pub(super) current: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonVolumeAudit {
    pub(super) local_pages: usize,
    pub(super) total_pages: usize,
    pub(super) percentage: f64,
    pub(super) needs_hydrate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) checksum: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonRepoArtifactAudit {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    #[serde(flatten)]
    pub(super) audit: RepoArtifactAudit,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonRepoArtifactRepair {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    #[serde(flatten)]
    pub(super) outcome: RepoArtifactRepairOutcome,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonLargeFilePruneOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    #[serde(flatten)]
    pub(super) outcome: RepoLargeFilePruneOutcome,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonStorageGcOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    #[serde(flatten)]
    pub(super) outcome: graft::local::fjall_storage::StorageGcOutcome,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonLargeFileFetchOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    #[serde(flatten)]
    pub(super) outcome: RepoLargeFileFetchOutcome,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonLargeFileStatusOutcome {
    pub(super) operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    #[serde(flatten)]
    pub(super) outcome: RepoLargeFileStatusOutcome,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonRepoDiffOutcome<T> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) kind: Option<&'static str>,
    #[serde(flatten)]
    pub(super) diff: T,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) content: Option<RepoTextContentDiff>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonRepoShowOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    #[serde(flatten)]
    pub(super) commit: CommitObject,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct JsonRepoLogOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_branch: Option<String>,
    pub(super) commits: Vec<CommitObject>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) next_cursor: Option<String>,
    pub(super) has_more: bool,
}
