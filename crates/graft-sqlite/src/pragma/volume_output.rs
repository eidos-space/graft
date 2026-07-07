use super::*;

pub(super) fn format_debug_log_lsn(runtime: &Runtime, file: &VolFile) -> Result<String, ErrCtx> {
    let volume = runtime.volume_get(&file.vid)?;
    let commits = runtime.volume_log(&file.vid)?;
    if commits.is_empty() {
        return Ok(format!("No storage commits yet for log {}.", volume.local));
    }

    let mut f = String::new();
    writeln!(&mut f, "log {}", volume.local)?;
    for commit in commits {
        writeln!(&mut f, "commit {}:{}", volume.local, commit.lsn)?;
        writeln!(&mut f, "page_count {}", commit.page_count)?;
        writeln!(&mut f, "changed_pages {}", commit.changed_pages)?;
        if let Some(segment) = commit.segment_id {
            writeln!(&mut f, "segment {}", segment.short())?;
        }
        if commit.is_checkpoint {
            writeln!(&mut f, "checkpoint true")?;
        }
        if let Some(timestamp) = commit.timestamp {
            writeln!(&mut f, "date {}", format_unix_millis(timestamp))?;
        }
        if let Some(message) = commit.message {
            writeln!(&mut f)?;
            writeln!(&mut f, "    {message}")?;
        }
        writeln!(&mut f)?;
    }
    Ok(f)
}

pub(super) fn format_debug_show_lsn(runtime: &Runtime, logref: &LogRef) -> Result<String, ErrCtx> {
    let Some(commit) = runtime.get_commit(&logref.log, logref.lsn)? else {
        return pragma_err!("commit not found");
    };
    let log = &commit.log;
    let lsn = commit.lsn;
    let page_count = commit.page_count;
    let commit_hash = &commit.commit_hash;
    let segment_idx = &commit.segment_idx;
    let checkpoints = &commit.checkpoints;
    Ok(formatdoc!(
        "
            Commit @ {log}:{lsn}
            page_count: {page_count}
            commit_hash: {commit_hash:?}
            segment_idx: {segment_idx:#?}
            checkpoints: {checkpoints:?}
        "
    ))
}

pub(super) fn format_volume_info(runtime: &Runtime, file: &VolFile) -> Result<String, ErrCtx> {
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

pub(super) fn format_volume_status(runtime: &Runtime, file: &VolFile) -> Result<String, ErrCtx> {
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
                        (use 'pragma graft_push' to push repository commits)
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
                        (use 'pragma graft_pull' to pull repository commits)
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

pub(super) fn format_volume_audit(runtime: &Runtime, file: &VolFile) -> Result<String, ErrCtx> {
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
                  (use 'pragma graft_debug_volume_hydrate' to fetch missing pages)
            ",
            pluralize!(pages, "page"),
        ))
    }
}

pub(super) fn json_volume_audit(
    runtime: &Runtime,
    file: &VolFile,
) -> Result<JsonVolumeAudit, ErrCtx> {
    let snapshot = file.snapshot_or_latest()?;
    let missing_pages = runtime.snapshot_missing_pages(&snapshot)?;
    let total_pages = file.page_count()?.to_usize();
    let missing = missing_pages.cardinality().to_usize();
    let local_pages = total_pages.saturating_sub(missing);
    let percentage = if total_pages == 0 {
        100.0
    } else {
        (local_pages as f64) / (total_pages as f64) * 100.0
    };
    let checksum = if missing_pages.is_empty() {
        Some(runtime.snapshot_checksum(&snapshot)?.to_string())
    } else {
        None
    };
    Ok(JsonVolumeAudit {
        local_pages,
        total_pages,
        percentage,
        needs_hydrate: !missing_pages.is_empty(),
        checksum,
    })
}

pub(super) fn fetch_or_pull(
    runtime: &Runtime,
    file: &mut VolFile,
    pull: bool,
) -> Result<String, ErrCtx> {
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

pub(super) fn push(runtime: &Runtime, file: &mut VolFile) -> Result<String, ErrCtx> {
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

pub(super) fn format_tags(runtime: &Runtime, file: &VolFile) -> Result<String, ErrCtx> {
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

pub(super) fn format_volumes(runtime: &Runtime, file: &VolFile) -> Result<String, ErrCtx> {
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

pub(super) fn json_volumes(
    runtime: &Runtime,
    file: &VolFile,
) -> Result<Vec<JsonVolumeListEntry>, ErrCtx> {
    let mut entries = Vec::new();
    let mut volumes = runtime.volume_iter();
    while let Some(volume) = volumes.try_next()? {
        let vid = volume.vid;
        let status = runtime.volume_status(&vid)?;
        entries.push(JsonVolumeListEntry {
            id: vid.to_string(),
            local: volume.local.to_string(),
            remote: volume.remote.to_string(),
            status: status.to_string(),
            current: vid == file.vid,
        });
    }
    Ok(entries)
}

pub(super) fn volume_export(
    _runtime: &Runtime,
    file: &VolFile,
    path: PathBuf,
) -> Result<String, ErrCtx> {
    // Get a reader based on the current state of the VolFile
    let reader = file.reader()?;

    let page_count = reader.page_count();
    let total_pages = page_count.to_usize();

    write_volume_reader_to_path(&reader, &path)?;

    Ok(format!(
        "exported {} {}",
        total_pages,
        pluralize!(total_pages, "page")
    ))
}

/// Format unix millis as "YYYY-MM-DD HH:MM:SS" without external crate
pub(super) fn format_unix_millis(ts: u64) -> String {
    let secs = (ts / 1000) as i64;
    let days = secs / 86400;
    // Algorithm from Howard Hinnant (C++ chrono)
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64) + (era * 400);
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let day_secs = secs.rem_euclid(86400) as u32;
    format!(
        "{}-{:02}-{:02} {:02}:{:02}:{:02}",
        y,
        m,
        d,
        day_secs / 3600,
        (day_secs / 60) % 60,
        day_secs % 60
    )
}

pub(super) fn format_debug_page_diff(diff: &graft::PageDiffResult) -> String {
    let mut f = String::new();
    writeln!(
        &mut f,
        "Diff between LSN {} and LSN {}:",
        diff.from_lsn, diff.to_lsn
    )
    .unwrap();
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
        let remaining = diff
            .added_or_modified_pages
            .cardinality()
            .to_usize()
            .saturating_sub(20);
        if remaining > 0 {
            writeln!(&mut f, "    ... and {remaining} more").unwrap();
        }
    }

    f
}
