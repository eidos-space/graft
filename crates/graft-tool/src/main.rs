use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use graft::{
    core::{LogId, SegmentId, VolumeId},
    remote::RemoteConfig,
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
    Log,

    /// Initialize a .graft repository next to a database path
    Init(DbArg),

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
        /// Branch to clone. Defaults to remote HEAD, then main.
        #[arg(short = 'b', long = "branch", conflicts_with = "branch")]
        branch_option: Option<String>,

        /// Remote URI: memory, fs://..., s3://..., or `s3_compatible://...`
        remote: String,

        /// Optional branch to clone. Defaults to remote HEAD, then main.
        branch: Option<String>,
    },

    /// Show repository status
    Status {
        /// Emit JSON status
        #[arg(long)]
        json: bool,
    },

    /// Stage a database snapshot
    Add(AddArgs),

    /// Stage removal of a database path
    Rm(RmArgs),

    /// Create a Graft commit for the database path
    Commit {
        /// Commit message
        #[arg(short, long)]
        message: String,
    },

    /// Compare repository revisions, staged changes, or the worktree
    Diff {
        /// Emit staged diff instead of worktree diff
        #[arg(long, alias = "cached")]
        staged: bool,

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
        /// Discard staged and unstaged changes before checking out a revision
        #[arg(short = 'f', long, conflicts_with = "path")]
        force: bool,

        /// Revision, for example HEAD~1
        rev: String,

        /// Optional repository-relative path to restore from the revision
        path: Option<PathBuf>,
    },

    /// Restore a worktree database path from the index or a revision
    Restore {
        /// Restore from this revision instead of the staged index
        #[arg(short = 's', long)]
        source: Option<String>,

        /// Restore the staged index entry from HEAD instead of touching the worktree
        #[arg(long, alias = "cached")]
        staged: bool,

        /// Repository-relative `SQLite` database path to restore
        path: PathBuf,
    },

    /// Export a Graft database snapshot as a physical SQLite file
    Export(ExportArgs),

    /// Reset the current branch to a repository revision
    Reset {
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
    Conflicts(DbArg),

    /// Resolve a database conflict using one side
    Resolve {
        /// Resolve using the current branch side
        #[arg(long, conflicts_with = "theirs", required_unless_present = "theirs")]
        ours: bool,

        /// Resolve using the merged-in branch side
        #[arg(long, conflicts_with = "ours", required_unless_present = "ours")]
        theirs: bool,

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

#[derive(Args)]
struct DbArg {
    /// Database path
    db: Option<PathBuf>,
}

#[derive(Args)]
struct AddArgs {
    /// Optional repository-relative `SQLite` database path to stage
    path: Option<PathBuf>,
}

#[derive(Args)]
struct RmArgs {
    /// Optional repository-relative `SQLite` database path to remove
    path: Option<PathBuf>,
}

#[derive(Args)]
struct ExportArgs {
    /// Restore from this revision instead of exporting the current worktree Volume
    #[arg(short = 's', long)]
    source: Option<String>,

    /// Output path for the physical SQLite database file
    #[arg(short, long)]
    output: PathBuf,

    /// Optional repository-relative `SQLite` database path to export. Defaults to app.db.
    path: Option<PathBuf>,
}

#[derive(Subcommand)]
enum RemoteCommand {
    /// Add a named remote
    Add {
        /// Remote name, for example origin
        name: String,

        /// Remote URI: memory, fs://..., s3://..., or `s3_compatible://...`
        uri: String,
    },

    /// List configured remotes
    #[command(alias = "ls")]
    List,

    /// Remove a named remote
    #[command(alias = "rm")]
    Remove {
        /// Remote name, for example origin
        name: String,
    },

    /// Rename a configured remote
    #[command(alias = "mv")]
    Rename {
        /// Existing remote name
        old: String,

        /// New remote name
        new: String,
    },

    /// Print the configured remote URL
    GetUrl {
        /// Remote name, for example origin
        name: String,
    },

    /// Change the configured remote URL
    SetUrl {
        /// Remote name, for example origin
        name: String,

        /// Remote URI: memory, fs://..., s3://..., or `s3_compatible://...`
        uri: String,
    },

    /// Delete stale remote-tracking branches for a remote
    Prune {
        /// Remote name, for example origin
        name: String,
    },
}

#[derive(Args)]
struct RemoteSyncArgs {
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
    /// Remote name. Defaults to origin.
    remote: Option<String>,

    /// Branch name or refspec. Defaults to the current branch.
    branch: Option<String>,
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    /// Database path used to enter the Graft repository. Defaults to app.db in the current project.
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
    match command {
        Command::Id { kind } => match kind {
            IdKind::Vid => println!("{}", VolumeId::random()),
            IdKind::Log => println!("{}", LogId::random()),
            IdKind::Sid => println!("{}", SegmentId::random()),
        },
        Command::Log => print_output(run_repo_pragma(db_override, None, "log", None)?),
        Command::Init(args) => {
            print_output(run_repo_pragma(
                db_override,
                args.db.as_deref(),
                "init",
                None,
            )?);
        }
        Command::Sql { sql } => print_output(run_sql(db_override, &sql)?),
        Command::Clone { branch_option, remote, branch } => {
            let branch = branch_option.as_deref().or(branch.as_deref());
            let arg = repo_clone_arg(&remote, branch);
            print_output(run_repo_pragma(db_override, None, "clone", Some(&arg))?);
        }
        Command::Status { json } => {
            let pragma = if json { "json_status" } else { "status" };
            print_output(run_repo_pragma(db_override, None, pragma, None)?);
        }
        Command::Add(args) => {
            let path = args.path.as_ref().map(|path| path.display().to_string());
            print_output(run_repo_pragma(db_override, None, "add", path.as_deref())?);
        }
        Command::Rm(args) => {
            let path = args.path.as_ref().map(|path| path.display().to_string());
            print_output(run_repo_pragma(db_override, None, "rm", path.as_deref())?);
        }
        Command::Commit { message } => {
            print_output(run_repo_pragma(
                db_override,
                None,
                "commit",
                Some(&message),
            )?);
        }
        Command::Diff { staged, from, to, path, json } => {
            let suffix = if json { "json_diff" } else { "diff" };
            let arg = repo_diff_arg(staged, from.as_deref(), to.as_deref(), path.as_deref())?;
            print_output(run_repo_pragma(db_override, None, suffix, arg.as_deref())?);
        }
        Command::Show { rev, json } => {
            let suffix = if json { "json_show" } else { "show" };
            print_output(run_repo_pragma(db_override, None, suffix, Some(&rev))?);
        }
        Command::Checkout { force, rev, path } => {
            let arg = repo_checkout_arg(force, &rev, path.as_deref());
            print_output(run_repo_pragma(db_override, None, "checkout", Some(&arg))?);
        }
        Command::Restore { source, staged, path } => {
            let arg = repo_restore_arg(source.as_deref(), staged, &path);
            print_output(run_repo_pragma(db_override, None, "restore", Some(&arg))?);
        }
        Command::Export(args) => {
            let arg = repo_export_arg(args.source.as_deref(), &args.output, args.path.as_deref());
            let command_db = if db_override.is_none() {
                args.path.as_deref()
            } else {
                None
            };
            print_output(run_repo_pragma(
                db_override,
                command_db,
                "export",
                Some(&arg),
            )?);
        }
        Command::Reset { soft, mixed, hard, rev } => {
            let arg = repo_reset_arg(&rev, soft, mixed, hard);
            print_output(run_repo_pragma(db_override, None, "reset", Some(&arg))?);
        }
        Command::Branch {
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
                    "branch_delete",
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
                    "branch_rename",
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
                    "branch_upstream",
                    Some(&arg),
                )?);
            } else if unset_upstream {
                if start_point.is_some() {
                    bail!("branch --unset-upstream accepts at most a branch name");
                }
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    "branch_unset_upstream",
                    name.as_deref(),
                )?);
            } else if remote || all {
                if name.is_some() || start_point.is_some() {
                    bail!("branch -r/-a accepts no branch name or start point");
                }
                let arg = if all { "--all" } else { "--remote" };
                print_output(run_repo_pragma(db_override, None, "branch", Some(arg))?);
            } else if let Some(name) = name {
                let arg = match start_point {
                    Some(start_point) => format!("{name} {start_point}"),
                    None => name,
                };
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    "branch_create",
                    Some(&arg),
                )?);
            } else {
                if start_point.is_some() {
                    bail!("branch list accepts no start point");
                }
                print_output(run_repo_pragma(db_override, None, "branch", None)?);
            }
        }
        Command::Tag {
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
                print_output(run_repo_pragma(db_override, None, "tags", None)?);
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
                    "tag_delete",
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
                    "tag_create",
                    Some(&arg),
                )?);
            } else {
                if annotated || message.is_some() {
                    bail!("tag list accepts no annotation flags");
                }
                print_output(run_repo_pragma(db_override, None, "tags", None)?);
            }
        }
        Command::Switch { create, force, branch, start_point } => {
            if !create && start_point.is_some() {
                bail!("switch accepts a start point only with --create");
            }
            let pragma = if create {
                "switch_create"
            } else {
                "switch_branch"
            };
            let arg = match (force, start_point) {
                (true, Some(start_point)) => format!("--force {branch} {start_point}"),
                (true, None) => format!("--force {branch}"),
                (false, Some(start_point)) => format!("{branch} {start_point}"),
                (false, None) => branch,
            };
            print_output(run_repo_pragma(db_override, None, pragma, Some(&arg))?);
        }
        Command::Merge { abort, continue_merge, message, rev } => {
            if abort {
                print_output(run_repo_pragma(db_override, None, "merge_abort", None)?);
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
                    "merge_continue",
                    Some(&message),
                )?);
            } else {
                let Some(rev) = rev else {
                    bail!("merge requires a revision unless --abort is used");
                };
                print_output(run_repo_pragma(db_override, None, "merge", Some(&rev))?);
            }
        }
        Command::Conflicts(args) => {
            print_output(run_repo_pragma(
                db_override,
                args.db.as_deref(),
                "conflicts",
                None,
            )?);
        }
        Command::Resolve { ours, theirs, path } => {
            let arg = repo_resolve_arg(ours, theirs, path.as_deref())?;
            print_output(run_repo_pragma(db_override, None, "resolve", Some(&arg))?);
        }
        Command::Remote { command } => match command {
            RemoteCommand::Add { name, uri } => {
                let arg = format!("{name} {uri}");
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    "remote_add",
                    Some(&arg),
                )?);
            }
            RemoteCommand::List => {
                print_output(run_repo_pragma(db_override, None, "remotes", None)?);
            }
            RemoteCommand::Remove { name } => {
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    "remote_remove",
                    Some(&name),
                )?);
            }
            RemoteCommand::Rename { old, new } => {
                let arg = format!("{old} {new}");
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    "remote_rename",
                    Some(&arg),
                )?);
            }
            RemoteCommand::GetUrl { name } => {
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    "remote_get_url",
                    Some(&name),
                )?);
            }
            RemoteCommand::SetUrl { name, uri } => {
                let arg = format!("{name} {uri}");
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    "remote_set_url",
                    Some(&arg),
                )?);
            }
            RemoteCommand::Prune { name } => {
                print_output(run_repo_pragma(
                    db_override,
                    None,
                    "remote_prune",
                    Some(&name),
                )?);
            }
        },
        Command::LsRemote { remote } => {
            print_output(run_repo_pragma(
                db_override,
                None,
                "ls_remote",
                Some(&remote),
            )?);
        }
        Command::Fetch(args) => {
            print_output(run_repo_pragma(
                db_override,
                None,
                "fetch",
                remote_sync_arg(&args)?.as_deref(),
            )?);
        }
        Command::Pull(args) => {
            print_output(run_repo_pragma(
                db_override,
                None,
                "pull",
                remote_branch_arg(&args)?.as_deref(),
            )?);
        }
        Command::Push(args) => {
            print_output(run_repo_pragma(
                db_override,
                None,
                "push",
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
    let db = resolve_cli_db(command_db.or(db_override))?;
    run_pragma(&db, suffix, arg)
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
    if graft::repo::Repository::discover_for_file(&db).is_err() && !sql_initializes_repo(sql) {
        bail!(
            "not a Graft repository: run `graft init {}` first, or include `PRAGMA graft_init;` in the SQL batch",
            db.display()
        );
    }
    execute_sql(&db, sql)
}

fn sql_initializes_repo(sql: &str) -> bool {
    sql.split(';').any(|statement| {
        statement
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("pragma graft_init")
    })
}

fn resolve_cli_db(path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = path {
        return absolute_db_path(path);
    }

    let cwd = std::env::current_dir().context("failed to read current directory")?;
    if let Ok(repo) = graft::repo::Repository::discover(&cwd) {
        return Ok(repo.worktree().join("app.db"));
    }

    absolute_db_path(Path::new("app.db"))
}

fn repo_diff_arg(
    staged: bool,
    from: Option<&str>,
    to: Option<&str>,
    path: Option<&Path>,
) -> Result<Option<String>> {
    if staged {
        if to.is_some() || path.is_some() {
            bail!("--staged accepts at most one optional path");
        }
        return Ok(Some(match from {
            Some(path) => format!("--staged -- {path}"),
            None => "--staged".to_string(),
        }));
    }

    Ok(match (from, to, path) {
        (None, None, None) => None,
        (None, None, Some(path)) => Some(format!("-- {}", path.display())),
        (Some(from), None, None) => Some(from.to_string()),
        (Some(from), None, Some(path)) => Some(format!("{from} -- {}", path.display())),
        (Some(from), Some(to), None) => Some(format!("{from} {to}")),
        (Some(from), Some(to), Some(path)) => Some(format!("{from} {to} -- {}", path.display())),
        (None, Some(_), _) => unreachable!("clap cannot provide `to` without `from`"),
    })
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

fn repo_clone_arg(remote: &str, branch: Option<&str>) -> String {
    match branch {
        Some(branch) => format!("{remote} {branch}"),
        None => remote.to_string(),
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

fn repo_restore_arg(source: Option<&str>, staged: bool, path: &Path) -> String {
    match (source, staged) {
        (Some(source), true) => format!("--staged --source {source} -- {}", path.display()),
        (None, true) => format!("--staged -- {}", path.display()),
        (Some(source), false) => format!("--source {source} -- {}", path.display()),
        (None, false) => format!("-- {}", path.display()),
    }
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

fn repo_resolve_arg(ours: bool, theirs: bool, path: Option<&Path>) -> Result<String> {
    let side = match (ours, theirs) {
        (true, false) => "--ours",
        (false, true) => "--theirs",
        _ => bail!("resolve requires exactly one of --ours or --theirs"),
    };
    Ok(match path {
        Some(path) => format!("{side} {}", path.display()),
        None => side.to_string(),
    })
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

        let Command::Log = cli.command else {
            panic!("expected log command");
        };
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

        let Command::Merge { abort, continue_merge, message, rev } = cli.command else {
            panic!("expected merge command");
        };
        assert!(!abort);
        assert!(continue_merge);
        assert_eq!(message.as_deref(), Some("merge feature"));
        assert_eq!(rev, None);
    }

    #[test]
    fn parses_resolve_ours_with_optional_path() {
        let cli = Cli::try_parse_from(["graft", "resolve", "--ours", "app.db"]).unwrap();

        let Command::Resolve { ours, theirs, path } = cli.command else {
            panic!("expected resolve command");
        };
        assert!(ours);
        assert!(!theirs);
        assert_eq!(path, Some(PathBuf::from("app.db")));
        assert_eq!(
            repo_resolve_arg(ours, theirs, path.as_deref()).unwrap(),
            "--ours app.db"
        );
    }

    #[test]
    fn parses_add_with_optional_path() {
        let cli = Cli::try_parse_from(["graft", "add", "external.db"]).unwrap();

        let Command::Add(args) = cli.command else {
            panic!("expected add command");
        };
        assert_eq!(args.path, Some(PathBuf::from("external.db")));
    }

    #[test]
    fn parses_rm_with_optional_path() {
        let cli = Cli::try_parse_from(["graft", "rm", "external.db"]).unwrap();

        let Command::Rm(args) = cli.command else {
            panic!("expected rm command");
        };
        assert_eq!(args.path, Some(PathBuf::from("external.db")));
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
    fn init_command_runs_through_graft_vfs() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db = temp_dir.path().join("app.db");

        run_command(Command::Init(DbArg { db: Some(db.clone()) }), None).unwrap();

        let repo = graft::repo::Repository::discover_for_file(&db).unwrap();
        assert!(repo.graft_dir().join("config.toml").exists());
        assert!(
            repo.store_dir().read_dir().unwrap().next().is_some(),
            "CLI init should initialize the repo-local storage via the SQLite pragma path"
        );
    }

    #[test]
    fn sql_command_runs_through_graft_vfs() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db = temp_dir.path().join("app.db");

        let output = run_sql(
            Some(&db),
            &[String::from(
                "PRAGMA graft_init; \
                 CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT); \
                 INSERT INTO users(name) VALUES ('Alice'), ('Bob'); \
                 SELECT name FROM users ORDER BY id; \
                 PRAGMA graft_status;",
            )],
        )
        .unwrap()
        .unwrap();
        assert!(
            output.contains("Initialized empty Graft repository"),
            "{output}"
        );
        assert!(output.contains("name\nAlice\nBob\n"), "{output}");
        assert!(output.contains("untracked: app.db"), "{output}");
    }

    #[test]
    fn sql_command_requires_graft_repo_unless_batch_initializes_one() {
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
    }

    #[test]
    fn init_without_db_defaults_to_app_db_in_current_directory() {
        let _guard = CWD_LOCK.lock().unwrap();
        let original_dir = std::env::current_dir().unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(temp_dir.path()).unwrap();

        let result = run_command(Command::Init(DbArg { db: None }), None);
        std::env::set_current_dir(original_dir).unwrap();
        result.unwrap();

        let db = temp_dir.path().join("app.db");
        let repo = graft::repo::Repository::discover_for_file(&db).unwrap();
        assert!(repo.graft_dir().join("config.toml").exists());
    }

    #[test]
    fn parses_clone_with_optional_branch() {
        let cli = Cli::try_parse_from(["graft", "clone", "fs:///srv/graft/app"]).unwrap();

        let Command::Clone { branch_option, remote, branch } = cli.command else {
            panic!("expected clone command");
        };
        assert_eq!(branch_option, None);
        assert_eq!(remote, "fs:///srv/graft/app");
        assert_eq!(branch, None);
        assert_eq!(
            repo_clone_arg(&remote, branch.as_deref()),
            "fs:///srv/graft/app"
        );

        let cli = Cli::try_parse_from(["graft", "clone", "fs:///srv/graft/app", "feature/search"])
            .unwrap();

        let Command::Clone { branch_option, remote, branch } = cli.command else {
            panic!("expected clone command");
        };
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

        let Command::Clone { branch_option, remote, branch } = cli.command else {
            panic!("expected clone command");
        };
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
    fn resolve_requires_a_side() {
        assert!(Cli::try_parse_from(["graft", "resolve"]).is_err());
    }

    #[test]
    fn parses_branch_force_delete() {
        let cli = Cli::try_parse_from(["graft", "branch", "-D", "feature/search"]).unwrap();

        let Command::Branch {
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
        let Command::Switch { create, force, branch, start_point } = cli.command else {
            panic!("expected switch command");
        };
        assert!(create);
        assert!(!force);
        assert_eq!(branch, "release/1.0");
        assert_eq!(start_point.as_deref(), Some("main"));
    }

    #[test]
    fn parses_force_checkout_and_switch() {
        let cli = Cli::try_parse_from(["graft", "checkout", "--force", "HEAD~1"]).unwrap();
        let Command::Checkout { force, rev, path } = cli.command else {
            panic!("expected checkout command");
        };
        assert!(force);
        assert_eq!(rev, "HEAD~1");
        assert_eq!(path, None);
        assert_eq!(
            repo_checkout_arg(force, &rev, path.as_deref()),
            "--force HEAD~1"
        );

        let cli = Cli::try_parse_from(["graft", "switch", "--force", "main"]).unwrap();
        let Command::Switch { create, force, branch, start_point } = cli.command else {
            panic!("expected switch command");
        };
        assert!(!create);
        assert!(force);
        assert_eq!(branch, "main");
        assert_eq!(start_point, None);
    }

    #[test]
    fn parses_restore_with_optional_source() {
        let cli =
            Cli::try_parse_from(["graft", "restore", "--source", "HEAD~1", "external.db"]).unwrap();
        let Command::Restore { source, staged, path } = cli.command else {
            panic!("expected restore command");
        };
        assert_eq!(source.as_deref(), Some("HEAD~1"));
        assert!(!staged);
        assert_eq!(path, PathBuf::from("external.db"));
        assert_eq!(
            repo_restore_arg(source.as_deref(), staged, &path),
            "--source HEAD~1 -- external.db"
        );

        let cli = Cli::try_parse_from(["graft", "restore", "--staged", "external.db"]).unwrap();
        let Command::Restore { source, staged, path } = cli.command else {
            panic!("expected restore command");
        };
        assert_eq!(source, None);
        assert!(staged);
        assert_eq!(path, PathBuf::from("external.db"));
        assert_eq!(
            repo_restore_arg(source.as_deref(), staged, &path),
            "--staged -- external.db"
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
        let Command::Restore { source, staged, path } = cli.command else {
            panic!("expected restore command");
        };
        assert_eq!(source.as_deref(), Some("HEAD~1"));
        assert!(staged);
        assert_eq!(path, PathBuf::from("external.db"));
        assert_eq!(
            repo_restore_arg(source.as_deref(), staged, &path),
            "--staged --source HEAD~1 -- external.db"
        );
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

        run_sql(
            Some(&db),
            &[format!(
                "PRAGMA graft_init; \
                 CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT); \
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
    fn parses_remote_remove_alias() {
        let cli = Cli::try_parse_from(["graft", "remote", "rm", "origin"]).unwrap();

        let Command::Remote { command: RemoteCommand::Remove { name } } = cli.command else {
            panic!("expected remote remove command");
        };
        assert_eq!(name, "origin");
    }

    #[test]
    fn parses_remote_rename_aliases() {
        let cli = Cli::try_parse_from(["graft", "remote", "rename", "origin", "upstream"]).unwrap();
        let Command::Remote {
            command: RemoteCommand::Rename { old, new },
        } = cli.command
        else {
            panic!("expected remote rename command");
        };
        assert_eq!(old, "origin");
        assert_eq!(new, "upstream");

        let cli = Cli::try_parse_from(["graft", "remote", "mv", "backup", "archive"]).unwrap();
        let Command::Remote {
            command: RemoteCommand::Rename { old, new },
        } = cli.command
        else {
            panic!("expected remote rename command");
        };
        assert_eq!(old, "backup");
        assert_eq!(new, "archive");
    }

    #[test]
    fn parses_remote_url_commands() {
        let cli = Cli::try_parse_from(["graft", "remote", "get-url", "origin"]).unwrap();
        let Command::Remote { command: RemoteCommand::GetUrl { name } } = cli.command else {
            panic!("expected remote get-url command");
        };
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
            command: RemoteCommand::SetUrl { name, uri },
        } = cli.command
        else {
            panic!("expected remote set-url command");
        };
        assert_eq!(name, "origin");
        assert_eq!(uri, "fs:///srv/graft/app");
    }

    #[test]
    fn parses_remote_prune_command() {
        let cli = Cli::try_parse_from(["graft", "remote", "prune", "origin"]).unwrap();
        let Command::Remote { command: RemoteCommand::Prune { name } } = cli.command else {
            panic!("expected remote prune command");
        };
        assert_eq!(name, "origin");
    }

    #[test]
    fn parses_ls_remote_command() {
        let cli = Cli::try_parse_from(["graft", "ls-remote", "origin"]).unwrap();
        let Command::LsRemote { remote } = cli.command else {
            panic!("expected ls-remote command");
        };
        assert_eq!(remote, "origin");
    }

    #[test]
    fn parses_fetch_and_push_all() {
        let cli = Cli::try_parse_from(["graft", "fetch", "--all", "origin"]).unwrap();
        let Command::Fetch(args) = cli.command else {
            panic!("expected fetch command");
        };
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
        assert_eq!(
            remote_push_arg(&args).unwrap().as_deref(),
            Some("--force origin feature/search:review/search")
        );

        let cli = Cli::try_parse_from(["graft", "push", "origin", ":old/branch"]).unwrap();
        let Command::Push(args) = cli.command else {
            panic!("expected push command");
        };
        assert_eq!(
            remote_push_arg(&args).unwrap().as_deref(),
            Some("origin :old/branch")
        );
    }

    #[test]
    fn parses_tag_create_and_delete() {
        let cli = Cli::try_parse_from(["graft", "tag", "v1.0", "HEAD~1"]).unwrap();

        let Command::Tag {
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
        assert!(!list);
        assert!(!delete);
        assert!(!annotated);
        assert_eq!(message, None);
        assert_eq!(name.as_deref(), Some("v1.0"));
        assert_eq!(rev.as_deref(), Some("HEAD~1"));

        let cli = Cli::try_parse_from(["graft", "tag", "-d", "v1.0"]).unwrap();
        let Command::Tag {
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
        assert!(!list);
        assert!(delete);
        assert!(!annotated);
        assert_eq!(message, None);
        assert_eq!(name.as_deref(), Some("v1.0"));
        assert_eq!(rev, None);

        let cli = Cli::try_parse_from(["graft", "tag", "-a", "-m", "release 1.0", "v1.0", "HEAD"])
            .unwrap();
        let Command::Tag {
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
        assert!(!list);
        assert!(!delete);
        assert!(annotated);
        assert_eq!(message.as_deref(), Some("release 1.0"));
        assert_eq!(name.as_deref(), Some("v1.0"));
        assert_eq!(rev.as_deref(), Some("HEAD"));

        let cli = Cli::try_parse_from(["graft", "tag", "-l"]).unwrap();
        let Command::Tag { list, name, rev, .. } = cli.command else {
            panic!("expected tag command");
        };
        assert!(list);
        assert_eq!(name, None);
        assert_eq!(rev, None);
    }
}
