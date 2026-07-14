use super::*;

pub(super) fn format_fetch_all_outcome(outcome: &FetchAllOutcome) -> Result<String, ErrCtx> {
    let mut f = String::new();
    let commits: usize = outcome.branches.iter().map(|branch| branch.commits).sum();
    writeln!(
        &mut f,
        "Fetched {} ({} {}, {} new {})",
        outcome.remote,
        outcome.branches.len(),
        pluralize!(outcome.branches.len(), "branch"),
        commits,
        pluralize!(commits, "commit")
    )?;
    for branch in &outcome.branches {
        writeln!(
            &mut f,
            "  {}/{} at {} ({} new {})",
            branch.remote,
            branch.branch,
            &branch.head[..12],
            branch.commits,
            pluralize!(branch.commits, "commit")
        )?;
    }
    Ok(f)
}

pub(super) fn format_push_all_outcome(outcome: &PushAllOutcome) -> Result<String, ErrCtx> {
    let mut f = String::new();
    let commits: usize = outcome.branches.iter().map(|branch| branch.commits).sum();
    let forced = outcome.branches.iter().any(|branch| branch.forced);
    writeln!(
        &mut f,
        "{} {} ({} {}, {} {})",
        if forced { "Force-pushed" } else { "Pushed" },
        outcome.remote,
        outcome.branches.len(),
        pluralize!(outcome.branches.len(), "branch"),
        commits,
        pluralize!(commits, "commit")
    )?;
    for branch in &outcome.branches {
        if branch.deleted {
            writeln!(
                &mut f,
                "  Deleted {}/{} (was {})",
                branch.remote,
                branch.remote_branch,
                &branch.head[..branch.head.len().min(12)]
            )?;
            continue;
        }
        writeln!(
            &mut f,
            "  {}{}/{} at {} ({} {})",
            if branch.forced { "+" } else { "" },
            branch.remote,
            branch.remote_branch,
            &branch.head[..12],
            branch.commits,
            pluralize!(branch.commits, "commit")
        )?;
    }
    Ok(f)
}

pub(super) fn format_repo_diff(diff: &RepoDiff) -> Result<String, ErrCtx> {
    let mut f = String::new();
    writeln!(
        &mut f,
        "Diff {}..{}",
        &diff.from[..diff.from.len().min(12)],
        &diff.to[..diff.to.len().min(12)]
    )?;
    if diff.files.is_empty() && diff.artifacts.is_empty() {
        writeln!(&mut f, "No changes.")?;
        return Ok(f);
    }

    for file in &diff.files {
        let change = repo_file_change_label(file.change);
        writeln!(&mut f, "{change}: {}", file.path)?;
        if let Some(from) = &file.from {
            writeln!(
                &mut f,
                "  from: {} page(s), {} range(s)",
                from.snapshot.page_count,
                from.snapshot.ranges.len()
            )?;
        }
        if let Some(to) = &file.to {
            writeln!(
                &mut f,
                "  to:   {} page(s), {} range(s)",
                to.snapshot.page_count,
                to.snapshot.ranges.len()
            )?;
        } else if let Some(worktree) = &file.worktree {
            writeln!(
                &mut f,
                "  to:   {} page(s), physical worktree",
                worktree.page_count
            )?;
        }
    }
    for artifact in &diff.artifacts {
        let change = repo_file_change_label(artifact.change);
        writeln!(&mut f, "{change}: {}", artifact.path)?;
        if let Some(from) = &artifact.from {
            writeln!(
                &mut f,
                "  from: {} byte(s), {}, {}",
                from.size(),
                repo_artifact_state_label(from),
                from.content_hash()
            )?;
        }
        if let Some(to) = &artifact.to {
            writeln!(
                &mut f,
                "  to:   {} byte(s), {}, {}",
                to.size(),
                repo_artifact_state_label(to),
                to.content_hash()
            )?;
        }
    }
    Ok(f)
}

pub(super) fn format_repo_row_diff(
    runtime: &Runtime,
    repo: &Repository,
    diff: &RepoDiff,
) -> Result<String, ErrCtx> {
    let mut f = String::new();
    writeln!(
        &mut f,
        "Row Diff {}..{}",
        &diff.from[..diff.from.len().min(12)],
        &diff.to[..diff.to.len().min(12)]
    )?;
    if diff.files.is_empty() {
        writeln!(&mut f, "No changes.")?;
        return Ok(f);
    }

    for file in &diff.files {
        let change = repo_file_change_label(file.change);
        writeln!(&mut f, "{change}: {}", file.path)?;
        let Some(row_diff) = repo_file_row_diff(runtime, repo, file)? else {
            writeln!(
                &mut f,
                "  Row diff unavailable for {} database snapshots.",
                change
            )?;
            continue;
        };
        write_indented(&mut f, &row_diff.to_report(), "  ")?;
    }
    Ok(f)
}

