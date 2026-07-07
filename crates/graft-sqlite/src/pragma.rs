use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{Display, Write},
    fs::File,
    io::{Read, Write as IoWrite},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use graft::core::{
    LogId, PageIdx, VolumeId,
    logref::LogRef,
    lsn::{LSN, LSNRangeExt},
    page::{PAGESIZE, Page},
};
use graft::remote::{Remote, RemoteConfig};
use graft::repo::{
    BranchInfo, BranchUpstream, CheckoutPlan, CommitArtifactState, CommitFileState, CommitObject,
    CommitTableSummary, FetchAllOutcome, FetchOutcome, Head, MergeOutcome, MergePlan, PullOutcome,
    PushAllOutcome, PushOutcome, RemoteBranchRef, RemoteInfo, RemotePruneOutcome,
    RepoArtifactAudit, RepoArtifactAuditIssueKind, RepoArtifactRepairOutcome, RepoConfigEntry,
    RepoDiff, RepoFileChange, RepoLargeFileFetchOutcome, RepoLargeFileFetchStatus,
    RepoLargeFilePruneOutcome, RepoLargeFileStatusOutcome, RepoLargeFileStatusState, RepoLogRange,
    RepoPathStorage, RepoSnapshot, RepoStatus, RepoStorageCommit, RepoTrackedPath,
    RepoTrackedPathDetail, RepoTrackedPathEntry, RepoTrackedPathKind, RepoWorktreeChangeKind,
    Repository, ResetMode, ResetOutcome, TagInfo,
};
use graft::{
    rt::runtime::Runtime, volume::AheadStatus, volume_reader::VolumeRead,
    volume_writer::VolumeWrite,
};
use indoc::{formatdoc, indoc, writedoc};
use parking_lot::Mutex;
use rusqlite::config::DbConfig;
use serde::{Deserialize, Serialize};
use sqlite_plugin::{
    vars::SQLITE_ERROR,
    vfs::{Pragma, PragmaErr},
};
use tryiter::TryIteratorExt;
use zerocopy::FromBytes;

use crate::{
    dbg::SqliteHeader,
    file::vol_file::VolFile,
    vfs::{ErrCtx, should_discover_repo},
};

macro_rules! pluralize {
    ($n:expr, $s:literal) => {
        if $n == 1 { $s } else { concat!($s, "s") }
    };
}

macro_rules! pragma_err {
    ($msg:expr) => {
        Err(ErrCtx::PragmaErr($msg.into()))
    };
}

mod jobs;
mod json;
mod output_types;
mod parse;
mod repo_checkout;
mod repo_conflicts;
mod repo_core;
mod repo_diff;
mod repo_history;
mod repo_merge;
mod repo_output;
mod repo_paths;
mod repo_refs;
mod repo_remote_output;
mod repo_snapshot;
mod repo_staging;
mod repo_switch;
mod repo_sync;
mod row_diff;
mod row_merge_output;
mod spec;
mod volume_output;

use self::{
    jobs::*, json::*, output_types::*, parse::*, repo_checkout::*, repo_conflicts::*, repo_core::*,
    repo_diff::*, repo_history::*, repo_merge::*, repo_output::*, repo_paths::*, repo_refs::*,
    repo_remote_output::*, repo_snapshot::*, repo_staging::*, repo_switch::*, repo_sync::*,
    row_diff::*, row_merge_output::*, spec::*, volume_output::*,
};

const SQLITE_DATABASE_MAGIC: &[u8; 16] = b"SQLite format 3\0";
static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);

/// Helper to create pragma errors concisely
fn pragma_fail(msg: impl Display) -> PragmaErr {
    PragmaErr::Fail(SQLITE_ERROR, Some(msg.to_string()))
}

/// Helper to parse with automatic error conversion
fn parse_or_fail<T>(s: &str) -> Result<T, PragmaErr>
where
    T: FromStr,
    T::Err: Display,
{
    s.parse().map_err(pragma_fail)
}

/// Helper to parse an optional value from colon-separated parts
fn parse_optional<T: FromStr>(s: Option<&&str>) -> Result<Option<T>, PragmaErr>
where
    T::Err: Display,
{
    s.map(|s| parse_or_fail(s)).transpose()
}

/// Extension trait for Pragma to get required arguments
trait PragmaExt<'a> {
    fn require_arg(&self) -> Result<&'a str, PragmaErr>;
}

impl<'a> PragmaExt<'a> for Pragma<'a> {
    fn require_arg(&self) -> Result<&'a str, PragmaErr> {
        self.arg.ok_or_else(|| PragmaErr::required_arg(self))
    }
}

pub(crate) enum GraftPragma {
    /// `pragma graft_debug_volume_list;`
    VolumeList,

    /// `pragma graft_debug_volume_json_list;`
    VolumeJsonList,

    /// `pragma graft_tags;`
    Tags,

    /// `pragma graft_json_tags [= "--with-status"];`
    JsonTags { mode: JsonTagsMode },

    /// `pragma graft_debug_volume_tags;`
    VolumeTags,

    /// `pragma graft_debug_volume_switch = "local_vid[:local[:remote]]";`
    VolumeSwitch {
        vid: VolumeId,
        local: Option<LogId>,
        remote: Option<LogId>,
    },

    /// `pragma graft_debug_volume_clone [= "remote"];`
    VolumeClone { remote: Option<LogId> },

    /// `pragma graft_debug_volume_fork;`
    VolumeFork,

    /// `pragma graft_checkout = "[--force] rev [-- path]";`
    RepoCheckout { spec: RepoCheckoutSpec },

    /// `pragma graft_json_checkout = "[--force] rev [-- path]";`
    JsonRepoCheckout { spec: RepoCheckoutSpec },

    /// `pragma graft_restore = "[--source rev] path|--staged --all [--kind kind]";`
    Restore { spec: RepoRestoreSpec },

    /// `pragma graft_json_restore = "[--source rev] path|--staged --all [--kind kind]";`
    JsonRestore { spec: RepoRestoreSpec },

    /// `pragma graft_export = "[--source rev] --output output.db [-- path]";`
    Export { spec: RepoExportSpec },

    /// `pragma graft_json_export = "[--source rev] --output output.db [-- path]";`
    JsonExport { spec: RepoExportSpec },

    /// `pragma graft_debug_volume_info;`
    VolumeInfo,

    /// `pragma graft_status [= "[--kind kind]"];`
    Status { spec: StatusSpec },

    /// `pragma graft_debug_volume_status;`
    VolumeStatus,

    /// `pragma graft_init;`
    RepoInit,

    /// `pragma graft_json_init;`
    JsonRepoInit,

    /// `pragma graft_clone = "remote-uri [branch]";`
    RepoClone { spec: RepoCloneSpec },

    /// `pragma graft_json_clone = "remote-uri [branch]";`
    JsonRepoClone { spec: RepoCloneSpec },

    /// `pragma graft_json_status [= "[--kind kind]"];`
    JsonStatus { spec: StatusSpec },

    /// `pragma graft_add = "[--all|-A] [--kind kind]|[--force] [path]";`
    Add { spec: RepoAddSpec },

    /// `pragma graft_json_add = "[--all|-A] [--kind kind]|[--force] [path]";`
    JsonAdd { spec: RepoAddSpec },

    /// `pragma graft_rm = "[--cached] [path]";`
    Remove { spec: RepoRemoveSpec },

    /// `pragma graft_json_rm = "[--cached] [path]";`
    JsonRemove { spec: RepoRemoveSpec },

    /// `pragma graft_commit = "message";`
    Commit { message: String },

    /// `pragma graft_json_commit = "message";`
    JsonCommit { message: String },

    /// `pragma graft_branch [= "-r|--remote|-a|--all"];`
    Branch { mode: BranchListMode },

    /// `pragma graft_json_branch [= "-r|--remote|-a|--all"];`
    JsonBranch { mode: BranchListMode },

    /// `pragma graft_branch_create = "name [start-point]";`
    BranchCreate {
        name: String,
        start_point: Option<String>,
    },

    /// `pragma graft_json_branch_create = "name [start-point]";`
    JsonBranchCreate {
        name: String,
        start_point: Option<String>,
    },

    /// `pragma graft_branch_delete = "[--force] name";`
    BranchDelete { name: String, force: bool },

    /// `pragma graft_json_branch_delete = "[--force] name";`
    JsonBranchDelete { name: String, force: bool },

    /// `pragma graft_branch_rename = "[--force] [old] new";`
    BranchRename {
        old: Option<String>,
        new: String,
        force: bool,
    },

    /// `pragma graft_json_branch_rename = "[--force] [old] new";`
    JsonBranchRename {
        old: Option<String>,
        new: String,
        force: bool,
    },

    /// `pragma graft_branch_upstream = "[branch] remote/branch";`
    BranchUpstream {
        branch: Option<String>,
        remote: String,
        remote_branch: String,
    },

    /// `pragma graft_json_branch_upstream = "[branch] remote/branch";`
    JsonBranchUpstream {
        branch: Option<String>,
        remote: String,
        remote_branch: String,
    },

    /// `pragma graft_branch_unset_upstream [= "branch"];`
    BranchUnsetUpstream { branch: Option<String> },

    /// `pragma graft_json_branch_unset_upstream [= "branch"];`
    JsonBranchUnsetUpstream { branch: Option<String> },

    /// `pragma graft_tag_create = "name [rev]";`
    /// `pragma graft_tag_create = "--annotated name [rev] -- message";`
    TagCreate {
        name: String,
        target: Option<String>,
        message: Option<String>,
    },

    /// `pragma graft_json_tag_create = "name [rev]";`
    /// `pragma graft_json_tag_create = "--annotated name [rev] -- message";`
    JsonTagCreate {
        name: String,
        target: Option<String>,
        message: Option<String>,
    },

    /// `pragma graft_tag_delete = "name";`
    TagDelete { name: String },

    /// `pragma graft_json_tag_delete = "name";`
    JsonTagDelete { name: String },

    /// `pragma graft_switch_branch = "[--force] name";`
    SwitchBranch { name: String, force: bool },

    /// `pragma graft_json_switch_branch = "[--force] name";`
    JsonSwitchBranch { name: String, force: bool },

    /// `pragma graft_switch_create = "[--force] name [start-point]";`
    SwitchCreate {
        name: String,
        start_point: Option<String>,
        force: bool,
    },

    /// `pragma graft_json_switch_create = "[--force] name [start-point]";`
    JsonSwitchCreate {
        name: String,
        start_point: Option<String>,
        force: bool,
    },

    /// `pragma graft_merge = "rev";`
    Merge { rev: String },

    /// `pragma graft_json_merge = "rev";`
    JsonMerge { rev: String },

    /// `pragma graft_merge_abort;`
    MergeAbort,

    /// `pragma graft_json_merge_abort;`
    JsonMergeAbort,

    /// `pragma graft_merge_continue = "message";`
    MergeContinue { message: String },

