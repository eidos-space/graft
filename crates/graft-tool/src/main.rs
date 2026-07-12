use std::{
    io::Read,
    num::NonZeroU64,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use graft::{
    core::{LogId, SegmentId, VolumeId},
    remote::RemoteConfig,
    repo::Repository,
    setup::GraftConfig,
};
use rusqlite::{
    Batch, Connection, OpenFlags, fallible_iterator::FallibleIterator, types::ValueRef,
};

#[derive(Subcommand)]
enum Command {
    /// Generate an internal test identifier
    Id {
        /// Identifier kind to generate
        #[arg(value_enum)]
        kind: IdKind,
    },

    /// Show repository history
    Log {
        /// Emit JSON history with current repository state
        #[arg(long)]
        json: bool,

        /// Return at most this many commits
        #[arg(long)]
        limit: Option<usize>,

        /// Continue after this exact commit id from the previous page
        #[arg(long, requires = "limit")]
        after: Option<String>,
    },

    /// Initialize a .graft repository in the current worktree
    Init(InitArgs),

    /// Execute SQL through the embedded Graft SQLite VFS
    Sql {
        /// SQL to execute. Reads SQL from stdin when omitted.
        #[arg(
            value_name = "SQL",
            num_args = 0..,
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        sql: Vec<String>,
    },

    /// Clone a remote Graft repository into the database path worktree
    Clone {
        /// Emit JSON clone output with current repository state
        #[arg(long)]
        json: bool,

        /// Branch to clone. Defaults to remote HEAD, then main.
        #[arg(short = 'b', long = "branch", conflicts_with = "branch")]
        branch_option: Option<String>,

        /// Remote URI: memory, fs://..., s3://..., s3_compatible://..., graft+https://..., or graft+http://...
        remote: String,

        /// Optional branch to clone. Defaults to remote HEAD, then main.
        branch: Option<String>,
    },

    /// Show repository status
    Status {
        /// Emit JSON status
        #[arg(long)]
        json: bool,

        /// Filter changed paths by kind
        #[arg(long, value_enum)]
        kind: Option<PathKind>,
    },

    /// Audit repository artifact payloads
    Audit(AuditArgs),

    /// List repository path inventory
    LsFiles {
        /// Emit JSON path inventory
        #[arg(long)]
        json: bool,

        /// Show raw index stages, including unmerged conflict entries
        #[arg(short = 's', long, conflicts_with_all = ["details", "others"])]
        stage: bool,

        /// Include artifact object ids, content hashes, and local payload presence
        #[arg(long, conflicts_with = "others")]
        details: bool,

        /// List untracked worktree paths that can be added
        #[arg(long)]
        others: bool,

        /// Filter paths by kind
        #[arg(long, value_enum)]
        kind: Option<PathKind>,
    },

    /// Manage external payload cache
    #[command(alias = "lfs")]
    Payload {
        #[command(subcommand)]
        command: PayloadCommand,
    },

    /// Get or set repository configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },

    /// Stage a database snapshot
    Add(AddArgs),

    /// Stage removal of a database path
    Rm(RmArgs),

    /// Create a Graft commit for the database path
    Commit {
        /// Emit JSON commit output with current repository state
        #[arg(long)]
        json: bool,

        /// Commit message
        #[arg(short, long)]
        message: String,
    },

    /// Compare repository revisions, staged changes, or the worktree
    Diff {
        /// Emit row-level changes for modified SQLite database snapshots
        #[arg(long)]
        rows: bool,

        /// Emit staged diff instead of worktree diff
        #[arg(long, alias = "cached")]
        staged: bool,

        /// Filter changed paths by kind
        #[arg(long, value_enum)]
        kind: Option<PathKind>,

        /// Include bounded UTF-8 content for one changed text path
        #[arg(long, requires = "json", conflicts_with_all = ["rows", "staged"])]
        content: bool,

        /// Maximum content bytes to read from each side
        #[arg(long, requires = "content")]
        max_content_bytes: Option<NonZeroU64>,

        /// Compare the empty tree to this target revision
        #[arg(long, value_name = "TO", conflicts_with = "staged")]
        root: Option<String>,

        /// Source revision, for example HEAD~1
        from: Option<String>,

        /// Target revision, for example HEAD
        to: Option<String>,

        /// Optional repository-relative path
        path: Option<PathBuf>,

        /// Emit JSON diff
        #[arg(long)]
        json: bool,
    },

    /// Show a repository revision
    Show {
        /// Revision, for example HEAD or HEAD~1
        rev: String,

        /// Emit JSON output
        #[arg(long)]
        json: bool,
    },

    /// Checkout a revision, or restore a path from a revision
    Checkout {
        /// Emit JSON checkout output with current repository state
        #[arg(long)]
        json: bool,

        /// Discard staged and unstaged changes before checking out a revision
        #[arg(short = 'f', long, conflicts_with = "path")]
        force: bool,

        /// Revision, for example HEAD~1
        rev: String,

        /// Optional repository-relative path to restore from the revision
        path: Option<PathBuf>,
    },

    /// Restore worktree paths from the index or a revision
    ///
    /// Multi-path restores preflight known path and payload failures before changing files or the
    /// index. They are not cross-path transactions: an unexpected operating-system I/O failure
    /// after apply begins can leave a subset of worktree paths, staged entries, or restore-status
    /// metadata updated. Correct the failure and rerun the same restore command.
    Restore {
        /// Emit JSON restore output with current repository state
        #[arg(long)]
        json: bool,

        /// Restore from this revision instead of the staged index
        #[arg(short = 's', long)]
        source: Option<String>,

        /// Fail unless HEAD still equals this full object id
        #[arg(long, value_name = "OID")]
        expected_head: Option<String>,

        /// Fail if staged or tracked worktree changes are present
        #[arg(long)]
        require_clean: bool,

        /// Restore the staged index entry from HEAD instead of touching the worktree
        #[arg(long, alias = "cached")]
        staged: bool,

        /// Restore all staged index entries
        #[arg(long, conflicts_with = "path", requires = "staged")]
        all: bool,

        /// When restoring all staged entries, filter paths by kind
        #[arg(long, value_enum, requires = "all")]
        kind: Option<PathKind>,

        /// Repository-relative `SQLite` database path to restore
        #[arg(required_unless_present = "all")]
        path: Option<PathBuf>,
    },

    /// Export a Graft database snapshot as a physical SQLite file
    Export(ExportArgs),

    /// Reset the current branch to a repository revision
    Reset {
        /// Emit JSON reset output with current repository state
        #[arg(long)]
        json: bool,

        /// Leave working tree and staged state in place
        #[arg(long, conflicts_with_all = ["mixed", "hard"])]
        soft: bool,

        /// Reset staged state but leave the database contents in place
        #[arg(long, conflicts_with_all = ["soft", "hard"])]
        mixed: bool,

        /// Reset staged state and materialize the target revision
        #[arg(long, conflicts_with_all = ["soft", "mixed"])]
        hard: bool,

        /// Revision, for example HEAD~1
        rev: String,
    },

    /// List branches or create a branch
    Branch {
        /// Emit JSON branch output with current repository state
        #[arg(long)]
        json: bool,

        /// Delete the named branch if it is fully merged
        #[arg(short = 'd', long, conflicts_with_all = ["force_delete", "remote", "all"])]
        delete: bool,

        /// Force-delete the named branch
        #[arg(short = 'D', long = "force-delete", conflicts_with_all = ["delete", "remote", "all"])]
        force_delete: bool,

        /// Rename a branch, or the current branch when only a new name is provided
        #[arg(short = 'm', long = "move", conflicts_with_all = ["delete", "force_delete", "force_move", "set_upstream_to", "unset_upstream", "remote", "all"])]
        move_branch: bool,

        /// Force-rename a branch, replacing the destination if it exists
        #[arg(short = 'M', long = "force-move", conflicts_with_all = ["delete", "force_delete", "move_branch", "set_upstream_to", "unset_upstream", "remote", "all"])]
        force_move: bool,

        /// Set the branch upstream, for example origin/main
        #[arg(short = 'u', long = "set-upstream-to", conflicts_with_all = ["delete", "force_delete", "move_branch", "force_move", "unset_upstream", "remote", "all"])]
        set_upstream_to: Option<String>,

        /// Unset the branch upstream
        #[arg(long, conflicts_with_all = ["delete", "force_delete", "move_branch", "force_move", "set_upstream_to", "remote", "all"])]
        unset_upstream: bool,

        /// List remote-tracking branches
        #[arg(short = 'r', long, conflicts_with_all = ["delete", "force_delete", "move_branch", "force_move", "set_upstream_to", "unset_upstream", "all"])]
        remote: bool,

        /// List local and remote-tracking branches
        #[arg(short = 'a', long, conflicts_with_all = ["delete", "force_delete", "move_branch", "force_move", "set_upstream_to", "unset_upstream", "remote"])]
        all: bool,

        /// Branch name to create, delete, or configure
        name: Option<String>,

        /// Optional start point when creating a branch
        start_point: Option<String>,
    },

    /// List, create, or delete tags
    Tag {
        /// Emit JSON tag output with current repository state
        #[arg(long)]
        json: bool,

        /// List tags explicitly
        #[arg(short = 'l', long, conflicts_with_all = ["delete", "annotated", "message"])]
        list: bool,

        /// Delete the named tag
        #[arg(short = 'd', long, conflicts_with = "list")]
        delete: bool,

        /// Create an annotated tag object
        #[arg(short = 'a', long, conflicts_with_all = ["delete", "list"])]
        annotated: bool,

        /// Annotated tag message
        #[arg(short = 'm', long, conflicts_with_all = ["delete", "list"])]
        message: Option<String>,

        /// Tag name to create or delete
        name: Option<String>,

        /// Optional revision to tag; defaults to HEAD
        rev: Option<String>,
    },

    /// Switch branches
    Switch {
        /// Emit JSON switch output with current repository state
        #[arg(long)]
        json: bool,

        /// Create the branch before switching
        #[arg(short = 'c', long)]
        create: bool,

        /// Discard staged and unstaged changes before switching
        #[arg(short = 'f', long)]
        force: bool,

        /// Branch name
        branch: String,

        /// Optional start point when creating a branch
        start_point: Option<String>,
    },

    /// Merge a revision into the current branch
    Merge {
        /// Emit JSON merge output with current repository state
        #[arg(long)]
        json: bool,

        /// Abort the in-progress merge
        #[arg(long, conflicts_with = "continue_merge")]
        abort: bool,

        /// Commit the resolved in-progress merge
        #[arg(long = "continue", conflicts_with = "abort")]
        continue_merge: bool,

        /// Merge commit message for --continue
        #[arg(short, long, requires = "continue_merge")]
        message: Option<String>,

        /// Revision, branch, or remote-tracking branch to merge
        rev: Option<String>,
    },

    /// Show unresolved merge conflicts
    Conflicts(ConflictsArgs),

    /// Resolve a database conflict using one side
    #[command(group(
        ArgGroup::new("resolve_side")
            .args(["ours", "theirs", "manual"])
            .required(true)
            .multiple(false)
    ))]
    Resolve {
        /// Emit JSON conflict resolution output with current repository state
        #[arg(long)]
        json: bool,

        /// Resolve using the current branch side
        #[arg(long)]
        ours: bool,

        /// Resolve using the merged-in branch side
        #[arg(long)]
        theirs: bool,

        /// Mark a conflict resolved after manual edits
        #[arg(long)]
        manual: bool,

        /// Resolve one row conflict by table name and rowid
        #[arg(long, num_args = 2, value_names = ["TABLE", "ROWID"])]
        row: Option<Vec<String>>,

        /// Optional repository-relative path; defaults to the database path
        path: Option<PathBuf>,
    },

    /// Manage remotes
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },

    /// List refs advertised by a remote
    LsRemote {
        /// Emit JSON remote refs with current repository state
        #[arg(long)]
        json: bool,

        /// Remote name, for example origin
        remote: String,
    },

    /// Fetch remote branches
    Fetch(RemoteSyncArgs),

    /// Pull a remote branch by fast-forwarding or staging a merge
    Pull(RemoteBranchArgs),

    /// Push local branches to a remote
    Push(RemotePushArgs),
}

#[derive(Clone, Copy, ValueEnum)]
enum IdKind {
    Vid,
    Log,
    Sid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum PathKind {
    #[value(
        name = "sqlite_database",
        alias = "sqlite-database",
        alias = "sqlite_database",
        alias = "sqlite",
        alias = "database",
        alias = "db"
    )]
    SqliteDatabase,
    #[value(name = "text_file", alias = "text-file", alias = "text")]
    TextFile,
    #[value(name = "binary_file", alias = "binary-file", alias = "binary")]
    BinaryFile,
}

#[derive(Args)]
struct InitArgs {
    /// Emit JSON init output with repository metadata
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct AddArgs {
    /// Emit JSON staging output with current repository state
    #[arg(long)]
    json: bool,

    /// Stage all unstaged worktree changes
    #[arg(short = 'A', long, conflicts_with_all = ["force", "path"])]
    all: bool,

    /// When staging all changes, filter paths by kind
    #[arg(long, value_enum, requires = "all")]
    kind: Option<PathKind>,

    /// Add an ignored path anyway
    #[arg(short, long)]
    force: bool,

    /// Optional repository-relative path to stage
    path: Option<PathBuf>,
}

#[derive(Args)]
struct AuditArgs {
    /// Emit JSON audit output
    #[arg(long)]
    json: bool,

    /// Fetch missing artifact objects and external payloads from a remote
    #[arg(long)]
    repair: bool,

    /// Remote name to use with --repair. Defaults to the current upstream remote, then origin.
    #[arg(requires = "repair")]
    remote: Option<String>,
}

#[derive(Args)]
struct RmArgs {
    /// Emit JSON remove output with current repository state
    #[arg(long)]
    json: bool,

    /// Remove from the index while keeping the worktree file
    #[arg(long)]
    cached: bool,

    /// Optional repository-relative `SQLite` database path to remove
    path: Option<PathBuf>,
}

#[derive(Args)]
struct ConflictsArgs {
    /// Emit JSON conflict details
    #[arg(long)]
    json: bool,

    /// Database path
    db: Option<PathBuf>,
}

#[derive(Args)]
struct ExportArgs {
    /// Emit JSON export output with current repository state
    #[arg(long)]
    json: bool,

    /// Restore from this revision instead of exporting the current worktree Volume
    #[arg(short = 's', long)]
    source: Option<String>,

    /// Output path for the physical SQLite database file
    #[arg(short, long)]
    output: PathBuf,

    /// Repository-relative `SQLite` database path to export when `--db` is not supplied.
    path: Option<PathBuf>,
}

#[derive(Subcommand)]
enum RemoteCommand {
    /// Add a named remote
    Add {
        /// Emit JSON remote mutation with current repository state
        #[arg(long)]
        json: bool,

        /// Remote name, for example origin
        name: String,

        /// Remote URI: memory, fs://..., s3://..., s3_compatible://..., graft+https://..., or graft+http://...
        uri: String,
    },

    /// List configured remotes
    #[command(alias = "ls")]
    List {
        /// Emit JSON remote list with current repository state
        #[arg(long)]
        json: bool,
    },

    /// Remove a named remote
    #[command(alias = "rm")]
    Remove {
        /// Emit JSON remote mutation with current repository state
        #[arg(long)]
        json: bool,

        /// Remote name, for example origin
        name: String,
    },

    /// Rename a configured remote
    #[command(alias = "mv")]
    Rename {
        /// Emit JSON remote mutation with current repository state
        #[arg(long)]
        json: bool,

        /// Existing remote name
        old: String,

        /// New remote name
        new: String,
    },

    /// Print the configured remote URL
    GetUrl {
        /// Emit JSON remote entry with current repository state
        #[arg(long)]
        json: bool,

        /// Remote name, for example origin
        name: String,
    },

    /// Change the configured remote URL
    SetUrl {
        /// Emit JSON remote mutation with current repository state
        #[arg(long)]
        json: bool,

        /// Remote name, for example origin
        name: String,

        /// Remote URI: memory, fs://..., s3://..., s3_compatible://..., graft+https://..., or graft+http://...
        uri: String,
    },