pub(super) fn repo_file_change_label(change: RepoFileChange) -> &'static str {
    match change {
        RepoFileChange::Added => "added",
        RepoFileChange::Deleted => "deleted",
        RepoFileChange::Modified => "modified",
    }
}

pub(super) fn repo_artifact_state_label(state: &CommitArtifactState) -> &'static str {
    if state.is_large() {
        "external payload"
    } else {
        "file"
    }
}

pub(super) fn repo_file_row_diff(
    runtime: &Runtime,
    repo: &Repository,
    file: &graft::repo::RepoFileDiff,
) -> Result<Option<crate::row_level_diff::RowLevelDiff>, ErrCtx> {
    let Some(from) = &file.from else {
        return Ok(None);
    };
    let resolver = RepoSnapshotResolver::local_then_remote(
        runtime,
        repo_default_remote_store(repo),
        RepoSnapshotPurpose::Diff,
        SnapshotHashPolicy::AllowHydratedMismatch,
    );
    resolver.resolve_snapshot(&from.snapshot)?;
    if file.worktree.is_some() {
        let physical_path = repo.worktree().join(&file.path);
        let physical = PhysicalSqliteReader::open(&physical_path)?;
        let from_snapshot = from.snapshot.to_snapshot();
        let from_lsn = from_snapshot.head().map_or(LSN::FIRST, |(_, lsn)| lsn);
        let from_reader = runtime.snapshot_reader(from_snapshot);
        return crate::row_level_diff::row_level_diff_readers(
            &from_reader,
            &physical,
            from_lsn,
            from_lsn.saturating_next(),
        )
        .map(Some)
        .map_err(|err| {
            ErrCtx::PragmaErr(format!("Row diff error for `{}`: {err:?}", file.path).into())
        });
    }
    let Some(to) = &file.to else {
        return Ok(None);
    };
    resolver.resolve_snapshot(&to.snapshot)?;
    crate::row_level_diff::row_level_diff_snapshots(
        runtime,
        &from.snapshot.to_snapshot(),
        &to.snapshot.to_snapshot(),
    )
    .map(Some)
    .map_err(|err| ErrCtx::PragmaErr(format!("Row diff error for `{}`: {err:?}", file.path).into()))
}

pub(super) fn repo_default_remote_store(repo: &Repository) -> Option<Arc<Remote>> {
    let remote = repo_default_remote(repo, None).ok()?;
    repo.remote_store(&remote).ok().map(Arc::new)
}

pub(super) fn write_indented(out: &mut String, text: &str, prefix: &str) -> Result<(), ErrCtx> {
    for line in text.lines() {
        writeln!(out, "{prefix}{line}")?;
    }
    Ok(())
}

pub(super) fn format_repo_show(commit: &graft::repo::CommitObject) -> Result<String, ErrCtx> {
    let mut f = String::new();
    writeln!(&mut f, "commit {}", commit.id)?;
    if commit.parents.is_empty() {
        if let Some(parent) = &commit.parent {
            writeln!(&mut f, "parent {parent}")?;
        }
    } else {
        for parent in &commit.parents {
            writeln!(&mut f, "parent {parent}")?;
        }
    }
    if let Some(tree) = &commit.tree {
        writeln!(&mut f, "tree {tree}")?;
    }
    writeln!(&mut f, "date {}", format_unix_millis(commit.timestamp_ms))?;
    writeln!(&mut f)?;
    writeln!(&mut f, "    {}", commit.message)?;
    if !commit.files.is_empty() {
        writeln!(&mut f)?;
        writeln!(&mut f, "Files:")?;
        for (path, state) in &commit.files {
            writeln!(
                &mut f,
                "  {} ({} page(s), {} range(s))",
                path,
                state.snapshot.page_count,
                state.snapshot.ranges.len()
            )?;
        }
    }
    if !commit.artifacts.is_empty() {
        writeln!(&mut f)?;
        writeln!(&mut f, "Artifacts:")?;
        for (path, state) in &commit.artifacts {
            writeln!(
                &mut f,
                "  {} ({} byte(s), {}, {})",
                path,
                state.size(),
                repo_artifact_state_label(state),
                state.content_hash()
            )?;
        }
    }
    Ok(f)
}

