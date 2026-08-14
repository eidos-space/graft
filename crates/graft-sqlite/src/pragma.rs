use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{Display, Write},
    fs::File,
    io::{Read, Seek, SeekFrom, Write as IoWrite},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{
    error::ErrCtx,
    session::{RepositorySessionContext, should_discover_repo},
};
use graft::core::{
    LogId, PageCount, PageIdx, VolumeId,
    byte_unit::ByteUnit,
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
    RepoPathStorage, RepoSnapshot, RepoStatus, RepoStorageCommit, RepoTextContentDiff,
    RepoTrackedPath, RepoTrackedPathDetail, RepoTrackedPathEntry, RepoTrackedPathKind,
    RepoWorktreeChangeKind, RepoWorktreeFileState, Repository, ResetMode, ResetOutcome, TagInfo,
};
use graft::{rt::runtime::Runtime, volume_reader::VolumeRead, volume_writer::VolumeWrite};
use indoc::formatdoc;
use parking_lot::Mutex;
use rusqlite::config::DbConfig;
use serde::{Deserialize, Serialize};

macro_rules! pluralize {
    ($n:expr, $s:literal) => {
        if $n == 1 { $s } else { concat!($s, "s") }
    };
}

macro_rules! pragma_err {
    ($msg:expr) => {
        Err(ErrCtx::InvalidCommand($msg.into()))
    };
}

mod jobs;
mod json;
mod output_types;
pub(crate) mod parse;
pub(crate) mod repo_checkout;
pub(crate) mod repo_conflicts;
pub(crate) mod repo_core;
pub(crate) mod repo_diff;
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
pub(crate) mod spec;
pub(crate) mod sqlite_worktree;

use self::{
    jobs::*, json::*, output_types::*, parse::*, repo_checkout::*, repo_conflicts::*, repo_core::*,
    repo_diff::*, repo_history::*, repo_merge::*, repo_output::*, repo_paths::*, repo_refs::*,
    repo_remote_output::*, repo_snapshot::*, repo_staging::*, repo_switch::*, repo_sync::*,
    row_diff::*, row_merge_output::*, spec::*, sqlite_worktree::*,
};

const SQLITE_DATABASE_MAGIC: &[u8; 16] = b"SQLite format 3\0";
static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);

/// Format Unix milliseconds as `YYYY-MM-DD HH:MM:SS` without another dependency.
fn format_unix_millis(timestamp_ms: u64) -> String {
    let seconds = (timestamp_ms / 1000) as i64;
    let days = seconds / 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u32;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = (year_of_era as i64) + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    let day_seconds = seconds.rem_euclid(86_400) as u32;
    format!(
        "{year}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        day_seconds / 3_600,
        (day_seconds / 60) % 60,
        day_seconds % 60
    )
}

pub(crate) struct Pragma<'a> {
    name: &'a str,
    arg: Option<&'a str>,
}

#[derive(Debug)]
pub(crate) enum PragmaErr {
    NotFound,
    Fail(Option<String>),
}

impl PragmaErr {
    fn required_arg(pragma: &Pragma<'_>) -> Self {
        Self::Fail(Some(format!(
            "command `{}` requires an argument",
            pragma.name
        )))
    }
}