    /// Delete stale remote-tracking branches for a remote
    Prune {
        /// Emit JSON prune result with current repository state
        #[arg(long)]
        json: bool,

        /// Remote name, for example origin
        name: String,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Print a repository config value
    Get {
        /// Emit JSON config entry with current repository state
        #[arg(long)]
        json: bool,

        /// Repository config key, for example files.inline_text_threshold
        key: String,
    },

    /// List repository config values
    List {
        /// Emit JSON config entries
        #[arg(long)]
        json: bool,
    },

    /// Update a repository config value
    Set {
        /// Emit JSON config mutation with current repository state
        #[arg(long)]
        json: bool,

        /// Repository config key, for example files.inline_text_threshold
        key: String,

        /// Repository config value, for example 8 MB
        #[arg(num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
        value: Vec<String>,
    },

    /// Clear a repository config value or reset it to its default
    Unset {
        /// Emit JSON config mutation with current repository state
        #[arg(long)]
        json: bool,

        /// Repository config key, for example merge.semantic_keys.documents
        key: String,
    },
}

#[derive(Subcommand)]
enum PayloadCommand {
    /// Fetch external payloads for a revision
    Fetch(PayloadFetchArgs),

    /// Show external payload cache status for a revision
    Status(PayloadStatusArgs),

    /// Prune unreferenced local external payloads
    Prune(PayloadPruneArgs),
}

#[derive(Args)]
struct PayloadFetchArgs {
    /// Emit JSON fetch output with current repository state
    #[arg(long)]
    json: bool,

    /// Remote to fetch payloads from. Defaults to the current upstream remote, then origin.
    #[arg(long)]
    remote: Option<String>,

    /// Revision whose external payloads should be present. Defaults to HEAD.
    #[arg(allow_hyphen_values = true)]
    rev: Option<String>,
}

#[derive(Args)]
struct PayloadStatusArgs {
    /// Emit JSON status output with current repository state
    #[arg(long)]
    json: bool,

    /// Revision whose external payloads should be inspected. Defaults to HEAD.
    #[arg(allow_hyphen_values = true)]
    rev: Option<String>,
}

#[derive(Args)]
struct PayloadPruneArgs {
    /// Emit JSON prune output with current repository state
    #[arg(long)]
    json: bool,

    /// Preview unreferenced payloads without deleting them
    #[arg(long, conflicts_with = "force")]
    dry_run: bool,

    /// Delete unreferenced payloads
    #[arg(long, conflicts_with = "dry_run")]
    force: bool,
}

#[derive(Args)]
struct RemoteSyncArgs {
    /// Emit JSON fetch output with current repository state
    #[arg(long)]
    json: bool,

    /// Fetch all branches for the remote
    #[arg(long, conflicts_with = "branch")]
    all: bool,

    /// Remote name. Defaults to origin.
    remote: Option<String>,

    /// Branch name or refspec. Defaults to the current branch.
    branch: Option<String>,
}

#[derive(Args)]
struct RemotePushArgs {
    /// Emit JSON push output with current repository state
    #[arg(long)]
    json: bool,

    /// Push all branches for the remote
    #[arg(long, conflicts_with = "branch")]
    all: bool,

    /// Allow overwriting a non-fast-forward remote ref
    #[arg(long, short = 'f')]
    force: bool,

    /// Remote name. Defaults to origin.
    remote: Option<String>,

    /// Branch name or refspec. Defaults to the current branch.
    branch: Option<String>,
}

#[derive(Args)]
struct RemoteBranchArgs {
    /// Emit JSON pull output with current repository state
    #[arg(long)]
    json: bool,

    /// Remote name. Defaults to origin.
    remote: Option<String>,

    /// Branch name or refspec. Defaults to the current branch.
    branch: Option<String>,
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    /// Database path used for SQLite-specific commands.
    #[arg(long, global = true, value_name = "PATH")]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    run_cli(cli)
}

fn run_cli(cli: Cli) -> Result<()> {
    run_command(cli.command, cli.db.as_deref())
}

fn run_command(command: Command, db_override: Option<&Path>) -> Result<()> {
    validate_command_repo_paths(&command)?;
    match command {
        Command::Id { kind } => match kind {
            IdKind::Vid => println!("{}", VolumeId::random()),
            IdKind::Log => println!("{}", LogId::random()),
            IdKind::Sid => println!("{}", SegmentId::random()),
        },
        Command::Log { json, limit, after } => {
            if limit == Some(0) {
                bail!("log --limit must be greater than zero");
            }
            if json {
                let arg = repo_log_arg(limit, after.as_deref())?;
                print_output(run_repo_pragma(db_override, None, "json_log", Some(&arg))?);
            } else {
                if limit.is_some() || after.is_some() {
                    bail!("log pagination requires --json");
                }
                print_output(run_repo_pragma(db_override, None, "log", None)?);
            }
        }
        Command::Init(args) => {
            print_output(run_repo_init(args.json)?);
        }
        Command::Sql { sql } => print_output(run_sql(db_override, &sql)?),
        Command::Clone { json, branch_option, remote, branch } => {
            let branch = branch_option.as_deref().or(branch.as_deref());
            let arg = repo_clone_arg(&remote, branch);
            let db = resolve_clone_db(db_override)?;
            print_output(run_pragma(&db, clone_pragma(json), Some(&arg))?);
        }
        Command::Status { json, kind } => {
            let pragma = if json { "json_status" } else { "status" };
            let arg = repo_status_arg(kind);
            print_output(run_repo_pragma(db_override, None, pragma, arg.as_deref())?);
        }
        Command::Audit(args) => {
            let pragma = if args.json { "json_audit" } else { "audit" };
            let arg = repo_audit_arg(args.repair, args.remote.as_deref());
            print_output(run_repo_pragma(db_override, None, pragma, arg.as_deref())?);
        }
        Command::LsFiles { json, stage, details, others, kind } => {
            let pragma = if json { "json_ls_files" } else { "ls_files" };
            let arg = repo_ls_files_arg(stage, details, others, kind);
            print_output(run_repo_pragma(db_override, None, pragma, arg.as_deref())?);
        }
        Command::Payload { command } => match command {
            PayloadCommand::Fetch(args) => {
                let pragma = if args.json {
                    "json_payload_fetch"
                } else {
                    "payload_fetch"
                };
                let arg = repo_payload_fetch_arg(args.remote.as_deref(), args.rev.as_deref());
                print_output(run_repo_pragma(db_override, None, pragma, arg.as_deref())?);
            }
            PayloadCommand::Status(args) => {
                let pragma = if args.json {
                    "json_payload_status"
                } else {
                    "payload_status"
                };
                let arg = repo_payload_status_arg(args.rev.as_deref());
                print_output(run_repo_pragma(db_override, None, pragma, arg.as_deref())?);
            }
            PayloadCommand::Prune(args) => {
                let pragma = if args.json {
                    "json_payload_prune"
                } else {
                    "payload_prune"
                };
                let arg = repo_payload_prune_arg(args.dry_run, args.force);
                print_output(run_repo_pragma(db_override, None, pragma, arg.as_deref())?);
            }
        },
        Command::Config { command } => match command {
            ConfigCommand::Get { json, key } => {
                let pragma = config_get_pragma(json);
                print_output(run_repo_pragma(db_override, None, pragma, Some(&key))?);
            }
            ConfigCommand::List { json } => {
                let (pragma, arg) = config_list_pragma(json);
                print_output(run_repo_pragma(db_override, None, pragma, arg)?);
            }
            ConfigCommand::Set { json, key, value } => {
                let arg = repo_config_set_arg(&key, &value)?;
                let pragma = config_set_pragma(json);
                print_output(run_repo_pragma(db_override, None, pragma, Some(&arg))?);
            }
            ConfigCommand::Unset { json, key } => {
                let pragma = config_unset_pragma(json);
                print_output(run_repo_pragma(db_override, None, pragma, Some(&key))?);
            }
        },
        Command::Add(args) => {
            if db_override.is_none() && !args.all && args.path.is_none() {
                bail!("add requires a path, --all, or --db <path>");
            }
            let arg = repo_add_arg(args.all, args.force, args.kind, args.path.as_deref())?;
            print_output(run_repo_pragma(
                db_override,
                None,
                add_pragma(args.json),
                arg.as_deref(),
            )?);
        }
        Command::Rm(args) => {
            if db_override.is_none() && args.path.is_none() {
                bail!("rm requires a path or --db <path>");
            }
            let arg = repo_rm_arg(args.cached, args.path.as_deref());
            print_output(run_repo_pragma(
                db_override,
                None,
                rm_pragma(args.json),
                arg.as_deref(),
            )?);
        }
        Command::Commit { json, message } => {
            print_output(run_repo_pragma(
                db_override,
                None,
                commit_pragma(json),
                Some(&message),
            )?);
        }
        Command::Diff {
            rows,
            staged,
            kind,
            content,
            max_content_bytes,
            root,
            from,
            to,
            path,
            json,
        } => {
            let suffix = if json { "json_diff" } else { "diff" };
            let arg = repo_diff_arg(RepoDiffArgSpec {
                rows,
                staged,
                kind,
                content,
                max_content_bytes,
                root: root.as_deref(),
                from: from.as_deref(),
                to: to.as_deref(),
                path: path.as_deref(),
            })?;
            print_output(run_repo_pragma(db_override, None, suffix, arg.as_deref())?);
        }
        Command::Show { rev, json } => {
            let suffix = if json { "json_show" } else { "show" };
            print_output(run_repo_pragma(db_override, None, suffix, Some(&rev))?);
        }
        Command::Checkout { json, force, rev, path } => {
            let arg = repo_checkout_arg(force, &rev, path.as_deref());
            print_output(run_repo_pragma(
                db_override,
                None,
                checkout_pragma(json),
                Some(&arg),
            )?);
        }
        Command::Restore {
            json,
            source,
            expected_head,
            require_clean,
            staged,
            all,
            kind,
            path,
        } => {
            let arg = repo_restore_arg(
                source.as_deref(),
                expected_head.as_deref(),
                require_clean,
                staged,
                all,
                kind,
                path.as_deref(),
            )?;
            print_output(run_repo_pragma(
                db_override,
                None,
                restore_pragma(json),
                Some(&arg),
            )?);
        }
        Command::Export(args) => {
            if db_override.is_none() && args.path.is_none() {
                bail!("export requires a database path or --db <path>");
            }
            let arg = repo_export_arg(args.source.as_deref(), &args.output, args.path.as_deref());
            let command_db = if db_override.is_none() {
                args.path.as_deref()
            } else {
                None
            };
            print_output(run_repo_pragma(
                db_override,
                command_db,
                export_pragma(args.json),
                Some(&arg),
            )?);
        }
        Command::Reset { json, soft, mixed, hard, rev } => {
            let arg = repo_reset_arg(&rev, soft, mixed, hard);
            print_output(run_repo_pragma(
                db_override,
                None,
                reset_pragma(json),
                Some(&arg),
            )?);
        }
        Command::Branch {
            json,
            delete,
            force_delete,
            move_branch,
            force_move,
            set_upstream_to,
            unset_upstream,
            remote,
            all,
            name,
            start_point,
        } => {
            if delete || force_delete {
                let Some(name) = name else {
                    bail!("branch delete requires a branch name");
                };
                if start_point.is_some() {
                    bail!("branch delete accepts only a branch name");
                }
                let arg = if force_delete {
                    format!("--force {name}")
                } else {
                    name
                };
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    branch_delete_pragma(json),
                    Some(&arg),
                )?);
            } else if move_branch || force_move {
                let Some(name) = name else {
                    bail!("branch rename requires a new branch name");
                };
                let arg = match start_point {
                    Some(new) => {
                        if force_move {
                            format!("--force {name} {new}")
                        } else {
                            format!("{name} {new}")
                        }
                    }
                    None => {
                        if force_move {
                            format!("--force {name}")
                        } else {
                            name
                        }
                    }
                };
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    branch_rename_pragma(json),
                    Some(&arg),
                )?);
            } else if let Some(upstream) = set_upstream_to {
                if start_point.is_some() {
                    bail!("branch --set-upstream-to accepts at most a branch name");
                }
                let arg = match name {
                    Some(name) => format!("{name} {upstream}"),
                    None => upstream,
                };
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    branch_upstream_pragma(json),
                    Some(&arg),
                )?);
            } else if unset_upstream {
                if start_point.is_some() {
                    bail!("branch --unset-upstream accepts at most a branch name");
                }
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    branch_unset_upstream_pragma(json),
                    name.as_deref(),
                )?);
            } else if remote || all {
                if name.is_some() || start_point.is_some() {
                    bail!("branch -r/-a accepts no branch name or start point");
                }
                let (pragma, arg) = branch_list_pragma(json, remote, all);
                print_output(run_repo_pragma(db_override, None, pragma, arg)?);
            } else if let Some(name) = name {
                let arg = match start_point {
                    Some(start_point) => format!("{name} {start_point}"),
                    None => name,
                };
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    branch_create_pragma(json),
                    Some(&arg),
                )?);
            } else {
                if start_point.is_some() {
                    bail!("branch list accepts no start point");
                }
                let (pragma, arg) = branch_list_pragma(json, remote, all);
                print_output(run_repo_pragma(db_override, None, pragma, arg)?);
            }
        }
        Command::Tag {
            json,
            list,
            delete,
            annotated,
            message,
            name,
            rev,
        } => {
            if list {
                if name.is_some() || rev.is_some() {
                    bail!("tag --list does not support patterns yet");
                }
                let (pragma, arg) = tag_list_pragma(json);
                print_output(run_repo_pragma(db_override, None, pragma, arg)?);
            } else if delete {
                let Some(name) = name else {
                    bail!("tag delete requires a tag name");
                };
                if rev.is_some() {
                    bail!("tag delete accepts only a tag name");
                }
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    tag_delete_pragma(json),
                    Some(&name),
                )?);
            } else if let Some(name) = name {
                let arg = if annotated {
                    let Some(message) = message else {
                        bail!("annotated tag requires --message");
                    };
                    match rev {
                        Some(rev) => format!("--annotated {name} {rev} -- {message}"),
                        None => format!("--annotated {name} -- {message}"),
                    }
                } else {
                    if message.is_some() {
                        bail!("--message requires --annotated");
                    }
                    match rev {
                        Some(rev) => format!("{name} {rev}"),
                        None => name,
                    }
                };
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    tag_create_pragma(json),
                    Some(&arg),
                )?);
            } else {
                if annotated || message.is_some() {
                    bail!("tag list accepts no annotation flags");
                }
                let (pragma, arg) = tag_list_pragma(json);
                print_output(run_repo_pragma(db_override, None, pragma, arg)?);
            }
        }
        Command::Switch { json, create, force, branch, start_point } => {
            if !create && start_point.is_some() {
                bail!("switch accepts a start point only with --create");
            }
            let pragma = if create {
                switch_create_pragma(json)
            } else {
                switch_branch_pragma(json)
            };
            let arg = repo_switch_arg(force, &branch, start_point.as_deref());
            print_output(run_repo_pragma(db_override, None, pragma, Some(&arg))?);
        }
        Command::Merge {
            json,
            abort,
            continue_merge,
            message,
            rev,
        } => {
            if abort {
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    merge_abort_pragma(json),
                    None,
                )?);
            } else if continue_merge {
                if rev.is_some() {
                    bail!("merge --continue does not accept a revision");
                }
                let Some(message) = message else {
                    bail!("merge --continue requires --message");
                };
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    merge_continue_pragma(json),
                    Some(&message),
                )?);
            } else {
                let Some(rev) = rev else {
                    bail!("merge requires a revision unless --abort is used");
                };
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    merge_pragma(json),
                    Some(&rev),
                )?);
            }
        }
        Command::Conflicts(args) => {
            print_output(run_repo_pragma(
                db_override,
                args.db.as_deref(),
                conflicts_pragma(args.json),
                None,
            )?);
        }
        Command::Resolve { json, ours, theirs, manual, row, path } => {
            let arg = repo_resolve_arg(ours, theirs, manual, row.as_deref(), path.as_deref())?;
            print_output(run_repo_pragma(
                db_override,
                None,
                resolve_pragma(json),
                Some(&arg),
            )?);
        }
        Command::Remote { command } => match command {
            RemoteCommand::Add { json, name, uri } => {
                let arg = format!("{name} {uri}");
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    remote_add_pragma(json),
                    Some(&arg),
                )?);
            }
            RemoteCommand::List { json } => {
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    remote_list_pragma(json),
                    None,
                )?);
            }
            RemoteCommand::Remove { json, name } => {
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    remote_remove_pragma(json),
                    Some(&name),
                )?);
            }
            RemoteCommand::Rename { json, old, new } => {
                let arg = format!("{old} {new}");
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    remote_rename_pragma(json),
                    Some(&arg),
                )?);
            }
            RemoteCommand::GetUrl { json, name } => {
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    remote_get_url_pragma(json),
                    Some(&name),
                )?);
            }
            RemoteCommand::SetUrl { json, name, uri } => {
                let arg = format!("{name} {uri}");
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    remote_set_url_pragma(json),
                    Some(&arg),
                )?);
            }
            RemoteCommand::Prune { json, name } => {
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    remote_prune_pragma(json),
                    Some(&name),
                )?);
            }
        },
        Command::LsRemote { json, remote } => {
            print_output(run_repo_pragma(
                db_override,
                None,
                ls_remote_pragma(json),
                Some(&remote),
            )?);
        }
        Command::Fetch(args) => {
            print_output(run_repo_pragma(
                db_override,
                None,
                fetch_pragma(args.json),
                remote_sync_arg(&args)?.as_deref(),
            )?);
        }
        Command::Pull(args) => {
            print_output(run_repo_pragma(
                db_override,
                None,
                pull_pragma(args.json),
                remote_branch_arg(&args)?.as_deref(),
            )?);
        }
        Command::Push(args) => {
            print_output(run_repo_pragma(
                db_override,
                None,
                push_pragma(args.json),
                remote_push_arg(&args)?.as_deref(),
            )?);
        }
    }
    Ok(())
}