pub(super) fn format_repo_status(status: &RepoStatus) -> Result<String, ErrCtx> {
    let mut f = String::new();
    match &status.head {
        Head::Branch { name } => writeln!(&mut f, "On branch {name}")?,
        Head::Detached { commit } => writeln!(&mut f, "HEAD detached at {commit}")?,
    }
    if let Some(upstream) = &status.upstream {
        writeln!(&mut f, "Tracking: {}/{}", upstream.remote, upstream.branch)?;
    }
    writeln!(&mut f, "Repository: {}", status.worktree.display())?;
    writeln!(&mut f, "Format: v{}", status.repository_format_version)?;
    match &status.head_target {
        Some(target) => writeln!(&mut f, "HEAD: {target}")?,
        None => writeln!(&mut f, "No commits yet")?,
    }
    if let Some(merge_head) = &status.merge_head {
        writeln!(
            &mut f,
            "Merge in progress with {}",
            &merge_head[..merge_head.len().min(12)]
        )?;
    }
    if !status.conflicted.is_empty() {
        writeln!(&mut f, "Unmerged paths:")?;
        for path in &status.conflicted {
            writeln!(&mut f, "  {path}")?;
        }
    }
    if !status.staged.is_empty() {
        writeln!(&mut f, "Changes to be committed:")?;
        for path in &status.staged {
            writeln!(&mut f, "  {path}")?;
        }
    }
    if !status.unstaged_changes.is_empty() {
        writeln!(&mut f, "Changes not staged for commit.")?;
        writeln!(&mut f, "  (use 'pragma graft_add' to stage)")?;
        for change in &status.unstaged_changes {
            writeln!(
                &mut f,
                "  {}: {}",
                worktree_change_label(change.change),
                change.path
            )?;
        }
    } else if !status.unstaged.is_empty() {
        writeln!(&mut f, "Changes not staged for commit.")?;
        writeln!(&mut f, "  (use 'pragma graft_add' to stage)")?;
        for path in &status.unstaged {
            writeln!(&mut f, "  {path}")?;
        }
    }
    if status.unstaged.is_empty()
        && status.staged.is_empty()
        && status.conflicted.is_empty()
        && status.merge_head.is_none()
    {
        writeln!(&mut f, "Worktree clean.")?;
    }
    Ok(f)
}

pub(super) fn worktree_change_label(change: RepoWorktreeChangeKind) -> &'static str {
    match change {
        RepoWorktreeChangeKind::Modified => "modified",
        RepoWorktreeChangeKind::Deleted => "deleted",
        RepoWorktreeChangeKind::Untracked => "untracked",
    }
}

pub(super) fn format_repo_artifact_audit(audit: &RepoArtifactAudit) -> Result<String, ErrCtx> {
    let mut f = String::new();
    if audit.ok() {
        writeln!(&mut f, "Repository artifact payloads OK.")?;
    } else {
        writeln!(&mut f, "Repository artifact payload issues:")?;
        for issue in &audit.issues {
            writeln!(
                &mut f,
                "  {}: {} ({})",
                repo_artifact_audit_issue_label(issue.kind),
                issue.path,
                issue.message
            )?;
        }
    }
    writeln!(&mut f, "Artifacts: {}", audit.artifacts)?;
    writeln!(&mut f, "External payloads: {}", audit.external_payloads)?;
    Ok(f)
}

pub(super) fn format_repo_artifact_repair(
    outcome: &RepoArtifactRepairOutcome,
) -> Result<String, ErrCtx> {
    let mut f = String::new();
    writeln!(
        &mut f,
        "Repaired repository artifact payloads from {}.",
        outcome.remote
    )?;
    writeln!(&mut f, "Fetched objects: {}", outcome.fetched_objects)?;
    writeln!(
        &mut f,
        "Fetched external payloads: {}",
        outcome.fetched_external_payloads
    )?;
    writeln!(&mut f, "Issues before: {}", outcome.before.issues.len())?;
    writeln!(&mut f, "Issues after: {}", outcome.after.issues.len())?;
    if !outcome.after.ok() {
        writeln!(&mut f, "Remaining repository artifact payload issues:")?;
        for issue in &outcome.after.issues {
            writeln!(
                &mut f,
                "  {}: {} ({})",
                repo_artifact_audit_issue_label(issue.kind),
                issue.path,
                issue.message
            )?;
        }
    }
    writeln!(&mut f, "Artifacts: {}", outcome.after.artifacts)?;
    writeln!(
        &mut f,
        "External payloads: {}",
        outcome.after.external_payloads
    )?;
    Ok(f)
}

pub(super) fn format_large_file_fetch_outcome(
    outcome: &RepoLargeFileFetchOutcome,
) -> Result<String, ErrCtx> {
    let mut f = String::new();
    if outcome.external_payloads == 0 {
        writeln!(
            &mut f,
            "No external payloads referenced by {}.",
            outcome.target
        )?;
    } else if outcome.fetched_payloads == 0 {
        writeln!(
            &mut f,
            "External payloads already present for {}.",
            outcome.target
        )?;
    } else {
        writeln!(
            &mut f,
            "Fetched {} external payload(s), {} byte(s), from {}.",
            outcome.fetched_payloads, outcome.fetched_bytes, outcome.remote
        )?;
    }
    writeln!(&mut f, "Remote: {}", outcome.remote)?;
    writeln!(&mut f, "Target: {}", outcome.target)?;
    writeln!(&mut f, "External payloads: {}", outcome.external_payloads)?;
    writeln!(
        &mut f,
        "Already present: {}",
        outcome.already_present_payloads
    )?;
    for file in &outcome.files {
        writeln!(
            &mut f,
            "  {} ({}, {} byte(s), {}, paths: {})",
            file.content_hash,
            large_file_fetch_status_label(file.status),
            file.size,
            file.store_path,
            file.paths.join(", ")
        )?;
    }
    Ok(f)
}