    /// `pragma graft_json_merge_continue = "message";`
    JsonMergeContinue { message: String },

    /// `pragma graft_conflicts;`
    Conflicts,

    /// `pragma graft_json_conflicts;`
    JsonConflicts,

    /// `pragma graft_resolve = "--ours|--theirs|--manual [path]";`
    Resolve { spec: RepoResolveSpec },

    /// `pragma graft_json_resolve_conflict = "--ours|--theirs|--manual [path]";`
    JsonResolveConflict { spec: RepoResolveSpec },

    /// `pragma graft_remote_add = "name remote-uri";`
    RemoteAdd { name: String, config: RemoteConfig },

    /// `pragma graft_json_remote_add = "name remote-uri";`
    JsonRemoteAdd { name: String, config: RemoteConfig },

    /// `pragma graft_remote_remove = "name";`
    RemoteRemove { name: String },

    /// `pragma graft_json_remote_remove = "name";`
    JsonRemoteRemove { name: String },

    /// `pragma graft_remote_rename = "old new";`
    RemoteRename { old: String, new: String },

    /// `pragma graft_json_remote_rename = "old new";`
    JsonRemoteRename { old: String, new: String },

    /// `pragma graft_remote_get_url = "name";`
    RemoteGetUrl { name: String },

    /// `pragma graft_json_remote_get_url = "name";`
    JsonRemoteGetUrl { name: String },

    /// `pragma graft_remote_set_url = "name remote-uri";`
    RemoteSetUrl { name: String, config: RemoteConfig },

    /// `pragma graft_json_remote_set_url = "name remote-uri";`
    JsonRemoteSetUrl { name: String, config: RemoteConfig },

    /// `pragma graft_remote_prune = "name";`
    RemotePrune { name: String },

    /// `pragma graft_json_remote_prune = "name";`
    JsonRemotePrune { name: String },

    /// `pragma graft_ls_remote = "name";`
    LsRemote { name: String },

    /// `pragma graft_json_ls_remote = "name";`
    JsonLsRemote { name: String },

    /// `pragma graft_remotes;`
    Remotes,

    /// `pragma graft_json_remotes;`
    JsonRemotes,

    /// `pragma graft_debug_volume_snapshot;`
    VolumeSnapshot,

    /// `pragma graft_fetch;`
    Fetch {
        remote: Option<String>,
        branch: Option<String>,
        refspec: Option<String>,
        all: bool,
    },

    /// `pragma graft_json_fetch;`
    JsonFetch {
        remote: Option<String>,
        branch: Option<String>,
        refspec: Option<String>,
        all: bool,
    },

    /// `pragma graft_fetch_async;`
    FetchAsync {
        remote: Option<String>,
        branch: Option<String>,
        refspec: Option<String>,
        all: bool,
    },

    /// `pragma graft_json_fetch_async;`
    JsonFetchAsync {
        remote: Option<String>,
        branch: Option<String>,
        refspec: Option<String>,
        all: bool,
        mode: JsonFetchAsyncMode,
    },

    /// `pragma graft_job_status = "job-id";`
    JobStatus { id: String },

    /// `pragma graft_json_job_status = "job-id";`
    JsonJobStatus { id: String },

    /// `pragma graft_job_result = "job-id";`
    JobResult { id: String },

    /// `pragma graft_json_job_result = "job-id";`
    JsonJobResult { id: String },

    /// `pragma graft_pull;`
    Pull {
        remote: Option<String>,
        branch: Option<String>,
        refspec: Option<String>,
        all: bool,
    },

    /// `pragma graft_json_pull;`
    JsonPull {
        remote: Option<String>,
        branch: Option<String>,
        refspec: Option<String>,
        all: bool,
    },

    /// `pragma graft_push;`
    Push {
        remote: Option<String>,
        branch: Option<String>,
        refspec: Option<String>,
        all: bool,
        force: bool,
    },

    /// `pragma graft_json_push;`
    JsonPush {
        remote: Option<String>,
        branch: Option<String>,
        refspec: Option<String>,
        all: bool,
        force: bool,
    },

    /// `pragma graft_debug_volume_fetch;`
    VolumeFetch,

    /// `pragma graft_debug_volume_pull;`
    VolumePull,

    /// `pragma graft_debug_volume_push;`
    VolumePush,

    /// `pragma graft_debug_volume_audit;`
    VolumeAudit,

    /// `pragma graft_debug_volume_json_audit;`
    VolumeJsonAudit,

    /// `pragma graft_audit [= "[--repair [remote]]"];`
    RepoAudit { spec: RepoAuditSpec },

    /// `pragma graft_json_audit [= "[--repair [remote]]"];`
    JsonRepoAudit { spec: RepoAuditSpec },

    /// `pragma graft_payload_fetch [= "[--remote remote] [rev]"];`
    LargeFileFetch { spec: LargeFileFetchSpec },

    /// `pragma graft_json_payload_fetch [= "[--remote remote] [rev]"];`
    JsonLargeFileFetch {
        spec: LargeFileFetchSpec,
        operation: &'static str,
    },

    /// `pragma graft_payload_status [= "[rev]"];`
    LargeFileStatus { spec: LargeFileStatusSpec },

    /// `pragma graft_json_payload_status [= "[rev]"];`
    JsonLargeFileStatus {
        spec: LargeFileStatusSpec,
        operation: &'static str,
    },

    /// `pragma graft_payload_prune [= "[--dry-run|--force]"];`
    LargeFilePrune { spec: LargeFilePruneSpec },

    /// `pragma graft_json_payload_prune [= "[--dry-run|--force]"];`
    JsonLargeFilePrune {
        spec: LargeFilePruneSpec,
        operation: &'static str,
    },

    /// `pragma graft_ls_files [= "[--stage|--details|--others] [--kind kind]"];`
    LsFiles { spec: LsFilesSpec },

    /// `pragma graft_json_ls_files [= "[--stage|--details|--others] [--kind kind]"];`
    JsonLsFiles { spec: LsFilesSpec },

    /// `pragma graft_config_get = "key";`
    ConfigGet { key: String },

    /// `pragma graft_json_config_get = "key";`
    JsonConfigGet { key: String },

    /// `pragma graft_config_list;`
    ConfigList,

    /// `pragma graft_json_config_list [= "--with-status"];`
    JsonConfigList { mode: JsonConfigListMode },

    /// `pragma graft_config_set = "key -- value";`
    ConfigSet { key: String, value: String },

    /// `pragma graft_json_config_set = "key -- value";`
    JsonConfigSet { key: String, value: String },

    /// `pragma graft_config_unset = "key";`
    ConfigUnset { key: String },

    /// `pragma graft_json_config_unset = "key";`
    JsonConfigUnset { key: String },

    /// `pragma graft_debug_volume_hydrate;`
    VolumeHydrate,

    /// `pragma graft_version;`
    Version,

    /// `pragma graft_debug_volume_import = "PATH";`
    VolumeImport,

    /// `pragma graft_debug_volume_export = "PATH";`
    VolumeExport(PathBuf),

    /// `pragma graft_debug_volume_dump_header;`
    VolumeDumpSqliteHeader,

    /// `pragma graft_debug_volume_dump_commit = "logid:LSN";`
    VolumeDumpCommit { logref: LogRef },

    /// `pragma graft_debug_log_lsn;`
    /// Display storage commit history for the current Volume by LSN.
    DebugLogLsn,

    /// `pragma graft_debug_show_lsn = "logid:LSN";`
    /// Display storage commit details for an internal Log/LSN coordinate.
    DebugShowLsn { logref: LogRef },

    /// `pragma graft_debug_diff_lsn = "logid:from logid:to";`
    /// Compare storage commits by internal Log/LSN coordinates.
    DebugDiffLsn { from: LogRef, to: LogRef },

    /// `pragma graft_log;`
    /// Display repository commit history
    Log,

    /// `pragma graft_debug_volume_checkout_lsn = "LSN";`
    /// Checkout to specified local LSN (creates new Volume)
    VolumeCheckoutLsn { lsn: LSN },

    /// `pragma graft_debug_volume_reset_to = "LSN";`
    /// Reset current tag to specified LSN
    VolumeResetTo { lsn: LSN },

    /// `pragma graft_reset = "[--soft|--mixed|--hard] rev";`
    /// Reset the current repository branch to a revision
    Reset { rev: String, mode: ResetMode },

    /// `pragma graft_json_reset = "[--soft|--mixed|--hard] rev";`
    /// Reset the current repository branch to a revision and return JSON
    JsonReset { rev: String, mode: ResetMode },

    /// `pragma graft_debug_volume_diff = "from_lsn,to_lsn[,mode]";`
    /// Compare legacy Volume commits by LSN
    /// mode: omitted=default (page + table level), "rows"=row-level detailed comparison
    VolumeDiff { from: LSN, to: LSN, mode: DiffMode },

    /// `pragma graft_diff = "[--rows] [--kind kind] [--staged] [rev] [rev] [-- path]";`
    /// Compare repository commits by revision syntax
    RepoDiff { spec: RepoDiffSpec },

    /// `pragma graft_show = "rev";`
    /// Display detailed info for specified revision
    Show { target: String },

    // JSON output variants (non-breaking additions)
    /// `pragma graft_json_log [= "--with-status"];`
    /// Repository commit history as JSON array, or app-facing JSON object with status
    JsonLog { mode: JsonLogMode },

    /// `pragma graft_debug_volume_json_diff = "from_lsn,to_lsn[,mode]";`
    /// Legacy Volume diff as JSON. mode: omitted=summary, "rows"=row-level detail
    VolumeJsonDiff { from: LSN, to: LSN, mode: DiffMode },

    /// `pragma graft_json_diff = "[--rows] [--kind kind] [--staged] [rev] [rev] [-- path]";`
    /// Repository diff as JSON
    JsonRepoDiff { spec: RepoDiffSpec },

    /// `pragma graft_json_show = "rev";`
    /// Commit details as JSON
    JsonShow { target: String },

    /// `pragma graft_debug_volume_json_info;`
    /// Volume info as JSON
    VolumeJsonInfo,

    /// `pragma graft_debug_volume_table_log = 'table_name';`
    /// Show commits that modified the given table
    VolumeTableLog { table: String },

    /// `pragma graft_debug_volume_json_table_log = 'table_name';`
    /// Show commits that modified the given table, as JSON
    VolumeJsonTableLog { table: String },

    /// `pragma graft_debug_volume_set_message = 'message';`
    /// Set a human-readable message for the next commit
    VolumeSetMessage { message: String },
}