fn run_repo_pragma(
    db_override: Option<&Path>,
    command_db: Option<&Path>,
    suffix: &str,
    arg: Option<&str>,
) -> Result<Option<String>> {
    let db = match command_db.or(db_override) {
        Some(path) => resolve_cli_db(Some(path))?,
        None => resolve_repo_control_db()?,
    };
    run_pragma(&db, suffix, arg)
}

fn run_repo_init(json: bool) -> Result<Option<String>> {
    let worktree = std::env::current_dir().context("failed to read current directory")?;
    let repo = Repository::init(&worktree)?;
    if json {
        return Ok(Some(format!(
            "{{\"operation\":\"init\",\"graft_dir\":\"{}\",\"worktree\":\"{}\"}}",
            json_escape(&repo.graft_dir().display().to_string()),
            json_escape(&repo.worktree().display().to_string())
        )));
    }
    Ok(Some(format!(
        "Initialized empty Graft repository in {}",
        repo.graft_dir().display()
    )))
}

fn run_sql(db_override: Option<&Path>, sql_parts: &[String]) -> Result<Option<String>> {
    let sql = if sql_parts.is_empty() {
        let mut sql = String::new();
        std::io::stdin()
            .read_to_string(&mut sql)
            .context("failed to read SQL from stdin")?;
        sql
    } else {
        sql_parts.join(" ")
    };
    let sql = sql.trim();
    if sql.is_empty() {
        bail!("sql command requires SQL on the command line or stdin");
    }

    let db = resolve_cli_db(db_override)?;
    if graft::repo::Repository::discover_for_file(&db).is_err() {
        bail!(
            "not a Graft repository: run `graft init` in the worktree before opening {}",
            db.display()
        );
    }
    execute_sql(&db, sql)
}

fn resolve_cli_db(path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = path {
        let db = absolute_db_path(path)?;
        ensure_db_parent_exists_in_repo(&db)?;
        return Ok(db);
    }

    bail!("SQLite command requires --db <path>")
}

fn ensure_db_parent_exists_in_repo(db: &Path) -> Result<()> {
    let Some(parent) = db.parent() else {
        return Ok(());
    };
    if parent.exists() {
        return Ok(());
    }

    let mut existing = parent;
    while !existing.exists() {
        let Some(next) = existing.parent() else {
            bail!(
                "database parent directory does not exist: {}",
                parent.display()
            );
        };
        existing = next;
    }

    let repo = Repository::discover(existing).with_context(|| {
        format!(
            "database parent directory does not exist: {}",
            parent.display()
        )
    })?;
    if !db.starts_with(repo.worktree()) {
        bail!(
            "database path {} is outside Graft worktree {}",
            db.display(),
            repo.worktree().display()
        );
    }
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create database parent {}", parent.display()))?;
    Ok(())
}

fn resolve_repo_control_db() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let repo = Repository::discover(&cwd)?;
    Ok(repo.graft_dir().join("control.sqlite"))
}

fn resolve_clone_db(path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = path {
        return resolve_cli_db(Some(path));
    }
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    Ok(cwd.join(".graft-clone.sqlite"))
}

fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out
}

fn repo_config_set_arg(key: &str, value: &[String]) -> Result<String> {
    if value.is_empty() {
        bail!("config set requires a value");
    }
    Ok(format!("{} -- {}", key, value.join(" ")))
}

fn config_get_pragma(json: bool) -> &'static str {
    if json {
        "json_config_get"
    } else {
        "config_get"
    }
}

fn repo_log_arg(limit: Option<usize>, after: Option<&str>) -> Result<String> {
    let mut parts = vec!["--with-status".to_string()];
    if let Some(limit) = limit {
        parts.push("--limit".to_string());
        parts.push(limit.to_string());
    }
    if let Some(after) = after {
        parts.push("--after".to_string());
        parts.push(quote_pragma_path(Path::new(after))?);
    }
    Ok(parts.join(" "))
}

fn validate_command_repo_paths(command: &Command) -> Result<()> {
    let (path, lossless_serialization) = match command {
        Command::Add(args) => (args.path.as_deref(), true),
        Command::Rm(args) => (args.path.as_deref(), false),
        Command::Diff { root: Some(_), from: Some(path), .. } => (Some(Path::new(path)), true),
        Command::Diff { staged: true, from: Some(path), .. } => (Some(Path::new(path)), true),
        Command::Diff {
            content: true,
            root: None,
            from: Some(_),
            to: Some(path),
            path: None,
            ..
        } => (Some(Path::new(path)), true),
        Command::Diff { path, .. } | Command::Restore { path, .. } => (path.as_deref(), true),
        Command::Checkout { path, .. } | Command::Resolve { path, .. } => (path.as_deref(), false),
        Command::Export(args) => (args.path.as_deref(), false),
        _ => (None, false),
    };
    if let Some(path) = path {
        validate_cli_repo_path(path, lossless_serialization)?;
    }
    Ok(())
}

fn validate_cli_repo_path(path: &Path, lossless_serialization: bool) -> Result<()> {
    let raw = path
        .to_str()
        .with_context(|| format!("repository path `{}` is not valid UTF-8", path.display()))?;
    if !path.is_absolute() {
        graft::repo::validate_repo_path_identity(raw)?;
    } else {
        #[cfg(not(windows))]
        if raw.contains('\\') {
            bail!(
                "path `{raw}` has an unsupported repository identity: backslashes are not supported in POSIX repository paths"
            );
        }
    }

    if lossless_serialization {
        return Ok(());
    }
    let normalized_whitespace = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized_whitespace != raw || raw.contains('\'') || raw.contains('"') {
        bail!(
            "path `{raw}` has an unsupported repository identity: the CLI cannot serialize this path without changing it"
        );
    }
    Ok(())
}

fn config_list_pragma(json: bool) -> (&'static str, Option<&'static str>) {
    if json {
        ("json_config_list", Some("--with-status"))
    } else {
        ("config_list", None)
    }
}

fn config_set_pragma(json: bool) -> &'static str {
    if json {
        "json_config_set"
    } else {
        "config_set"
    }
}

fn config_unset_pragma(json: bool) -> &'static str {
    if json {
        "json_config_unset"
    } else {
        "config_unset"
    }
}

fn clone_pragma(json: bool) -> &'static str {
    if json { "json_clone" } else { "clone" }
}

fn export_pragma(json: bool) -> &'static str {
    if json { "json_export" } else { "export" }
}

fn add_pragma(json: bool) -> &'static str {
    if json { "json_add" } else { "add" }
}

fn rm_pragma(json: bool) -> &'static str {
    if json { "json_rm" } else { "rm" }
}

fn commit_pragma(json: bool) -> &'static str {
    if json { "json_commit" } else { "commit" }
}

fn checkout_pragma(json: bool) -> &'static str {
    if json { "json_checkout" } else { "checkout" }
}

fn restore_pragma(json: bool) -> &'static str {
    if json { "json_restore" } else { "restore" }
}

fn reset_pragma(json: bool) -> &'static str {
    if json { "json_reset" } else { "reset" }
}

fn switch_branch_pragma(json: bool) -> &'static str {
    if json {
        "json_switch_branch"
    } else {
        "switch_branch"
    }
}

fn switch_create_pragma(json: bool) -> &'static str {
    if json {
        "json_switch_create"
    } else {
        "switch_create"
    }
}

fn merge_pragma(json: bool) -> &'static str {
    if json { "json_merge" } else { "merge" }
}

fn merge_abort_pragma(json: bool) -> &'static str {
    if json {
        "json_merge_abort"
    } else {
        "merge_abort"
    }
}

fn merge_continue_pragma(json: bool) -> &'static str {
    if json {
        "json_merge_continue"
    } else {
        "merge_continue"
    }
}

fn conflicts_pragma(json: bool) -> &'static str {
    if json { "json_conflicts" } else { "conflicts" }
}

fn resolve_pragma(json: bool) -> &'static str {
    if json {
        "json_resolve_conflict"
    } else {
        "resolve"
    }
}

fn branch_list_pragma(json: bool, remote: bool, all: bool) -> (&'static str, Option<&'static str>) {
    let arg = if all {
        Some("--all")
    } else if remote {
        Some("--remote")
    } else {
        None
    };
    if json {
        ("json_branch", arg)
    } else {
        ("branch", arg)
    }
}

fn branch_create_pragma(json: bool) -> &'static str {
    if json {
        "json_branch_create"
    } else {
        "branch_create"
    }
}

fn branch_delete_pragma(json: bool) -> &'static str {
    if json {
        "json_branch_delete"
    } else {
        "branch_delete"
    }
}

fn branch_rename_pragma(json: bool) -> &'static str {
    if json {
        "json_branch_rename"
    } else {
        "branch_rename"
    }
}

fn branch_upstream_pragma(json: bool) -> &'static str {
    if json {
        "json_branch_upstream"
    } else {
        "branch_upstream"
    }
}

fn branch_unset_upstream_pragma(json: bool) -> &'static str {
    if json {
        "json_branch_unset_upstream"
    } else {
        "branch_unset_upstream"
    }
}

fn tag_list_pragma(json: bool) -> (&'static str, Option<&'static str>) {
    if json {
        ("json_tags", Some("--with-status"))
    } else {
        ("tags", None)
    }
}

fn tag_create_pragma(json: bool) -> &'static str {
    if json {
        "json_tag_create"
    } else {
        "tag_create"
    }
}

fn tag_delete_pragma(json: bool) -> &'static str {
    if json {
        "json_tag_delete"
    } else {
        "tag_delete"
    }
}

fn remote_add_pragma(json: bool) -> &'static str {
    if json {
        "json_remote_add"
    } else {
        "remote_add"
    }
}

fn remote_list_pragma(json: bool) -> &'static str {
    if json { "json_remotes" } else { "remotes" }
}

fn remote_remove_pragma(json: bool) -> &'static str {
    if json {
        "json_remote_remove"
    } else {
        "remote_remove"
    }
}

fn remote_rename_pragma(json: bool) -> &'static str {
    if json {
        "json_remote_rename"
    } else {
        "remote_rename"
    }
}

fn remote_get_url_pragma(json: bool) -> &'static str {
    if json {
        "json_remote_get_url"
    } else {
        "remote_get_url"
    }
}

fn remote_set_url_pragma(json: bool) -> &'static str {
    if json {
        "json_remote_set_url"
    } else {
        "remote_set_url"
    }
}

fn remote_prune_pragma(json: bool) -> &'static str {
    if json {
        "json_remote_prune"
    } else {
        "remote_prune"
    }
}

fn ls_remote_pragma(json: bool) -> &'static str {
    if json { "json_ls_remote" } else { "ls_remote" }
}

fn fetch_pragma(json: bool) -> &'static str {
    if json { "json_fetch" } else { "fetch" }
}

fn pull_pragma(json: bool) -> &'static str {
    if json { "json_pull" } else { "pull" }
}

fn push_pragma(json: bool) -> &'static str {
    if json { "json_push" } else { "push" }
}

#[derive(Default)]
struct RepoDiffArgSpec<'a> {
    rows: bool,
    staged: bool,
    kind: Option<PathKind>,
    content: bool,
    max_content_bytes: Option<NonZeroU64>,
    root: Option<&'a str>,
    from: Option<&'a str>,
    to: Option<&'a str>,
    path: Option<&'a Path>,
}

fn repo_diff_arg(spec: RepoDiffArgSpec<'_>) -> Result<Option<String>> {
    let RepoDiffArgSpec {
        rows,
        staged,
        kind,
        content,
        max_content_bytes,
        root,
        from,
        to,
        path,
    } = spec;
    if max_content_bytes.is_some() && !content {
        bail!("--max-content-bytes requires --content");
    }
    let implicit_worktree_content_path =
        content && root.is_none() && from.is_some() && to.is_some() && path.is_none();
    if content
        && (rows
            || staged
            || if root.is_some() {
                from.is_none() || to.is_some() || path.is_some()
            } else {
                from.is_none() || (path.is_none() && !implicit_worktree_content_path)
            })
    {
        bail!(
            "--content requires JSON, a source revision, an optional target revision, and one path"
        );
    }
    let mut prefixes = Vec::new();
    if rows {
        prefixes.push("--rows".to_string());
    }
    if let Some(kind) = kind {
        prefixes.push("--kind".to_string());
        prefixes.push(path_kind_arg(kind).to_string());
    }
    if content {
        prefixes.push("--content".to_string());
        if let Some(max_content_bytes) = max_content_bytes {
            prefixes.push("--max-content-bytes".to_string());
            prefixes.push(max_content_bytes.get().to_string());
        }
    }
    if let Some(root) = root {
        if staged || to.is_some() || path.is_some() {
            bail!("--root accepts one target revision and an optional path");
        }
        let mut arg = prefixes.join(" ");
        if !arg.is_empty() {
            arg.push(' ');
        }
        arg.push_str("--root ");
        arg.push_str(root);
        if let Some(path) = from {
            arg.push_str(" -- ");
            arg.push_str(&quote_pragma_path(Path::new(path))?);
        }
        return Ok(Some(arg));
    }
    let prefix = prefixes.join(" ");
    if implicit_worktree_content_path {
        return Ok(Some(format!(
            "{prefix} {} -- {}",
            from.expect("validated source revision"),
            quote_pragma_path(Path::new(to.expect("implicit worktree content path")))?
        )));
    }
    if staged {
        if to.is_some() || path.is_some() {
            bail!("--staged accepts at most one optional path");
        }
        let mut arg = prefix;
        if !arg.is_empty() {
            arg.push(' ');
        }
        arg.push_str("--staged");
        if let Some(path) = from {
            arg.push_str(" -- ");
            arg.push_str(&quote_pragma_path(Path::new(path))?);
        }
        return Ok(Some(arg));
    }

    let arg = match (from, to, path) {
        (None, None, None) => (!prefix.is_empty()).then_some(prefix),
        (None, None, Some(path)) => Some(format!("{prefix} -- {}", quote_pragma_path(path)?)),
        (Some(from), None, None) => Some(format!("{prefix} {from}")),
        (Some(from), None, Some(path)) => {
            Some(format!("{prefix} {from} -- {}", quote_pragma_path(path)?))
        }
        (Some(from), Some(to), None) => Some(format!("{prefix} {from} {to}")),
        (Some(from), Some(to), Some(path)) => Some(format!(
            "{prefix} {from} {to} -- {}",
            quote_pragma_path(path)?
        )),
        (None, Some(_), _) => unreachable!("clap cannot provide `to` without `from`"),
    };
    Ok(arg.map(|arg| arg.trim_start().to_string()))
}

fn repo_reset_arg(rev: &str, soft: bool, mixed: bool, hard: bool) -> String {
    if soft {
        format!("--soft {rev}")
    } else if hard {
        format!("--hard {rev}")
    } else if mixed {
        format!("--mixed {rev}")
    } else {
        rev.to_string()
    }
}

fn repo_switch_arg(force: bool, branch: &str, start_point: Option<&str>) -> String {
    match (force, start_point) {
        (true, Some(start_point)) => format!("--force {branch} {start_point}"),
        (true, None) => format!("--force {branch}"),
        (false, Some(start_point)) => format!("{branch} {start_point}"),
        (false, None) => branch.to_string(),
    }
}

fn repo_status_arg(kind: Option<PathKind>) -> Option<String> {
    kind.map(|kind| format!("--kind {}", path_kind_arg(kind)))
}