pub(super) fn large_file_fetch_status_label(status: RepoLargeFileFetchStatus) -> &'static str {
    match status {
        RepoLargeFileFetchStatus::Present => "present",
        RepoLargeFileFetchStatus::Fetched => "fetched",
    }
}

pub(super) fn format_large_file_status_outcome(
    outcome: &RepoLargeFileStatusOutcome,
) -> Result<String, ErrCtx> {
    let mut f = String::new();
    if outcome.external_payloads == 0 {
        writeln!(
            &mut f,
            "No external payloads referenced by {}.",
            outcome.target
        )?;
    } else {
        writeln!(
            &mut f,
            "External payloads for {}: {} present, {} missing, {} invalid.",
            outcome.target,
            outcome.present_payloads,
            outcome.missing_payloads,
            outcome.invalid_payloads
        )?;
    }
    writeln!(&mut f, "External payloads: {}", outcome.external_payloads)?;
    writeln!(&mut f, "Present bytes: {}", outcome.present_bytes)?;
    writeln!(&mut f, "Missing bytes: {}", outcome.missing_bytes)?;
    writeln!(&mut f, "Invalid bytes: {}", outcome.invalid_bytes)?;
    for file in &outcome.files {
        let message = file
            .message
            .as_ref()
            .map(|message| format!(", {message}"))
            .unwrap_or_default();
        writeln!(
            &mut f,
            "  {} ({}, {} byte(s), {}, paths: {}{})",
            file.content_hash,
            large_file_status_state_label(file.status),
            file.size,
            file.store_path,
            file.paths.join(", "),
            message
        )?;
    }
    Ok(f)
}

pub(super) fn large_file_status_state_label(status: RepoLargeFileStatusState) -> &'static str {
    match status {
        RepoLargeFileStatusState::Present => "present",
        RepoLargeFileStatusState::Missing => "missing",
        RepoLargeFileStatusState::Invalid => "invalid",
    }
}

pub(super) fn format_large_file_prune_outcome(
    outcome: &RepoLargeFilePruneOutcome,
) -> Result<String, ErrCtx> {
    let mut f = String::new();
    if outcome.candidate_payloads == 0 {
        writeln!(&mut f, "No unreferenced external payloads.")?;
    } else if outcome.dry_run {
        writeln!(
            &mut f,
            "Would prune {} external payload(s), {} byte(s).",
            outcome.candidate_payloads, outcome.candidate_bytes
        )?;
    } else {
        writeln!(
            &mut f,
            "Pruned {} external payload(s), {} byte(s).",
            outcome.pruned_payloads, outcome.pruned_bytes
        )?;
    }
    writeln!(
        &mut f,
        "Referenced external payloads: {}",
        outcome.referenced_payloads
    )?;
    for file in &outcome.files {
        writeln!(
            &mut f,
            "  {} ({} byte(s), {})",
            file.content_hash, file.size, file.path
        )?;
    }
    Ok(f)
}

pub(super) fn format_storage_gc_outcome(
    outcome: &graft::local::fjall_storage::StorageGcOutcome,
) -> Result<String, ErrCtx> {
    let mut f = String::new();
    if outcome.candidate_volumes == 0
        && outcome.candidate_commits == 0
        && outcome.candidate_segments == 0
        && outcome.candidate_pages == 0
    {
        writeln!(&mut f, "No unreachable SQLite storage records.")?;
    } else if outcome.dry_run {
        writeln!(
            &mut f,
            "Would prune {} volume(s), {} commit(s), {} segment(s), and {} page(s) ({} byte(s)).",
            outcome.candidate_volumes,
            outcome.candidate_commits,
            outcome.candidate_segments,
            outcome.candidate_pages,
            outcome.candidate_page_bytes
        )?;
    } else {
        writeln!(
            &mut f,
            "Pruned {} volume(s), {} commit(s), {} segment(s), and {} page(s) ({} byte(s)).",
            outcome.pruned_volumes,
            outcome.pruned_commits,
            outcome.pruned_segments,
            outcome.pruned_pages,
            outcome.pruned_page_bytes
        )?;
    }
    writeln!(
        &mut f,
        "Retained {} volume(s), {} commit(s), {} segment(s), and {} page(s).",
        outcome.retained_volumes,
        outcome.retained_commits,
        outcome.retained_segments,
        outcome.retained_pages
    )?;
    Ok(f)
}