/// Helper to create pragma errors concisely
fn pragma_fail(msg: impl Display) -> PragmaErr {
    PragmaErr::Fail(Some(msg.to_string()))
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

pub(crate) enum GraftCommand {
    /// `pragma graft_tags;`
    Tags,

    /// `pragma graft_json_tags [= "--with-status"];`
    JsonTags { mode: JsonTagsMode },

    /// `pragma graft_checkout = "[--force] rev [-- path]";`
    RepoCheckout { spec: RepoCheckoutSpec },

    /// `pragma graft_json_checkout = "[--force] rev [-- path]";`
    JsonRepoCheckout { spec: RepoCheckoutSpec },

    /// `pragma graft_restore = "[--source rev] [--expected-head oid] [--require-clean] path|--staged --all [--kind kind]";`
    Restore { spec: RepoRestoreSpec },

    /// `pragma graft_json_restore = "[--source rev] [--expected-head oid] [--require-clean] path|--staged --all [--kind kind]";`
    JsonRestore { spec: RepoRestoreSpec },

    /// `pragma graft_export = "[--source rev] --output output.db [-- path]";`
    Export { spec: RepoExportSpec },

    /// `pragma graft_json_export = "[--source rev] --output output.db [-- path]";`
    JsonExport { spec: RepoExportSpec },

    /// `pragma graft_status [= "[--kind kind]"];`
    Status { spec: StatusSpec },

    /// `pragma graft_init [= "[--worktree] path"];`
    RepoInit { spec: RepoInitSpec },

    /// `pragma graft_json_init [= "[--worktree] path"];`
    JsonRepoInit { spec: RepoInitSpec },

    /// `pragma graft_clone = "[--worktree path] remote-uri [branch]";`
    RepoClone { spec: RepoCloneSpec },

    /// `pragma graft_json_clone = "[--worktree path] remote-uri [branch]";`
    JsonRepoClone { spec: RepoCloneSpec },

    /// `pragma graft_json_status [= "[--kind kind]"];`
    JsonStatus { spec: StatusSpec },

    /// `pragma graft_add = "[--with-status] [--all|-A] [--kind kind]|[--force] [path]";`
    Add { spec: RepoAddSpec },

    /// `pragma graft_json_add = "[--with-status] [--all|-A] [--kind kind]|[--force] [path]";`
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

    /// SDK-only continuation after an exact merge-state token proved the worktree unchanged.
    JsonMergeContinueValidated { message: String },

    /// `pragma graft_conflicts;`
    Conflicts,

    /// `pragma graft_json_conflicts;`
    JsonConflicts,

    /// `pragma graft_resolve = "--ours|--theirs|--manual [path]";`
    Resolve { spec: RepoResolveSpec },

    /// `pragma graft_json_resolve_conflict = "--ours|--theirs|--manual [path]";`
    JsonResolveConflict { spec: RepoResolveSpec },

    /// Typed SDK-only atomic table conflict selection.
    JsonResolveTableConflict {
        path: PathBuf,
        table: String,
        side: ResolveSide,
    },

    /// Typed SDK-only path resolve-undo operation.
    JsonUnresolveConflict { path: PathBuf },

    /// Typed SDK-only journal update after staging an edited path result.
    JsonRecordMergePathResolution {
        path: PathBuf,
        resolution: &'static str,
    },

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

    /// `pragma graft_gc [= "[--dry-run|--force]"];`
    StorageGc { spec: StorageGcSpec },

    /// `pragma graft_json_gc [= "[--dry-run|--force]"];`
    JsonStorageGc { spec: StorageGcSpec },

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

    /// `pragma graft_log;`
    /// Display repository commit history
    Log,

    /// `pragma graft_reset = "[--soft|--mixed|--hard] rev";`
    /// Reset the current repository branch to a revision
    Reset { rev: String, mode: ResetMode },

    /// `pragma graft_json_reset = "[--soft|--mixed|--hard] rev";`
    /// Reset the current repository branch to a revision and return JSON
    JsonReset { rev: String, mode: ResetMode },

    /// `pragma graft_diff = "[--rows] [--kind kind] [--staged] [rev] [rev] [-- path]";`
    /// Compare repository commits by revision syntax
    RepoDiff { spec: RepoDiffSpec },

    /// `pragma graft_show = "rev";`
    /// Display detailed info for specified revision
    Show { target: String },

    // JSON output variants (non-breaking additions)
    /// `pragma graft_json_log [= "--with-status [--limit n] [--after oid]"];`
    /// Repository commit history as JSON array, or app-facing JSON object with status
    JsonLog { spec: JsonLogSpec },

    /// `pragma graft_json_diff = "[--rows] [--content [--max-content-bytes bytes]] [--kind kind] [--staged] [rev] [rev] [-- path] | --root rev [-- path]";`
    /// Repository diff as JSON
    JsonRepoDiff { spec: RepoDiffSpec },

    /// `pragma graft_json_show = "rev";`
    /// Commit details as JSON
    JsonShow { target: String },
}

impl GraftCommand {
    pub(crate) fn parse(p: &Pragma<'_>) -> Result<Self, PragmaErr> {
        if let Some((prefix, suffix)) = p.name.split_once("_")
            && prefix == "graft"
        {
            return match suffix {
                "tags" => Ok(GraftCommand::Tags),
                "json_tags" => Ok(GraftCommand::JsonTags { mode: parse_json_tags_arg(p.arg)? }),
                "checkout" => {
                    let arg = p.require_arg()?;
                    let spec = parse_repo_checkout_arg(arg)?;
                    Ok(GraftCommand::RepoCheckout { spec })
                }
                "json_checkout" => {
                    let arg = p.require_arg()?;
                    let spec = parse_repo_checkout_arg(arg)?;
                    Ok(GraftCommand::JsonRepoCheckout { spec })
                }
                "restore" => {
                    let arg = p.require_arg()?;
                    let spec = parse_repo_restore_arg(arg)?;
                    Ok(GraftCommand::Restore { spec })
                }
                "json_restore" => {
                    let arg = p.require_arg()?;
                    let spec = parse_repo_restore_arg(arg)?;
                    Ok(GraftCommand::JsonRestore { spec })
                }
                "export" => {
                    let arg = p.require_arg()?;
                    let spec = parse_repo_export_arg(arg)?;
                    Ok(GraftCommand::Export { spec })
                }
                "json_export" => {
                    let arg = p.require_arg()?;
                    let spec = parse_repo_export_arg(arg)?;
                    Ok(GraftCommand::JsonExport { spec })
                }
                "status" => Ok(GraftCommand::Status { spec: parse_status_arg(p.arg)? }),
                "init" => Ok(GraftCommand::RepoInit { spec: parse_repo_init_arg(p.arg)? }),
                "json_init" => Ok(GraftCommand::JsonRepoInit { spec: parse_repo_init_arg(p.arg)? }),
                "clone" => {
                    let spec = parse_repo_clone_arg(p.require_arg()?)?;
                    Ok(GraftCommand::RepoClone { spec })
                }
                "json_clone" => {
                    let spec = parse_repo_clone_arg(p.require_arg()?)?;
                    Ok(GraftCommand::JsonRepoClone { spec })
                }
                "json_status" => Ok(GraftCommand::JsonStatus { spec: parse_status_arg(p.arg)? }),
                "add" => Ok(GraftCommand::Add { spec: parse_repo_add_arg(p.arg)? }),
                "json_add" => Ok(GraftCommand::JsonAdd { spec: parse_repo_add_arg(p.arg)? }),
                "rm" => Ok(GraftCommand::Remove { spec: parse_repo_remove_arg(p.arg)? }),
                "json_rm" => Ok(GraftCommand::JsonRemove { spec: parse_repo_remove_arg(p.arg)? }),
                "commit" => Ok(GraftCommand::Commit { message: p.require_arg()?.to_string() }),
                "json_commit" => {
                    Ok(GraftCommand::JsonCommit { message: p.require_arg()?.to_string() })
                }
                "branch" => Ok(GraftCommand::Branch { mode: parse_branch_list_mode(p.arg)? }),
                "json_branch" => {
                    Ok(GraftCommand::JsonBranch { mode: parse_branch_list_mode(p.arg)? })
                }
                "branch_create" => {
                    let (name, start_point) = parse_branch_create_arg(p.require_arg()?)?;
                    Ok(GraftCommand::BranchCreate { name, start_point })
                }
                "json_branch_create" => {
                    let (name, start_point) = parse_branch_create_arg(p.require_arg()?)?;
                    Ok(GraftCommand::JsonBranchCreate { name, start_point })
                }
                "branch_delete" => {
                    let (name, force) = parse_branch_delete_arg(p.require_arg()?)?;
                    Ok(GraftCommand::BranchDelete { name, force })
                }
                "json_branch_delete" => {
                    let (name, force) = parse_branch_delete_arg(p.require_arg()?)?;
                    Ok(GraftCommand::JsonBranchDelete { name, force })
                }
                "branch_rename" => {
                    let (old, new, force) = parse_branch_rename_arg(p.require_arg()?)?;
                    Ok(GraftCommand::BranchRename { old, new, force })
                }
                "json_branch_rename" => {
                    let (old, new, force) = parse_branch_rename_arg(p.require_arg()?)?;
                    Ok(GraftCommand::JsonBranchRename { old, new, force })
                }
                "branch_upstream" => {
                    let (branch, remote, remote_branch) =
                        parse_branch_upstream_arg(p.require_arg()?)?;
                    Ok(GraftCommand::BranchUpstream { branch, remote, remote_branch })
                }
                "json_branch_upstream" => {
                    let (branch, remote, remote_branch) =
                        parse_branch_upstream_arg(p.require_arg()?)?;
                    Ok(GraftCommand::JsonBranchUpstream { branch, remote, remote_branch })
                }
                "branch_unset_upstream" => {
                    Ok(GraftCommand::BranchUnsetUpstream { branch: p.arg.map(str::to_string) })
                }
                "json_branch_unset_upstream" => {
                    Ok(GraftCommand::JsonBranchUnsetUpstream { branch: p.arg.map(str::to_string) })
                }
                "tag_create" => {
                    let (name, target, message) = parse_tag_create_arg(p.require_arg()?)?;
                    Ok(GraftCommand::TagCreate { name, target, message })
                }
                "json_tag_create" => {
                    let (name, target, message) = parse_tag_create_arg(p.require_arg()?)?;
                    Ok(GraftCommand::JsonTagCreate { name, target, message })
                }
                "tag_delete" => Ok(GraftCommand::TagDelete { name: p.require_arg()?.to_string() }),
                "json_tag_delete" => {
                    Ok(GraftCommand::JsonTagDelete { name: p.require_arg()?.to_string() })
                }
                "switch_branch" => {
                    let (name, force) = parse_switch_branch_arg(p.require_arg()?)?;
                    Ok(GraftCommand::SwitchBranch { name, force })
                }
                "json_switch_branch" => {
                    let (name, force) = parse_switch_branch_arg(p.require_arg()?)?;
                    Ok(GraftCommand::JsonSwitchBranch { name, force })
                }
                "switch_create" => {
                    let (name, start_point, force) = parse_switch_create_arg(p.require_arg()?)?;
                    Ok(GraftCommand::SwitchCreate { name, start_point, force })
                }
                "json_switch_create" => {
                    let (name, start_point, force) = parse_switch_create_arg(p.require_arg()?)?;
                    Ok(GraftCommand::JsonSwitchCreate { name, start_point, force })
                }
                "merge" => Ok(GraftCommand::Merge { rev: p.require_arg()?.to_string() }),
                "json_merge" => Ok(GraftCommand::JsonMerge { rev: p.require_arg()?.to_string() }),
                "merge_abort" => Ok(GraftCommand::MergeAbort),
                "json_merge_abort" => Ok(GraftCommand::JsonMergeAbort),
                "merge_continue" => {
                    Ok(GraftCommand::MergeContinue { message: p.require_arg()?.to_string() })
                }
                "json_merge_continue" => {
                    Ok(GraftCommand::JsonMergeContinue { message: p.require_arg()?.to_string() })
                }
                "conflicts" => Ok(GraftCommand::Conflicts),
                "json_conflicts" => Ok(GraftCommand::JsonConflicts),
                "resolve" => Ok(GraftCommand::Resolve {
                    spec: parse_repo_resolve_arg(p.require_arg()?)?,
                }),
                "json_resolve_conflict" => Ok(GraftCommand::JsonResolveConflict {
                    spec: parse_repo_resolve_arg(p.require_arg()?)?,
                }),
                "remote_add" => {
                    let (name, config) = parse_remote_add(p.require_arg()?)?;
                    Ok(GraftCommand::RemoteAdd { name, config })
                }
                "json_remote_add" => {
                    let (name, config) = parse_remote_add(p.require_arg()?)?;
                    Ok(GraftCommand::JsonRemoteAdd { name, config })
                }
                "remote_remove" => {
                    Ok(GraftCommand::RemoteRemove { name: p.require_arg()?.to_string() })
                }
                "json_remote_remove" => {
                    Ok(GraftCommand::JsonRemoteRemove { name: p.require_arg()?.to_string() })
                }
                "remote_rename" => {
                    let (old, new) = parse_remote_rename(p.require_arg()?)?;
                    Ok(GraftCommand::RemoteRename { old, new })
                }
                "json_remote_rename" => {
                    let (old, new) = parse_remote_rename(p.require_arg()?)?;
                    Ok(GraftCommand::JsonRemoteRename { old, new })
                }
                "remote_get_url" => {
                    Ok(GraftCommand::RemoteGetUrl { name: p.require_arg()?.to_string() })
                }
                "json_remote_get_url" => {
                    Ok(GraftCommand::JsonRemoteGetUrl { name: p.require_arg()?.to_string() })
                }
                "remote_set_url" => {
                    let (name, config) = parse_remote_add(p.require_arg()?)?;
                    Ok(GraftCommand::RemoteSetUrl { name, config })
                }
                "json_remote_set_url" => {
                    let (name, config) = parse_remote_add(p.require_arg()?)?;
                    Ok(GraftCommand::JsonRemoteSetUrl { name, config })
                }
                "remote_prune" => {
                    Ok(GraftCommand::RemotePrune { name: p.require_arg()?.to_string() })
                }
                "json_remote_prune" => {
                    Ok(GraftCommand::JsonRemotePrune { name: p.require_arg()?.to_string() })
                }
                "ls_remote" => Ok(GraftCommand::LsRemote { name: p.require_arg()?.to_string() }),
                "json_ls_remote" => {
                    Ok(GraftCommand::JsonLsRemote { name: p.require_arg()?.to_string() })
                }
                "remotes" => Ok(GraftCommand::Remotes),
                "json_remotes" => Ok(GraftCommand::JsonRemotes),
                "fetch" => {
                    let arg = parse_remote_branch_arg(p.arg)?;
                    if arg.force {
                        return Err(pragma_fail("fetch does not support --force"));
                    }
                    let RemoteBranchArg { remote, branch, refspec, all, .. } = arg;
                    Ok(GraftCommand::Fetch { remote, branch, refspec, all })
                }
                "json_fetch" => {
                    let arg = parse_remote_branch_arg(p.arg)?;
                    if arg.force {
                        return Err(pragma_fail("json_fetch does not support --force"));
                    }
                    let RemoteBranchArg { remote, branch, refspec, all, .. } = arg;
                    Ok(GraftCommand::JsonFetch { remote, branch, refspec, all })
                }
                "fetch_async" => {
                    let arg = parse_remote_branch_arg(p.arg)?;
                    if arg.force {
                        return Err(pragma_fail("fetch_async does not support --force"));
                    }
                    let RemoteBranchArg { remote, branch, refspec, all, .. } = arg;
                    Ok(GraftCommand::FetchAsync { remote, branch, refspec, all })
                }
                "json_fetch_async" => {
                    let (arg, mode) = parse_json_fetch_async_arg(p.arg)?;
                    if arg.force {
                        return Err(pragma_fail("json_fetch_async does not support --force"));
                    }
                    let RemoteBranchArg { remote, branch, refspec, all, .. } = arg;
                    Ok(GraftCommand::JsonFetchAsync { remote, branch, refspec, all, mode })
                }
                "job_status" => Ok(GraftCommand::JobStatus { id: p.require_arg()?.to_string() }),
                "json_job_status" => {
                    Ok(GraftCommand::JsonJobStatus { id: p.require_arg()?.to_string() })
                }
                "job_result" => Ok(GraftCommand::JobResult { id: p.require_arg()?.to_string() }),
                "json_job_result" => {
                    Ok(GraftCommand::JsonJobResult { id: p.require_arg()?.to_string() })
                }
                "pull" => {
                    let arg = parse_remote_branch_arg(p.arg)?;
                    if arg.force {
                        return Err(pragma_fail("pull does not support --force"));
                    }
                    let RemoteBranchArg { remote, branch, refspec, all, .. } = arg;
                    Ok(GraftCommand::Pull { remote, branch, refspec, all })
                }
                "json_pull" => {
                    let arg = parse_remote_branch_arg(p.arg)?;
                    if arg.force {
                        return Err(pragma_fail("json_pull does not support --force"));
                    }
                    let RemoteBranchArg { remote, branch, refspec, all, .. } = arg;
                    Ok(GraftCommand::JsonPull { remote, branch, refspec, all })
                }
                "push" => {
                    let RemoteBranchArg { remote, branch, refspec, all, force } =
                        parse_remote_branch_arg(p.arg)?;
                    Ok(GraftCommand::Push { remote, branch, refspec, all, force })
                }
                "json_push" => {
                    let RemoteBranchArg { remote, branch, refspec, all, force } =
                        parse_remote_branch_arg(p.arg)?;
                    Ok(GraftCommand::JsonPush { remote, branch, refspec, all, force })
                }
                "audit" => Ok(GraftCommand::RepoAudit { spec: parse_repo_audit_arg(p.arg)? }),
                "json_audit" => {
                    Ok(GraftCommand::JsonRepoAudit { spec: parse_repo_audit_arg(p.arg)? })
                }
                "lfs_fetch" | "payload_fetch" => {
                    Ok(GraftCommand::LargeFileFetch { spec: parse_lfs_fetch_arg(p.arg)? })
                }
                "json_lfs_fetch" => Ok(GraftCommand::JsonLargeFileFetch {
                    spec: parse_lfs_fetch_arg(p.arg)?,
                    operation: "lfs_fetch",
                }),
                "json_payload_fetch" => Ok(GraftCommand::JsonLargeFileFetch {
                    spec: parse_lfs_fetch_arg(p.arg)?,
                    operation: "payload_fetch",
                }),
                "lfs_status" | "payload_status" => {
                    Ok(GraftCommand::LargeFileStatus { spec: parse_lfs_status_arg(p.arg)? })
                }
                "json_lfs_status" => Ok(GraftCommand::JsonLargeFileStatus {
                    spec: parse_lfs_status_arg(p.arg)?,
                    operation: "lfs_status",
                }),
                "json_payload_status" => Ok(GraftCommand::JsonLargeFileStatus {
                    spec: parse_lfs_status_arg(p.arg)?,
                    operation: "payload_status",
                }),
                "lfs_prune" | "payload_prune" => {
                    Ok(GraftCommand::LargeFilePrune { spec: parse_lfs_prune_arg(p.arg)? })
                }
                "json_lfs_prune" => Ok(GraftCommand::JsonLargeFilePrune {
                    spec: parse_lfs_prune_arg(p.arg)?,
                    operation: "lfs_prune",
                }),
                "json_payload_prune" => Ok(GraftCommand::JsonLargeFilePrune {
                    spec: parse_lfs_prune_arg(p.arg)?,
                    operation: "payload_prune",
                }),
                "gc" => Ok(GraftCommand::StorageGc { spec: parse_storage_gc_arg(p.arg)? }),
                "json_gc" => Ok(GraftCommand::JsonStorageGc { spec: parse_storage_gc_arg(p.arg)? }),
                "ls_files" => Ok(GraftCommand::LsFiles { spec: parse_ls_files_arg(p.arg)? }),
                "json_ls_files" => {
                    Ok(GraftCommand::JsonLsFiles { spec: parse_ls_files_arg(p.arg)? })
                }
                "config_get" => Ok(GraftCommand::ConfigGet { key: p.require_arg()?.to_string() }),
                "json_config_get" => {
                    Ok(GraftCommand::JsonConfigGet { key: p.require_arg()?.to_string() })
                }
                "config_list" => Ok(GraftCommand::ConfigList),
                "json_config_list" => {
                    Ok(GraftCommand::JsonConfigList { mode: parse_json_config_list_arg(p.arg)? })
                }
                "config_set" => {
                    let (key, value) = parse_repo_config_set_arg(p.require_arg()?)?;
                    Ok(GraftCommand::ConfigSet { key, value })
                }
                "json_config_set" => {
                    let (key, value) = parse_repo_config_set_arg(p.require_arg()?)?;
                    Ok(GraftCommand::JsonConfigSet { key, value })
                }
                "config_unset" => {
                    Ok(GraftCommand::ConfigUnset { key: p.require_arg()?.to_string() })
                }
                "json_config_unset" => {
                    Ok(GraftCommand::JsonConfigUnset { key: p.require_arg()?.to_string() })
                }
                "log" => Ok(GraftCommand::Log),
                "reset" => {
                    let (mode, rev) = parse_repo_reset_arg(p.require_arg()?)?;
                    Ok(GraftCommand::Reset { rev, mode })
                }
                "json_reset" => {
                    let (mode, rev) = parse_repo_reset_arg(p.require_arg()?)?;
                    Ok(GraftCommand::JsonReset { rev, mode })
                }
                "diff" => {
                    let spec = parse_repo_diff_arg(p.arg)?;
                    Ok(GraftCommand::RepoDiff { spec })
                }
                "show" => Ok(GraftCommand::Show { target: p.require_arg()?.to_string() }),
                "json_log" => Ok(GraftCommand::JsonLog { spec: parse_json_log_arg(p.arg)? }),
                "json_diff" => {
                    let spec = parse_repo_diff_arg(p.arg)?;
                    Ok(GraftCommand::JsonRepoDiff { spec })
                }
                "json_show" => Ok(GraftCommand::JsonShow { target: p.require_arg()?.to_string() }),
                _ => Err(pragma_fail(format!("invalid graft pragma `{}`", p.name))),
            };
        }
        Err(PragmaErr::NotFound)
    }

    pub(crate) fn parse_repository(name: &str, argument: Option<&str>) -> Result<Self, ErrCtx> {
        let full_name = format!("graft_{name}");
        let input = Pragma { name: &full_name, arg: argument };
        let command = Self::parse(&input).map_err(|error| match error {
            PragmaErr::NotFound => ErrCtx::UnknownCommand,
            PragmaErr::Fail(message) => ErrCtx::InvalidCommand(
                message
                    .unwrap_or_else(|| "invalid repository command".to_string())
                    .into(),
            ),
        })?;
        Ok(command)
    }
}

impl GraftCommand {
    pub fn eval(
        self,
        _runtime: &Runtime,
        file: &mut RepositorySessionContext,
    ) -> Result<Option<String>, ErrCtx> {
        let runtime = file.runtime().clone();
        match self {
            GraftCommand::Tags => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_repo_tags(&repo.tags()?)?))
            }
            GraftCommand::JsonTags { mode } => {
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
            GraftCommand::RepoCheckout { spec } => {
                let outcome = run_repo_checkout(&runtime, file, spec)?;
                Ok(Some(format_checkout_outcome(&outcome)))
            }
            GraftCommand::JsonRepoCheckout { spec } => {
                let outcome = run_repo_checkout(&runtime, file, spec)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftCommand::Restore { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot restore while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let outcome = restore_repo_path(&runtime, file, &repo, &spec)?;
                Ok(Some(format_restore_outcome(&outcome)))
            }
            GraftCommand::JsonRestore { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot restore while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let outcome = restore_repo_path(&runtime, file, &repo, &spec)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftCommand::Export { spec } => {
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
            GraftCommand::JsonExport { spec } => {
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

            GraftCommand::Status { spec } => {
                let repo = repo_for_file(file)?;
                let status = repo_status_for_file(&runtime, file, &repo)?;
                let status = filter_repo_status_by_kind(status, spec.kind);
                Ok(Some(format_repo_status(&status)?))
            }
            GraftCommand::RepoInit { spec } => {
                let outcome = run_repo_init(file, spec)?;
                Ok(Some(format_repo_init_outcome(&outcome)))
            }
            GraftCommand::JsonRepoInit { spec } => {
                let outcome = run_repo_init(file, spec)?;
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

            GraftCommand::RepoClone { spec } => {
                let outcome = run_repo_clone(file, spec)?;
                Ok(Some(format!(
                    "Cloned origin/{} at {} into {}",
                    outcome.branch,
                    &outcome.head[..outcome.head.len().min(12)],
                    outcome.graft_dir.display()
                )))
            }
            GraftCommand::JsonRepoClone { spec } => {
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

            GraftCommand::JsonStatus { spec } => {
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

            GraftCommand::Add { spec } => {
                let entries = run_repo_add(&runtime, file, &spec)?;
                Ok(Some(format_added_entries(&entries)))
            }
            GraftCommand::JsonAdd { spec } => {
                let entries = run_repo_add(&runtime, file, &spec)?;
                let repo = repo_for_file(file)?;
                let kind = spec.kind.map(repo_tracked_path_kind_json_label);
                let status = if spec.with_status {
                    let status = repo_status_for_file(&runtime, file, &repo)?;
                    let status = filter_repo_status_by_kind(status, spec.kind);
                    let current_head = status.head_target.clone();
                    let current_branch = repo.current_branch()?;
                    let conflict_analysis =
                        current_file_status_row_merge_analysis_lossy(&runtime, file, &repo, None);
                    Some(JsonRepoStatus {
                        current_head,
                        current_branch,
                        kind,
                        status,
                        conflict_analysis,
                    })
                } else {
                    None
                };
                let (current_head, current_branch) = match status.as_ref() {
                    Some(status) => (status.current_head.clone(), status.current_branch.clone()),
                    None => repo_head_and_branch(&repo)?,
                };
                Ok(Some(to_json(&JsonAddOutcome {
                    operation: "add",
                    current_head,
                    current_branch,
                    kind,
                    paths: json_staged_entry_paths(&repo, &entries)?,
                    status,
                })?))
            }

            GraftCommand::Remove { spec } => {
                let paths = run_repo_remove(&runtime, file, &spec)?;
                Ok(Some(format_removed_paths(&paths)))
            }
            GraftCommand::JsonRemove { spec } => {
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

            GraftCommand::Commit { message } => {
                let outcome = run_repo_commit(&runtime, file, message)?;
                let commit = outcome.commit;
                Ok(Some(format!("[{}] {}", &commit.id[..12], commit.message)))
            }
            GraftCommand::JsonCommit { message } => {
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

            GraftCommand::Branch { mode } => {
                let repo = repo_for_file(file)?;
                let branches = repo.branches()?;
                let remote_branches = if mode.includes_remote() {
                    repo.remote_tracking_branches()?
                } else {
                    Vec::new()
                };
                Ok(Some(format_branches(&branches, &remote_branches, mode)?))
            }
            GraftCommand::JsonBranch { mode } => {
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

            GraftCommand::BranchCreate { name, start_point } => {
                let branch = run_repo_branch_create(file, name, start_point)?;
                Ok(Some(format_branch_created(&branch)))
            }
            GraftCommand::JsonBranchCreate { name, start_point } => {
                let branch = run_repo_branch_create(file, name, start_point)?;
                let outcome = json_branch_mutation_outcome(file, "branch_create", branch, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftCommand::BranchDelete { name, force } => {
                let branch = run_repo_branch_delete(file, name, force)?;
                Ok(Some(format_branch_deleted(&branch, force)))
            }
            GraftCommand::JsonBranchDelete { name, force } => {
                let branch = run_repo_branch_delete(file, name, force)?;
                let outcome = json_branch_mutation_outcome(file, "branch_delete", branch, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftCommand::BranchRename { old, new, force } => {
                let (old, branch) = run_repo_branch_rename(file, old, new, force)?;
                Ok(Some(format_branch_renamed(&old, &branch, force)))
            }
            GraftCommand::JsonBranchRename { old, new, force } => {
                let (old, branch) = run_repo_branch_rename(file, old, new, force)?;
                let outcome =
                    json_branch_mutation_outcome(file, "branch_rename", branch, Some(old))?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftCommand::BranchUpstream { branch, remote, remote_branch } => {
                let branch = run_repo_branch_upstream(file, branch, remote, remote_branch)?;
                Ok(Some(format_branch_upstream(&branch)))
            }
            GraftCommand::JsonBranchUpstream { branch, remote, remote_branch } => {
                let branch = run_repo_branch_upstream(file, branch, remote, remote_branch)?;
                let outcome = json_branch_mutation_outcome(file, "branch_upstream", branch, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftCommand::BranchUnsetUpstream { branch } => {
                let branch = run_repo_branch_unset_upstream(file, branch)?;
                Ok(Some(format_branch_upstream_unset(&branch)))
            }
            GraftCommand::JsonBranchUnsetUpstream { branch } => {
                let branch = run_repo_branch_unset_upstream(file, branch)?;
                let outcome =
                    json_branch_mutation_outcome(file, "branch_unset_upstream", branch, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftCommand::TagCreate { name, target, message } => {
                let tag = run_repo_tag_create(file, name, target, message)?;
                Ok(Some(format_tag_created(&tag)))
            }
            GraftCommand::JsonTagCreate { name, target, message } => {
                let tag = run_repo_tag_create(file, name, target, message)?;
                let outcome = json_tag_mutation_outcome(file, "tag_create", tag)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftCommand::TagDelete { name } => {
                let tag = run_repo_tag_delete(file, name)?;
                Ok(Some(format_tag_deleted(&tag)))
            }
            GraftCommand::JsonTagDelete { name } => {
                let tag = run_repo_tag_delete(file, name)?;
                let outcome = json_tag_mutation_outcome(file, "tag_delete", tag)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftCommand::SwitchBranch { name, force } => {
                run_repo_switch_branch(&runtime, file, name.clone(), force)?;
                Ok(Some(format!("Switched to branch '{name}'")))
            }
            GraftCommand::JsonSwitchBranch { name, force } => {
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

            GraftCommand::SwitchCreate { name, start_point, force } => {
                let outcome = run_repo_switch_create(&runtime, file, name, start_point, force)?;
                Ok(Some(format_branch_created(&outcome.branch)))
            }
            GraftCommand::JsonSwitchCreate { name, start_point, force } => {
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

            GraftCommand::Merge { rev } => {
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
            GraftCommand::JsonMerge { rev } => {
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

            GraftCommand::MergeAbort => {
                let outcome = run_repo_merge_abort(&runtime, file)?;
                Ok(Some(format!(
                    "Aborted merge; reset HEAD to {}",
                    &outcome.target[..outcome.target.len().min(12)]
                )))
            }
            GraftCommand::JsonMergeAbort => {
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

            GraftCommand::MergeContinue { message } => {
                let outcome = run_repo_merge_continue(&runtime, file, message, true)?;
                let commit = outcome.commit;
                Ok(Some(format!(
                    "Merge commit [{}] {}",
                    &commit.id[..commit.id.len().min(12)],
                    commit.message
                )))
            }
            GraftCommand::JsonMergeContinue { message } => {
                let outcome = run_repo_merge_continue(&runtime, file, message, true)?;
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
            GraftCommand::JsonMergeContinueValidated { message } => {
                let outcome = run_repo_merge_continue(&runtime, file, message, false)?;
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

            GraftCommand::Conflicts => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_conflicts(&repo.status()?)?))
            }

            GraftCommand::JsonConflicts => {
                let repo = repo_for_file(file)?;
                let remote = repo_default_remote_store(&repo);
                Ok(Some(to_json(&repo_conflict_artifacts(
                    &runtime, file, &repo, remote,
                )?)?))
            }

            GraftCommand::Resolve { spec } => {
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

            GraftCommand::JsonResolveConflict { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot resolve while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let side = spec.side;
                let outcome = resolve_repo_conflict_for_file(&runtime, file, &repo, spec)?;
                let remote = repo_default_remote_store(&repo);
                let remaining_conflicts =
                    unresolved_conflict_artifact_count(&runtime, file, &repo, remote)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonResolveConflictOutcome {
                    operation: "resolve_conflict",
                    current_head,
                    current_branch,
                    path: outcome.path,
                    path_kind: repo_tracked_path_kind_json_label(outcome.path_kind),
                    storage: repo_path_storage_json_label(outcome.path_storage),
                    resolution: side.label(),
                    materialized: outcome.materialized,
                    remaining_conflicts,
                })?))
            }

            GraftCommand::JsonResolveTableConflict { path, table, side } => {
                if !file.is_idle() {
                    return pragma_err!("cannot resolve while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let outcome =
                    resolve_repo_table_conflicts(&runtime, file, &repo, &path, &table, side)?;
                let remote = repo_default_remote_store(&repo);
                let remaining_conflicts =
                    unresolved_conflict_artifact_count(&runtime, file, &repo, remote)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&serde_json::json!({
                    "operation": "resolve_merge_table",
                    "current_head": current_head,
                    "current_branch": current_branch,
                    "path": outcome.path,
                    "path_kind": repo_tracked_path_kind_json_label(outcome.path_kind),
                    "storage": repo_path_storage_json_label(outcome.path_storage),
                    "table": table,
                    "resolution": side.label(),
                    "materialized": outcome.materialized,
                    "remaining_conflicts": remaining_conflicts,
                }))?))
            }

            GraftCommand::JsonUnresolveConflict { path } => {
                if !file.is_idle() {
                    return pragma_err!("cannot unresolve while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let outcome = unresolve_repo_conflict_for_file(&runtime, file, &repo, &path)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&serde_json::json!({
                    "operation": "unresolve_merge_path",
                    "current_head": current_head,
                    "current_branch": current_branch,
                    "path": outcome.path,
                    "path_kind": repo_tracked_path_kind_json_label(outcome.path_kind),
                    "storage": repo_path_storage_json_label(outcome.path_storage),
                    "resolution": "unresolved",
                    "materialized": outcome.materialized,
                }))?))
            }

            GraftCommand::JsonRecordMergePathResolution { path, resolution } => {
                let repo = repo_for_file(file)?;
                let (key, _) = repo_physical_path_arg(&repo, &path)?;
                set_merge_path_resolution(&repo, &key, Some(resolution))?;
                Ok(Some(to_json(&serde_json::json!({
                    "operation": "record_merge_path_resolution",
                    "path": key,
                    "resolution": resolution,
                }))?))
            }

            GraftCommand::RemoteAdd { name, config } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_add(&name, config)?;
                Ok(Some(format_remote(&remote)))
            }
            GraftCommand::JsonRemoteAdd { name, config } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_add(&name, config)?;
                let outcome = json_remote_mutation_outcome(file, "remote_add", remote, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftCommand::RemoteRemove { name } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_remove(&name)?;
                Ok(Some(format!("Removed remote '{}'", remote.name)))
            }
            GraftCommand::JsonRemoteRemove { name } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_remove(&name)?;
                let outcome = json_remote_mutation_outcome(file, "remote_remove", remote, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftCommand::RemoteRename { old, new } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_rename(&old, &new)?;
                Ok(Some(format!(
                    "Renamed remote '{}' to '{}': {}",
                    old,
                    remote.name,
                    remote_config_uri(&remote.config)
                )))
            }
            GraftCommand::JsonRemoteRename { old, new } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_rename(&old, &new)?;
                let outcome =
                    json_remote_mutation_outcome(file, "remote_rename", remote, Some(old))?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftCommand::RemoteGetUrl { name } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_get_url(&name)?;
                Ok(Some(remote_config_uri(&remote.config)))
            }
            GraftCommand::JsonRemoteGetUrl { name } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_get_url(&name)?;
                let outcome = json_remote_mutation_outcome(file, "remote_get_url", remote, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftCommand::RemoteSetUrl { name, config } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_set_url(&name, config)?;
                Ok(Some(format!(
                    "Updated remote '{}': {}",
                    remote.name,
                    remote_config_uri(&remote.config)
                )))
            }
            GraftCommand::JsonRemoteSetUrl { name, config } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_set_url(&name, config)?;
                let outcome = json_remote_mutation_outcome(file, "remote_set_url", remote, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftCommand::RemotePrune { name } => {
                let repo = repo_for_file(file)?;
                let outcome = repo.remote_prune(&name)?;
                Ok(Some(format_remote_prune_outcome(&outcome)?))
            }
            GraftCommand::JsonRemotePrune { name } => {
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

            GraftCommand::LsRemote { name } => {
                let repo = repo_for_file(file)?;
                let default_branch = repo.remote_default_branch(&name)?;
                let refs = repo.remote_branch_refs(&name)?;
                Ok(Some(format_ls_remote(
                    &name,
                    default_branch.as_deref(),
                    &refs,
                )?))
            }
            GraftCommand::JsonLsRemote { name } => {
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

            GraftCommand::Remotes => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_remotes(&repo.remotes()?)?))
            }
            GraftCommand::JsonRemotes => {
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonRemoteList {
                    current_head,
                    current_branch,
                    remotes: repo.remotes()?.into_iter().map(json_remote_info).collect(),
                })?))
            }

            GraftCommand::Fetch { remote, branch, refspec, all } => {
                let repo = repo_for_file(file)?;
                Ok(Some(run_repo_fetch(&repo, remote, branch, refspec, all)?))
            }
            GraftCommand::JsonFetch { remote, branch, refspec, all } => {
                let repo = repo_for_file(file)?;
                Ok(Some(run_repo_fetch_json(
                    &repo, remote, branch, refspec, all,
                )?))
            }
            GraftCommand::FetchAsync { remote, branch, refspec, all } => {
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
            GraftCommand::JsonFetchAsync { remote, branch, refspec, all, mode } => {
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
            GraftCommand::JobStatus { id } => Ok(Some(async_jobs().status_json(&id)?)),
            GraftCommand::JsonJobStatus { id } => Ok(Some(async_jobs().json_status(&id)?)),
            GraftCommand::JobResult { id } => Ok(Some(async_jobs().result(&id)?)),
            GraftCommand::JsonJobResult { id } => Ok(Some(async_jobs().result(&id)?)),
            GraftCommand::Pull { remote, branch, refspec, all } => {
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
            GraftCommand::JsonPull { remote, branch, refspec, all } => {
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

            GraftCommand::Push { remote, branch, refspec, all, force } => {
                let repo = repo_for_file(file)?;
                let outcome = run_repo_push(&runtime, &repo, remote, branch, refspec, all, force)?;
                Ok(Some(format_push_command_outcome(&outcome)?))
            }
            GraftCommand::JsonPush { remote, branch, refspec, all, force } => {
                let repo = repo_for_file(file)?;
                let outcome = run_repo_push(&runtime, &repo, remote, branch, refspec, all, force)?;
                Ok(Some(to_json(&json_push_command_outcome(&repo, &outcome)?)?))
            }
            GraftCommand::RepoAudit { spec } => {
                let repo = repo_for_file(file)?;
                if spec.repair {
                    let remote = repo_default_remote(&repo, spec.remote.clone())?;
                    let outcome = repo.repair_artifacts_from_remote(&remote)?;
                    Ok(Some(format_repo_artifact_repair(&outcome)?))
                } else {
                    Ok(Some(format_repo_artifact_audit(&repo.audit_artifacts()?)?))
                }
            }
            GraftCommand::JsonRepoAudit { spec } => {
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
            GraftCommand::LargeFileFetch { spec } => {
                let repo = repo_for_file(file)?;
                let remote = repo_default_remote(&repo, spec.remote.clone())?;
                let outcome = repo.fetch_large_file_payloads(&remote, spec.rev.as_deref())?;
                Ok(Some(format_large_file_fetch_outcome(&outcome)?))
            }
            GraftCommand::JsonLargeFileFetch { spec, operation } => {
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
            GraftCommand::LargeFileStatus { spec } => {
                let repo = repo_for_file(file)?;
                let outcome = repo.large_file_payloads_status(spec.rev.as_deref())?;
                Ok(Some(format_large_file_status_outcome(&outcome)?))
            }
            GraftCommand::JsonLargeFileStatus { spec, operation } => {
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
            GraftCommand::LargeFilePrune { spec } => {
                let repo = repo_for_file(file)?;
                let outcome = repo.prune_large_file_payloads(spec.dry_run)?;
                Ok(Some(format_large_file_prune_outcome(&outcome)?))
            }
            GraftCommand::JsonLargeFilePrune { spec, operation } => {
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
            GraftCommand::StorageGc { spec } => {
                let outcome = run_repo_storage_gc(&runtime, file, spec.dry_run)?;
                Ok(Some(format_storage_gc_outcome(&outcome)?))
            }
            GraftCommand::JsonStorageGc { spec } => {
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                let outcome = run_repo_storage_gc(&runtime, file, spec.dry_run)?;
                Ok(Some(to_json(&JsonStorageGcOutcome {
                    operation: "gc",
                    current_head,
                    current_branch,
                    outcome,
                })?))
            }
            GraftCommand::LsFiles { spec } => {
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
            GraftCommand::JsonLsFiles { spec } => {
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
            GraftCommand::ConfigGet { key } => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_repo_config_entry(&repo.config_get(&key)?)?))
            }
            GraftCommand::JsonConfigGet { key } => {
                let repo = repo_for_file(file)?;
                let entry = repo.config_get(&key)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonConfigEntryOutcome {
                    current_head,
                    current_branch,
                    entry,
                })?))
            }
            GraftCommand::ConfigList => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_repo_config_entries(&repo.config_list()?)?))
            }
            GraftCommand::JsonConfigList { mode } => {
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
            GraftCommand::ConfigSet { key, value } => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_repo_config_entry(
                    &repo.config_set(&key, &value)?,
                )?))
            }
            GraftCommand::JsonConfigSet { key, value } => {
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
            GraftCommand::ConfigUnset { key } => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_repo_config_entry(&repo.config_unset(&key)?)?))
            }
            GraftCommand::JsonConfigUnset { key } => {
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

            GraftCommand::Log => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_repo_log(&repo)?))
            }

            GraftCommand::Reset { rev, mode } => {
                let outcome = run_repo_reset(&runtime, file, &rev, mode)?;

                Ok(Some(format!(
                    "Reset HEAD to {} ({})",
                    &outcome.outcome.target[..outcome.outcome.target.len().min(12)],
                    reset_mode_label(mode)
                )))
            }
            GraftCommand::JsonReset { rev, mode } => {
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

            GraftCommand::RepoDiff { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot diff while there is an open transaction");
                }
                if spec.content.is_some() {
                    return pragma_err!("diff --content is only available through graft_json_diff");
                }
                let mode = spec.mode;
                let table = spec.table.clone();
                let repo = repo_for_file(file)?;
                let diff = repo_diff_for_spec(&runtime, file, &repo, spec)?;
                match mode {
                    DiffMode::Default => Ok(Some(format_repo_diff(&diff)?)),
                    DiffMode::Rows => Ok(Some(format_repo_row_diff(
                        &runtime,
                        &repo,
                        &diff,
                        table.as_deref(),
                    )?)),
                    DiffMode::SqliteSummary => {
                        pragma_err!("diff --sqlite-summary requires JSON output")
                    }
                }
            }

            GraftCommand::Show { target } => {
                if !file.is_idle() {
                    return pragma_err!("cannot show while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let commit = repo.show_revision(&target)?;
                Ok(Some(format_repo_show(&commit)?))
            }

            GraftCommand::JsonLog { spec } => {
                let repo = repo_for_file(file)?;
                let (commits, has_more) = match spec.limit {
                    Some(limit) => repo.log_page(limit, spec.after.as_deref())?,
                    None => (repo.log()?, false),
                };
                match spec.mode {
                    JsonLogMode::LegacyArray => Ok(Some(to_json(&commits)?)),
                    JsonLogMode::WithStatus => {
                        let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                        let next_cursor = has_more
                            .then(|| commits.last().map(|commit| commit.id.clone()))
                            .flatten();
                        Ok(Some(to_json(&JsonRepoLogOutcome {
                            current_head,
                            current_branch,
                            commits,
                            next_cursor,
                            has_more,
                        })?))
                    }
                }
            }

            GraftCommand::JsonRepoDiff { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot diff while there is an open transaction");
                }
                let mode = spec.mode;
                let kind = spec.kind.map(repo_tracked_path_kind_json_label);
                let table = spec.table.clone();
                let row_page = spec.row_page.clone();
                let repo = repo_for_file(file)?;
                let content_request = match (&spec.content, &spec.target) {
                    (
                        Some(content),
                        RepoDiffTarget::RevisionToWorktree { path: Some(path), .. }
                        | RepoDiffTarget::Revisions { path: Some(path), .. }
                        | RepoDiffTarget::Root { path: Some(path), .. },
                    ) => Some((repo_path_arg(&repo, path)?, content.max_bytes)),
                    (Some(_), _) => unreachable!("content diff target is validated while parsing"),
                    (None, _) => None,
                };
                let mut diff = repo_diff_for_spec(&runtime, file, &repo, spec)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                match mode {
                    DiffMode::Default => {
                        let content = content_request
                            .map(|(path, max_bytes)| {
                                repo_text_content_for_path(&repo, &mut diff, &path, max_bytes)
                            })
                            .transpose()?;
                        Ok(Some(to_json(&JsonRepoDiffOutcome {
                            current_head,
                            current_branch,
                            kind,
                            diff,
                            content,
                        })?))
                    }
                    DiffMode::Rows => {
                        let rows = if let Some(page) = row_page {
                            let table = table.as_ref().expect("paged rows require one table");
                            let offset = bounded_row_offset(page.after.as_deref(), table)?;
                            let mode = crate::row_level_diff::BoundedRowDiffMode::Rows {
                                table: table.clone(),
                                limit: page.limit,
                                offset,
                            };
                            json_repo_bounded_diff(&runtime, &repo, &diff, &mode)?
                        } else {
                            let rows =
                                json_repo_row_diff(&runtime, &repo, &diff, table.as_deref())?;
                            return Ok(Some(to_json(&JsonRepoDiffOutcome {
                                current_head,
                                current_branch,
                                kind,
                                diff: rows,
                                content: None,
                            })?));
                        };
                        Ok(Some(to_json(&JsonRepoDiffOutcome {
                            current_head,
                            current_branch,
                            kind,
                            diff: rows,
                            content: None,
                        })?))
                    }
                    DiffMode::SqliteSummary => {
                        let mode = crate::row_level_diff::BoundedRowDiffMode::Summary;
                        let summary = json_repo_bounded_diff(&runtime, &repo, &diff, &mode)?;
                        Ok(Some(to_json(&JsonRepoDiffOutcome {
                            current_head,
                            current_branch,
                            kind,
                            diff: summary,
                            content: None,
                        })?))
                    }
                }
            }

            GraftCommand::JsonShow { target } => {
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
        }
    }
}

fn run_repo_storage_gc(
    runtime: &Runtime,
    file: &mut RepositorySessionContext,
    dry_run: bool,
) -> Result<graft::local::fjall_storage::StorageGcOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot run gc while there is an open transaction");
    }

    let repo = repo_for_file(file)?;
    let states = repo.referenced_storage_states()?;
    let root_volumes = states
        .iter()
        .map(|state| state.volume.clone())
        .collect::<BTreeSet<_>>();
    let root_snapshots = states
        .iter()
        .map(|state| state.snapshot.to_snapshot())
        .collect::<Vec<_>>();
    runtime
        .storage_gc(&root_volumes, &root_snapshots, dry_run)
        .map_err(ErrCtx::from)
}

#[cfg(test)]
mod tests;