impl TryFrom<&Pragma<'_>> for GraftPragma {
    type Error = PragmaErr;

    fn try_from(p: &Pragma<'_>) -> Result<Self, Self::Error> {
        if let Some((prefix, suffix)) = p.name.split_once("_")
            && prefix == "graft"
        {
            return match suffix {
                "debug_volume_list" => Ok(GraftPragma::VolumeList),
                "debug_volume_json_list" => Ok(GraftPragma::VolumeJsonList),
                "tags" => Ok(GraftPragma::Tags),
                "json_tags" => Ok(GraftPragma::JsonTags { mode: parse_json_tags_arg(p.arg)? }),
                "debug_volume_tags" => Ok(GraftPragma::VolumeTags),
                "debug_volume_clone" => {
                    let remote = p.arg.map(parse_or_fail).transpose()?;
                    Ok(GraftPragma::VolumeClone { remote })
                }
                "debug_volume_fork" => Ok(GraftPragma::VolumeFork),
                "checkout" => {
                    let arg = p.require_arg()?;
                    let spec = parse_repo_checkout_arg(arg)?;
                    Ok(GraftPragma::RepoCheckout { spec })
                }
                "json_checkout" => {
                    let arg = p.require_arg()?;
                    let spec = parse_repo_checkout_arg(arg)?;
                    Ok(GraftPragma::JsonRepoCheckout { spec })
                }
                "restore" => {
                    let arg = p.require_arg()?;
                    let spec = parse_repo_restore_arg(arg)?;
                    Ok(GraftPragma::Restore { spec })
                }
                "json_restore" => {
                    let arg = p.require_arg()?;
                    let spec = parse_repo_restore_arg(arg)?;
                    Ok(GraftPragma::JsonRestore { spec })
                }
                "export" => {
                    let arg = p.require_arg()?;
                    let spec = parse_repo_export_arg(arg)?;
                    Ok(GraftPragma::Export { spec })
                }
                "json_export" => {
                    let arg = p.require_arg()?;
                    let spec = parse_repo_export_arg(arg)?;
                    Ok(GraftPragma::JsonExport { spec })
                }
                "debug_volume_new" => Ok(GraftPragma::VolumeSwitch {
                    vid: VolumeId::random(),
                    local: None,
                    remote: None,
                }),
                "debug_volume_switch" => {
                    let parts: Vec<&str> = p.require_arg()?.split(':').collect();
                    if parts.is_empty() || parts.len() > 3 {
                        return Err(pragma_fail(
                            "argument must be in the form: `local_vid[:local[:remote]]`",
                        ));
                    }
                    Ok(GraftPragma::VolumeSwitch {
                        vid: parse_or_fail(parts[0])?,
                        local: parse_optional(parts.get(1))?,
                        remote: parse_optional(parts.get(2))?,
                    })
                }
                "debug_volume_info" => Ok(GraftPragma::VolumeInfo),
                "status" => Ok(GraftPragma::Status { spec: parse_status_arg(p.arg)? }),
                "debug_volume_status" => Ok(GraftPragma::VolumeStatus),
                "init" => Ok(GraftPragma::RepoInit),
                "json_init" => Ok(GraftPragma::JsonRepoInit),
                "clone" => {
                    let spec = parse_repo_clone_arg(p.require_arg()?)?;
                    Ok(GraftPragma::RepoClone { spec })
                }
                "json_clone" => {
                    let spec = parse_repo_clone_arg(p.require_arg()?)?;
                    Ok(GraftPragma::JsonRepoClone { spec })
                }
                "json_status" => Ok(GraftPragma::JsonStatus { spec: parse_status_arg(p.arg)? }),
                "add" => Ok(GraftPragma::Add { spec: parse_repo_add_arg(p.arg)? }),
                "json_add" => Ok(GraftPragma::JsonAdd { spec: parse_repo_add_arg(p.arg)? }),
                "rm" => Ok(GraftPragma::Remove { spec: parse_repo_remove_arg(p.arg)? }),
                "json_rm" => Ok(GraftPragma::JsonRemove { spec: parse_repo_remove_arg(p.arg)? }),
                "commit" => Ok(GraftPragma::Commit { message: p.require_arg()?.to_string() }),
                "json_commit" => {
                    Ok(GraftPragma::JsonCommit { message: p.require_arg()?.to_string() })
                }
                "branch" => Ok(GraftPragma::Branch { mode: parse_branch_list_mode(p.arg)? }),
                "json_branch" => {
                    Ok(GraftPragma::JsonBranch { mode: parse_branch_list_mode(p.arg)? })
                }
                "branch_create" => {
                    let (name, start_point) = parse_branch_create_arg(p.require_arg()?)?;
                    Ok(GraftPragma::BranchCreate { name, start_point })
                }
                "json_branch_create" => {
                    let (name, start_point) = parse_branch_create_arg(p.require_arg()?)?;
                    Ok(GraftPragma::JsonBranchCreate { name, start_point })
                }
                "branch_delete" => {
                    let (name, force) = parse_branch_delete_arg(p.require_arg()?)?;
                    Ok(GraftPragma::BranchDelete { name, force })
                }
                "json_branch_delete" => {
                    let (name, force) = parse_branch_delete_arg(p.require_arg()?)?;
                    Ok(GraftPragma::JsonBranchDelete { name, force })
                }
                "branch_rename" => {
                    let (old, new, force) = parse_branch_rename_arg(p.require_arg()?)?;
                    Ok(GraftPragma::BranchRename { old, new, force })
                }
                "json_branch_rename" => {
                    let (old, new, force) = parse_branch_rename_arg(p.require_arg()?)?;
                    Ok(GraftPragma::JsonBranchRename { old, new, force })
                }
                "branch_upstream" => {
                    let (branch, remote, remote_branch) =
                        parse_branch_upstream_arg(p.require_arg()?)?;
                    Ok(GraftPragma::BranchUpstream { branch, remote, remote_branch })
                }
                "json_branch_upstream" => {
                    let (branch, remote, remote_branch) =
                        parse_branch_upstream_arg(p.require_arg()?)?;
                    Ok(GraftPragma::JsonBranchUpstream { branch, remote, remote_branch })
                }
                "branch_unset_upstream" => {
                    Ok(GraftPragma::BranchUnsetUpstream { branch: p.arg.map(str::to_string) })
                }
                "json_branch_unset_upstream" => {
                    Ok(GraftPragma::JsonBranchUnsetUpstream { branch: p.arg.map(str::to_string) })
                }
                "tag_create" => {
                    let (name, target, message) = parse_tag_create_arg(p.require_arg()?)?;
                    Ok(GraftPragma::TagCreate { name, target, message })
                }
                "json_tag_create" => {
                    let (name, target, message) = parse_tag_create_arg(p.require_arg()?)?;
                    Ok(GraftPragma::JsonTagCreate { name, target, message })
                }
                "tag_delete" => Ok(GraftPragma::TagDelete { name: p.require_arg()?.to_string() }),
                "json_tag_delete" => {
                    Ok(GraftPragma::JsonTagDelete { name: p.require_arg()?.to_string() })
                }
                "switch_branch" => {
                    let (name, force) = parse_switch_branch_arg(p.require_arg()?)?;
                    Ok(GraftPragma::SwitchBranch { name, force })
                }
                "json_switch_branch" => {
                    let (name, force) = parse_switch_branch_arg(p.require_arg()?)?;
                    Ok(GraftPragma::JsonSwitchBranch { name, force })
                }
                "switch_create" => {
                    let (name, start_point, force) = parse_switch_create_arg(p.require_arg()?)?;
                    Ok(GraftPragma::SwitchCreate { name, start_point, force })
                }
                "json_switch_create" => {
                    let (name, start_point, force) = parse_switch_create_arg(p.require_arg()?)?;
                    Ok(GraftPragma::JsonSwitchCreate { name, start_point, force })
                }
                "merge" => Ok(GraftPragma::Merge { rev: p.require_arg()?.to_string() }),
                "json_merge" => Ok(GraftPragma::JsonMerge { rev: p.require_arg()?.to_string() }),
                "merge_abort" => Ok(GraftPragma::MergeAbort),
                "json_merge_abort" => Ok(GraftPragma::JsonMergeAbort),
                "merge_continue" => {
                    Ok(GraftPragma::MergeContinue { message: p.require_arg()?.to_string() })
                }
                "json_merge_continue" => {
                    Ok(GraftPragma::JsonMergeContinue { message: p.require_arg()?.to_string() })
                }
                "conflicts" => Ok(GraftPragma::Conflicts),
                "json_conflicts" => Ok(GraftPragma::JsonConflicts),
                "resolve" => Ok(GraftPragma::Resolve {
                    spec: parse_repo_resolve_arg(p.require_arg()?)?,
                }),
                "json_resolve_conflict" => Ok(GraftPragma::JsonResolveConflict {
                    spec: parse_repo_resolve_arg(p.require_arg()?)?,
                }),
                "remote_add" => {
                    let (name, config) = parse_remote_add(p.require_arg()?)?;
                    Ok(GraftPragma::RemoteAdd { name, config })
                }
                "json_remote_add" => {
                    let (name, config) = parse_remote_add(p.require_arg()?)?;
                    Ok(GraftPragma::JsonRemoteAdd { name, config })
                }
                "remote_remove" => {
                    Ok(GraftPragma::RemoteRemove { name: p.require_arg()?.to_string() })
                }
                "json_remote_remove" => {
                    Ok(GraftPragma::JsonRemoteRemove { name: p.require_arg()?.to_string() })
                }
                "remote_rename" => {
                    let (old, new) = parse_remote_rename(p.require_arg()?)?;
                    Ok(GraftPragma::RemoteRename { old, new })
                }
                "json_remote_rename" => {
                    let (old, new) = parse_remote_rename(p.require_arg()?)?;
                    Ok(GraftPragma::JsonRemoteRename { old, new })
                }
                "remote_get_url" => {
                    Ok(GraftPragma::RemoteGetUrl { name: p.require_arg()?.to_string() })
                }
                "json_remote_get_url" => {
                    Ok(GraftPragma::JsonRemoteGetUrl { name: p.require_arg()?.to_string() })
                }
                "remote_set_url" => {
                    let (name, config) = parse_remote_add(p.require_arg()?)?;
                    Ok(GraftPragma::RemoteSetUrl { name, config })
                }
                "json_remote_set_url" => {
                    let (name, config) = parse_remote_add(p.require_arg()?)?;
                    Ok(GraftPragma::JsonRemoteSetUrl { name, config })
                }
                "remote_prune" => {
                    Ok(GraftPragma::RemotePrune { name: p.require_arg()?.to_string() })
                }
                "json_remote_prune" => {
                    Ok(GraftPragma::JsonRemotePrune { name: p.require_arg()?.to_string() })
                }
                "ls_remote" => Ok(GraftPragma::LsRemote { name: p.require_arg()?.to_string() }),
                "json_ls_remote" => {
                    Ok(GraftPragma::JsonLsRemote { name: p.require_arg()?.to_string() })
                }
                "remotes" => Ok(GraftPragma::Remotes),
                "json_remotes" => Ok(GraftPragma::JsonRemotes),
                "debug_volume_snapshot" => Ok(GraftPragma::VolumeSnapshot),
                "fetch" => {
                    let arg = parse_remote_branch_arg(p.arg)?;
                    if arg.force {
                        return Err(pragma_fail("fetch does not support --force"));
                    }
                    let RemoteBranchArg { remote, branch, refspec, all, .. } = arg;
                    Ok(GraftPragma::Fetch { remote, branch, refspec, all })
                }
                "json_fetch" => {
                    let arg = parse_remote_branch_arg(p.arg)?;
                    if arg.force {
                        return Err(pragma_fail("json_fetch does not support --force"));
                    }
                    let RemoteBranchArg { remote, branch, refspec, all, .. } = arg;
                    Ok(GraftPragma::JsonFetch { remote, branch, refspec, all })
                }
                "fetch_async" => {
                    let arg = parse_remote_branch_arg(p.arg)?;
                    if arg.force {
                        return Err(pragma_fail("fetch_async does not support --force"));
                    }
                    let RemoteBranchArg { remote, branch, refspec, all, .. } = arg;
                    Ok(GraftPragma::FetchAsync { remote, branch, refspec, all })
                }
                "json_fetch_async" => {
                    let (arg, mode) = parse_json_fetch_async_arg(p.arg)?;
                    if arg.force {
                        return Err(pragma_fail("json_fetch_async does not support --force"));
                    }
                    let RemoteBranchArg { remote, branch, refspec, all, .. } = arg;
                    Ok(GraftPragma::JsonFetchAsync { remote, branch, refspec, all, mode })
                }
                "job_status" => Ok(GraftPragma::JobStatus { id: p.require_arg()?.to_string() }),
                "json_job_status" => {
                    Ok(GraftPragma::JsonJobStatus { id: p.require_arg()?.to_string() })
                }
                "job_result" => Ok(GraftPragma::JobResult { id: p.require_arg()?.to_string() }),
                "json_job_result" => {
                    Ok(GraftPragma::JsonJobResult { id: p.require_arg()?.to_string() })
                }
                "pull" => {
                    let arg = parse_remote_branch_arg(p.arg)?;
                    if arg.force {
                        return Err(pragma_fail("pull does not support --force"));
                    }
                    let RemoteBranchArg { remote, branch, refspec, all, .. } = arg;
                    Ok(GraftPragma::Pull { remote, branch, refspec, all })
                }
                "json_pull" => {
                    let arg = parse_remote_branch_arg(p.arg)?;
                    if arg.force {
                        return Err(pragma_fail("json_pull does not support --force"));
                    }
                    let RemoteBranchArg { remote, branch, refspec, all, .. } = arg;
                    Ok(GraftPragma::JsonPull { remote, branch, refspec, all })
                }
                "push" => {
                    let RemoteBranchArg { remote, branch, refspec, all, force } =
                        parse_remote_branch_arg(p.arg)?;
                    Ok(GraftPragma::Push { remote, branch, refspec, all, force })
                }
                "json_push" => {
                    let RemoteBranchArg { remote, branch, refspec, all, force } =
                        parse_remote_branch_arg(p.arg)?;
                    Ok(GraftPragma::JsonPush { remote, branch, refspec, all, force })
                }
                "debug_volume_fetch" => Ok(GraftPragma::VolumeFetch),
                "debug_volume_pull" => Ok(GraftPragma::VolumePull),
                "debug_volume_push" => Ok(GraftPragma::VolumePush),
                "debug_volume_audit" => Ok(GraftPragma::VolumeAudit),
                "debug_volume_json_audit" => Ok(GraftPragma::VolumeJsonAudit),
                "audit" => Ok(GraftPragma::RepoAudit { spec: parse_repo_audit_arg(p.arg)? }),
                "json_audit" => {
                    Ok(GraftPragma::JsonRepoAudit { spec: parse_repo_audit_arg(p.arg)? })
                }
                "lfs_fetch" | "payload_fetch" => {
                    Ok(GraftPragma::LargeFileFetch { spec: parse_lfs_fetch_arg(p.arg)? })
                }
                "json_lfs_fetch" => Ok(GraftPragma::JsonLargeFileFetch {
                    spec: parse_lfs_fetch_arg(p.arg)?,
                    operation: "lfs_fetch",
                }),
                "json_payload_fetch" => Ok(GraftPragma::JsonLargeFileFetch {
                    spec: parse_lfs_fetch_arg(p.arg)?,
                    operation: "payload_fetch",
                }),
                "lfs_status" | "payload_status" => {
                    Ok(GraftPragma::LargeFileStatus { spec: parse_lfs_status_arg(p.arg)? })
                }
                "json_lfs_status" => Ok(GraftPragma::JsonLargeFileStatus {
                    spec: parse_lfs_status_arg(p.arg)?,
                    operation: "lfs_status",
                }),
                "json_payload_status" => Ok(GraftPragma::JsonLargeFileStatus {
                    spec: parse_lfs_status_arg(p.arg)?,
                    operation: "payload_status",
                }),
                "lfs_prune" | "payload_prune" => {
                    Ok(GraftPragma::LargeFilePrune { spec: parse_lfs_prune_arg(p.arg)? })
                }
                "json_lfs_prune" => Ok(GraftPragma::JsonLargeFilePrune {
                    spec: parse_lfs_prune_arg(p.arg)?,
                    operation: "lfs_prune",
                }),
                "json_payload_prune" => Ok(GraftPragma::JsonLargeFilePrune {
                    spec: parse_lfs_prune_arg(p.arg)?,
                    operation: "payload_prune",
                }),
                "ls_files" => Ok(GraftPragma::LsFiles { spec: parse_ls_files_arg(p.arg)? }),
                "json_ls_files" => {
                    Ok(GraftPragma::JsonLsFiles { spec: parse_ls_files_arg(p.arg)? })
                }
                "config_get" => Ok(GraftPragma::ConfigGet { key: p.require_arg()?.to_string() }),
                "json_config_get" => {
                    Ok(GraftPragma::JsonConfigGet { key: p.require_arg()?.to_string() })
                }
                "config_list" => Ok(GraftPragma::ConfigList),
                "json_config_list" => {
                    Ok(GraftPragma::JsonConfigList { mode: parse_json_config_list_arg(p.arg)? })
                }
                "config_set" => {
                    let (key, value) = parse_repo_config_set_arg(p.require_arg()?)?;
                    Ok(GraftPragma::ConfigSet { key, value })
                }
                "json_config_set" => {
                    let (key, value) = parse_repo_config_set_arg(p.require_arg()?)?;
                    Ok(GraftPragma::JsonConfigSet { key, value })
                }
                "config_unset" => {
                    Ok(GraftPragma::ConfigUnset { key: p.require_arg()?.to_string() })
                }
                "json_config_unset" => {
                    Ok(GraftPragma::JsonConfigUnset { key: p.require_arg()?.to_string() })
                }
                "debug_volume_hydrate" => Ok(GraftPragma::VolumeHydrate),
                "version" => Ok(GraftPragma::Version),
                "debug_volume_import" => {
                    let _ = p.require_arg()?;
                    Ok(GraftPragma::VolumeImport)
                }
                "debug_volume_export" => {
                    Ok(GraftPragma::VolumeExport(PathBuf::from(p.require_arg()?)))
                }
                "debug_volume_dump_header" => Ok(GraftPragma::VolumeDumpSqliteHeader),
                "debug_volume_dump_commit" => {
                    Ok(GraftPragma::VolumeDumpCommit { logref: parse_or_fail(p.require_arg()?)? })
                }
                "debug_log_lsn" => Ok(GraftPragma::DebugLogLsn),
                "debug_show_lsn" => {
                    Ok(GraftPragma::DebugShowLsn { logref: parse_or_fail(p.require_arg()?)? })
                }
                "debug_diff_lsn" => {
                    let (from, to) = parse_debug_diff_lsn_arg(p.require_arg()?)?;
                    Ok(GraftPragma::DebugDiffLsn { from, to })
                }
                "log" => Ok(GraftPragma::Log),
                "debug_volume_checkout_lsn" => {
                    Ok(GraftPragma::VolumeCheckoutLsn { lsn: parse_or_fail(p.require_arg()?)? })
                }
                "debug_volume_reset_to" => {
                    Ok(GraftPragma::VolumeResetTo { lsn: parse_or_fail(p.require_arg()?)? })
                }
                "reset" => {
                    let (mode, rev) = parse_repo_reset_arg(p.require_arg()?)?;
                    Ok(GraftPragma::Reset { rev, mode })
                }
                "json_reset" => {
                    let (mode, rev) = parse_repo_reset_arg(p.require_arg()?)?;
                    Ok(GraftPragma::JsonReset { rev, mode })
                }
                "diff" => {
                    let spec = parse_repo_diff_arg(p.arg)?;
                    Ok(GraftPragma::RepoDiff { spec })
                }
                "debug_volume_diff" => {
                    let (from, to, mode) = parse_volume_diff_arg(p.require_arg()?)?;
                    Ok(GraftPragma::VolumeDiff { from, to, mode })
                }
                "show" => Ok(GraftPragma::Show { target: p.require_arg()?.to_string() }),
                "json_log" => Ok(GraftPragma::JsonLog { mode: parse_json_log_arg(p.arg)? }),
                "json_diff" => {
                    let spec = parse_repo_diff_arg(p.arg)?;
                    Ok(GraftPragma::JsonRepoDiff { spec })
                }
                "debug_volume_json_diff" => {
                    let (from, to, mode) = parse_volume_diff_arg(p.require_arg()?)?;
                    Ok(GraftPragma::VolumeJsonDiff { from, to, mode })
                }
                "json_show" => Ok(GraftPragma::JsonShow { target: p.require_arg()?.to_string() }),
                "debug_volume_json_info" => Ok(GraftPragma::VolumeJsonInfo),
                "debug_volume_table_log" => {
                    Ok(GraftPragma::VolumeTableLog { table: p.require_arg()?.to_string() })
                }
                "debug_volume_json_table_log" => {
                    Ok(GraftPragma::VolumeJsonTableLog { table: p.require_arg()?.to_string() })
                }
                "debug_volume_set_message" => {
                    Ok(GraftPragma::VolumeSetMessage { message: p.require_arg()?.to_string() })
                }
                _ => Err(pragma_fail(format!("invalid graft pragma `{}`", p.name))),
            };
        }
        Err(PragmaErr::NotFound)
    }
}