pub(super) fn repo_artifact_audit_issue_label(kind: RepoArtifactAuditIssueKind) -> &'static str {
    match kind {
        RepoArtifactAuditIssueKind::MissingObject => "missing object",
        RepoArtifactAuditIssueKind::InvalidObject => "invalid object",
        RepoArtifactAuditIssueKind::MissingExternalPayload => "missing external payload",
        RepoArtifactAuditIssueKind::InvalidExternalPayload => "invalid external payload",
    }
}

pub(super) fn format_repo_tracked_paths(paths: &[RepoTrackedPath]) -> Result<String, ErrCtx> {
    format_repo_path_inventory(paths, "No tracked paths.")
}

pub(super) fn format_repo_untracked_paths(paths: &[RepoTrackedPath]) -> Result<String, ErrCtx> {
    format_repo_path_inventory(paths, "No untracked paths.")
}

pub(super) fn format_repo_path_inventory(
    paths: &[RepoTrackedPath],
    empty_message: &str,
) -> Result<String, ErrCtx> {
    let mut f = String::new();
    if paths.is_empty() {
        writeln!(&mut f, "{empty_message}")?;
        return Ok(f);
    }
    for path in paths {
        match path.kind {
            RepoTrackedPathKind::SqliteDatabase => {
                if let Some(page_count) = path.page_count {
                    writeln!(
                        &mut f,
                        "{} (sqlite, {}, {page_count} page(s))",
                        path.path,
                        repo_path_storage_label(path.storage)
                    )?;
                } else {
                    writeln!(
                        &mut f,
                        "{} (sqlite, {}, {} byte(s))",
                        path.path,
                        repo_path_storage_label(path.storage),
                        path.size
                            .map(|size| size.to_string())
                            .unwrap_or_else(|| "?".to_string())
                    )?;
                }
            }
            RepoTrackedPathKind::TextFile | RepoTrackedPathKind::BinaryFile => writeln!(
                &mut f,
                "{} ({}, {}, {} byte(s))",
                path.path,
                repo_tracked_path_kind_label(path.kind),
                repo_path_storage_label(path.storage),
                path.size
                    .map(|size| size.to_string())
                    .unwrap_or_else(|| "?".to_string())
            )?,
        }
    }
    Ok(f)
}

pub(super) fn format_repo_tracked_path_details(
    paths: &[RepoTrackedPathDetail],
) -> Result<String, ErrCtx> {
    let mut f = String::new();
    if paths.is_empty() {
        writeln!(&mut f, "No tracked paths.")?;
        return Ok(f);
    }
    for path in paths {
        match path.kind {
            RepoTrackedPathKind::SqliteDatabase => writeln!(
                &mut f,
                "{} (sqlite, {}, {} page(s))",
                path.path,
                repo_path_storage_label(path.storage),
                path.page_count
                    .map(|count| count.to_string())
                    .unwrap_or_else(|| "?".to_string())
            )?,
            RepoTrackedPathKind::TextFile | RepoTrackedPathKind::BinaryFile
                if path.storage == RepoPathStorage::External =>
            {
                writeln!(
                    &mut f,
                    "{} ({}, {}, {} byte(s), oid {}, hash {}, object {}, payload {})",
                    path.path,
                    repo_tracked_path_kind_label(path.kind),
                    repo_path_storage_label(path.storage),
                    path.size
                        .map(|size| size.to_string())
                        .unwrap_or_else(|| "?".to_string()),
                    option_object_id_label(path.oid.as_ref()),
                    option_object_id_label(path.content_hash.as_ref()),
                    presence_label(path.object_present),
                    presence_label(path.external_payload_present)
                )?
            }
            RepoTrackedPathKind::TextFile | RepoTrackedPathKind::BinaryFile => writeln!(
                &mut f,
                "{} ({}, {}, {} byte(s), oid {}, hash {}, object {})",
                path.path,
                repo_tracked_path_kind_label(path.kind),
                repo_path_storage_label(path.storage),
                path.size
                    .map(|size| size.to_string())
                    .unwrap_or_else(|| "?".to_string()),
                option_object_id_label(path.oid.as_ref()),
                option_object_id_label(path.content_hash.as_ref()),
                presence_label(path.object_present)
            )?,
        }
    }
    Ok(f)
}

pub(super) fn option_object_id_label(id: Option<&graft::repo::object::ObjectId>) -> String {
    id.map(ToString::to_string)
        .unwrap_or_else(|| "?".to_string())
}

pub(super) fn presence_label(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "present",
        Some(false) => "missing",
        None => "n/a",
    }
}

