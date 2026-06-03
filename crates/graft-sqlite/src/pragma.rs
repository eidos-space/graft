use std::{
    fmt::{Display, Write},
    fs::File,
    io::Write as IoWrite,
    path::PathBuf,
    str::FromStr,
};

use graft::core::{
    LogId, PageIdx, VolumeId, commit::Commit, logref::LogRef, lsn::{LSN, LSNRangeExt},
    page::PAGESIZE,
};
use graft::{rt::runtime::Runtime, volume::AheadStatus, volume_reader::VolumeRead};
use indoc::{formatdoc, indoc, writedoc};
use sqlite_plugin::{
    vars::SQLITE_ERROR,
    vfs::{Pragma, PragmaErr},
};
use tryiter::TryIteratorExt;
use zerocopy::FromBytes;

use crate::{dbg::SqliteHeader, file::vol_file::VolFile, vfs::ErrCtx};

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

/// Diff granularity mode
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DiffMode {
    /// Default: page-level + table-level
    Default,
    /// Row-level: detailed comparison of each row
    Rows,
}

pub enum GraftPragma {
    /// `pragma graft_volumes;`
    Volumes,

    /// `pragma graft_tags;`
    Tags,

    /// `pragma graft_switch = "local_vid[:local[:remote]]";`
    Switch {
        vid: VolumeId,
        local: Option<LogId>,
        remote: Option<LogId>,
    },

    /// `pragma graft_clone [= "remote"];`
    Clone { remote: Option<LogId> },

    /// `pragma graft_fork;`
    Fork,

    /// `pragma graft_checkout = "remote:LSN";`
    Checkout { logref: LogRef },

    /// `pragma graft_info;`
    Info,

    /// `pragma graft_status;`
    Status,

    /// `pragma graft_snapshot;`
    Snapshot,

    /// `pragma graft_fetch;`
    Fetch,

    /// `pragma graft_pull;`
    Pull,

    /// `pragma graft_push;`
    Push,

    /// `pragma graft_audit;`
    Audit,

    /// `pragma graft_hydrate;`
    Hydrate,

    /// `pragma graft_version;`
    Version,

    /// `pragma graft_import = "PATH";`
    Import(PathBuf),

    /// `pragma graft_export = "PATH";`
    Export(PathBuf),

    /// `pragma graft_dump_header;`
    DumpSqliteHeader,

    /// `pragma graft_dump_commit = "logid:LSN";`
    DumpCommit { logref: LogRef },

    /// `pragma graft_log;`
    /// Display commit history for current Volume
    Log,

    /// `pragma graft_checkout_lsn = "LSN";`
    /// Checkout to specified local LSN (creates new Volume)
    CheckoutLsn { lsn: LSN },

    /// `pragma graft_reset_to = "LSN";`
    /// Reset current tag to specified LSN
    ResetTo { lsn: LSN },

    /// `pragma graft_diff = "from_lsn,to_lsn[,mode]";`
    /// Compare differences between two commits
    /// mode: omitted=default (page + table level), "rows"=row-level detailed comparison
    Diff { from: LSN, to: LSN, mode: DiffMode },

    /// `pragma graft_show = "lsn";`
    /// Display detailed info for specified LSN (includes summary of all table data)
    Show { lsn: LSN },
}