impl GraftPragma {
    pub fn eval(self, _runtime: &Runtime, file: &mut VolFile) -> Result<Option<String>, ErrCtx> {
        let runtime = file.runtime().clone();
        match self {
            GraftPragma::VolumeList => Ok(Some(format_volumes(&runtime, file)?)),
            GraftPragma::VolumeJsonList => Ok(Some(to_json(&json_volumes(&runtime, file)?)?)),
            GraftPragma::Tags => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_repo_tags(&repo.tags()?)?))
            }
            GraftPragma::JsonTags { mode } => {
                let repo = repo_for_file(file)?;
                let tags = repo.tags()?;
                match mode {
                    JsonTagsMode::LegacyArray => Ok(Some(to_json(&tags)?)),
                    JsonTagsMode::WithStatus => {
                        let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                        Ok(Some(to_json(&JsonTagListOutcome {
                            current_head,
                            current_branch,
                            tags,
                        })?))
                    }
                }
            }
            GraftPragma::VolumeTags => Ok(Some(format_tags(&runtime, file)?)),

            GraftPragma::VolumeClone { remote } => {
                if !file.is_idle() {
                    return pragma_err!("cannot clone while there is an open transaction");
                }

                let remote = match remote {
                    Some(remote) => remote,
                    None => runtime.volume_get(&file.vid)?.remote,
                };
                let volume = runtime.volume_open(None, None, Some(remote))?;
                file.switch_volume(&volume.vid)?;

                Ok(Some(format!(
                    "Created new Volume {} from remote Log {}",
                    volume.vid, volume.remote
                )))
            }

            GraftPragma::VolumeFork => {
                if !file.is_idle() {
                    return pragma_err!("cannot fork while there is an open transaction");
                }

                let snapshot = file.snapshot_or_latest()?;
                let missing = runtime.snapshot_missing_pages(&snapshot)?;
                if missing.is_empty() {
                    let volume = runtime.volume_from_snapshot(&snapshot)?;
                    file.switch_volume(&volume.vid)?;

                    Ok(Some(format!(
                        "Forked current snapshot into Volume: {}",
                        volume.vid,
                    )))
                } else {
                    pragma_err!("ERROR: must hydrate volume before forking")
                }
            }

            GraftPragma::RepoCheckout { spec } => {
                let outcome = run_repo_checkout(&runtime, file, spec)?;
                Ok(Some(format_checkout_outcome(&outcome)))
            }
            GraftPragma::JsonRepoCheckout { spec } => {
                let outcome = run_repo_checkout(&runtime, file, spec)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::Restore { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot restore while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let outcome = restore_repo_path(&runtime, file, &repo, &spec)?;
                Ok(Some(format_restore_outcome(&outcome)))
            }
            GraftPragma::JsonRestore { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot restore while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let outcome = restore_repo_path(&runtime, file, &repo, &spec)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::Export { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot export while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let exported = export_repo_path(&runtime, file, &repo, &spec)?;
                Ok(Some(format!(
                    "Exported {exported} to {}",
                    spec.output.display()
                )))
            }
            GraftPragma::JsonExport { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot export while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let exported = export_repo_path(&runtime, file, &repo, &spec)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonExportOutcome {
                    operation: "export",
                    current_head,
                    current_branch,
                    source: spec.source,
                    path: exported,
                    kind: repo_tracked_path_kind_json_label(RepoTrackedPathKind::SqliteDatabase),
                    output: spec.output.display().to_string(),
                })?))
            }

            GraftPragma::VolumeSwitch { vid, local, remote } => {
                if !file.is_idle() {
                    return pragma_err!("cannot switch while there is an open transaction");
                }

                let volume = runtime.volume_open(Some(vid), local, remote)?;
                file.switch_volume(&volume.vid)?;

                Ok(Some(format!(
                    "Switched to Volume {} with local Log {} and remote Log {}",
                    volume.vid, volume.local, volume.remote,
                )))
            }

            GraftPragma::VolumeInfo => Ok(Some(format_volume_info(&runtime, file)?)),
            GraftPragma::Status { spec } => {
                let repo = repo_for_file(file)?;
                let status = repo_status_for_file(&runtime, file, &repo)?;
                let status = filter_repo_status_by_kind(status, spec.kind);
                Ok(Some(format_repo_status(&status)?))
            }
            GraftPragma::VolumeStatus => Ok(Some(format_volume_status(&runtime, file)?)),

            GraftPragma::RepoInit => {
                let outcome = run_repo_init(file)?;
                Ok(Some(format_repo_init_outcome(&outcome)))
            }
            GraftPragma::JsonRepoInit => {
                let outcome = run_repo_init(file)?;
                Ok(Some(to_json(&JsonInitOutcome {
                    operation: "init",
                    current_head: outcome.current_head,
                    current_branch: outcome.current_branch,
                    graft_dir: outcome.graft_dir.display().to_string(),
                    worktree: outcome.worktree.display().to_string(),
                    path: outcome.path,
                    kind: repo_tracked_path_kind_json_label(RepoTrackedPathKind::SqliteDatabase),
                    preserved_contents: outcome.preserved_contents,
                })?))
            }

            GraftPragma::RepoClone { spec } => {
                let outcome = run_repo_clone(file, spec)?;
                Ok(Some(format!(
                    "Cloned origin/{} at {} into {}",
                    outcome.branch,
                    &outcome.head[..outcome.head.len().min(12)],
                    outcome.graft_dir.display()
                )))
            }
            GraftPragma::JsonRepoClone { spec } => {
                let outcome = run_repo_clone(file, spec)?;
                Ok(Some(to_json(&JsonCloneOutcome {
                    operation: "clone",
                    current_head: outcome.current_head,
                    current_branch: outcome.current_branch,
                    remote: json_remote_info(outcome.remote),
                    branch: outcome.branch,
                    head: outcome.head,
                    commits: outcome.commits,
                    graft_dir: outcome.graft_dir.display().to_string(),
                    paths: outcome.paths,
                })?))
            }

            GraftPragma::JsonStatus { spec } => {
                let repo = repo_for_file(file)?;
                let status = repo_status_for_file(&runtime, file, &repo)?;
                let status = filter_repo_status_by_kind(status, spec.kind);
                let kind = spec.kind.map(repo_tracked_path_kind_json_label);
                let current_head = status.head_target.clone();
                let current_branch = repo.current_branch()?;
                let conflict_analysis =
                    current_file_status_row_merge_analysis_lossy(&runtime, file, &repo, None);
                Ok(Some(to_json(&JsonRepoStatus {
                    current_head,
                    current_branch,
                    kind,
                    status,
                    conflict_analysis,
                })?))
            }

            GraftPragma::Add { spec } => {
                let entries = run_repo_add(&runtime, file, &spec)?;
                Ok(Some(format_added_entries(&entries)))
            }
            GraftPragma::JsonAdd { spec } => {
                let entries = run_repo_add(&runtime, file, &spec)?;
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                let kind = spec.kind.map(repo_tracked_path_kind_json_label);
                Ok(Some(to_json(&JsonAddOutcome {
                    operation: "add",
                    current_head,
                    current_branch,
                    kind,
                    paths: json_staged_entry_paths(&repo, &entries)?,
                })?))
            }

            GraftPragma::Remove { spec } => {
                let paths = run_repo_remove(&runtime, file, &spec)?;
                Ok(Some(format_removed_paths(&paths)))
            }
            GraftPragma::JsonRemove { spec } => {
                let paths = run_repo_remove(&runtime, file, &spec)?;
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonRemoveOutcome {
                    operation: "rm",
                    current_head,
                    current_branch,
                    cached: spec.cached,
                    paths,
                })?))
            }

            GraftPragma::Commit { message } => {
                let outcome = run_repo_commit(&runtime, file, message)?;
                let commit = outcome.commit;
                Ok(Some(format!("[{}] {}", &commit.id[..12], commit.message)))
            }
            GraftPragma::JsonCommit { message } => {
                let outcome = run_repo_commit(&runtime, file, message)?;
                let head = outcome.commit.id.clone();
                let paths = json_commit_path_changes(&outcome.commit);
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonCommitOutcome {
                    operation: "commit",
                    current_head,
                    current_branch,
                    head,
                    branch: outcome.branch,
                    paths,
                    materialized: outcome.materialized,
                    commit: json_commit_summary(outcome.commit),
                })?))
            }

            GraftPragma::Branch { mode } => {
                let repo = repo_for_file(file)?;
                let branches = repo.branches()?;
                let remote_branches = if mode.includes_remote() {
                    repo.remote_tracking_branches()?
                } else {
                    Vec::new()
                };
                Ok(Some(format_branches(&branches, &remote_branches, mode)?))
            }
            GraftPragma::JsonBranch { mode } => {
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                let branches = repo.branches()?;
                let remote_branches = if mode.includes_remote() {
                    repo.remote_tracking_branches()?
                } else {
                    Vec::new()
                };
                Ok(Some(to_json(&JsonBranchList {
                    current_head,
                    current_branch,
                    branches,
                    remote_branches,
                })?))
            }

            GraftPragma::BranchCreate { name, start_point } => {
                let branch = run_repo_branch_create(file, name, start_point)?;
                Ok(Some(format_branch_created(&branch)))
            }
            GraftPragma::JsonBranchCreate { name, start_point } => {
                let branch = run_repo_branch_create(file, name, start_point)?;
                let outcome = json_branch_mutation_outcome(file, "branch_create", branch, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::BranchDelete { name, force } => {
                let branch = run_repo_branch_delete(file, name, force)?;
                Ok(Some(format_branch_deleted(&branch, force)))
            }
            GraftPragma::JsonBranchDelete { name, force } => {
                let branch = run_repo_branch_delete(file, name, force)?;
                let outcome = json_branch_mutation_outcome(file, "branch_delete", branch, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::BranchRename { old, new, force } => {
                let (old, branch) = run_repo_branch_rename(file, old, new, force)?;
                Ok(Some(format_branch_renamed(&old, &branch, force)))
            }
            GraftPragma::JsonBranchRename { old, new, force } => {
                let (old, branch) = run_repo_branch_rename(file, old, new, force)?;
                let outcome =
                    json_branch_mutation_outcome(file, "branch_rename", branch, Some(old))?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::BranchUpstream { branch, remote, remote_branch } => {
                let branch = run_repo_branch_upstream(file, branch, remote, remote_branch)?;
                Ok(Some(format_branch_upstream(&branch)))
            }
            GraftPragma::JsonBranchUpstream { branch, remote, remote_branch } => {
                let branch = run_repo_branch_upstream(file, branch, remote, remote_branch)?;
                let outcome = json_branch_mutation_outcome(file, "branch_upstream", branch, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::BranchUnsetUpstream { branch } => {
                let branch = run_repo_branch_unset_upstream(file, branch)?;
                Ok(Some(format_branch_upstream_unset(&branch)))
            }
            GraftPragma::JsonBranchUnsetUpstream { branch } => {
                let branch = run_repo_branch_unset_upstream(file, branch)?;
                let outcome =
                    json_branch_mutation_outcome(file, "branch_unset_upstream", branch, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::TagCreate { name, target, message } => {
                let tag = run_repo_tag_create(file, name, target, message)?;
                Ok(Some(format_tag_created(&tag)))
            }
            GraftPragma::JsonTagCreate { name, target, message } => {
                let tag = run_repo_tag_create(file, name, target, message)?;
                let outcome = json_tag_mutation_outcome(file, "tag_create", tag)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::TagDelete { name } => {
                let tag = run_repo_tag_delete(file, name)?;
                Ok(Some(format_tag_deleted(&tag)))
            }
            GraftPragma::JsonTagDelete { name } => {
                let tag = run_repo_tag_delete(file, name)?;
                let outcome = json_tag_mutation_outcome(file, "tag_delete", tag)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::SwitchBranch { name, force } => {
                run_repo_switch_branch(&runtime, file, name.clone(), force)?;
                Ok(Some(format!("Switched to branch '{name}'")))
            }
            GraftPragma::JsonSwitchBranch { name, force } => {
                let outcome = run_repo_switch_branch(&runtime, file, name, force)?;
                let head = outcome.target.clone();
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonSwitchOutcome {
                    operation: "switch_branch",
                    current_head,
                    current_branch,
                    head,
                    branch: outcome.branch,
                    target: outcome.target,
                    paths: outcome.paths,
                })?))
            }

            GraftPragma::SwitchCreate { name, start_point, force } => {
                let outcome = run_repo_switch_create(&runtime, file, name, start_point, force)?;
                Ok(Some(format_branch_created(&outcome.branch)))
            }
            GraftPragma::JsonSwitchCreate { name, start_point, force } => {
                let outcome = run_repo_switch_create(&runtime, file, name, start_point, force)?;
                let head = outcome.branch.target.clone();
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonSwitchOutcome {
                    operation: "switch_create",
                    current_head,
                    current_branch,
                    head,
                    branch: outcome.branch.name,
                    target: outcome.branch.target,
                    paths: outcome.paths,
                })?))
            }

            GraftPragma::Merge { rev } => {
                let outcome = run_repo_merge(&runtime, file, &rev)?;
                let repo = repo_for_file(file)?;
                Ok(Some(format_merge_outcome_with_row_auto_merge(
                    &runtime,
                    file,
                    &repo,
                    &outcome.outcome,
                    outcome.row_auto_merge.as_ref(),
                    None,
                )?))
            }
            GraftPragma::JsonMerge { rev } => {
                let outcome = run_repo_merge(&runtime, file, &rev)?;
                let repo = repo_for_file(file)?;
                let conflict_analysis =
                    current_file_status_row_merge_analysis_lossy(&runtime, file, &repo, None);
                let head = merge_fast_forward_head(&outcome.outcome);
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonMergeCommandOutcome {
                    operation: "merge",
                    current_head,
                    current_branch,
                    head,
                    branch: outcome.branch,
                    outcome: outcome.outcome,
                    paths: outcome.paths,
                    conflict_analysis,
                })?))
            }

            GraftPragma::MergeAbort => {
                let outcome = run_repo_merge_abort(&runtime, file)?;
                Ok(Some(format!(
                    "Aborted merge; reset HEAD to {}",
                    &outcome.target[..outcome.target.len().min(12)]
                )))
            }
            GraftPragma::JsonMergeAbort => {
                let outcome = run_repo_merge_abort(&runtime, file)?;
                let head = outcome.target.clone();
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonMergeAbortCommandOutcome {
                    operation: "merge_abort",
                    current_head,
                    current_branch,
                    head,
                    branch: outcome.branch,
                    target: outcome.target,
                    paths: outcome.paths,
                })?))
            }

            GraftPragma::MergeContinue { message } => {
                let outcome = run_repo_merge_continue(&runtime, file, message)?;
                let commit = outcome.commit;
                Ok(Some(format!(
                    "Merge commit [{}] {}",
                    &commit.id[..commit.id.len().min(12)],
                    commit.message
                )))
            }
            GraftPragma::JsonMergeContinue { message } => {
                let outcome = run_repo_merge_continue(&runtime, file, message)?;
                let head = outcome.commit.id.clone();
                let paths = json_commit_path_changes(&outcome.commit);
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonMergeContinueCommandOutcome {
                    operation: "merge_continue",
                    current_head,
                    current_branch,
                    head,
                    branch: outcome.branch,
                    paths,
                    materialized: outcome.materialized,
                    commit: json_commit_summary(outcome.commit),
                })?))
            }

            GraftPragma::Conflicts => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_conflicts(&repo.status()?)?))
            }

            GraftPragma::JsonConflicts => {
                let repo = repo_for_file(file)?;
                let remote = repo_default_remote_store(&repo);
                Ok(Some(to_json(&repo_conflict_artifacts(
                    &runtime, &repo, remote,
                )?)?))
            }

            GraftPragma::Resolve { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot resolve while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let side = spec.side;
                let outcome = resolve_repo_conflict_for_file(&runtime, file, &repo, spec)?;
                Ok(Some(format!(
                    "Resolved {} using {}",
                    outcome.path,
                    side.label()
                )))
            }

            GraftPragma::JsonResolveConflict { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot resolve while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let side = spec.side;
                let outcome = resolve_repo_conflict_for_file(&runtime, file, &repo, spec)?;
                let remote = repo_default_remote_store(&repo);
                let remaining_conflicts =
                    unresolved_conflict_artifact_count(&runtime, &repo, remote)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonResolveConflictOutcome {
                    operation: "resolve_conflict",
                    current_head,
                    current_branch,
                    path: outcome.path,
                    path_kind: repo_tracked_path_kind_json_label(outcome.path_kind),
                    storage: repo_path_storage_json_label(outcome.path_storage),
                    resolution: side.label(),
                    remaining_conflicts,
                })?))
            }

            GraftPragma::RemoteAdd { name, config } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_add(&name, config)?;
                Ok(Some(format_remote(&remote)))
            }
            GraftPragma::JsonRemoteAdd { name, config } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_add(&name, config)?;
                let outcome = json_remote_mutation_outcome(file, "remote_add", remote, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::RemoteRemove { name } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_remove(&name)?;
                Ok(Some(format!("Removed remote '{}'", remote.name)))
            }
            GraftPragma::JsonRemoteRemove { name } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_remove(&name)?;
                let outcome = json_remote_mutation_outcome(file, "remote_remove", remote, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::RemoteRename { old, new } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_rename(&old, &new)?;
                Ok(Some(format!(
                    "Renamed remote '{}' to '{}': {}",
                    old,
                    remote.name,
                    remote_config_uri(&remote.config)
                )))
            }
            GraftPragma::JsonRemoteRename { old, new } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_rename(&old, &new)?;
                let outcome =
                    json_remote_mutation_outcome(file, "remote_rename", remote, Some(old))?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::RemoteGetUrl { name } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_get_url(&name)?;
                Ok(Some(remote_config_uri(&remote.config)))
            }
            GraftPragma::JsonRemoteGetUrl { name } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_get_url(&name)?;
                let outcome = json_remote_mutation_outcome(file, "remote_get_url", remote, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::RemoteSetUrl { name, config } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_set_url(&name, config)?;
                Ok(Some(format!(
                    "Updated remote '{}': {}",
                    remote.name,
                    remote_config_uri(&remote.config)
                )))
            }
            GraftPragma::JsonRemoteSetUrl { name, config } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_set_url(&name, config)?;
                let outcome = json_remote_mutation_outcome(file, "remote_set_url", remote, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::RemotePrune { name } => {
                let repo = repo_for_file(file)?;
                let outcome = repo.remote_prune(&name)?;
                Ok(Some(format_remote_prune_outcome(&outcome)?))
            }
            GraftPragma::JsonRemotePrune { name } => {
                let repo = repo_for_file(file)?;
                let outcome = repo.remote_prune(&name)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonRemotePruneCommandOutcome {
                    operation: "remote_prune",
                    current_head,
                    current_branch,
                    outcome,
                })?))
            }

            GraftPragma::LsRemote { name } => {
                let repo = repo_for_file(file)?;
                let default_branch = repo.remote_default_branch(&name)?;
                let refs = repo.remote_branch_refs(&name)?;
                Ok(Some(format_ls_remote(
                    &name,
                    default_branch.as_deref(),
                    &refs,
                )?))
            }
            GraftPragma::JsonLsRemote { name } => {
                let repo = repo_for_file(file)?;
                let default_branch = repo.remote_default_branch(&name)?;
                let refs = repo.remote_branch_refs(&name)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonLsRemoteOutcome {
                    operation: "ls_remote",
                    current_head,
                    current_branch,
                    remote: name,
                    default_branch,
                    refs,
                })?))
            }

            GraftPragma::Remotes => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_remotes(&repo.remotes()?)?))
            }
            GraftPragma::JsonRemotes => {
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonRemoteList {
                    current_head,
                    current_branch,
                    remotes: repo.remotes()?.into_iter().map(json_remote_info).collect(),
                })?))
            }

            GraftPragma::VolumeSnapshot => {
                let snapshot = file.snapshot_or_latest()?;
                Ok(Some(format!("{snapshot:?}")))
            }

            GraftPragma::Fetch { remote, branch, refspec, all } => {
                let repo = repo_for_file(file)?;
                Ok(Some(run_repo_fetch(&repo, remote, branch, refspec, all)?))
            }
            GraftPragma::JsonFetch { remote, branch, refspec, all } => {
                let repo = repo_for_file(file)?;
                Ok(Some(run_repo_fetch_json(
                    &repo, remote, branch, refspec, all,
                )?))
            }
            GraftPragma::FetchAsync { remote, branch, refspec, all } => {
                repo_for_file(file)?;
                let id = async_jobs().spawn_fetch(
                    PathBuf::from(file.tag.clone()),
                    remote,
                    branch,
                    refspec,
                    all,
                    AsyncJobResultFormat::Text,
                );
                Ok(Some(id))
            }
            GraftPragma::JsonFetchAsync { remote, branch, refspec, all, mode } => {
                repo_for_file(file)?;
                let id = async_jobs().spawn_fetch(
                    PathBuf::from(file.tag.clone()),
                    remote,
                    branch,
                    refspec,
                    all,
                    AsyncJobResultFormat::Json,
                );
                match mode {
                    JsonFetchAsyncMode::LegacyId => Ok(Some(id)),
                    JsonFetchAsyncMode::WithStatus => Ok(Some(async_jobs().json_status(&id)?)),
                }
            }
            GraftPragma::JobStatus { id } => Ok(Some(async_jobs().status_json(&id)?)),
            GraftPragma::JsonJobStatus { id } => Ok(Some(async_jobs().json_status(&id)?)),
            GraftPragma::JobResult { id } => Ok(Some(async_jobs().result(&id)?)),
            GraftPragma::JsonJobResult { id } => Ok(Some(async_jobs().result(&id)?)),
            GraftPragma::Pull { remote, branch, refspec, all } => {
                let outcome = run_repo_pull(&runtime, file, remote, branch, refspec, all)?;
                let repo = repo_for_file(file)?;
                let checkout_remote = Arc::new(repo.remote_store(&outcome.outcome.remote)?);
                Ok(Some(format_pull_outcome_with_row_analysis(
                    &runtime,
                    file,
                    &repo,
                    &outcome.outcome,
                    Some(checkout_remote),
                )?))
            }
            GraftPragma::JsonPull { remote, branch, refspec, all } => {
                let outcome = run_repo_pull(&runtime, file, remote, branch, refspec, all)?;
                let repo = repo_for_file(file)?;
                let remote = repo
                    .remote_store(&outcome.outcome.remote)
                    .ok()
                    .map(Arc::new);
                let conflict_analysis =
                    current_file_status_row_merge_analysis_lossy(&runtime, file, &repo, remote);
                Ok(Some(to_json(&JsonPullCommandOutcome {
                    operation: "pull",
                    current_head: outcome.current_head,
                    current_branch: outcome.current_branch,
                    outcome: outcome.outcome,
                    paths: outcome.paths,
                    conflict_analysis,
                })?))
            }

            GraftPragma::Push { remote, branch, refspec, all, force } => {
                let repo = repo_for_file(file)?;
                let outcome = run_repo_push(&runtime, &repo, remote, branch, refspec, all, force)?;
                Ok(Some(format_push_command_outcome(&outcome)?))
            }
            GraftPragma::JsonPush { remote, branch, refspec, all, force } => {
                let repo = repo_for_file(file)?;
                let outcome = run_repo_push(&runtime, &repo, remote, branch, refspec, all, force)?;
                Ok(Some(to_json(&json_push_command_outcome(&repo, &outcome)?)?))
            }
            GraftPragma::VolumeFetch => Ok(Some(fetch_or_pull(&runtime, file, false)?)),
            GraftPragma::VolumePull => Ok(Some(fetch_or_pull(&runtime, file, true)?)),
            GraftPragma::VolumePush => Ok(Some(push(&runtime, file)?)),

            GraftPragma::VolumeAudit => Ok(Some(format_volume_audit(&runtime, file)?)),
            GraftPragma::VolumeJsonAudit => Ok(Some(to_json(&json_volume_audit(&runtime, file)?)?)),
            GraftPragma::RepoAudit { spec } => {
                let repo = repo_for_file(file)?;
                if spec.repair {
                    let remote = repo_default_remote(&repo, spec.remote.clone())?;
                    let outcome = repo.repair_artifacts_from_remote(&remote)?;
                    Ok(Some(format_repo_artifact_repair(&outcome)?))
                } else {
                    Ok(Some(format_repo_artifact_audit(&repo.audit_artifacts()?)?))
                }
            }
            GraftPragma::JsonRepoAudit { spec } => {
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                if spec.repair {
                    let remote = repo_default_remote(&repo, spec.remote.clone())?;
                    let outcome = repo.repair_artifacts_from_remote(&remote)?;
                    Ok(Some(to_json(&JsonRepoArtifactRepair {
                        operation: "audit_repair",
                        current_head,
                        current_branch,
                        outcome,
                    })?))
                } else {
                    Ok(Some(to_json(&JsonRepoArtifactAudit {
                        current_head,
                        current_branch,
                        audit: repo.audit_artifacts()?,
                    })?))
                }
            }
            GraftPragma::LargeFileFetch { spec } => {
                let repo = repo_for_file(file)?;
                let remote = repo_default_remote(&repo, spec.remote.clone())?;
                let outcome = repo.fetch_large_file_payloads(&remote, spec.rev.as_deref())?;
                Ok(Some(format_large_file_fetch_outcome(&outcome)?))
            }
            GraftPragma::JsonLargeFileFetch { spec, operation } => {
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                let remote = repo_default_remote(&repo, spec.remote.clone())?;
                let outcome = repo.fetch_large_file_payloads(&remote, spec.rev.as_deref())?;
                Ok(Some(to_json(&JsonLargeFileFetchOutcome {
                    operation,
                    current_head,
                    current_branch,
                    outcome,
                })?))
            }
            GraftPragma::LargeFileStatus { spec } => {
                let repo = repo_for_file(file)?;
                let outcome = repo.large_file_payloads_status(spec.rev.as_deref())?;
                Ok(Some(format_large_file_status_outcome(&outcome)?))
            }
            GraftPragma::JsonLargeFileStatus { spec, operation } => {
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                let outcome = repo.large_file_payloads_status(spec.rev.as_deref())?;
                Ok(Some(to_json(&JsonLargeFileStatusOutcome {
                    operation,
                    current_head,
                    current_branch,
                    outcome,
                })?))
            }
            GraftPragma::LargeFilePrune { spec } => {
                let repo = repo_for_file(file)?;
                let outcome = repo.prune_large_file_payloads(spec.dry_run)?;
                Ok(Some(format_large_file_prune_outcome(&outcome)?))
            }
            GraftPragma::JsonLargeFilePrune { spec, operation } => {
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                let outcome = repo.prune_large_file_payloads(spec.dry_run)?;
                Ok(Some(to_json(&JsonLargeFilePruneOutcome {
                    operation,
                    current_head,
                    current_branch,
                    outcome,
                })?))
            }
            GraftPragma::LsFiles { spec } => {
                let repo = repo_for_file(file)?;
                if spec.others {
                    let paths = filter_tracked_paths_by_kind(repo.untracked_paths()?, spec.kind);
                    Ok(Some(format_repo_untracked_paths(&paths)?))
                } else if spec.stage {
                    let paths = filter_tracked_path_entries_by_kind(
                        repo.tracked_path_entries()?,
                        spec.kind,
                    );
                    Ok(Some(format_repo_tracked_path_entries(&paths)?))
                } else if spec.details {
                    let paths = filter_tracked_path_details_by_kind(
                        repo.tracked_path_details()?,
                        spec.kind,
                    );
                    Ok(Some(format_repo_tracked_path_details(&paths)?))
                } else {
                    let paths = filter_tracked_paths_by_kind(repo.tracked_paths()?, spec.kind);
                    Ok(Some(format_repo_tracked_paths(&paths)?))
                }
            }
            GraftPragma::JsonLsFiles { spec } => {
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                let kind = spec.kind.map(repo_tracked_path_kind_json_label);
                if spec.others {
                    let paths = filter_tracked_paths_by_kind(repo.untracked_paths()?, spec.kind);
                    Ok(Some(to_json(&JsonLsFilesOutcome {
                        current_head,
                        current_branch,
                        stage: spec.stage,
                        details: spec.details,
                        others: spec.others,
                        kind,
                        paths,
                    })?))
                } else if spec.stage {
                    let paths = filter_tracked_path_entries_by_kind(
                        repo.tracked_path_entries()?,
                        spec.kind,
                    );
                    Ok(Some(to_json(&JsonLsFilesOutcome {
                        current_head,
                        current_branch,
                        stage: spec.stage,
                        details: spec.details,
                        others: spec.others,
                        kind,
                        paths,
                    })?))
                } else if spec.details {
                    let paths = filter_tracked_path_details_by_kind(
                        repo.tracked_path_details()?,
                        spec.kind,
                    );
                    Ok(Some(to_json(&JsonLsFilesOutcome {
                        current_head,
                        current_branch,
                        stage: spec.stage,
                        details: spec.details,
                        others: spec.others,
                        kind,
                        paths,
                    })?))
                } else {
                    let paths = filter_tracked_paths_by_kind(repo.tracked_paths()?, spec.kind);
                    Ok(Some(to_json(&JsonLsFilesOutcome {
                        current_head,
                        current_branch,
                        stage: spec.stage,
                        details: spec.details,
                        others: spec.others,
                        kind,
                        paths,
                    })?))
                }
            }
            GraftPragma::ConfigGet { key } => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_repo_config_entry(&repo.config_get(&key)?)?))
            }
            GraftPragma::JsonConfigGet { key } => {
                let repo = repo_for_file(file)?;
                let entry = repo.config_get(&key)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonConfigEntryOutcome {
                    current_head,
                    current_branch,
                    entry,
                })?))
            }
            GraftPragma::ConfigList => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_repo_config_entries(&repo.config_list()?)?))
            }
            GraftPragma::JsonConfigList { mode } => {
                let repo = repo_for_file(file)?;
                let entries = repo.config_list()?;
                match mode {
                    JsonConfigListMode::LegacyArray => Ok(Some(to_json(&entries)?)),
                    JsonConfigListMode::WithStatus => {
                        let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                        Ok(Some(to_json(&JsonConfigListOutcome {
                            current_head,
                            current_branch,
                            entries,
                        })?))
                    }
                }
            }
            GraftPragma::ConfigSet { key, value } => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_repo_config_entry(
                    &repo.config_set(&key, &value)?,
                )?))
            }
            GraftPragma::JsonConfigSet { key, value } => {
                let repo = repo_for_file(file)?;
                let entry = repo.config_set(&key, &value)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonConfigMutationOutcome {
                    operation: "config_set",
                    current_head,
                    current_branch,
                    entry,
                })?))
            }
            GraftPragma::ConfigUnset { key } => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_repo_config_entry(&repo.config_unset(&key)?)?))
            }
            GraftPragma::JsonConfigUnset { key } => {
                let repo = repo_for_file(file)?;
                let entry = repo.config_unset(&key)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonConfigMutationOutcome {
                    operation: "config_unset",
                    current_head,
                    current_branch,
                    entry,
                })?))
            }

            GraftPragma::VolumeHydrate => {
                let snapshot = file.snapshot_or_latest()?;
                runtime.snapshot_hydrate(snapshot)?;
                Ok(None)
            }

            GraftPragma::Version => {
                const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
                const GITHUB_SHA: Option<&str> = option_env!("GITHUB_SHA");
                let mut out = format!("Graft Version: {PKG_VERSION}");
                if let Some(sha) = GITHUB_SHA {
                    writeln!(&mut out, "\nGit Commit: {sha}")?;
                }
                Ok(Some(out))
            }

            GraftPragma::VolumeImport => {
                pragma_err!(
                    "deprecated: use `vacuum into` instead: https://graft.rs/r/graft_import"
                )
            }

            GraftPragma::VolumeExport(path) => volume_export(&runtime, file, path).map(Some),

            GraftPragma::VolumeDumpSqliteHeader => {
                let reader = runtime.volume_reader(file.vid.clone())?;
                let page = reader.read_page(PageIdx::FIRST)?;
                let header = SqliteHeader::read_from_bytes(&page[..100])
                    .expect("failed to parse SQLite header");
                Ok(Some(format!("{header:#?}")))
            }

            GraftPragma::VolumeDumpCommit { logref } => {
                format_debug_show_lsn(&runtime, &logref).map(Some)
            }

            GraftPragma::DebugLogLsn => format_debug_log_lsn(&runtime, file).map(Some),

            GraftPragma::DebugShowLsn { logref } => {
                format_debug_show_lsn(&runtime, &logref).map(Some)
            }

            GraftPragma::DebugDiffLsn { from, to } => {
                if from.log != to.log {
                    return pragma_err!("debug LSN diff requires both refs to use the same log");
                }
                let diff = runtime.diff_commits(&from.log, from.lsn, to.lsn)?;
                Ok(Some(format_debug_page_diff(&diff)))
            }

            GraftPragma::Log => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_repo_log(&repo)?))
            }

            GraftPragma::VolumeCheckoutLsn { lsn } => {
                if !file.is_idle() {
                    return pragma_err!("cannot checkout while there is an open transaction");
                }

                let new_volume = runtime.volume_checkout(&file.vid, lsn)?;
                file.switch_volume(&new_volume.vid)?;

                Ok(Some(format!(
                    "Checked out LSN {} into new Volume {} (local log: {})",
                    lsn, new_volume.vid, new_volume.local
                )))
            }

            GraftPragma::VolumeResetTo { lsn } => {
                if !file.is_idle() {
                    return pragma_err!("cannot reset while there is an open transaction");
                }

                let tag = file.tag.clone();
                let new_volume = runtime.volume_reset_to(&tag, lsn)?;
                file.switch_volume(&new_volume.vid)?;

                Ok(Some(format!(
                    "Reset tag '{}' to LSN {} (new Volume: {}, local log: {})",
                    tag, lsn, new_volume.vid, new_volume.local
                )))
            }

            GraftPragma::Reset { rev, mode } => {
                let outcome = run_repo_reset(&runtime, file, &rev, mode)?;

                Ok(Some(format!(
                    "Reset HEAD to {} ({})",
                    &outcome.outcome.target[..outcome.outcome.target.len().min(12)],
                    reset_mode_label(mode)
                )))
            }
            GraftPragma::JsonReset { rev, mode } => {
                let outcome = run_repo_reset(&runtime, file, &rev, mode)?;
                let head = outcome.outcome.target.clone();
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonResetCommandOutcome {
                    operation: "reset",
                    current_head,
                    current_branch,
                    head,
                    branch: outcome.branch,
                    outcome: outcome.outcome,
                    paths: outcome.paths,
                })?))
            }

            GraftPragma::VolumeDiff { from, to, mode } => {
                if !file.is_idle() {
                    return pragma_err!("cannot diff while there is an open transaction");
                }
                match mode {
                    DiffMode::Default => {
                        // Built-in table-level diff using our B-tree parser
                        let report =
                            crate::sql_diff::generate_diff_report(&runtime, file, from, to)?;
                        Ok(Some(report))
                    }
                    DiffMode::Rows => {
                        // Row-level detailed diff
                        row_diff_impl(&runtime, file, from, to)
                    }
                }
            }

            GraftPragma::RepoDiff { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot diff while there is an open transaction");
                }
                let mode = spec.mode;
                let repo = repo_for_file(file)?;
                let diff = repo_diff_for_spec(&runtime, file, &repo, spec)?;
                match mode {
                    DiffMode::Default => Ok(Some(format_repo_diff(&diff)?)),
                    DiffMode::Rows => Ok(Some(format_repo_row_diff(&runtime, &repo, &diff)?)),
                }
            }

            GraftPragma::Show { target } => {
                if !file.is_idle() {
                    return pragma_err!("cannot show while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let commit = repo.show_revision(&target)?;
                Ok(Some(format_repo_show(&commit)?))
            }

            GraftPragma::JsonLog { mode } => {
                let repo = repo_for_file(file)?;
                let commits = repo.log()?;
                match mode {
                    JsonLogMode::LegacyArray => Ok(Some(to_json(&commits)?)),
                    JsonLogMode::WithStatus => {
                        let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                        Ok(Some(to_json(&JsonRepoLogOutcome {
                            current_head,
                            current_branch,
                            commits,
                        })?))
                    }
                }
            }

            GraftPragma::VolumeJsonDiff { from, to, mode } => {
                if !file.is_idle() {
                    return pragma_err!("cannot diff while there is an open transaction");
                }
                match mode {
                    DiffMode::Default => {
                        let diff =
                            crate::row_level_diff::row_level_diff(&runtime, &file.vid, from, to)
                                .map_err(|e| {
                                    ErrCtx::PragmaErr(format!("Diff error: {e:?}").into())
                                })?;
                        let tables: Vec<crate::json::JsonTableSummary> = diff
                            .table_changes
                            .iter()
                            .map(|t| {
                                let (inserts, deletes, updates) = count_changes_json(&t.changes);
                                crate::json::JsonTableSummary {
                                    name: t.table_name.clone(),
                                    inserts,
                                    deletes,
                                    updates,
                                }
                            })
                            .collect();
                        let result = crate::json::JsonDiffResult {
                            from_lsn: from.to_u64(),
                            to_lsn: to.to_u64(),
                            logical_status: diff.logical_status().as_str().to_string(),
                            capabilities: json_diff_capabilities(&diff),
                            limitations: json_diff_limitations(&diff),
                            tables,
                            opaque_changes: json_opaque_changes(&diff.opaque_changes),
                        };
                        Ok(Some(serde_json::to_string(&result).map_err(|e| {
                            ErrCtx::PragmaErr(format!("JSON error: {e}").into())
                        })?))
                    }
                    DiffMode::Rows => {
                        let diff =
                            crate::row_level_diff::row_level_diff(&runtime, &file.vid, from, to)
                                .map_err(|e| {
                                    ErrCtx::PragmaErr(format!("Diff error: {e:?}").into())
                                })?;
                        let tables: Vec<crate::json::JsonTableChanges> = diff
                            .table_changes
                            .iter()
                            .map(|t| {
                                let changes: Vec<crate::json::JsonRowChange> = t
                                    .changes
                                    .iter()
                                    .map(|c| match c {
                                        crate::row_level_diff::RowChange::Insert { rowid, row } => {
                                            crate::json::JsonRowChange {
                                                op: "insert".into(),
                                                rowid: *rowid,
                                                values: row
                                                    .values
                                                    .iter()
                                                    .map(crate::json::JsonRowChange::value_to_json)
                                                    .collect(),
                                                old_values: None,
                                            }
                                        }
                                        crate::row_level_diff::RowChange::Delete { rowid, row } => {
                                            crate::json::JsonRowChange {
                                                op: "delete".into(),
                                                rowid: *rowid,
                                                values: row
                                                    .values
                                                    .iter()
                                                    .map(crate::json::JsonRowChange::value_to_json)
                                                    .collect(),
                                                old_values: None,
                                            }
                                        }
                                        crate::row_level_diff::RowChange::Update {
                                            rowid,
                                            old_row,
                                            new_row,
                                        } => crate::json::JsonRowChange {
                                            op: "update".into(),
                                            rowid: *rowid,
                                            values: new_row
                                                .values
                                                .iter()
                                                .map(crate::json::JsonRowChange::value_to_json)
                                                .collect(),
                                            old_values: Some(
                                                old_row
                                                    .values
                                                    .iter()
                                                    .map(crate::json::JsonRowChange::value_to_json)
                                                    .collect(),
                                            ),
                                        },
                                    })
                                    .collect();
                                crate::json::JsonTableChanges {
                                    name: t.table_name.clone(),
                                    columns: t.columns.clone(),
                                    changes,
                                }
                            })
                            .collect();
                        let result = crate::json::JsonRowDiffResult {
                            from_lsn: from.to_u64(),
                            to_lsn: to.to_u64(),
                            logical_status: diff.logical_status().as_str().to_string(),
                            capabilities: json_diff_capabilities(&diff),
                            limitations: json_diff_limitations(&diff),
                            tables,
                            opaque_changes: json_opaque_changes(&diff.opaque_changes),
                        };
                        Ok(Some(serde_json::to_string(&result).map_err(|e| {
                            ErrCtx::PragmaErr(format!("JSON error: {e}").into())
                        })?))
                    }
                }
            }

            GraftPragma::JsonRepoDiff { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot diff while there is an open transaction");
                }
                let mode = spec.mode;
                let kind = spec.kind.map(repo_tracked_path_kind_json_label);
                let repo = repo_for_file(file)?;
                let diff = repo_diff_for_spec(&runtime, file, &repo, spec)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                match mode {
                    DiffMode::Default => Ok(Some(to_json(&JsonRepoDiffOutcome {
                        current_head,
                        current_branch,
                        kind,
                        diff,
                    })?)),
                    DiffMode::Rows => {
                        let rows = json_repo_row_diff(&runtime, &repo, &diff)?;
                        Ok(Some(to_json(&JsonRepoDiffOutcome {
                            current_head,
                            current_branch,
                            kind,
                            diff: rows,
                        })?))
                    }
                }
            }

            GraftPragma::JsonShow { target } => {
                if !file.is_idle() {
                    return pragma_err!("cannot show while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let commit = repo.show_revision(&target)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonRepoShowOutcome {
                    current_head,
                    current_branch,
                    commit,
                })?))
            }

            GraftPragma::VolumeJsonInfo => {
                let result = json_volume_info(&runtime, file)?;
                Ok(Some(serde_json::to_string(&result).map_err(|e| {
                    ErrCtx::PragmaErr(format!("JSON error: {e}").into())
                })?))
            }

            GraftPragma::VolumeTableLog { table } => {
                let entries = table_log_entries(&runtime, &file.vid, &table)?;
                if entries.is_empty() {
                    return Ok(Some(format!("No changes found for table '{table}'.")));
                }
                let mut f = String::new();
                writeln!(&mut f, "Changes for table '{table}':")?;
                writeln!(
                    &mut f,
                    "{:<6} {:<20} {:<10} DETAIL",
                    "LSN", "WHEN", "CHANGES"
                )?;
                writeln!(&mut f, "{}", "-".repeat(75))?;
                for e in entries {
                    writeln!(
                        &mut f,
                        "{:<6} {:<20} {:<10} {}",
                        e.lsn, e.when, e.summary, e.detail
                    )?;
                }
                Ok(Some(f))
            }

            GraftPragma::VolumeJsonTableLog { table } => {
                let entries = table_log_entries(&runtime, &file.vid, &table)?;
                let json_entries: Vec<crate::json::JsonTableLogEntry> = entries
                    .iter()
                    .map(|e| crate::json::JsonTableLogEntry {
                        lsn: e.lsn,
                        timestamp_ms: e.timestamp_ms,
                        summary: e.summary.clone(),
                        detail: e.detail.clone(),
                    })
                    .collect();
                Ok(Some(serde_json::to_string(&json_entries).map_err(|e| {
                    ErrCtx::PragmaErr(format!("JSON error: {e}").into())
                })?))
            }

            GraftPragma::VolumeSetMessage { message } => {
                file.pending_message = Some(message.clone());
                Ok(Some(format!("Commit message set: '{message}'")))
            }
        }
    }
}

#[cfg(test)]
mod tests;