pub(super) fn format_repo_tracked_path_entries(
    paths: &[RepoTrackedPathEntry],
) -> Result<String, ErrCtx> {
    let mut f = String::new();
    if paths.is_empty() {
        writeln!(&mut f, "No tracked paths.")?;
        return Ok(f);
    }
    for path in paths {
        let mode = path
            .mode
            .map(|mode| mode.to_string())
            .unwrap_or_else(|| "------".to_string());
        let oid = path
            .oid
            .as_ref()
            .map(|oid| oid.short())
            .unwrap_or("------------");
        match path.kind {
            RepoTrackedPathKind::SqliteDatabase => writeln!(
                &mut f,
                "{} {mode} {oid} {} (sqlite, {}, {} page(s))",
                repo_index_stage_label(path.stage),
                path.path,
                repo_path_storage_label(path.storage),
                path.page_count
                    .map(|count| count.to_string())
                    .unwrap_or_else(|| "?".to_string())
            )?,
            RepoTrackedPathKind::TextFile | RepoTrackedPathKind::BinaryFile => writeln!(
                &mut f,
                "{} {mode} {oid} {} ({}, {}, {} byte(s))",
                repo_index_stage_label(path.stage),
                path.path,
                repo_tracked_path_kind_label(path.kind),
                repo_path_storage_label(path.storage),
                path.size
                    .map(|size| size.to_string())
                    .unwrap_or_else(|| "?".to_string())
            )?,
        }
    }
    Ok(f)
}

pub(super) fn format_repo_config_entry(entry: &RepoConfigEntry) -> Result<String, ErrCtx> {
    let mut f = String::new();
    writeln!(&mut f, "{} = {}", entry.key, entry.value)?;
    Ok(f)
}

pub(super) fn format_repo_config_entries(entries: &[RepoConfigEntry]) -> Result<String, ErrCtx> {
    let mut f = String::new();
    for entry in entries {
        writeln!(&mut f, "{} = {}", entry.key, entry.value)?;
    }
    Ok(f)
}

pub(super) fn repo_tracked_path_kind_label(kind: RepoTrackedPathKind) -> &'static str {
    match kind {
        RepoTrackedPathKind::SqliteDatabase => "sqlite",
        RepoTrackedPathKind::TextFile => "text file",
        RepoTrackedPathKind::BinaryFile => "binary file",
    }
}

pub(super) fn repo_path_storage_label(storage: RepoPathStorage) -> &'static str {
    match storage {
        RepoPathStorage::SqliteSnapshot => "sqlite snapshot",
        RepoPathStorage::Inline => "inline",
        RepoPathStorage::External => "external",
    }
}

pub(super) fn repo_index_stage_label(stage: graft::repo::index::IndexStage) -> &'static str {
    match stage {
        graft::repo::index::IndexStage::Normal => "normal",
        graft::repo::index::IndexStage::Base => "base",
        graft::repo::index::IndexStage::Ours => "ours",
        graft::repo::index::IndexStage::Theirs => "theirs",
    }
}

pub(super) fn repo_tracked_path_kind_json_label(kind: RepoTrackedPathKind) -> &'static str {
    match kind {
        RepoTrackedPathKind::SqliteDatabase => "sqlite_database",
        RepoTrackedPathKind::TextFile => "text_file",
        RepoTrackedPathKind::BinaryFile => "binary_file",
    }
}

pub(super) fn repo_path_storage_json_label(storage: RepoPathStorage) -> &'static str {
    match storage {
        RepoPathStorage::SqliteSnapshot => "sqlite_snapshot",
        RepoPathStorage::Inline => "inline",
        RepoPathStorage::External => "external",
    }
}

pub(super) fn format_conflicts(status: &RepoStatus) -> Result<String, ErrCtx> {
    let mut f = String::new();
    if status.conflicted.is_empty() {
        writeln!(&mut f, "No conflicts.")?;
        return Ok(f);
    }

    writeln!(&mut f, "Unmerged paths:")?;
    for path in &status.conflicted {
        writeln!(&mut f, "  {path}")?;
    }
    writeln!(&mut f)?;
    writeln!(
        &mut f,
        "Resolve a path with `pragma graft_resolve = \"--ours [path]\"`, `pragma graft_resolve = \"--theirs [path]\"`, or `pragma graft_resolve = \"--manual [path]\"`."
    )?;
    Ok(f)
}

pub(super) fn format_branches(
    branches: &[BranchInfo],
    remote_branches: &[RemoteBranchRef],
    mode: BranchListMode,
) -> Result<String, ErrCtx> {
    if branches.is_empty() && remote_branches.is_empty() && !matches!(mode, BranchListMode::Remote)
    {
        return Ok("No branches.".to_string());
    }
    if remote_branches.is_empty() && matches!(mode, BranchListMode::Remote) {
        return Ok("No remote branches.".to_string());
    }

    let mut f = String::new();
    if !matches!(mode, BranchListMode::Remote) {
        for branch in branches {
            let marker = if branch.current { "*" } else { " " };
            let target = branch
                .target
                .as_deref()
                .map_or("(unborn)", |target| &target[..target.len().min(12)]);
            let upstream = branch
                .upstream
                .as_ref()
                .map(|upstream| format!(" [{}{}{}]", upstream.remote, "/", upstream.branch))
                .unwrap_or_default();
            writeln!(&mut f, "{marker} {:<24} {target}{upstream}", branch.name)?;
        }
    }
    if mode.includes_remote() {
        for branch in remote_branches {
            let name = if matches!(mode, BranchListMode::All) {
                format!("remotes/{}/{}", branch.remote, branch.branch)
            } else {
                format!("{}/{}", branch.remote, branch.branch)
            };
            writeln!(
                &mut f,
                "  {name:<24} {}",
                &branch.head[..branch.head.len().min(12)]
            )?;
        }
    }
    Ok(f)
}