impl TryFrom<&Pragma<'_>> for GraftPragma {
    type Error = PragmaErr;

    fn try_from(p: &Pragma<'_>) -> Result<Self, Self::Error> {
        if let Some((prefix, suffix)) = p.name.split_once("_")
            && prefix == "graft"
        {
            return match suffix {
                "volumes" => Ok(GraftPragma::Volumes),
                "tags" => Ok(GraftPragma::Tags),
                "clone" => {
                    let remote = p.arg.map(parse_or_fail).transpose()?;
                    Ok(GraftPragma::Clone { remote })
                }
                "fork" => Ok(GraftPragma::Fork),
                "checkout" => {
                    Ok(GraftPragma::Checkout { logref: parse_or_fail(p.require_arg()?)? })
                }
                "new" => Ok(GraftPragma::Switch {
                    vid: VolumeId::random(),
                    local: None,
                    remote: None,
                }),
                "switch" => {
                    let parts: Vec<&str> = p.require_arg()?.split(':').collect();
                    if parts.is_empty() || parts.len() > 3 {
                        return Err(pragma_fail(
                            "argument must be in the form: `local_vid[:local[:remote]]`",
                        ));
                    }
                    Ok(GraftPragma::Switch {
                        vid: parse_or_fail(parts[0])?,
                        local: parse_optional(parts.get(1))?,
                        remote: parse_optional(parts.get(2))?,
                    })
                }
                "info" => Ok(GraftPragma::Info),
                "status" => Ok(GraftPragma::Status),
                "snapshot" => Ok(GraftPragma::Snapshot),
                "fetch" => Ok(GraftPragma::Fetch),
                "pull" => Ok(GraftPragma::Pull),
                "push" => Ok(GraftPragma::Push),
                "audit" => Ok(GraftPragma::Audit),
                "hydrate" => Ok(GraftPragma::Hydrate),
                "version" => Ok(GraftPragma::Version),
                "import" => Ok(GraftPragma::Import(PathBuf::from(p.require_arg()?))),
                "export" => Ok(GraftPragma::Export(PathBuf::from(p.require_arg()?))),
                "dump_header" => Ok(GraftPragma::DumpSqliteHeader),
                "dump_commit" => {
                    Ok(GraftPragma::DumpCommit { logref: parse_or_fail(p.require_arg()?)? })
                }
                "log" => Ok(GraftPragma::Log),
                "checkout_lsn" => {
                    Ok(GraftPragma::CheckoutLsn { lsn: parse_or_fail(p.require_arg()?)? })
                }
                "reset_to" => {
                    Ok(GraftPragma::ResetTo { lsn: parse_or_fail(p.require_arg()?)? })
                }
                "diff" => {
                    let parts: Vec<&str> = p.require_arg()?.split(',').collect();
                    if parts.len() < 2 || parts.len() > 3 {
                        return Err(pragma_fail("argument must be in the form: `from_lsn,to_lsn[,mode]`"));
                    }
                    let mode = if parts.len() == 3 {
                        match parts[2] {
                            "rows" => DiffMode::Rows,
                            _ => return Err(pragma_fail("mode must be 'rows' or omitted")),
                        }
                    } else {
                        DiffMode::Default
                    };
                    Ok(GraftPragma::Diff {
                        from: parse_or_fail(parts[0])?,
                        to: parse_or_fail(parts[1])?,
                        mode,
                    })
                }
                "show" => {
                    Ok(GraftPragma::Show { lsn: parse_or_fail(p.require_arg()?)? })
                }
                _ => Err(pragma_fail(format!("invalid graft pragma `{}`", p.name))),
            };
        }
        Err(PragmaErr::NotFound)
    }
}

macro_rules! pragma_err {
    ($msg:expr) => {
        Err(ErrCtx::PragmaErr($msg.into()))
    };
}