fn repo_ls_files_arg(
    stage: bool,
    details: bool,
    others: bool,
    kind: Option<PathKind>,
) -> Option<String> {
    let mut parts = Vec::new();
    if stage {
        parts.push("--stage".to_string());
    }
    if details {
        parts.push("--details".to_string());
    }
    if others {
        parts.push("--others".to_string());
    }
    if let Some(kind) = kind {
        parts.push("--kind".to_string());
        parts.push(path_kind_arg(kind).to_string());
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn repo_audit_arg(repair: bool, remote: Option<&str>) -> Option<String> {
    match (repair, remote) {
        (false, None) => None,
        (false, Some(_)) => unreachable!("clap prevents audit remote without --repair"),
        (true, None) => Some("--repair".to_string()),
        (true, Some(remote)) => Some(format!("--repair {remote}")),
    }
}

fn repo_payload_prune_arg(dry_run: bool, force: bool) -> Option<String> {
    match (dry_run, force) {
        (false, false) => None,
        (true, false) => Some("--dry-run".to_string()),
        (false, true) => Some("--force".to_string()),
        (true, true) => unreachable!("clap prevents --dry-run with --force"),
    }
}

fn repo_payload_fetch_arg(remote: Option<&str>, rev: Option<&str>) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(remote) = remote {
        parts.push("--remote".to_string());
        parts.push(remote.to_string());
    }
    if let Some(rev) = rev {
        parts.push(rev.to_string());
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn repo_payload_status_arg(rev: Option<&str>) -> Option<String> {
    rev.map(ToString::to_string)
}

fn path_kind_arg(kind: PathKind) -> &'static str {
    match kind {
        PathKind::SqliteDatabase => "sqlite_database",
        PathKind::TextFile => "text_file",
        PathKind::BinaryFile => "binary_file",
    }
}

fn repo_clone_arg(remote: &str, branch: Option<&str>) -> String {
    match branch {
        Some(branch) => format!("{remote} {branch}"),
        None => remote.to_string(),
    }
}

fn repo_add_arg(
    all: bool,
    force: bool,
    kind: Option<PathKind>,
    path: Option<&Path>,
) -> Result<Option<String>> {
    if all {
        if force || path.is_some() {
            unreachable!("clap prevents --all with --force or path");
        }
        let mut parts = vec!["--all".to_string()];
        if let Some(kind) = kind {
            parts.push("--kind".to_string());
            parts.push(path_kind_arg(kind).to_string());
        }
        return Ok(Some(parts.join(" ")));
    }

    if kind.is_some() {
        unreachable!("clap prevents --kind without --all");
    }

    Ok(match (force, path) {
        (false, None) => None,
        (false, Some(path)) => Some(format!("-- {}", quote_pragma_path(path)?)),
        (true, None) => Some("--force".to_string()),
        (true, Some(path)) => Some(format!("--force -- {}", quote_pragma_path(path)?)),
    })
}

fn repo_rm_arg(cached: bool, path: Option<&Path>) -> Option<String> {
    match (cached, path) {
        (false, None) => None,
        (false, Some(path)) => Some(path.display().to_string()),
        (true, None) => Some("--cached".to_string()),
        (true, Some(path)) => Some(format!("--cached -- {}", path.display())),
    }
}

fn repo_checkout_arg(force: bool, rev: &str, path: Option<&Path>) -> String {
    match (force, path) {
        (false, Some(path)) => format!("{rev} -- {}", path.display()),
        (false, None) => rev.to_string(),
        (true, None) => format!("--force {rev}"),
        (true, Some(_)) => unreachable!("clap prevents --force with path checkout"),
    }
}

fn repo_restore_arg(
    source: Option<&str>,
    expected_head: Option<&str>,
    require_clean: bool,
    staged: bool,
    all: bool,
    kind: Option<PathKind>,
    path: Option<&Path>,
) -> Result<String> {
    let mut parts = Vec::new();
    if staged {
        parts.push("--staged".to_string());
    }
    if let Some(source) = source {
        parts.push("--source".to_string());
        parts.push(source.to_string());
    }
    if let Some(expected_head) = expected_head {
        parts.push("--expected-head".to_string());
        parts.push(expected_head.to_string());
    }
    if require_clean {
        parts.push("--require-clean".to_string());
    }
    if all {
        if !staged || path.is_some() {
            unreachable!("clap prevents --all without --staged or with a path");
        }
        parts.push("--all".to_string());
        if let Some(kind) = kind {
            parts.push("--kind".to_string());
            parts.push(path_kind_arg(kind).to_string());
        }
        return Ok(parts.join(" "));
    }

    if kind.is_some() {
        unreachable!("clap prevents --kind without --all");
    }

    let path = path.expect("clap requires a restore path unless --all is present");
    parts.push("--".to_string());
    parts.push(quote_pragma_path(path)?);
    Ok(parts.join(" "))
}

fn quote_pragma_path(path: &Path) -> Result<String> {
    let raw = path
        .to_str()
        .with_context(|| format!("repository path `{}` is not valid UTF-8", path.display()))?;
    #[cfg(not(windows))]
    let raw = raw.replace('"', "\\\"");
    Ok(format!("\"{raw}\""))
}

fn repo_export_arg(source: Option<&str>, output: &Path, path: Option<&Path>) -> String {
    let mut arg = match source {
        Some(source) => format!("--source {source} --output {}", output.display()),
        None => format!("--output {}", output.display()),
    };
    if let Some(path) = path {
        arg.push_str(" -- ");
        arg.push_str(&path.display().to_string());
    }
    arg
}

fn repo_resolve_arg(
    ours: bool,
    theirs: bool,
    manual: bool,
    row: Option<&[String]>,
    path: Option<&Path>,
) -> Result<String> {
    let side = match (ours, theirs, manual) {
        (true, false, false) => "--ours",
        (false, true, false) => "--theirs",
        (false, false, true) => "--manual",
        _ => bail!("resolve requires exactly one of --ours, --theirs, or --manual"),
    };
    let mut arg = side.to_string();
    if let Some(row) = row {
        let [table, rowid] = row else {
            bail!("resolve --row requires table and rowid");
        };
        arg.push_str(" --row ");
        arg.push_str(table);
        arg.push(' ');
        arg.push_str(rowid);
    }
    if let Some(path) = path {
        arg.push(' ');
        arg.push_str(&path.display().to_string());
    }
    Ok(arg)
}

fn print_output(output: Option<String>) {
    if let Some(output) = output
        && !output.is_empty()
    {
        print!("{output}");
        if !output.ends_with('\n') {
            println!();
        }
    }
}

fn remote_sync_arg(args: &RemoteSyncArgs) -> Result<Option<String>> {
    if args.all {
        if args.branch.is_some() {
            bail!("--all does not accept a branch");
        }
        return Ok(Some(match &args.remote {
            Some(remote) => format!("--all {remote}"),
            None => "--all".to_string(),
        }));
    }

    Ok(match (&args.remote, &args.branch) {
        (None, None) => None,
        (Some(remote), None) => Some(remote.clone()),
        (Some(remote), Some(branch)) => Some(format!("{remote} {branch}")),
        (None, Some(branch)) => Some(format!("origin {branch}")),
    })
}

fn remote_push_arg(args: &RemotePushArgs) -> Result<Option<String>> {
    let mut parts = Vec::new();
    if args.force {
        parts.push("--force".to_string());
    }

    if args.all {
        if args.branch.is_some() {
            bail!("--all does not accept a branch");
        }
        parts.push("--all".to_string());
        if let Some(remote) = &args.remote {
            parts.push(remote.clone());
        }
    } else {
        match (&args.remote, &args.branch) {
            (None, None) => {}
            (Some(remote), None) => parts.push(remote.clone()),
            (Some(remote), Some(branch)) => {
                parts.push(remote.clone());
                parts.push(branch.clone());
            }
            (None, Some(branch)) => {
                parts.push("origin".to_string());
                parts.push(branch.clone());
            }
        }
    }

    Ok((!parts.is_empty()).then(|| parts.join(" ")))
}

fn remote_branch_arg(args: &RemoteBranchArgs) -> Result<Option<String>> {
    Ok(match (&args.remote, &args.branch) {
        (None, None) => None,
        (Some(remote), None) => Some(remote.clone()),
        (Some(remote), Some(branch)) => Some(format!("{remote} {branch}")),
        (None, Some(branch)) => Some(format!("origin {branch}")),
    })
}

fn run_pragma(db: &Path, suffix: &str, arg: Option<&str>) -> Result<Option<String>> {
    let graft = open_graft_connection(db)?;
    let pragma = format!("graft_{suffix}");

    let mut output = None;
    if let Some(arg) = arg {
        graft.conn.pragma(None, &pragma, arg, |row| {
            output = Some(row.get(0)?);
            Ok(())
        })?;
    } else {
        graft.conn.pragma_query(None, &pragma, |row| {
            output = Some(row.get(0)?);
            Ok(())
        })?;
    }
    Ok(output)
}

fn execute_sql(db: &Path, sql: &str) -> Result<Option<String>> {
    let graft = open_graft_connection(db)?;
    let mut batch = Batch::new(&graft.conn, sql);
    let mut output = String::new();
    let mut statement_count = 0;
    let mut result_count = 0;

    while let Some(mut stmt) = batch.next()? {
        statement_count += 1;
        if stmt.column_count() == 0 {
            stmt.execute([])?;
            continue;
        }

        if result_count > 0 {
            output.push('\n');
        }
        append_query_output(&mut output, &mut stmt)?;
        result_count += 1;
    }

    if statement_count == 0 {
        bail!("sql command did not contain a SQLite statement");
    }

    Ok(Some(if result_count == 0 {
        "OK".to_string()
    } else {
        output
    }))
}

fn append_query_output(output: &mut String, stmt: &mut rusqlite::Statement<'_>) -> Result<()> {
    let column_count = stmt.column_count();
    let column_names: Vec<String> = stmt
        .column_names()
        .into_iter()
        .map(ToOwned::to_owned)
        .collect();
    let show_header = !is_graft_pragma_statement(stmt);
    if show_header {
        output.push_str(&column_names.join("|"));
        output.push('\n');
    }

    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        for column in 0..column_count {
            if column > 0 {
                output.push('|');
            }
            output.push_str(&render_sql_value(row.get_ref(column)?));
        }
        output.push('\n');
    }
    Ok(())
}

fn is_graft_pragma_statement(stmt: &rusqlite::Statement<'_>) -> bool {
    stmt.expanded_sql().is_some_and(|sql| {
        sql.trim_start()
            .to_ascii_lowercase()
            .starts_with("pragma graft_")
    })
}

fn render_sql_value(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Null => "NULL".to_string(),
        ValueRef::Integer(value) => value.to_string(),
        ValueRef::Real(value) => value.to_string(),
        ValueRef::Text(value) => String::from_utf8_lossy(value).into_owned(),
        ValueRef::Blob(value) => format!("x'{}'", hex_encode(value)),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

struct GraftConnection {
    conn: Connection,
    _vfs: RegisteredVfs,
}

fn open_graft_connection(db: &Path) -> Result<GraftConnection> {
    let vfs = register_graft_vfs()?;
    let db = absolute_db_path(db)?;
    let uri = format!("file:{}?vfs={}", db.display(), vfs.name);
    let conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("failed to open {uri}"))?;
    Ok(GraftConnection { conn, _vfs: vfs })
}

struct RegisteredVfs {
    name: String,
    _data_dir: tempfile::TempDir,
}

fn register_graft_vfs() -> Result<RegisteredVfs> {
    let name = format!("graft_cli_{}_{}", std::process::id(), unique_suffix());
    let data_dir = tempfile::Builder::new()
        .prefix(&name)
        .tempdir()
        .context("failed to create temporary Graft data directory")?;
    graft_sqlite::register_static(
        &name,
        false,
        GraftConfig {
            remote: RemoteConfig::Memory,
            data_dir: data_dir.path().to_path_buf(),
            autosync: None,
        },
    )?;
    Ok(RegisteredVfs { name, _data_dir: data_dir })
}

fn absolute_db_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    if let Ok(canonical) = std::fs::canonicalize(&absolute) {
        return Ok(canonical);
    }

    if let (Some(parent), Some(file_name)) = (absolute.parent(), absolute.file_name())
        && let Ok(parent) = std::fs::canonicalize(parent)
    {
        return Ok(parent.join(file_name));
    }

    Ok(absolute)
}