pub(super) fn format_branch_created(branch: &BranchInfo) -> String {
    match &branch.target {
        Some(target) => format!("Created branch '{}' at {}", branch.name, &target[..12]),
        None => format!("Created unborn branch '{}'", branch.name),
    }
}

pub(super) fn format_branch_upstream(branch: &BranchInfo) -> String {
    match &branch.upstream {
        Some(upstream) => format!(
            "Branch '{}' set to track {}/{}",
            branch.name, upstream.remote, upstream.branch
        ),
        None => format!("Branch '{}' has no upstream", branch.name),
    }
}

pub(super) fn format_branch_upstream_unset(branch: &BranchInfo) -> String {
    format!("Branch '{}' upstream unset", branch.name)
}

pub(super) fn format_branch_deleted(branch: &BranchInfo, force: bool) -> String {
    let forced = if force { " forcibly" } else { "" };
    match &branch.target {
        Some(target) => format!(
            "Deleted branch '{}'{} (was {})",
            branch.name,
            forced,
            &target[..target.len().min(12)]
        ),
        None => format!("Deleted unborn branch '{}'{}", branch.name, forced),
    }
}

pub(super) fn format_branch_renamed(old: &str, branch: &BranchInfo, force: bool) -> String {
    let forced = if force { " forcibly" } else { "" };
    match &branch.target {
        Some(target) => format!(
            "Renamed branch '{}' to '{}'{} at {}",
            old,
            branch.name,
            forced,
            &target[..target.len().min(12)]
        ),
        None => format!(
            "Renamed unborn branch '{}' to '{}'{}",
            old, branch.name, forced
        ),
    }
}

pub(super) fn format_repo_tags(tags: &[TagInfo]) -> Result<String, ErrCtx> {
    if tags.is_empty() {
        return Ok("No tags.".to_string());
    }

    let mut f = String::new();
    for tag in tags {
        writeln!(
            &mut f,
            "{:<24} {}{}",
            tag.name,
            &tag.target[..tag.target.len().min(12)],
            if tag.annotated {
                format!(" (annotated {})", &tag.object[..tag.object.len().min(12)])
            } else {
                String::new()
            }
        )?;
    }
    Ok(f)
}

pub(super) fn format_tag_created(tag: &TagInfo) -> String {
    if tag.annotated {
        format!(
            "Created annotated tag '{}' at {} ({})",
            tag.name,
            &tag.target[..tag.target.len().min(12)],
            &tag.object[..tag.object.len().min(12)]
        )
    } else {
        format!(
            "Created tag '{}' at {}",
            tag.name,
            &tag.target[..tag.target.len().min(12)]
        )
    }
}

pub(super) fn format_tag_deleted(tag: &TagInfo) -> String {
    if tag.annotated {
        format!(
            "Deleted annotated tag '{}' (was {} via {})",
            tag.name,
            &tag.target[..tag.target.len().min(12)],
            &tag.object[..tag.object.len().min(12)]
        )
    } else {
        format!(
            "Deleted tag '{}' (was {})",
            tag.name,
            &tag.target[..tag.target.len().min(12)]
        )
    }
}

pub(super) fn format_merge_outcome(outcome: &MergeOutcome) -> Result<String, ErrCtx> {
    let mut f = String::new();
    match outcome {
        MergeOutcome::FastForward { from, to } => {
            if let Some(from) = from {
                writeln!(
                    &mut f,
                    "Fast-forward {}..{}",
                    &from[..from.len().min(12)],
                    &to[..to.len().min(12)]
                )?;
            } else {
                writeln!(&mut f, "Fast-forward to {}", &to[..to.len().min(12)])?;
            }
        }
        MergeOutcome::AlreadyUpToDate { head } => {
            writeln!(
                &mut f,
                "Already up to date at {}",
                &head[..head.len().min(12)]
            )?;
        }
        MergeOutcome::Merged {
            target, merge_base, staged, conflicted, ..
        } => {
            writeln!(&mut f, "Merged {}", &target[..target.len().min(12)])?;
            if let Some(merge_base) = merge_base {
                writeln!(
                    &mut f,
                    "Merge base {}",
                    &merge_base[..merge_base.len().min(12)]
                )?;
            }
            if !staged.is_empty() {
                writeln!(&mut f, "Staged paths:")?;
                for path in staged {
                    writeln!(&mut f, "  {path}")?;
                }
            }
            if !conflicted.is_empty() {
                writeln!(&mut f, "Unmerged paths:")?;
                for path in conflicted {
                    writeln!(&mut f, "  {path}")?;
                }
            }
            if staged.is_empty() && conflicted.is_empty() {
                writeln!(&mut f, "No changes.")?;
            }
        }
    }
    Ok(f)
}