impl GraftPragma {
    pub fn eval(self, runtime: &Runtime, file: &mut VolFile) -> Result<Option<String>, ErrCtx> {
        match self {
            GraftPragma::Volumes => Ok(Some(format_volumes(runtime, file)?)),
            GraftPragma::Tags => Ok(Some(format_tags(runtime, file)?)),

            GraftPragma::Clone { remote } => {
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

            GraftPragma::Fork => {
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

            GraftPragma::Checkout { logref } => {
                if !file.is_idle() {
                    return pragma_err!("cannot checkout while there is an open transaction");
                }

                let Some(volume) = runtime.volume_from_logref(logref.clone())? else {
                    return pragma_err!("logref not found");
                };
                file.switch_volume(&volume.vid)?;

                Ok(Some(format!(
                    "Checked out Volume {} at Log {} LSN {}",
                    file.vid, logref.log, logref.lsn,
                )))
            }

            GraftPragma::Switch { vid, local, remote } => {
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

            GraftPragma::Info => Ok(Some(format_volume_info(runtime, file)?)),
            GraftPragma::Status => Ok(Some(format_volume_status(runtime, file)?)),

            GraftPragma::Snapshot => {
                let snapshot = file.snapshot_or_latest()?;
                Ok(Some(format!("{snapshot:?}")))
            }

            GraftPragma::Fetch => Ok(Some(fetch_or_pull(runtime, file, false)?)),
            GraftPragma::Pull => Ok(Some(fetch_or_pull(runtime, file, true)?)),

            GraftPragma::Push => Ok(Some(push(runtime, file)?)),

            GraftPragma::Audit => Ok(Some(format_volume_audit(runtime, file)?)),

            GraftPragma::Hydrate => {
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

            GraftPragma::Import(_) => {
                pragma_err!(
                    "deprecated: use `vacuum into` instead: https://graft.rs/r/graft_import"
                )
            }

            GraftPragma::Export(path) => volume_export(runtime, file, path).map(Some),

            GraftPragma::DumpSqliteHeader => {
                let reader = runtime.volume_reader(file.vid.clone())?;
                let page = reader.read_page(PageIdx::FIRST)?;
                let header = SqliteHeader::read_from_bytes(&page[..100])
                    .expect("failed to parse SQLite header");
                Ok(Some(format!("{header:#?}")))
            }

            GraftPragma::DumpCommit { logref } => {
                if let Some(commit) = runtime.get_commit(&logref.log, logref.lsn)? {
                    let Commit {
                        log,
                        lsn,
                        page_count,
                        commit_hash,
                        segment_idx,
                        checkpoints,
                    } = commit;
                    Ok(Some(formatdoc!(
                        "
                            Commit @ {log}:{lsn}
                            page_count: {page_count}
                            commit_hash: {commit_hash:?}
                            segment_idx: {segment_idx:#?}
                            checkpoints: {checkpoints:?}
                        "
                    )))
                } else {
                    pragma_err!("commit not found")
                }
            }

            GraftPragma::Log => Ok(Some(format_volume_log(runtime, file)?)),

            GraftPragma::CheckoutLsn { lsn } => {
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

            GraftPragma::ResetTo { lsn } => {
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

            GraftPragma::Diff { from, to, mode } => {
                if !file.is_idle() {
                    return pragma_err!("cannot diff while there is an open transaction");
                }
                match mode {
                    DiffMode::Default => {
                        // Built-in table-level diff using our B-tree parser
                        let report = crate::sql_diff::generate_diff_report(runtime, file, from, to)?;
                        Ok(Some(report))
                    }
                    DiffMode::Rows => {
                        // Row-level detailed diff
                        row_diff_impl(runtime, file, from, to)
                    }
                }
            }

            GraftPragma::Show { lsn } => {
                if !file.is_idle() {
                    return pragma_err!("cannot show while there is an open transaction");
                }
                let result = show_lsn(runtime, file, lsn)?;
                Ok(Some(result))
            }
        }
    }
}

macro_rules! pluralize {
    ($n:expr, $s:literal) => {
        if $n == 1 { $s } else { concat!($s, "s") }
    };
}

fn format_volume_info(runtime: &Runtime, file: &VolFile) -> Result<String, ErrCtx> {
    let state = runtime.volume_get(&file.vid)?;
    let sync = state.sync().map_or_else(
        || "Never synced".into(),
        |sync| match sync.local_watermark {
            Some(local) => format!("L{local} | R{}", sync.remote),
            None => format!("R{}", sync.remote),
        },
    );
    let vid = state.vid;
    let local = state.local;
    let remote = state.remote;
    let snapshot = file.snapshot_or_latest()?;
    let page_count = file.page_count()?;
    let snapshot_size = PAGESIZE * page_count.to_usize();

    Ok(formatdoc!(
        "
            Volume: {vid}
            Local: {local}
            Remote: {remote}
            Last sync: {sync}
            Snapshot: {snapshot:?}
            Snapshot pages: {page_count}
            Snapshot size: {snapshot_size}
        "
    ))
}

fn format_volume_status(runtime: &Runtime, file: &VolFile) -> Result<String, ErrCtx> {
    let mut f = String::new();

    let tag = &file.tag;
    writeln!(&mut f, "On tag {tag}")?;

    let status = runtime.volume_status(&file.vid)?;
    let local_changes = status.local_status.changes();
    let remote_changes = status.remote_status.changes();

    writeln!(
        &mut f,
        indoc! {"
            Local Log {} is grafted to
            remote Log {}.
        "},
        status.local, status.remote,
    )?;

    match (local_changes, remote_changes) {
        (Some(local), Some(remote)) => {
            write!(
                &mut f,
                indoc! {"
                    The Volume and the remote have diverged,
                    and have {} and {} different commits each, respectively.
                "},
                local.len(),
                remote.len(),
            )?;
        }
        (Some(local), None) => {
            write!(
                &mut f,
                indoc! {"
                    The Volume is ahead of the remote by {} {}.
                      (use 'pragma graft_push' to push changes)
                "},
                local.len(),
                pluralize!(local.len(), "commit")
            )?;
        }
        (None, Some(remote)) => {
            writeln!(
                &mut f,
                indoc! {"
                    The Volume is behind the remote by {} {}.
                      (use 'pragma graft_pull' to pull changes)
                "},
                remote.len(),
                pluralize!(remote.len(), "commit")
            )?;
        }
        (None, None) => {
            write!(&mut f, "The Volume is up to date with the remote.")?;
        }
    }

    Ok(f)
}

fn format_volume_audit(runtime: &Runtime, file: &VolFile) -> Result<String, ErrCtx> {
    let snapshot = file.snapshot_or_latest()?;
    let missing_pages = runtime.snapshot_missing_pages(&snapshot)?;
    let pages = file.page_count()?.to_usize();
    if missing_pages.is_empty() {
        let checksum = runtime.snapshot_checksum(&snapshot)?;
        Ok(formatdoc!(
            "
                Cached {pages} of {pages} {} (100%%) from the remote Log.
                Checksum: {checksum}
            ",
            pluralize!(pages, "page"),
        ))
    } else {
        let missing = missing_pages.cardinality().to_usize();
        let have = pages - missing;
        let pct = (have as f64) / (pages as f64) * 100.0;
        Ok(formatdoc!(
            "
                Cached {have} of {pages} {} ({pct:.02}%%) from the remote Log.
                  (use 'pragma graft_hydrate' to fetch missing pages)
            ",
            pluralize!(pages, "page"),
        ))
    }
}

fn fetch_or_pull(runtime: &Runtime, file: &mut VolFile, pull: bool) -> Result<String, ErrCtx> {
    let pre = runtime.volume_status(&file.vid)?;
    if pull {
        runtime.volume_pull(file.vid.clone())?;
    } else {
        runtime.fetch_log(pre.remote, None)?;
    }
    let post = runtime.volume_status(&file.vid)?;

    let mut f = String::new();

    if let Some(diff) = AheadStatus::new(post.remote_status.base, pre.remote_status.base).changes()
    {
        writeln!(
            &mut f,
            "Pulled LSNs {} into remote Log {}",
            diff.to_string(),
            post.remote
        )?;
    } else {
        writeln!(&mut f, "No changes to remote Log {}", post.remote)?;
    }

    Ok(f)
}

fn push(runtime: &Runtime, file: &mut VolFile) -> Result<String, ErrCtx> {
    let pre = runtime.volume_status(&file.vid)?;
    if let Some(changes) = pre.local_status.changes()
        && !changes.is_empty()
    {
        runtime.volume_push(file.vid.clone())?;
        let post = runtime.volume_status(&file.vid)?;

        let pushed = AheadStatus::new(post.local_status.base, pre.local_status.base).changes();

        Ok(formatdoc!(
            "
                Pushed LSNs {} from local Log {}
                to remote Log {} @ {}
            ",
            pushed.map_or("unknown".into(), |lsns| lsns.to_string()),
            post.local,
            post.remote,
            post.remote_status
                .base
                .map_or("unknown".into(), |l| l.to_string())
        ))
    } else {
        Ok("Everything up-to-date".to_string())
    }
}

fn format_tags(runtime: &Runtime, file: &VolFile) -> Result<String, ErrCtx> {
    let mut f = String::new();
    let mut tags = runtime.tag_iter();
    while let Some((tag, vid)) = tags.try_next()? {
        let status = runtime.volume_status(&vid)?;
        let local = &status.local;
        let remote = &status.remote;

        writedoc!(
            &mut f,
            "
                Tag: {tag}{}
                  Volume: {vid}
                    Local: {local}
                    Remote: {remote}
                    Status: {status}
            ",
            if tag == file.tag { " (current)" } else { "" }
        )?;
    }
    Ok(f)
}

fn format_volumes(runtime: &Runtime, file: &VolFile) -> Result<String, ErrCtx> {
    let mut f = String::new();
    let mut volumes = runtime.volume_iter();
    while let Some(volume) = volumes.try_next()? {
        let vid = volume.vid;
        let status = runtime.volume_status(&vid)?;
        let local = volume.local;
        let remote = volume.remote;

        writedoc!(
            &mut f,
            "
                Volume: {vid}{}
                  Local: {local}
                  Remote: {remote}
                  Status: {status}
            ",
            if vid == file.vid { " (current)" } else { "" }
        )?;
    }
    Ok(f)
}

fn volume_export(_runtime: &Runtime, file: &VolFile, path: PathBuf) -> Result<String, ErrCtx> {
    // Get a reader based on the current state of the VolFile
    let reader = file.reader()?;

    let page_count = reader.page_count();
    let total_pages = page_count.to_usize();

    let mut output_file = File::create(&path)?;

    // Iterate over all pages and write them to the output file
    for page_idx in page_count.iter() {
        let page = reader.read_page(page_idx)?;
        output_file.write_all(page.as_ref())?;
    }

    Ok(format!(
        "exported {} {}",
        total_pages,
        pluralize!(total_pages, "page")
    ))
}

fn format_volume_log(runtime: &Runtime, file: &VolFile) -> Result<String, ErrCtx> {
    let commits = runtime.volume_log(&file.vid)?;

    if commits.is_empty() {
        return Ok("No commits in this Volume.".to_string());
    }

    let mut f = String::new();
    writeln!(&mut f, "Commit history for Volume {}:", file.vid)?;
    writeln!(&mut f, "{:<6} {:<12} {:<10} {:<12} SEGMENT", "LSN", "PAGES", "CHANGED", "CHECKPOINT")?;
    writeln!(&mut f, "{}", "-".repeat(70))?;

    for commit in commits {
        let checkpoint_mark = if commit.is_checkpoint { "✓" } else { "" };
        let segment_short = commit
            .segment_id.map_or_else(|| "-".to_string(), |s| s.short());
        writeln!(
            &mut f,
            "{:<6} {:<12} {:<10} {:<12} {}",
            commit.lsn,
            commit.page_count,
            commit.changed_pages,
            checkpoint_mark,
            segment_short
        )?;
    }

    Ok(f)
}

fn _format_diff(diff: &graft::PageDiffResult) -> String {
    let mut f = String::new();
    writeln!(&mut f, "Diff between LSN {} and LSN {}:", diff.from_lsn, diff.to_lsn).unwrap();
    writeln!(&mut f, "  Page count delta: {:+}", diff.page_count_delta).unwrap();
    writeln!(
        &mut f,
        "  Changed pages: {}",
        diff.added_or_modified_pages.cardinality()
    )
    .unwrap();

    if !diff.added_or_modified_pages.is_empty() {
        writeln!(&mut f, "  Page indices:").unwrap();
        for page_idx in diff.added_or_modified_pages.iter().take(20) {
            writeln!(&mut f, "    - Page {page_idx}").unwrap();
        }
        let remaining = diff.added_or_modified_pages.cardinality().to_usize().saturating_sub(20);
        if remaining > 0 {
            writeln!(&mut f, "    ... and {remaining} more").unwrap();
        }
    }

    f
}

/// Display detailed info for specified LSN
fn show_lsn(runtime: &Runtime, file: &VolFile, lsn: LSN) -> Result<String, ErrCtx> {
    let volume = runtime.volume_get(&file.vid)?;

    // Get commit info
    let commit = runtime
        .get_commit(&volume.local, lsn)?
        .ok_or_else(|| ErrCtx::PragmaErr(format!("LSN {lsn} not found").into()))?;

    let mut output = String::new();

    writeln!(&mut output, "Commit {} (LSN {})", volume.local, lsn)?;
    writeln!(&mut output, "  Page count: {}", commit.page_count)?;
    writeln!(
        &mut output,
        "  Checkpoint: {}",
        if commit.is_checkpoint() { "yes" } else { "no" }
    )?;

    if let Some(idx) = commit.segment_idx() {
        writeln!(&mut output, "  Segment: {}", idx.sid())?;
        writeln!(
            &mut output,
            "  Changed pages: {}",
            idx.pageset.cardinality()
        )?;
    }

    // Checkout and gather table info using B-tree parser
    let checkout_vol = runtime
        .volume_checkout(&file.vid, lsn)
        .map_err(|e| ErrCtx::PragmaErr(format!("Failed to checkout LSN {lsn}: {e:?}").into()))?;
    let vid = checkout_vol.vid;
    let reader = runtime.volume_reader(vid.clone()).map_err(|e| {
        ErrCtx::PragmaErr(format!("Failed to create reader: {e:?}").into())
    })?;

    // Use B-tree parser to read sqlite_master
    match crate::sqlite_parse::TableScanner::new(&reader) {
        Ok(scanner) => {
            match scanner.read_master_table() {
                Ok(entries) => {
                    writeln!(&mut output)?;
                    writeln!(&mut output, "Schema ({} objects):", entries.len())?;
                    for entry in &entries {
                        if entry.entry_type == "table" && !entry.name.starts_with("sqlite_") {
                            let row_count = match crate::sqlite_parse::read_all_rows(
                                &reader,
                                entry.root_page,
                            ) {
                                Ok(rows) => rows.len().to_string(),
                                Err(_) => "?".to_string(),
                            };
                            writeln!(
                                &mut output,
                                "  {} {} (root_page={}, rows={})",
                                entry.entry_type, entry.name, entry.root_page, row_count
                            )?;
                        } else {
                            writeln!(
                                &mut output,
                                "  {} {}",
                                entry.entry_type, entry.name
                            )?;
                        }
                    }
                }
                Err(e) => {
                    writeln!(&mut output)?;
                    writeln!(
                        &mut output,
                        "  (Failed to read schema: {e:?})"
                    )?;
                }
            }
        }
        Err(e) => {
            writeln!(&mut output)?;
            writeln!(
                &mut output,
                "  (Failed to parse B-tree: {e:?})"
            )?;
        }
    }

    // Cleanup
    let _ = runtime.volume_delete(&vid);

    Ok(output)
}

/// Row-level diff implementation
fn row_diff_impl(
    runtime: &Runtime,
    file: &VolFile,
    from: LSN,
    to: LSN,
) -> Result<Option<String>, ErrCtx> {
    let mut output = String::new();
    
    // Call row-level diff
    let diff = crate::row_level_diff::row_level_diff(runtime, &file.vid, from, to)
        .map_err(|e| ErrCtx::PragmaErr(format!("Row diff error: {e:?}").into()))?;
    
    writeln!(&mut output, "Row-level diff from LSN {from} to LSN {to}")?;
    writeln!(&mut output)?;
    
    if diff.table_changes.is_empty() {
        writeln!(&mut output, "No table changes detected.")?;
        return Ok(Some(output));
    }
    
    writeln!(&mut output, "Changed tables: {}", diff.table_changes.len())?;
    writeln!(&mut output)?;
    
    // Show changes for each table
    for table in &diff.table_changes {
        writeln!(&mut output, "Table: {}", table.table_name)?;
        writeln!(&mut output, "  Changes: {}", table.changes.len())?;
        
        // Count change types
        let mut inserts = 0;
        let mut deletes = 0;
        let mut updates = 0;
        
        for change in &table.changes {
            match change {
                crate::row_level_diff::RowChange::Insert { .. } => inserts += 1,
                crate::row_level_diff::RowChange::Delete { .. } => deletes += 1,
                crate::row_level_diff::RowChange::Update { .. } => updates += 1,
            }
        }
        
        if inserts > 0 {
            writeln!(&mut output, "    +{inserts} inserts")?;
        }
        if deletes > 0 {
            writeln!(&mut output, "    -{deletes} deletes")?;
        }
        if updates > 0 {
            writeln!(&mut output, "    ~{updates} updates")?;
        }
        
        // Show details for first few changes
        for (i, change) in table.changes.iter().take(5).enumerate() {
            match change {
                crate::row_level_diff::RowChange::Insert { rowid, row } => {
                    writeln!(&mut output, "    [{}] INSERT rowid={}", i + 1, rowid)?;
                    writeln!(&mut output, "      values: {:?}", row.values.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>().join(", "))?;
                }
                crate::row_level_diff::RowChange::Delete { rowid, .. } => {
                    writeln!(&mut output, "    [{}] DELETE rowid={}", i + 1, rowid)?;
                }
                crate::row_level_diff::RowChange::Update { rowid, old_row, new_row } => {
                    writeln!(&mut output, "    [{}] UPDATE rowid={}", i + 1, rowid)?;
                    writeln!(&mut output, "      old: {:?}", old_row.values.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>().join(", "))?;
                    writeln!(&mut output, "      new: {:?}", new_row.values.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>().join(", "))?;
                }
            }
        }
        
        if table.changes.len() > 5 {
            writeln!(&mut output, "    ... and {} more changes", table.changes.len() - 5)?;
        }
        
        writeln!(&mut output)?;
    }
    
    // Generate SQL script
    writeln!(&mut output, "-- SQL Script --")?;
    for table in &diff.table_changes {
        writeln!(&mut output, "{}", table.to_sql())?;
    }
    
    Ok(Some(output))
}