fn unique_suffix() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos() as u64);
    now ^ COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn parses_log_as_repository_history() {
        let cli = Cli::try_parse_from(["graft", "log"]).unwrap();

        let Command::Log { json, limit, after } = cli.command else {
            panic!("expected log command");
        };
        assert!(!json);
        assert_eq!(limit, None);
        assert_eq!(after, None);
    }

    #[test]
    fn parses_log_json_history() {
        let cli = Cli::try_parse_from(["graft", "log", "--json"]).unwrap();

        let Command::Log { json, limit, after } = cli.command else {
            panic!("expected log command");
        };
        assert!(json);
        assert_eq!(limit, None);
        assert_eq!(after, None);
    }

    #[test]
    fn parses_log_json_pagination() {
        let cli = Cli::try_parse_from([
            "graft", "log", "--json", "--limit", "25", "--after", "abc123",
        ])
        .unwrap();

        let Command::Log { json, limit, after } = cli.command else {
            panic!("expected log command");
        };
        assert!(json);
        assert_eq!(limit, Some(25));
        assert_eq!(after.as_deref(), Some("abc123"));
        assert_eq!(
            repo_log_arg(limit, after.as_deref()).unwrap(),
            "--with-status --limit 25 --after \"abc123\""
        );
    }

    #[test]
    fn parses_internal_id_generator() {
        let cli = Cli::try_parse_from(["graft", "id", "log"]).unwrap();

        let Command::Id { kind } = cli.command else {
            panic!("expected id command");
        };
        assert!(matches!(kind, IdKind::Log));
    }

    #[test]
    fn parses_merge_continue_with_message() {
        let cli =
            Cli::try_parse_from(["graft", "merge", "--continue", "-m", "merge feature"]).unwrap();

        let Command::Merge {
            json,
            abort,
            continue_merge,
            message,
            rev,
        } = cli.command
        else {
            panic!("expected merge command");
        };
        assert!(!json);
        assert!(!abort);
        assert!(continue_merge);
        assert_eq!(message.as_deref(), Some("merge feature"));
        assert_eq!(rev, None);
    }

    #[test]
    fn parses_resolve_ours_with_optional_path() {
        let cli = Cli::try_parse_from(["graft", "resolve", "--ours", "app.db"]).unwrap();

        let Command::Resolve { json, ours, theirs, manual, row, path } = cli.command else {
            panic!("expected resolve command");
        };
        assert!(!json);
        assert!(ours);
        assert!(!theirs);
        assert!(!manual);
        assert_eq!(row, None);
        assert_eq!(path, Some(PathBuf::from("app.db")));
        assert_eq!(
            repo_resolve_arg(ours, theirs, manual, row.as_deref(), path.as_deref()).unwrap(),
            "--ours app.db"
        );
    }

    #[test]
    fn parses_resolve_manual_and_row_selectors() {
        let cli = Cli::try_parse_from(["graft", "resolve", "--manual", "app.db"]).unwrap();

        let Command::Resolve { json, ours, theirs, manual, row, path } = cli.command else {
            panic!("expected resolve command");
        };
        assert!(!json);
        assert!(!ours);
        assert!(!theirs);
        assert!(manual);
        assert_eq!(row, None);
        assert_eq!(path, Some(PathBuf::from("app.db")));
        assert_eq!(
            repo_resolve_arg(ours, theirs, manual, row.as_deref(), path.as_deref()).unwrap(),
            "--manual app.db"
        );

        let cli = Cli::try_parse_from([
            "graft", "resolve", "--json", "--theirs", "--row", "docs", "42", "app.db",
        ])
        .unwrap();

        let Command::Resolve { json, ours, theirs, manual, row, path } = cli.command else {
            panic!("expected resolve command");
        };
        assert!(json);
        assert!(!ours);
        assert!(theirs);
        assert!(!manual);
        assert_eq!(row, Some(vec!["docs".to_string(), "42".to_string()]));
        assert_eq!(path, Some(PathBuf::from("app.db")));
        assert_eq!(
            repo_resolve_arg(ours, theirs, manual, row.as_deref(), path.as_deref()).unwrap(),
            "--theirs --row docs 42 app.db"
        );
        assert_eq!(resolve_pragma(json), "json_resolve_conflict");
    }

    #[test]
    fn parses_switch_merge_conflicts_resolve_json_flags() {
        let cli = Cli::try_parse_from(["graft", "switch", "--json", "-c", "release/1.0", "main"])
            .unwrap();
        let Command::Switch { json, create, force, branch, start_point } = cli.command else {
            panic!("expected switch command");
        };
        assert!(json);
        assert!(create);
        assert!(!force);
        assert_eq!(branch, "release/1.0");
        assert_eq!(start_point.as_deref(), Some("main"));
        assert_eq!(
            repo_switch_arg(force, &branch, start_point.as_deref()),
            "release/1.0 main"
        );
        assert_eq!(switch_create_pragma(json), "json_switch_create");

        let cli = Cli::try_parse_from(["graft", "merge", "--json", "feature/search"]).unwrap();
        let Command::Merge {
            json,
            abort,
            continue_merge,
            message,
            rev,
        } = cli.command
        else {
            panic!("expected merge command");
        };
        assert!(json);
        assert!(!abort);
        assert!(!continue_merge);
        assert_eq!(message, None);
        assert_eq!(rev.as_deref(), Some("feature/search"));
        assert_eq!(merge_pragma(json), "json_merge");

        let cli = Cli::try_parse_from(["graft", "conflicts", "--json", "app.db"]).unwrap();
        let Command::Conflicts(args) = cli.command else {
            panic!("expected conflicts command");
        };
        assert!(args.json);
        assert_eq!(args.db, Some(PathBuf::from("app.db")));
        assert_eq!(conflicts_pragma(args.json), "json_conflicts");

        let cli =
            Cli::try_parse_from(["graft", "resolve", "--json", "--theirs", "app.db"]).unwrap();
        let Command::Resolve { json, ours, theirs, manual, row, path } = cli.command else {
            panic!("expected resolve command");
        };
        assert!(json);
        assert!(!ours);
        assert!(theirs);
        assert!(!manual);
        assert_eq!(row, None);
        assert_eq!(path, Some(PathBuf::from("app.db")));
        assert_eq!(
            repo_resolve_arg(ours, theirs, manual, row.as_deref(), path.as_deref()).unwrap(),
            "--theirs app.db"
        );
        assert_eq!(resolve_pragma(json), "json_resolve_conflict");
    }

    #[test]
    fn parses_add_with_optional_path() {
        let cli = Cli::try_parse_from(["graft", "add", "external.db"]).unwrap();

        let Command::Add(args) = cli.command else {
            panic!("expected add command");
        };
        assert_eq!(args.path, Some(PathBuf::from("external.db")));
        assert!(!args.json);
        assert!(!args.all);
        assert!(!args.force);
        assert_eq!(args.kind, None);
        assert_eq!(
            repo_add_arg(args.all, args.force, args.kind, args.path.as_deref())
                .unwrap()
                .as_deref(),
            Some("-- \"external.db\"")
        );
        assert_eq!(
            repo_add_arg(
                false,
                false,
                None,
                Some(Path::new("notes/it's  \"quoted\".md"))
            )
            .unwrap()
            .as_deref(),
            Some("-- \"notes/it's  \\\"quoted\\\".md\"")
        );
    }

    #[test]
    fn parses_audit_json_flag() {
        let cli = Cli::try_parse_from(["graft", "audit", "--json"]).unwrap();

        let Command::Audit(args) = cli.command else {
            panic!("expected audit command");
        };
        assert!(args.json);
        assert!(!args.repair);
        assert_eq!(args.remote, None);
        assert_eq!(repo_audit_arg(args.repair, args.remote.as_deref()), None);

        let cli = Cli::try_parse_from(["graft", "audit", "--json", "--repair", "origin"]).unwrap();
        let Command::Audit(args) = cli.command else {
            panic!("expected audit command");
        };
        assert!(args.json);
        assert!(args.repair);
        assert_eq!(args.remote.as_deref(), Some("origin"));
        assert_eq!(
            repo_audit_arg(args.repair, args.remote.as_deref()).as_deref(),
            Some("--repair origin")
        );
    }

    #[test]
    fn parses_status_json_kind_filter() {
        let cli = Cli::try_parse_from(["graft", "status", "--json", "--kind", "db"]).unwrap();

        let Command::Status { json, kind } = cli.command else {
            panic!("expected status command");
        };
        assert!(json);
        assert_eq!(kind, Some(PathKind::SqliteDatabase));
        assert_eq!(
            repo_status_arg(kind).as_deref(),
            Some("--kind sqlite_database")
        );
    }

    #[test]
    fn parses_ls_files_json_flag() {
        let cli = Cli::try_parse_from(["graft", "ls-files", "--json"]).unwrap();

        let Command::LsFiles { json, stage, details, others, kind } = cli.command else {
            panic!("expected ls-files command");
        };
        assert!(json);
        assert!(!stage);
        assert!(!details);
        assert!(!others);
        assert_eq!(kind, None);
        assert_eq!(repo_ls_files_arg(stage, details, others, kind), None);
    }

    #[test]
    fn parses_ls_files_stage_json_flags() {
        let cli = Cli::try_parse_from([
            "graft",
            "ls-files",
            "--stage",
            "--json",
            "--kind",
            "sqlite_database",
        ])
        .unwrap();

        let Command::LsFiles { json, stage, details, others, kind } = cli.command else {
            panic!("expected ls-files command");
        };
        assert!(json);
        assert!(stage);
        assert!(!details);
        assert!(!others);
        assert_eq!(kind, Some(PathKind::SqliteDatabase));
        assert_eq!(
            repo_ls_files_arg(stage, details, others, kind).as_deref(),
            Some("--stage --kind sqlite_database")
        );

        let cli = Cli::try_parse_from(["graft", "ls-files", "--details", "--kind", "binary_file"])
            .unwrap();
        let Command::LsFiles { json, stage, details, others, kind } = cli.command else {
            panic!("expected ls-files command");
        };
        assert!(!json);
        assert!(!stage);
        assert!(details);
        assert!(!others);
        assert_eq!(kind, Some(PathKind::BinaryFile));
        assert_eq!(
            repo_ls_files_arg(stage, details, others, kind).as_deref(),
            Some("--details --kind binary_file")
        );

        let cli = Cli::try_parse_from([
            "graft",
            "ls-files",
            "--others",
            "--json",
            "--kind",
            "text_file",
        ])
        .unwrap();
        let Command::LsFiles { json, stage, details, others, kind } = cli.command else {
            panic!("expected ls-files command");
        };
        assert!(json);
        assert!(!stage);
        assert!(!details);
        assert!(others);
        assert_eq!(kind, Some(PathKind::TextFile));
        assert_eq!(
            repo_ls_files_arg(stage, details, others, kind).as_deref(),
            Some("--others --kind text_file")
        );

        assert!(Cli::try_parse_from(["graft", "ls-files", "--stage", "--details"]).is_err());
        assert!(Cli::try_parse_from(["graft", "ls-files", "--stage", "--others"]).is_err());
        assert!(Cli::try_parse_from(["graft", "ls-files", "--details", "--others"]).is_err());
    }

    #[test]
    fn parses_payload_fetch_flags() {
        let cli = Cli::try_parse_from(["graft", "payload", "fetch"]).unwrap();
        let Command::Payload { command } = cli.command else {
            panic!("expected payload command");
        };
        let PayloadCommand::Fetch(args) = command else {
            panic!("expected payload fetch command");
        };
        assert!(!args.json);
        assert_eq!(args.remote, None);
        assert_eq!(args.rev, None);
        assert_eq!(
            repo_payload_fetch_arg(args.remote.as_deref(), args.rev.as_deref()),
            None
        );

        let cli = Cli::try_parse_from(["graft", "lfs", "fetch"]).unwrap();
        assert!(matches!(cli.command, Command::Payload { .. }));

        let cli = Cli::try_parse_from([
            "graft", "payload", "fetch", "--json", "--remote", "origin", "HEAD~1",
        ])
        .unwrap();
        let Command::Payload { command } = cli.command else {
            panic!("expected payload command");
        };
        let PayloadCommand::Fetch(args) = command else {
            panic!("expected payload fetch command");
        };
        assert!(args.json);
        assert_eq!(args.remote.as_deref(), Some("origin"));
        assert_eq!(args.rev.as_deref(), Some("HEAD~1"));
        assert_eq!(
            repo_payload_fetch_arg(args.remote.as_deref(), args.rev.as_deref()).as_deref(),
            Some("--remote origin HEAD~1")
        );

        assert!(Cli::try_parse_from(["graft", "payload", "fetch", "--remote"]).is_err());
    }

    #[test]
    fn parses_payload_status_flags() {
        let cli = Cli::try_parse_from(["graft", "payload", "status"]).unwrap();
        let Command::Payload { command } = cli.command else {
            panic!("expected payload command");
        };
        let PayloadCommand::Status(args) = command else {
            panic!("expected payload status command");
        };
        assert!(!args.json);
        assert_eq!(args.rev, None);
        assert_eq!(repo_payload_status_arg(args.rev.as_deref()), None);

        let cli = Cli::try_parse_from(["graft", "payload", "status", "--json", "HEAD~1"]).unwrap();
        let Command::Payload { command } = cli.command else {
            panic!("expected payload command");
        };
        let PayloadCommand::Status(args) = command else {
            panic!("expected payload status command");
        };
        assert!(args.json);
        assert_eq!(args.rev.as_deref(), Some("HEAD~1"));
        assert_eq!(
            repo_payload_status_arg(args.rev.as_deref()).as_deref(),
            Some("HEAD~1")
        );
    }

    #[test]
    fn parses_payload_prune_flags() {
        let cli = Cli::try_parse_from(["graft", "payload", "prune", "--json"]).unwrap();
        let Command::Payload { command } = cli.command else {
            panic!("expected payload command");
        };
        let PayloadCommand::Prune(args) = command else {
            panic!("expected payload prune command");
        };
        assert!(args.json);
        assert!(!args.dry_run);
        assert!(!args.force);
        assert_eq!(repo_payload_prune_arg(args.dry_run, args.force), None);

        let cli = Cli::try_parse_from(["graft", "payload", "prune", "--force"]).unwrap();
        let Command::Payload { command } = cli.command else {
            panic!("expected payload command");
        };
        let PayloadCommand::Prune(args) = command else {
            panic!("expected payload prune command");
        };
        assert!(!args.json);
        assert!(!args.dry_run);
        assert!(args.force);
        assert_eq!(
            repo_payload_prune_arg(args.dry_run, args.force).as_deref(),
            Some("--force")
        );

        assert!(
            Cli::try_parse_from(["graft", "payload", "prune", "--dry-run", "--force"]).is_err()
        );
    }

    #[test]
    fn parses_config_get() {
        let cli =
            Cli::try_parse_from(["graft", "config", "get", "files.inline_text_threshold"]).unwrap();

        let Command::Config { command } = cli.command else {
            panic!("expected config command");
        };
        let ConfigCommand::Get { json, key } = command else {
            panic!("expected config get command");
        };
        assert!(!json);
        assert_eq!(key, "files.inline_text_threshold");
    }

    #[test]
    fn parses_config_get_json_flag() {
        let cli = Cli::try_parse_from([
            "graft",
            "config",
            "get",
            "--json",
            "files.inline_text_threshold",
        ])
        .unwrap();

        let Command::Config { command } = cli.command else {
            panic!("expected config command");
        };
        let ConfigCommand::Get { json, key } = command else {
            panic!("expected config get command");
        };
        assert!(json);
        assert_eq!(key, "files.inline_text_threshold");
        assert_eq!(config_get_pragma(json), "json_config_get");
    }

    #[test]
    fn parses_config_list_json_flag() {
        let cli = Cli::try_parse_from(["graft", "config", "list", "--json"]).unwrap();

        let Command::Config { command } = cli.command else {
            panic!("expected config command");
        };
        let ConfigCommand::List { json } = command else {
            panic!("expected config list command");
        };
        assert!(json);
    }

    #[test]
    fn config_list_json_uses_status_wrapper() {
        assert_eq!(config_list_pragma(false), ("config_list", None));
        assert_eq!(
            config_list_pragma(true),
            ("json_config_list", Some("--with-status"))
        );
    }

    #[test]
    fn parses_config_set_value_with_spaces() {
        let cli = Cli::try_parse_from([
            "graft",
            "config",
            "set",
            "files.inline_text_threshold",
            "8",
            "MB",
        ])
        .unwrap();

        let Command::Config { command } = cli.command else {
            panic!("expected config command");
        };
        let ConfigCommand::Set { json, key, value } = command else {
            panic!("expected config set command");
        };
        assert!(!json);
        assert_eq!(key, "files.inline_text_threshold");
        assert_eq!(value, ["8".to_string(), "MB".to_string()]);
        assert_eq!(
            repo_config_set_arg(&key, &value).unwrap(),
            "files.inline_text_threshold -- 8 MB"
        );
    }

    #[test]
    fn parses_config_set_json_flag() {
        let cli = Cli::try_parse_from([
            "graft",
            "config",
            "set",
            "--json",
            "files.inline_text_threshold",
            "8",
            "MB",
        ])
        .unwrap();

        let Command::Config { command } = cli.command else {
            panic!("expected config command");
        };
        let ConfigCommand::Set { json, key, value } = command else {
            panic!("expected config set command");
        };
        assert!(json);
        assert_eq!(key, "files.inline_text_threshold");
        assert_eq!(value, ["8".to_string(), "MB".to_string()]);
        assert_eq!(config_set_pragma(json), "json_config_set");
    }

    #[test]
    fn parses_config_unset() {
        let cli =
            Cli::try_parse_from(["graft", "config", "unset", "merge.semantic_keys.documents"])
                .unwrap();

        let Command::Config { command } = cli.command else {
            panic!("expected config command");
        };
        let ConfigCommand::Unset { json, key } = command else {
            panic!("expected config unset command");
        };
        assert!(!json);
        assert_eq!(key, "merge.semantic_keys.documents");
    }

    #[test]
    fn parses_config_unset_json_flag() {
        let cli = Cli::try_parse_from([
            "graft",
            "config",
            "unset",
            "--json",
            "merge.semantic_keys.documents",
        ])
        .unwrap();

        let Command::Config { command } = cli.command else {
            panic!("expected config command");
        };
        let ConfigCommand::Unset { json, key } = command else {
            panic!("expected config unset command");
        };
        assert!(json);
        assert_eq!(key, "merge.semantic_keys.documents");
        assert_eq!(config_unset_pragma(json), "json_config_unset");
    }

    #[test]
    fn parses_add_force_with_optional_path() {
        let cli = Cli::try_parse_from(["graft", "add", "--force", "external.db"]).unwrap();

        let Command::Add(args) = cli.command else {
            panic!("expected add command");
        };
        assert_eq!(args.path, Some(PathBuf::from("external.db")));
        assert!(!args.json);
        assert!(!args.all);
        assert!(args.force);
        assert_eq!(args.kind, None);
        assert_eq!(
            repo_add_arg(args.all, args.force, args.kind, args.path.as_deref())
                .unwrap()
                .as_deref(),
            Some("--force -- \"external.db\"")
        );
    }

    #[test]
    fn parses_add_all() {
        let cli = Cli::try_parse_from(["graft", "add", "--all"]).unwrap();

        let Command::Add(args) = cli.command else {
            panic!("expected add command");
        };
        assert!(!args.json);
        assert!(args.all);
        assert!(!args.force);
        assert_eq!(args.kind, None);
        assert_eq!(args.path, None);
        assert_eq!(
            repo_add_arg(args.all, args.force, args.kind, args.path.as_deref())
                .unwrap()
                .as_deref(),
            Some("--all")
        );

        assert!(Cli::try_parse_from(["graft", "add", "--all", "external.db"]).is_err());
        assert!(Cli::try_parse_from(["graft", "add", "--all", "--force"]).is_err());
    }

    #[test]
    fn parses_add_all_kind_filter() {
        let cli = Cli::try_parse_from(["graft", "add", "--all", "--kind", "db"]).unwrap();

        let Command::Add(args) = cli.command else {
            panic!("expected add command");
        };
        assert!(!args.json);
        assert!(args.all);
        assert!(!args.force);
        assert_eq!(args.kind, Some(PathKind::SqliteDatabase));
        assert_eq!(args.path, None);
        assert_eq!(
            repo_add_arg(args.all, args.force, args.kind, args.path.as_deref())
                .unwrap()
                .as_deref(),
            Some("--all --kind sqlite_database")
        );

        assert!(Cli::try_parse_from(["graft", "add", "--kind", "db"]).is_err());
        assert!(
            Cli::try_parse_from(["graft", "add", "--all", "--kind", "db", "external.db"]).is_err()
        );
    }

    #[test]
    fn parses_rm_with_optional_path() {
        let cli = Cli::try_parse_from(["graft", "rm", "external.db"]).unwrap();

        let Command::Rm(args) = cli.command else {
            panic!("expected rm command");
        };
        assert!(!args.json);
        assert!(!args.cached);
        assert_eq!(args.path, Some(PathBuf::from("external.db")));
        assert_eq!(
            repo_rm_arg(args.cached, args.path.as_deref()).as_deref(),
            Some("external.db")
        );

        let cli = Cli::try_parse_from(["graft", "rm", "--cached", "external.db"]).unwrap();
        let Command::Rm(args) = cli.command else {
            panic!("expected rm command");
        };
        assert!(!args.json);
        assert!(args.cached);
        assert_eq!(args.path, Some(PathBuf::from("external.db")));
        assert_eq!(
            repo_rm_arg(args.cached, args.path.as_deref()).as_deref(),
            Some("--cached -- external.db")
        );
    }

    #[test]
    fn parses_stage_commit_json_flags() {
        let cli = Cli::try_parse_from(["graft", "add", "--json", "--all"]).unwrap();
        let Command::Add(args) = cli.command else {
            panic!("expected add command");
        };
        assert!(args.json);
        assert!(args.all);
        assert_eq!(args.kind, None);
        assert_eq!(
            repo_add_arg(args.all, args.force, args.kind, args.path.as_deref())
                .unwrap()
                .as_deref(),
            Some("--all")
        );
        assert_eq!(add_pragma(args.json), "json_add");

        let cli = Cli::try_parse_from(["graft", "rm", "--json", "external.db"]).unwrap();
        let Command::Rm(args) = cli.command else {
            panic!("expected rm command");
        };
        assert!(args.json);
        assert!(!args.cached);
        assert_eq!(args.path, Some(PathBuf::from("external.db")));
        assert_eq!(
            repo_rm_arg(args.cached, args.path.as_deref()).as_deref(),
            Some("external.db")
        );
        assert_eq!(rm_pragma(args.json), "json_rm");

        let cli = Cli::try_parse_from(["graft", "commit", "--json", "-m", "save state"]).unwrap();
        let Command::Commit { json, message } = cli.command else {
            panic!("expected commit command");
        };
        assert!(json);
        assert_eq!(message, "save state");
        assert_eq!(commit_pragma(json), "json_commit");
    }

    #[test]
    fn parses_diff_rows_and_builds_pragma_arg() {
        let cli = Cli::try_parse_from([
            "graft", "diff", "--rows", "--json", "HEAD~1", "HEAD", "app.db",
        ])
        .unwrap();

        let Command::Diff {
            rows,
            staged,
            kind,
            content,
            max_content_bytes,
            root,
            from,
            to,
            path,
            json,
        } = cli.command
        else {
            panic!("expected diff command");
        };
        assert!(rows);
        assert!(!staged);
        assert_eq!(kind, None);
        assert!(!content);
        assert_eq!(max_content_bytes, None);
        assert_eq!(from.as_deref(), Some("HEAD~1"));
        assert_eq!(to.as_deref(), Some("HEAD"));
        assert_eq!(path, Some(PathBuf::from("app.db")));
        assert!(json);
        assert_eq!(
            repo_diff_arg(RepoDiffArgSpec {
                rows,
                staged,
                kind,
                content,
                max_content_bytes,
                root: root.as_deref(),
                from: from.as_deref(),
                to: to.as_deref(),
                path: path.as_deref(),
            })
            .unwrap(),
            Some("--rows HEAD~1 HEAD -- \"app.db\"".to_string())
        );
        assert_eq!(
            repo_diff_arg(RepoDiffArgSpec {
                rows: true,
                staged: true,
                from: Some("app.db"),
                ..Default::default()
            })
            .unwrap(),
            Some("--rows --staged -- \"app.db\"".to_string())
        );
        assert_eq!(
            repo_diff_arg(RepoDiffArgSpec { rows: true, ..Default::default() }).unwrap(),
            Some("--rows".to_string())
        );

        let cli = Cli::try_parse_from([
            "graft",
            "diff",
            "--json",
            "--kind",
            "binary_file",
            "--staged",
        ])
        .unwrap();
        let Command::Diff {
            rows,
            staged,
            kind,
            content,
            max_content_bytes,
            root,
            from,
            to,
            path,
            json,
        } = cli.command
        else {
            panic!("expected diff command");
        };
        assert!(!rows);
        assert!(staged);
        assert_eq!(kind, Some(PathKind::BinaryFile));
        assert!(!content);
        assert_eq!(max_content_bytes, None);
        assert_eq!(from, None);
        assert_eq!(to, None);
        assert_eq!(path, None);
        assert!(json);
        assert_eq!(
            repo_diff_arg(RepoDiffArgSpec {
                rows,
                staged,
                kind,
                content,
                max_content_bytes,
                root: root.as_deref(),
                from: from.as_deref(),
                to: to.as_deref(),
                path: path.as_deref(),
            })
            .unwrap(),
            Some("--kind binary_file --staged".to_string())
        );
    }

    #[test]
    fn parses_bounded_single_path_text_content_diff() {
        let cli = Cli::try_parse_from([
            "graft",
            "diff",
            "--json",
            "--content",
            "--max-content-bytes",
            "4096",
            "HEAD~1",
            "HEAD",
            "--",
            "notes/readme.md",
        ])
        .unwrap();
        let Command::Diff {
            rows,
            staged,
            kind,
            content,
            max_content_bytes,
            root,
            from,
            to,
            path,
            json,
        } = cli.command
        else {
            panic!("expected diff command");
        };

        assert!(json);
        assert!(content);
        assert!(!rows);
        assert!(!staged);
        assert_eq!(kind, None);
        assert_eq!(max_content_bytes.map(NonZeroU64::get), Some(4096));
        assert_eq!(
            repo_diff_arg(RepoDiffArgSpec {
                rows,
                staged,
                kind,
                content,
                max_content_bytes,
                root: root.as_deref(),
                from: from.as_deref(),
                to: to.as_deref(),
                path: path.as_deref(),
            })
            .unwrap(),
            Some(
                "--content --max-content-bytes 4096 HEAD~1 HEAD -- \"notes/readme.md\"".to_string()
            )
        );

        assert!(
            Cli::try_parse_from(["graft", "diff", "--content", "HEAD~1", "HEAD", "note.md",])
                .is_err()
        );
        let cli = Cli::try_parse_from(["graft", "diff", "--json", "--content", "HEAD~1", "HEAD"])
            .unwrap();
        assert!(run_command(cli.command, None).is_err());
        assert!(
            Cli::try_parse_from([
                "graft",
                "diff",
                "--json",
                "--rows",
                "--content",
                "HEAD~1",
                "HEAD",
                "note.md",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "graft",
                "diff",
                "--json",
                "--content",
                "--max-content-bytes",
                "0",
                "HEAD~1",
                "HEAD",
                "note.md",
            ])
            .is_err()
        );
    }

    #[test]
    fn parses_single_path_worktree_text_content_diff() {
        let cli = Cli::try_parse_from([
            "graft",
            "diff",
            "--json",
            "--content",
            "HEAD",
            "--",
            "notes/readme.md",
        ])
        .unwrap();
        let Command::Diff {
            rows,
            staged,
            kind,
            content,
            max_content_bytes,
            root,
            from,
            to,
            path,
            json,
        } = cli.command
        else {
            panic!("expected diff command");
        };

        assert!(json);
        assert!(content);
        assert_eq!(from.as_deref(), Some("HEAD"));
        assert_eq!(to.as_deref(), Some("notes/readme.md"));
        assert_eq!(path, None);
        assert_eq!(
            repo_diff_arg(RepoDiffArgSpec {
                rows,
                staged,
                kind,
                content,
                max_content_bytes,
                root: root.as_deref(),
                from: from.as_deref(),
                to: to.as_deref(),
                path: path.as_deref(),
            })
            .unwrap(),
            Some("--content HEAD -- \"notes/readme.md\"".to_string())
        );
    }

    #[test]
    fn parses_explicit_root_content_diff() {
        let cli = Cli::try_parse_from([
            "graft",
            "diff",
            "--json",
            "--content",
            "--max-content-bytes",
            "4096",
            "--root",
            "HEAD",
            "--",
            "notes/first  draft.md",
        ])
        .unwrap();
        let Command::Diff {
            rows,
            staged,
            kind,
            content,
            max_content_bytes,
            root,
            from,
            to,
            path,
            json,
        } = cli.command
        else {
            panic!("expected diff command");
        };
        assert!(json);
        assert!(content);
        assert!(!rows);
        assert!(!staged);
        assert_eq!(root.as_deref(), Some("HEAD"));
        assert_eq!(from.as_deref(), Some("notes/first  draft.md"));
        assert_eq!(to, None);
        assert_eq!(path, None);
        assert_eq!(
            repo_diff_arg(RepoDiffArgSpec {
                rows,
                staged,
                kind,
                content,
                max_content_bytes,
                root: root.as_deref(),
                from: from.as_deref(),
                to: to.as_deref(),
                path: path.as_deref(),
            })
            .unwrap(),
            Some(
                "--content --max-content-bytes 4096 --root HEAD -- \"notes/first  draft.md\""
                    .to_string()
            )
        );
    }

    #[test]
    fn parses_sql_command() {
        let cli = Cli::try_parse_from(["graft", "sql", "select", "1"]).unwrap();

        let Command::Sql { sql } = cli.command else {
            panic!("expected sql command");
        };
        assert_eq!(sql, ["select", "1"]);
    }

    #[test]
    fn init_command_initializes_current_directory() {
        let _guard = CWD_LOCK.lock().unwrap();
        let original_dir = std::env::current_dir().unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(temp_dir.path()).unwrap();

        let result = run_command(Command::Init(InitArgs { json: false }), None);
        std::env::set_current_dir(original_dir).unwrap();
        result.unwrap();

        let repo = graft::repo::Repository::open(temp_dir.path()).unwrap();
        assert!(repo.graft_dir().join("config.toml").exists());
        assert!(!temp_dir.path().join("app.db").exists());
    }

    #[test]
    fn sql_command_runs_through_graft_vfs() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db = temp_dir.path().join("app.db");
        graft::repo::Repository::init(temp_dir.path()).unwrap();

        let output = run_sql(
            Some(&db),
            &[String::from(
                "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT); \
                 INSERT INTO users(name) VALUES ('Alice'), ('Bob'); \
                 SELECT name FROM users ORDER BY id; \
                 PRAGMA graft_status;",
            )],
        )
        .unwrap()
        .unwrap();
        assert!(output.contains("name\nAlice\nBob\n"), "{output}");
        assert!(output.contains("untracked: app.db"), "{output}");
    }

    #[test]
    fn sql_command_materializes_subdir_database_on_commit() {
        let _guard = CWD_LOCK.lock().unwrap();
        let original_dir = std::env::current_dir().unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(temp_dir.path()).unwrap();

        let result = (|| -> Result<()> {
            run_command(Command::Init(InitArgs { json: false }), None)?;
            let output = run_sql(
                Some(Path::new("sub-app/main.sqlite")),
                &[String::from(
                    "CREATE TABLE docs(id TEXT PRIMARY KEY, title TEXT); \
                     INSERT INTO docs VALUES ('1', 'Hello'); \
                     PRAGMA graft_add; \
                     PRAGMA graft_json_commit = 'initial docs';",
                )],
            )?
            .unwrap();
            assert!(output.contains("\"materialized\""), "{output}");

            let materialized = temp_dir.path().join("sub-app/main.sqlite");
            assert!(materialized.exists());
            let conn = Connection::open(materialized).unwrap();
            let title: String = conn
                .query_row("SELECT title FROM docs WHERE id = '1'", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(title, "Hello");
            Ok(())
        })();

        std::env::set_current_dir(original_dir).unwrap();
        result.unwrap();
    }

    #[test]
    fn sql_command_requires_explicit_db_and_existing_graft_repo() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db = temp_dir.path().join("app.db");

        let err = run_sql(
            Some(&db),
            &[String::from("CREATE TABLE users(id INTEGER PRIMARY KEY)")],
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("not a Graft repository"),
            "{err:#}"
        );

        let err = run_sql(None, &[String::from("SELECT 1")]).unwrap_err();
        assert!(err.to_string().contains("requires --db <path>"), "{err:#}");
    }

    #[test]
    fn parses_clone_with_optional_branch() {
        let cli = Cli::try_parse_from(["graft", "clone", "fs:///srv/graft/app"]).unwrap();

        let Command::Clone { json, branch_option, remote, branch } = cli.command else {
            panic!("expected clone command");
        };
        assert!(!json);
        assert_eq!(branch_option, None);
        assert_eq!(remote, "fs:///srv/graft/app");
        assert_eq!(branch, None);
        assert_eq!(
            repo_clone_arg(&remote, branch.as_deref()),
            "fs:///srv/graft/app"
        );

        let cli = Cli::try_parse_from(["graft", "clone", "fs:///srv/graft/app", "feature/search"])
            .unwrap();

        let Command::Clone { json, branch_option, remote, branch } = cli.command else {
            panic!("expected clone command");
        };
        assert!(!json);
        assert_eq!(branch_option, None);
        assert_eq!(remote, "fs:///srv/graft/app");
        assert_eq!(branch.as_deref(), Some("feature/search"));
        assert_eq!(
            repo_clone_arg(&remote, branch.as_deref()),
            "fs:///srv/graft/app feature/search"
        );

        let cli = Cli::try_parse_from([
            "graft",
            "clone",
            "--branch",
            "feature/search",
            "fs:///srv/graft/app",
        ])
        .unwrap();

        let Command::Clone { json, branch_option, remote, branch } = cli.command else {
            panic!("expected clone command");
        };
        assert!(!json);
        assert_eq!(branch_option.as_deref(), Some("feature/search"));
        assert_eq!(remote, "fs:///srv/graft/app");
        assert_eq!(branch, None);
        assert_eq!(
            repo_clone_arg(&remote, branch_option.as_deref().or(branch.as_deref())),
            "fs:///srv/graft/app feature/search"
        );

        assert!(
            Cli::try_parse_from([
                "graft",
                "clone",
                "-b",
                "feature/search",
                "fs:///srv/graft/app",
                "main",
            ])
            .is_err()
        );
    }

    #[test]
    fn clone_defaults_its_database_tag_to_the_current_worktree() {
        let _guard = CWD_LOCK.lock().unwrap();
        let original_dir = std::env::current_dir().unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(temp_dir.path()).unwrap();

        let result = resolve_clone_db(None);

        std::env::set_current_dir(original_dir).unwrap();
        assert_eq!(
            result.unwrap(),
            std::fs::canonicalize(temp_dir.path())
                .unwrap()
                .join(".graft-clone.sqlite")
        );
    }

    #[test]
    fn parses_lifecycle_export_json_flags() {
        let cli = Cli::try_parse_from(["graft", "init", "--json"]).unwrap();
        let Command::Init(args) = cli.command else {
            panic!("expected init command");
        };
        assert!(args.json);
        assert!(Cli::try_parse_from(["graft", "init", "app.db"]).is_err());

        let cli = Cli::try_parse_from([
            "graft",
            "clone",
            "--json",
            "--branch",
            "main",
            "fs:///srv/graft/app",
        ])
        .unwrap();
        let Command::Clone { json, branch_option, remote, branch } = cli.command else {
            panic!("expected clone command");
        };
        assert!(json);
        assert_eq!(branch_option.as_deref(), Some("main"));
        assert_eq!(remote, "fs:///srv/graft/app");
        assert_eq!(branch, None);
        assert_eq!(
            repo_clone_arg(&remote, branch_option.as_deref().or(branch.as_deref())),
            "fs:///srv/graft/app main"
        );
        assert_eq!(clone_pragma(json), "json_clone");

        let cli = Cli::try_parse_from([
            "graft",
            "export",
            "--json",
            "--source",
            "HEAD",
            "--output",
            "snapshot.db",
            "app.db",
        ])
        .unwrap();
        let Command::Export(args) = cli.command else {
            panic!("expected export command");
        };
        assert!(args.json);
        assert_eq!(args.source.as_deref(), Some("HEAD"));
        assert_eq!(args.output, PathBuf::from("snapshot.db"));
        assert_eq!(args.path, Some(PathBuf::from("app.db")));
        assert_eq!(
            repo_export_arg(args.source.as_deref(), &args.output, args.path.as_deref()),
            "--source HEAD --output snapshot.db -- app.db"
        );
        assert_eq!(export_pragma(args.json), "json_export");
    }

    #[test]
    fn resolve_requires_a_side() {
        assert!(Cli::try_parse_from(["graft", "resolve"]).is_err());
    }

    #[test]
    fn parses_branch_force_delete() {
        let cli = Cli::try_parse_from(["graft", "branch", "-D", "feature/search"]).unwrap();

        let Command::Branch {
            json,
            delete,
            force_delete,
            move_branch,
            force_move,
            set_upstream_to,
            unset_upstream,
            remote,
            all,
            name,
            start_point,
        } = cli.command
        else {
            panic!("expected branch command");
        };
        assert!(!json);
        assert!(!delete);
        assert!(force_delete);
        assert!(!move_branch);
        assert!(!force_move);
        assert_eq!(set_upstream_to, None);
        assert!(!unset_upstream);
        assert!(!remote);
        assert!(!all);
        assert_eq!(name.as_deref(), Some("feature/search"));
        assert_eq!(start_point, None);
    }

    #[test]
    fn parses_branch_rename_flags() {
        let cli = Cli::try_parse_from(["graft", "branch", "-m", "feature/query"]).unwrap();
        let Command::Branch {
            move_branch,
            force_move,
            name,
            start_point,
            ..
        } = cli.command
        else {
            panic!("expected branch command");
        };
        assert!(move_branch);
        assert!(!force_move);
        assert_eq!(name.as_deref(), Some("feature/query"));
        assert_eq!(start_point, None);

        let cli = Cli::try_parse_from(["graft", "branch", "-M", "feature/search", "feature/query"])
            .unwrap();
        let Command::Branch {
            move_branch,
            force_move,
            name,
            start_point,
            ..
        } = cli.command
        else {
            panic!("expected branch command");
        };
        assert!(!move_branch);
        assert!(force_move);
        assert_eq!(name.as_deref(), Some("feature/search"));
        assert_eq!(start_point.as_deref(), Some("feature/query"));
    }

    #[test]
    fn parses_branch_and_switch_create_start_points() {
        let cli = Cli::try_parse_from(["graft", "branch", "release/1.0", "HEAD~1"]).unwrap();

        let Command::Branch {
            json,
            delete,
            force_delete,
            move_branch,
            force_move,
            set_upstream_to,
            unset_upstream,
            remote,
            all,
            name,
            start_point,
        } = cli.command
        else {
            panic!("expected branch command");
        };
        assert!(!json);
        assert!(!delete);
        assert!(!force_delete);
        assert!(!move_branch);
        assert!(!force_move);
        assert_eq!(set_upstream_to, None);
        assert!(!unset_upstream);
        assert!(!remote);
        assert!(!all);
        assert_eq!(name.as_deref(), Some("release/1.0"));
        assert_eq!(start_point.as_deref(), Some("HEAD~1"));

        let cli = Cli::try_parse_from(["graft", "switch", "-c", "release/1.0", "main"]).unwrap();
        let Command::Switch { json, create, force, branch, start_point } = cli.command else {
            panic!("expected switch command");
        };
        assert!(!json);
        assert!(create);
        assert!(!force);
        assert_eq!(branch, "release/1.0");
        assert_eq!(start_point.as_deref(), Some("main"));
        assert_eq!(
            repo_switch_arg(force, &branch, start_point.as_deref()),
            "release/1.0 main"
        );
    }

    #[test]
    fn parses_force_checkout_and_switch() {
        let cli = Cli::try_parse_from(["graft", "checkout", "--force", "HEAD~1"]).unwrap();
        let Command::Checkout { json, force, rev, path } = cli.command else {
            panic!("expected checkout command");
        };
        assert!(!json);
        assert!(force);
        assert_eq!(rev, "HEAD~1");
        assert_eq!(path, None);
        assert_eq!(
            repo_checkout_arg(force, &rev, path.as_deref()),
            "--force HEAD~1"
        );

        let cli = Cli::try_parse_from(["graft", "switch", "--force", "main"]).unwrap();
        let Command::Switch { json, create, force, branch, start_point } = cli.command else {
            panic!("expected switch command");
        };
        assert!(!json);
        assert!(!create);
        assert!(force);
        assert_eq!(branch, "main");
        assert_eq!(start_point, None);
        assert_eq!(
            repo_switch_arg(force, &branch, start_point.as_deref()),
            "--force main"
        );
    }

    #[test]
    fn parses_restore_with_optional_source() {
        let cli =
            Cli::try_parse_from(["graft", "restore", "--source", "HEAD~1", "external.db"]).unwrap();
        let Command::Restore {
            json, source, staged, all, kind, path, ..
        } = cli.command
        else {
            panic!("expected restore command");
        };
        assert!(!json);
        assert_eq!(source.as_deref(), Some("HEAD~1"));
        assert!(!staged);
        assert!(!all);
        assert_eq!(kind, None);
        assert_eq!(path, Some(PathBuf::from("external.db")));
        assert_eq!(
            repo_restore_arg(
                source.as_deref(),
                None,
                false,
                staged,
                all,
                kind,
                path.as_deref(),
            )
            .unwrap(),
            "--source HEAD~1 -- \"external.db\""
        );

        let cli = Cli::try_parse_from(["graft", "restore", "--staged", "external.db"]).unwrap();
        let Command::Restore {
            json, source, staged, all, kind, path, ..
        } = cli.command
        else {
            panic!("expected restore command");
        };
        assert!(!json);
        assert_eq!(source, None);
        assert!(staged);
        assert!(!all);
        assert_eq!(kind, None);
        assert_eq!(path, Some(PathBuf::from("external.db")));
        assert_eq!(
            repo_restore_arg(
                source.as_deref(),
                None,
                false,
                staged,
                all,
                kind,
                path.as_deref(),
            )
            .unwrap(),
            "--staged -- \"external.db\""
        );

        let cli = Cli::try_parse_from([
            "graft",
            "restore",
            "--staged",
            "--source",
            "HEAD~1",
            "external.db",
        ])
        .unwrap();
        let Command::Restore {
            json, source, staged, all, kind, path, ..
        } = cli.command
        else {
            panic!("expected restore command");
        };
        assert!(!json);
        assert_eq!(source.as_deref(), Some("HEAD~1"));
        assert!(staged);
        assert!(!all);
        assert_eq!(kind, None);
        assert_eq!(path, Some(PathBuf::from("external.db")));
        assert_eq!(
            repo_restore_arg(
                source.as_deref(),
                None,
                false,
                staged,
                all,
                kind,
                path.as_deref(),
            )
            .unwrap(),
            "--staged --source HEAD~1 -- \"external.db\""
        );

        let expected = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let cli = Cli::try_parse_from([
            "graft",
            "restore",
            "--source",
            "HEAD~1",
            "--expected-head",
            expected,
            "--require-clean",
            "external.db",
        ])
        .unwrap();
        let Command::Restore {
            source,
            expected_head,
            require_clean,
            staged,
            all,
            kind,
            path,
            ..
        } = cli.command
        else {
            panic!("expected restore command");
        };
        assert_eq!(expected_head.as_deref(), Some(expected));
        assert!(require_clean);
        assert_eq!(
            repo_restore_arg(
                source.as_deref(),
                expected_head.as_deref(),
                require_clean,
                staged,
                all,
                kind,
                path.as_deref(),
            )
            .unwrap(),
            format!(
                "--source HEAD~1 --expected-head {expected} --require-clean -- \"external.db\""
            )
        );
    }

    #[test]
    fn rejects_ambiguous_repo_paths_before_pragma_serialization() {
        let cli = Cli::try_parse_from(["graft", "add", " note.md "]).unwrap();
        let err = validate_command_repo_paths(&cli.command).unwrap_err();
        assert!(
            err.to_string()
                .contains("path components must not start or end with whitespace"),
            "{err}"
        );

        let cli =
            Cli::try_parse_from(["graft", "restore", "--source", "HEAD", " note.md "]).unwrap();
        let err = validate_command_repo_paths(&cli.command).unwrap_err();
        assert!(
            err.to_string()
                .contains("path components must not start or end with whitespace"),
            "{err}"
        );

        let cli = Cli::try_parse_from(["graft", "restore", "my  note.md"]).unwrap();
        validate_command_repo_paths(&cli.command).unwrap();
        let Command::Restore { source, staged, all, kind, path, .. } = cli.command else {
            panic!("expected restore command");
        };
        assert_eq!(
            repo_restore_arg(
                source.as_deref(),
                None,
                false,
                staged,
                all,
                kind,
                path.as_deref(),
            )
            .unwrap(),
            "-- \"my  note.md\""
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn rejects_posix_backslash_repo_path_before_pragma_serialization() {
        let cli =
            Cli::try_parse_from(["graft", "restore", "--source", "HEAD", "foo\\bar.md"]).unwrap();
        let err = validate_command_repo_paths(&cli.command).unwrap_err();
        assert!(
            err.to_string()
                .contains("backslashes are not supported in POSIX repository paths"),
            "{err}"
        );
    }

    #[test]
    fn parses_restore_staged_all_kind_filter() {
        let cli =
            Cli::try_parse_from(["graft", "restore", "--staged", "--all", "--kind", "db"]).unwrap();
        let Command::Restore {
            json, source, staged, all, kind, path, ..
        } = cli.command
        else {
            panic!("expected restore command");
        };
        assert!(!json);
        assert_eq!(source, None);
        assert!(staged);
        assert!(all);
        assert_eq!(kind, Some(PathKind::SqliteDatabase));
        assert_eq!(path, None);
        assert_eq!(
            repo_restore_arg(
                source.as_deref(),
                None,
                false,
                staged,
                all,
                kind,
                path.as_deref(),
            )
            .unwrap(),
            "--staged --all --kind sqlite_database"
        );

        assert!(Cli::try_parse_from(["graft", "restore", "--all"]).is_err());
        assert!(Cli::try_parse_from(["graft", "restore", "--kind", "db"]).is_err());
        assert!(
            Cli::try_parse_from(["graft", "restore", "--staged", "--all", "external.db"]).is_err()
        );
    }

    #[test]
    fn parses_checkout_restore_reset_json_flags() {
        let cli =
            Cli::try_parse_from(["graft", "checkout", "--json", "--force", "HEAD~1"]).unwrap();
        let Command::Checkout { json, force, rev, path } = cli.command else {
            panic!("expected checkout command");
        };
        assert!(json);
        assert!(force);
        assert_eq!(rev, "HEAD~1");
        assert_eq!(path, None);
        assert_eq!(
            repo_checkout_arg(force, &rev, path.as_deref()),
            "--force HEAD~1"
        );
        assert_eq!(checkout_pragma(json), "json_checkout");

        let cli =
            Cli::try_parse_from(["graft", "restore", "--json", "--staged", "external.db"]).unwrap();
        let Command::Restore {
            json, source, staged, all, kind, path, ..
        } = cli.command
        else {
            panic!("expected restore command");
        };
        assert!(json);
        assert_eq!(source, None);
        assert!(staged);
        assert!(!all);
        assert_eq!(kind, None);
        assert_eq!(path, Some(PathBuf::from("external.db")));
        assert_eq!(
            repo_restore_arg(
                source.as_deref(),
                None,
                false,
                staged,
                all,
                kind,
                path.as_deref(),
            )
            .unwrap(),
            "--staged -- \"external.db\""
        );
        assert_eq!(restore_pragma(json), "json_restore");

        let cli = Cli::try_parse_from(["graft", "reset", "--json", "--hard", "HEAD~1"]).unwrap();
        let Command::Reset { json, soft, mixed, hard, rev } = cli.command else {
            panic!("expected reset command");
        };
        assert!(json);
        assert!(!soft);
        assert!(!mixed);
        assert!(hard);
        assert_eq!(rev, "HEAD~1");
        assert_eq!(repo_reset_arg(&rev, soft, mixed, hard), "--hard HEAD~1");
        assert_eq!(reset_pragma(json), "json_reset");
    }

    #[test]
    fn parses_export_with_optional_source_and_path() {
        let cli = Cli::try_parse_from([
            "graft",
            "export",
            "--source",
            "HEAD~1",
            "--output",
            "snapshot.db",
            "app.db",
        ])
        .unwrap();
        let Command::Export(args) = cli.command else {
            panic!("expected export command");
        };
        assert!(!args.json);
        assert_eq!(args.source.as_deref(), Some("HEAD~1"));
        assert_eq!(args.output, PathBuf::from("snapshot.db"));
        assert_eq!(args.path, Some(PathBuf::from("app.db")));
        assert_eq!(
            repo_export_arg(args.source.as_deref(), &args.output, args.path.as_deref()),
            "--source HEAD~1 --output snapshot.db -- app.db"
        );
    }

    #[test]
    fn export_pragma_writes_physical_sqlite_file() {
        let temp_dir = tempfile::Builder::new()
            .prefix("graft-export-test")
            .tempdir_in("/tmp")
            .unwrap();
        let db = temp_dir.path().join("app.db");
        let output = temp_dir.path().join("snapshot.db");
        graft::repo::Repository::init(temp_dir.path()).unwrap();

        run_sql(
            Some(&db),
            &[format!(
                "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT); \
                 INSERT INTO users(name) VALUES ('Alice'), ('Bob'); \
                 PRAGMA graft_add; \
                 PRAGMA graft_commit = 'initial users'; \
                 PRAGMA graft_export = '--source HEAD --output {}';",
                output.display()
            )],
        )
        .unwrap();

        let conn = Connection::open(&output).unwrap();
        let names: String = conn
            .query_row(
                "SELECT group_concat(name, ',') FROM users ORDER BY id",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(names, "Alice,Bob");
    }

    #[test]
    fn parses_branch_upstream_flags() {
        let cli = Cli::try_parse_from([
            "graft",
            "branch",
            "--set-upstream-to",
            "origin/main",
            "feature/search",
        ])
        .unwrap();

        let Command::Branch {
            json,
            delete,
            force_delete,
            move_branch,
            force_move,
            set_upstream_to,
            unset_upstream,
            remote,
            all,
            name,
            start_point,
        } = cli.command
        else {
            panic!("expected branch command");
        };
        assert!(!json);
        assert!(!delete);
        assert!(!force_delete);
        assert!(!move_branch);
        assert!(!force_move);
        assert_eq!(set_upstream_to.as_deref(), Some("origin/main"));
        assert!(!unset_upstream);
        assert!(!remote);
        assert!(!all);
        assert_eq!(name.as_deref(), Some("feature/search"));
        assert_eq!(start_point, None);

        let cli =
            Cli::try_parse_from(["graft", "branch", "--unset-upstream", "feature/search"]).unwrap();

        let Command::Branch { unset_upstream, name, .. } = cli.command else {
            panic!("expected branch command");
        };
        assert!(unset_upstream);
        assert_eq!(name.as_deref(), Some("feature/search"));

        let cli = Cli::try_parse_from(["graft", "branch", "-u", "origin/main", "feature/search"])
            .unwrap();

        let Command::Branch { set_upstream_to, name, .. } = cli.command else {
            panic!("expected branch command");
        };
        assert_eq!(set_upstream_to.as_deref(), Some("origin/main"));
        assert_eq!(name.as_deref(), Some("feature/search"));
    }

    #[test]
    fn parses_branch_remote_and_all_flags() {
        let cli = Cli::try_parse_from(["graft", "branch", "-r"]).unwrap();
        let Command::Branch { remote, all, name, start_point, .. } = cli.command else {
            panic!("expected branch command");
        };
        assert!(remote);
        assert!(!all);
        assert_eq!(name, None);
        assert_eq!(start_point, None);

        let cli = Cli::try_parse_from(["graft", "branch", "--all"]).unwrap();
        let Command::Branch { remote, all, name, start_point, .. } = cli.command else {
            panic!("expected branch command");
        };
        assert!(!remote);
        assert!(all);
        assert_eq!(name, None);
        assert_eq!(start_point, None);
    }

    #[test]
    fn parses_branch_json_flags() {
        let cli = Cli::try_parse_from(["graft", "branch", "--json"]).unwrap();
        let Command::Branch { json, remote, all, name, start_point, .. } = cli.command else {
            panic!("expected branch command");
        };
        assert!(json);
        assert!(!remote);
        assert!(!all);
        assert_eq!(name, None);
        assert_eq!(start_point, None);
        assert_eq!(branch_list_pragma(json, remote, all), ("json_branch", None));

        let cli = Cli::try_parse_from(["graft", "branch", "--json", "--all"]).unwrap();
        let Command::Branch { json, remote, all, .. } = cli.command else {
            panic!("expected branch command");
        };
        assert!(json);
        assert!(!remote);
        assert!(all);
        assert_eq!(
            branch_list_pragma(json, remote, all),
            ("json_branch", Some("--all"))
        );

        let cli = Cli::try_parse_from(["graft", "branch", "--json", "-r"]).unwrap();
        let Command::Branch { json, remote, all, .. } = cli.command else {
            panic!("expected branch command");
        };
        assert!(json);
        assert!(remote);
        assert!(!all);
        assert_eq!(
            branch_list_pragma(json, remote, all),
            ("json_branch", Some("--remote"))
        );

        let cli =
            Cli::try_parse_from(["graft", "branch", "--json", "feature/search", "HEAD"]).unwrap();
        let Command::Branch { json, name, start_point, .. } = cli.command else {
            panic!("expected branch command");
        };
        assert!(json);
        assert_eq!(name.as_deref(), Some("feature/search"));
        assert_eq!(start_point.as_deref(), Some("HEAD"));
        assert_eq!(branch_create_pragma(json), "json_branch_create");

        let cli =
            Cli::try_parse_from(["graft", "branch", "--json", "-D", "feature/search"]).unwrap();
        let Command::Branch { json, force_delete, name, .. } = cli.command else {
            panic!("expected branch command");
        };
        assert!(json);
        assert!(force_delete);
        assert_eq!(name.as_deref(), Some("feature/search"));
        assert_eq!(branch_delete_pragma(json), "json_branch_delete");

        let cli = Cli::try_parse_from([
            "graft",
            "branch",
            "--json",
            "-M",
            "feature/search",
            "feature/query",
        ])
        .unwrap();
        let Command::Branch { json, force_move, name, start_point, .. } = cli.command else {
            panic!("expected branch command");
        };
        assert!(json);
        assert!(force_move);
        assert_eq!(name.as_deref(), Some("feature/search"));
        assert_eq!(start_point.as_deref(), Some("feature/query"));
        assert_eq!(branch_rename_pragma(json), "json_branch_rename");

        let cli = Cli::try_parse_from([
            "graft",
            "branch",
            "--json",
            "-u",
            "origin/main",
            "feature/search",
        ])
        .unwrap();
        let Command::Branch { json, set_upstream_to, name, .. } = cli.command else {
            panic!("expected branch command");
        };
        assert!(json);
        assert_eq!(set_upstream_to.as_deref(), Some("origin/main"));
        assert_eq!(name.as_deref(), Some("feature/search"));
        assert_eq!(branch_upstream_pragma(json), "json_branch_upstream");

        let cli = Cli::try_parse_from([
            "graft",
            "branch",
            "--json",
            "--unset-upstream",
            "feature/search",
        ])
        .unwrap();
        let Command::Branch { json, unset_upstream, name, .. } = cli.command else {
            panic!("expected branch command");
        };
        assert!(json);
        assert!(unset_upstream);
        assert_eq!(name.as_deref(), Some("feature/search"));
        assert_eq!(
            branch_unset_upstream_pragma(json),
            "json_branch_unset_upstream"
        );
    }

    #[test]
    fn parses_remote_remove_alias() {
        let cli = Cli::try_parse_from(["graft", "remote", "rm", "origin"]).unwrap();

        let Command::Remote {
            command: RemoteCommand::Remove { json, name },
        } = cli.command
        else {
            panic!("expected remote remove command");
        };
        assert!(!json);
        assert_eq!(name, "origin");
    }

    #[test]
    fn parses_remote_rename_aliases() {
        let cli = Cli::try_parse_from(["graft", "remote", "rename", "origin", "upstream"]).unwrap();
        let Command::Remote {
            command: RemoteCommand::Rename { json, old, new },
        } = cli.command
        else {
            panic!("expected remote rename command");
        };
        assert!(!json);
        assert_eq!(old, "origin");
        assert_eq!(new, "upstream");

        let cli = Cli::try_parse_from(["graft", "remote", "mv", "backup", "archive"]).unwrap();
        let Command::Remote {
            command: RemoteCommand::Rename { json, old, new },
        } = cli.command
        else {
            panic!("expected remote rename command");
        };
        assert!(!json);
        assert_eq!(old, "backup");
        assert_eq!(new, "archive");
    }

    #[test]
    fn parses_remote_url_commands() {
        let cli = Cli::try_parse_from(["graft", "remote", "get-url", "origin"]).unwrap();
        let Command::Remote {
            command: RemoteCommand::GetUrl { json, name },
        } = cli.command
        else {
            panic!("expected remote get-url command");
        };
        assert!(!json);
        assert_eq!(name, "origin");

        let cli = Cli::try_parse_from([
            "graft",
            "remote",
            "set-url",
            "origin",
            "fs:///srv/graft/app",
        ])
        .unwrap();
        let Command::Remote {
            command: RemoteCommand::SetUrl { json, name, uri },
        } = cli.command
        else {
            panic!("expected remote set-url command");
        };
        assert!(!json);
        assert_eq!(name, "origin");
        assert_eq!(uri, "fs:///srv/graft/app");
    }

    #[test]
    fn parses_remote_prune_command() {
        let cli = Cli::try_parse_from(["graft", "remote", "prune", "origin"]).unwrap();
        let Command::Remote {
            command: RemoteCommand::Prune { json, name },
        } = cli.command
        else {
            panic!("expected remote prune command");
        };
        assert!(!json);
        assert_eq!(name, "origin");
    }

    #[test]
    fn parses_remote_json_flags() {
        let cli = Cli::try_parse_from([
            "graft",
            "remote",
            "add",
            "--json",
            "origin",
            "fs:///srv/graft/app",
        ])
        .unwrap();
        let Command::Remote {
            command: RemoteCommand::Add { json, name, uri },
        } = cli.command
        else {
            panic!("expected remote add command");
        };
        assert!(json);
        assert_eq!(name, "origin");
        assert_eq!(uri, "fs:///srv/graft/app");
        assert_eq!(remote_add_pragma(json), "json_remote_add");

        let cli = Cli::try_parse_from(["graft", "remote", "list", "--json"]).unwrap();
        let Command::Remote { command: RemoteCommand::List { json } } = cli.command else {
            panic!("expected remote list command");
        };
        assert!(json);
        assert_eq!(remote_list_pragma(json), "json_remotes");

        let cli = Cli::try_parse_from(["graft", "remote", "remove", "--json", "origin"]).unwrap();
        let Command::Remote {
            command: RemoteCommand::Remove { json, name },
        } = cli.command
        else {
            panic!("expected remote remove command");
        };
        assert!(json);
        assert_eq!(name, "origin");
        assert_eq!(remote_remove_pragma(json), "json_remote_remove");

        let cli =
            Cli::try_parse_from(["graft", "remote", "rename", "--json", "origin", "upstream"])
                .unwrap();
        let Command::Remote {
            command: RemoteCommand::Rename { json, old, new },
        } = cli.command
        else {
            panic!("expected remote rename command");
        };
        assert!(json);
        assert_eq!(old, "origin");
        assert_eq!(new, "upstream");
        assert_eq!(remote_rename_pragma(json), "json_remote_rename");

        let cli = Cli::try_parse_from(["graft", "remote", "get-url", "--json", "origin"]).unwrap();
        let Command::Remote {
            command: RemoteCommand::GetUrl { json, name },
        } = cli.command
        else {
            panic!("expected remote get-url command");
        };
        assert!(json);
        assert_eq!(name, "origin");
        assert_eq!(remote_get_url_pragma(json), "json_remote_get_url");

        let cli = Cli::try_parse_from([
            "graft",
            "remote",
            "set-url",
            "--json",
            "origin",
            "fs:///srv/graft/app",
        ])
        .unwrap();
        let Command::Remote {
            command: RemoteCommand::SetUrl { json, name, uri },
        } = cli.command
        else {
            panic!("expected remote set-url command");
        };
        assert!(json);
        assert_eq!(name, "origin");
        assert_eq!(uri, "fs:///srv/graft/app");
        assert_eq!(remote_set_url_pragma(json), "json_remote_set_url");

        let cli = Cli::try_parse_from(["graft", "remote", "prune", "--json", "origin"]).unwrap();
        let Command::Remote {
            command: RemoteCommand::Prune { json, name },
        } = cli.command
        else {
            panic!("expected remote prune command");
        };
        assert!(json);
        assert_eq!(name, "origin");
        assert_eq!(remote_prune_pragma(json), "json_remote_prune");
    }

    #[test]
    fn parses_ls_remote_command() {
        let cli = Cli::try_parse_from(["graft", "ls-remote", "origin"]).unwrap();
        let Command::LsRemote { json, remote } = cli.command else {
            panic!("expected ls-remote command");
        };
        assert!(!json);
        assert_eq!(remote, "origin");
    }

    #[test]
    fn parses_ls_remote_json_flag() {
        let cli = Cli::try_parse_from(["graft", "ls-remote", "--json", "origin"]).unwrap();
        let Command::LsRemote { json, remote } = cli.command else {
            panic!("expected ls-remote command");
        };
        assert!(json);
        assert_eq!(remote, "origin");
        assert_eq!(ls_remote_pragma(json), "json_ls_remote");
    }

    #[test]
    fn parses_fetch_and_push_all() {
        let cli = Cli::try_parse_from(["graft", "fetch", "--all", "origin"]).unwrap();
        let Command::Fetch(args) = cli.command else {
            panic!("expected fetch command");
        };
        assert!(!args.json);
        assert!(args.all);
        assert_eq!(args.remote.as_deref(), Some("origin"));
        assert_eq!(args.branch, None);
        assert_eq!(
            remote_sync_arg(&args).unwrap().as_deref(),
            Some("--all origin")
        );

        let cli = Cli::try_parse_from(["graft", "push", "--all"]).unwrap();
        let Command::Push(args) = cli.command else {
            panic!("expected push command");
        };
        assert!(!args.json);
        assert!(args.all);
        assert!(!args.force);
        assert_eq!(args.remote, None);
        assert_eq!(args.branch, None);
        assert_eq!(remote_push_arg(&args).unwrap().as_deref(), Some("--all"));
    }

    #[test]
    fn parses_force_push() {
        let cli = Cli::try_parse_from(["graft", "push", "--force", "origin", "main"]).unwrap();
        let Command::Push(args) = cli.command else {
            panic!("expected push command");
        };
        assert!(!args.json);
        assert!(args.force);
        assert!(!args.all);
        assert_eq!(
            remote_push_arg(&args).unwrap().as_deref(),
            Some("--force origin main")
        );

        let cli = Cli::try_parse_from(["graft", "push", "--force", "--all", "origin"]).unwrap();
        let Command::Push(args) = cli.command else {
            panic!("expected push command");
        };
        assert!(!args.json);
        assert!(args.force);
        assert!(args.all);
        assert_eq!(
            remote_push_arg(&args).unwrap().as_deref(),
            Some("--force --all origin")
        );
    }

    #[test]
    fn parses_fetch_and_push_refspecs() {
        let cli = Cli::try_parse_from([
            "graft",
            "fetch",
            "origin",
            "refs/heads/main:refs/remotes/origin/review",
        ])
        .unwrap();
        let Command::Fetch(args) = cli.command else {
            panic!("expected fetch command");
        };
        assert!(!args.json);
        assert_eq!(
            remote_sync_arg(&args).unwrap().as_deref(),
            Some("origin refs/heads/main:refs/remotes/origin/review")
        );

        let cli = Cli::try_parse_from([
            "graft",
            "push",
            "--force",
            "origin",
            "feature/search:review/search",
        ])
        .unwrap();
        let Command::Push(args) = cli.command else {
            panic!("expected push command");
        };
        assert!(!args.json);
        assert_eq!(
            remote_push_arg(&args).unwrap().as_deref(),
            Some("--force origin feature/search:review/search")
        );

        let cli = Cli::try_parse_from(["graft", "push", "origin", ":old/branch"]).unwrap();
        let Command::Push(args) = cli.command else {
            panic!("expected push command");
        };
        assert!(!args.json);
        assert_eq!(
            remote_push_arg(&args).unwrap().as_deref(),
            Some("origin :old/branch")
        );
    }

    #[test]
    fn parses_sync_json_flags() {
        let cli = Cli::try_parse_from(["graft", "fetch", "--json", "--all", "origin"]).unwrap();
        let Command::Fetch(args) = cli.command else {
            panic!("expected fetch command");
        };
        assert!(args.json);
        assert!(args.all);
        assert_eq!(
            remote_sync_arg(&args).unwrap().as_deref(),
            Some("--all origin")
        );
        assert_eq!(fetch_pragma(args.json), "json_fetch");

        let cli = Cli::try_parse_from(["graft", "pull", "--json", "origin", "main"]).unwrap();
        let Command::Pull(args) = cli.command else {
            panic!("expected pull command");
        };
        assert!(args.json);
        assert_eq!(
            remote_branch_arg(&args).unwrap().as_deref(),
            Some("origin main")
        );
        assert_eq!(pull_pragma(args.json), "json_pull");

        let cli =
            Cli::try_parse_from(["graft", "push", "--json", "--force", "origin", "main"]).unwrap();
        let Command::Push(args) = cli.command else {
            panic!("expected push command");
        };
        assert!(args.json);
        assert!(args.force);
        assert_eq!(
            remote_push_arg(&args).unwrap().as_deref(),
            Some("--force origin main")
        );
        assert_eq!(push_pragma(args.json), "json_push");
    }

    #[test]
    fn parses_tag_create_and_delete() {
        let cli = Cli::try_parse_from(["graft", "tag", "v1.0", "HEAD~1"]).unwrap();

        let Command::Tag {
            json,
            list,
            delete,
            annotated,
            message,
            name,
            rev,
        } = cli.command
        else {
            panic!("expected tag command");
        };
        assert!(!json);
        assert!(!list);
        assert!(!delete);
        assert!(!annotated);
        assert_eq!(message, None);
        assert_eq!(name.as_deref(), Some("v1.0"));
        assert_eq!(rev.as_deref(), Some("HEAD~1"));

        let cli = Cli::try_parse_from(["graft", "tag", "-d", "v1.0"]).unwrap();
        let Command::Tag {
            json,
            list,
            delete,
            annotated,
            message,
            name,
            rev,
        } = cli.command
        else {
            panic!("expected tag command");
        };
        assert!(!json);
        assert!(!list);
        assert!(delete);
        assert!(!annotated);
        assert_eq!(message, None);
        assert_eq!(name.as_deref(), Some("v1.0"));
        assert_eq!(rev, None);

        let cli = Cli::try_parse_from(["graft", "tag", "-a", "-m", "release 1.0", "v1.0", "HEAD"])
            .unwrap();
        let Command::Tag {
            json,
            list,
            delete,
            annotated,
            message,
            name,
            rev,
        } = cli.command
        else {
            panic!("expected tag command");
        };
        assert!(!json);
        assert!(!list);
        assert!(!delete);
        assert!(annotated);
        assert_eq!(message.as_deref(), Some("release 1.0"));
        assert_eq!(name.as_deref(), Some("v1.0"));
        assert_eq!(rev.as_deref(), Some("HEAD"));

        let cli = Cli::try_parse_from(["graft", "tag", "-l"]).unwrap();
        let Command::Tag { json, list, name, rev, .. } = cli.command else {
            panic!("expected tag command");
        };
        assert!(!json);
        assert!(list);
        assert_eq!(name, None);
        assert_eq!(rev, None);
    }

    #[test]
    fn parses_tag_json_flags() {
        let cli = Cli::try_parse_from(["graft", "tag", "--json"]).unwrap();
        let Command::Tag { json, list, name, rev, .. } = cli.command else {
            panic!("expected tag command");
        };
        assert!(json);
        assert!(!list);
        assert_eq!(name, None);
        assert_eq!(rev, None);
        assert_eq!(tag_list_pragma(json), ("json_tags", Some("--with-status")));

        let cli = Cli::try_parse_from(["graft", "tag", "--json", "v1.0", "HEAD"]).unwrap();
        let Command::Tag { json, name, rev, .. } = cli.command else {
            panic!("expected tag command");
        };
        assert!(json);
        assert_eq!(name.as_deref(), Some("v1.0"));
        assert_eq!(rev.as_deref(), Some("HEAD"));
        assert_eq!(tag_create_pragma(json), "json_tag_create");

        let cli = Cli::try_parse_from(["graft", "tag", "--json", "-d", "v1.0"]).unwrap();
        let Command::Tag { json, delete, name, .. } = cli.command else {
            panic!("expected tag command");
        };
        assert!(json);
        assert!(delete);
        assert_eq!(name.as_deref(), Some("v1.0"));
        assert_eq!(tag_delete_pragma(json), "json_tag_delete");
    }
}