pub(super) fn format_merge_outcome_with_row_auto_merge(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    outcome: &MergeOutcome,
    row_auto_merge: Option<&RowAutoMergeResult>,
    remote: Option<Arc<Remote>>,
) -> Result<String, ErrCtx> {
    let display_outcome = row_auto_merge
        .map(|result| merge_outcome_with_row_auto_merge(outcome, &result.key))
        .unwrap_or_else(|| outcome.clone());
    let mut f = format_merge_outcome(&display_outcome)?;
    if let Some(result) = row_auto_merge {
        append_row_auto_merge_result(&mut f, result)?;
    } else {
        append_row_merge_analysis(&mut f, runtime, file, repo, outcome, remote)?;
    }
    Ok(f)
}

pub(super) fn merge_outcome_with_row_auto_merge(outcome: &MergeOutcome, key: &str) -> MergeOutcome {
    let MergeOutcome::Merged {
        head,
        target,
        merge_base,
        staged,
        conflicted,
    } = outcome
    else {
        return outcome.clone();
    };

    let mut staged = staged.clone();
    if !staged.iter().any(|path| path == key) {
        staged.push(key.to_string());
        staged.sort();
    }
    let conflicted = conflicted
        .iter()
        .filter(|path| path.as_str() != key)
        .cloned()
        .collect();

    MergeOutcome::Merged {
        head: head.clone(),
        target: target.clone(),
        merge_base: merge_base.clone(),
        staged,
        conflicted,
    }
}

pub(super) fn append_row_auto_merge_result(
    output: &mut String,
    result: &RowAutoMergeResult,
) -> Result<(), ErrCtx> {
    if !output.ends_with('\n') {
        output.push('\n');
    }
    writeln!(output, "Row-level auto-merged {}:", result.key)?;
    writeln!(
        output,
        "  applied {} row change(s) from theirs",
        result.applied_changes
    )?;
    writeln!(output, "  ours: {} row change(s)", result.ours_changes)?;
    writeln!(output, "  theirs: {} row change(s)", result.theirs_changes)?;
    Ok(())
}

pub(super) fn format_pull_outcome(outcome: &PullOutcome) -> Result<String, ErrCtx> {
    let mut f = String::new();
    writeln!(
        &mut f,
        "Fetched {}/{} at {} ({} new commits)",
        outcome.remote,
        outcome.remote_branch,
        &outcome.head[..outcome.head.len().min(12)],
        outcome.commits
    )?;
    match &outcome.merge {
        MergeOutcome::FastForward { from, to } => {
            if let Some(from) = from {
                writeln!(
                    &mut f,
                    "Fast-forwarded {} {}..{}",
                    outcome.local_branch,
                    &from[..from.len().min(12)],
                    &to[..to.len().min(12)]
                )?;
            } else {
                writeln!(
                    &mut f,
                    "Fast-forwarded {} to {}",
                    outcome.local_branch,
                    &to[..to.len().min(12)]
                )?;
            }
        }
        MergeOutcome::AlreadyUpToDate { head } => {
            writeln!(
                &mut f,
                "{} already up to date at {}",
                outcome.local_branch,
                &head[..head.len().min(12)]
            )?;
        }
        MergeOutcome::Merged {
            target, merge_base, staged, conflicted, ..
        } => {
            writeln!(
                &mut f,
                "Merged {}/{} ({}) into {}",
                outcome.remote,
                outcome.remote_branch,
                &target[..target.len().min(12)],
                outcome.local_branch
            )?;
            if let Some(merge_base) = merge_base {
                writeln!(
                    &mut f,
                    "Merge base {}",
                    &merge_base[..merge_base.len().min(12)]
                )?;
            }
            if !staged.is_empty() {
                writeln!(&mut f, "Staged paths:")?;
                for path in staged {
                    writeln!(&mut f, "  {path}")?;
                }
            }
            if !conflicted.is_empty() {
                writeln!(&mut f, "Unmerged paths:")?;
                for path in conflicted {
                    writeln!(&mut f, "  {path}")?;
                }
            }
            if conflicted.is_empty() {
                writeln!(&mut f, "Commit to complete the merge.")?;
            }
        }
    }
    Ok(f)
}

pub(super) fn format_pull_outcome_with_row_analysis(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    outcome: &PullOutcome,
    remote: Option<Arc<Remote>>,
) -> Result<String, ErrCtx> {
    let mut f = format_pull_outcome(outcome)?;
    append_row_merge_analysis(&mut f, runtime, file, repo, &outcome.merge, remote)?;
    Ok(f)
}
