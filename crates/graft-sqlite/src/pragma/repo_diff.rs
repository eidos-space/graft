use super::*;

pub(super) fn repo_diff_for_spec(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    spec: RepoDiffSpec,
) -> Result<RepoDiff, ErrCtx> {
    let kind = spec.kind;
    let mut diff = match spec.target {
        RepoDiffTarget::Worktree { path } => {
            let path = repo_diff_path(repo, path.as_deref())?;
            let current_key = repo.file_key(&file.tag)?;
            if path.is_none() {
                repo_worktree_diff_for_filter(runtime, file, repo, None, "")
            } else if let Some(path) = path.as_deref()
                && path != current_key
            {
                let (key, physical_path) = repo_physical_path_arg(repo, Path::new(path))?;
                match std::fs::symlink_metadata(&physical_path) {
                    Ok(metadata) if metadata.file_type().is_dir() => {
                        repo_worktree_diff_for_filter(runtime, file, repo, None, &key)
                    }
                    Ok(metadata) if !metadata.file_type().is_file() => Err(ErrCtx::PragmaErr(
                        format!("path `{}` is not a regular file", physical_path.display()).into(),
                    )),
                    Ok(_) if is_sqlite_database_path(&physical_path)? => {
                        let state = import_physical_sqlite_file_state(runtime, &physical_path)?;
                        let expected = repo.index_files()?.get(&key).cloned();
                        let state = if let Some(expected) = expected
                            && repo_file_state_content_eq(runtime, &state, &expected)?
                        {
                            expected
                        } else {
                            state
                        };
                        Ok(repo.diff_worktree_file(&physical_path, state, Some(&key))?)
                    }
                    Ok(_) => Ok(repo.diff_worktree_artifact(&physical_path, Some(&key))?),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        if repo.index_artifact(&physical_path)?.is_some()
                            || repo.head_artifact(&physical_path)?.is_some()
                        {
                            Ok(repo.diff_worktree_artifact_removal(&physical_path, Some(&key))?)
                        } else {
                            Ok(repo.diff_worktree_file_removal(&physical_path, Some(&key))?)
                        }
                    }
                    Err(err) => Err(err.into()),
                }
            } else {
                let state = current_repo_file_state(runtime, file)?;
                Ok(repo.diff_worktree_file(&file.tag, state, path.as_deref())?)
            }
        }
        RepoDiffTarget::Staged { path } => {
            let path = repo_diff_path(repo, path.as_deref())?;
            Ok(repo.diff_staged(path.as_deref())?)
        }
        RepoDiffTarget::RevisionToWorktree { rev, path } => {
            let path = repo_diff_path(repo, path.as_deref())?;
            let current_key = repo.file_key(&file.tag)?;
            if path.is_none() {
                repo_worktree_diff_for_filter(runtime, file, repo, Some(&rev), "")
            } else if let Some(path) = path.as_deref()
                && path != current_key
            {
                let (key, physical_path) = repo_physical_path_arg(repo, Path::new(path))?;
                match std::fs::symlink_metadata(&physical_path) {
                    Ok(metadata) if metadata.file_type().is_dir() => {
                        repo_worktree_diff_for_filter(runtime, file, repo, Some(&rev), &key)
                    }
                    Ok(metadata) if !metadata.file_type().is_file() => Err(ErrCtx::PragmaErr(
                        format!("path `{}` is not a regular file", physical_path.display()).into(),
                    )),
                    Ok(_) if is_sqlite_database_path(&physical_path)? => {
                        let state = import_physical_sqlite_file_state(runtime, &physical_path)?;
                        let from_id = repo.resolve_revision(&rev)?;
                        let expected = repo.read_commit(&from_id)?.files.get(&key).cloned();
                        let state = if let Some(expected) = expected
                            && repo_file_state_content_eq(runtime, &state, &expected)?
                        {
                            expected
                        } else {
                            state
                        };
                        Ok(repo.diff_revision_to_worktree_file(
                            &rev,
                            &physical_path,
                            state,
                            Some(&key),
                        )?)
                    }
                    Ok(_) => Ok(repo.diff_revision_to_worktree_artifact(
                        &rev,
                        &physical_path,
                        Some(&key),
                    )?),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        if repo.artifact_from_revision(&rev, &physical_path)?.is_some() {
                            Ok(repo.diff_revision_to_worktree_artifact_removal(
                                &rev,
                                &physical_path,
                                Some(&key),
                            )?)
                        } else {
                            Ok(repo.diff_revision_to_worktree_file_removal(
                                &rev,
                                &physical_path,
                                Some(&key),
                            )?)
                        }
                    }
                    Err(err) => Err(err.into()),
                }
            } else {
                let state = current_repo_file_state(runtime, file)?;
                Ok(repo.diff_revision_to_worktree_file(&rev, &file.tag, state, path.as_deref())?)
            }
        }
        RepoDiffTarget::Revisions { from, to, path } => {
            let path = repo_diff_path(repo, path.as_deref())?;
            Ok(repo.diff_revisions(&from, &to, path.as_deref())?)
        }
    }?;
    filter_repo_diff_by_kind(&mut diff, kind);
    Ok(diff)
}

pub(super) fn filter_repo_diff_by_kind(diff: &mut RepoDiff, kind: Option<RepoTrackedPathKind>) {
    let Some(kind) = kind else {
        return;
    };
    diff.files.retain(|file| file.kind == kind);
    diff.artifacts.retain(|artifact| artifact.kind == kind);
    diff.refresh_paths();
}

pub(super) fn repo_worktree_diff_for_filter(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    rev: Option<&str>,
    filter: &str,
) -> Result<RepoDiff, ErrCtx> {
    let from = if let Some(rev) = rev {
        repo.resolve_revision(rev)?
    } else {
        "index".to_string()
    };
    let mut diff = RepoDiff {
        from: from.clone(),
        to: "worktree".to_string(),
        paths: Vec::new(),
        files: Vec::new(),
        artifacts: Vec::new(),
    };
    let current_key = repo.file_key(&file.tag)?;
    let index_files = current_repo_files_for_checkout(repo)?;
    let index_artifacts = current_repo_artifacts_for_checkout(repo)?;
    let mut file_keys = BTreeSet::new();
    let mut artifact_keys = BTreeSet::new();

    if let Some(_) = rev {
        let commit = repo.read_commit(&from)?;
        file_keys.extend(
            commit
                .files
                .keys()
                .filter(|key| repo_key_matches_filter(key, filter))
                .cloned(),
        );
        artifact_keys.extend(
            commit
                .artifacts
                .keys()
                .filter(|key| repo_key_matches_filter(key, filter))
                .cloned(),
        );
    }
    file_keys.extend(
        index_files
            .keys()
            .filter(|key| repo_key_matches_filter(key, filter))
            .cloned(),
    );
    artifact_keys.extend(
        index_artifacts
            .keys()
            .filter(|key| repo_key_matches_filter(key, filter))
            .cloned(),
    );
    if !current_key.starts_with(".graft/") && repo_key_matches_filter(&current_key, filter) {
        file_keys.insert(current_key.clone());
    }

    for key in file_keys {
        let physical_path = repo.worktree().join(&key);
        let path_diff = if key == current_key {
            let state = current_repo_file_state(runtime, file)?;
            if let Some(rev) = rev {
                repo.diff_revision_to_worktree_file(rev, &file.tag, state, Some(&key))?
            } else {
                repo.diff_worktree_file(&file.tag, state, Some(&key))?
            }
        } else {
            match std::fs::symlink_metadata(&physical_path) {
                Ok(metadata) if metadata.file_type().is_file() => {
                    if !is_sqlite_database_path(&physical_path)? {
                        continue;
                    }
                    let state = import_physical_sqlite_file_state(runtime, &physical_path)?;
                    let expected = if let Some(rev) = rev {
                        let from_id = repo.resolve_revision(rev)?;
                        repo.read_commit(&from_id)?.files.get(&key).cloned()
                    } else {
                        index_files.get(&key).cloned()
                    };
                    let state = if let Some(expected) = expected
                        && repo_file_state_content_eq(runtime, &state, &expected)?
                    {
                        expected
                    } else {
                        state
                    };
                    if let Some(rev) = rev {
                        repo.diff_revision_to_worktree_file(rev, &physical_path, state, Some(&key))?
                    } else {
                        repo.diff_worktree_file(&physical_path, state, Some(&key))?
                    }
                }
                Ok(_) => continue,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    if let Some(rev) = rev {
                        repo.diff_revision_to_worktree_file_removal(
                            rev,
                            &physical_path,
                            Some(&key),
                        )?
                    } else {
                        repo.diff_worktree_file_removal(&physical_path, Some(&key))?
                    }
                }
                Err(err) => return Err(err.into()),
            }
        };
        append_repo_diff(&mut diff, path_diff);
    }

    for key in artifact_keys {
        let physical_path = repo.worktree().join(&key);
        let path_diff = match std::fs::symlink_metadata(&physical_path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                if let Some(rev) = rev {
                    repo.diff_revision_to_worktree_artifact(rev, &physical_path, Some(&key))?
                } else {
                    repo.diff_worktree_artifact(&physical_path, Some(&key))?
                }
            }
            Ok(_) => continue,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if let Some(rev) = rev {
                    repo.diff_revision_to_worktree_artifact_removal(
                        rev,
                        &physical_path,
                        Some(&key),
                    )?
                } else {
                    repo.diff_worktree_artifact_removal(&physical_path, Some(&key))?
                }
            }
            Err(err) => return Err(err.into()),
        };
        append_repo_diff(&mut diff, path_diff);
    }

    Ok(diff)
}

pub(super) fn append_repo_diff(target: &mut RepoDiff, mut source: RepoDiff) {
    target.files.append(&mut source.files);
    target.artifacts.append(&mut source.artifacts);
    target.refresh_paths();
}

pub(super) fn repo_key_matches_filter(key: &str, filter: &str) -> bool {
    filter.is_empty()
        || key == filter
        || key
            .strip_prefix(filter)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

pub(super) fn repo_status_for_file(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
) -> Result<RepoStatus, ErrCtx> {
    let mut status = repo.status()?;
    let current_key = repo.file_key(&file.tag)?;
    let tracked = match repo.index_files() {
        Ok(tracked) => tracked,
        Err(graft::repo::RepoErr::UnresolvedConflicts) => return Ok(status),
        Err(err) => return Err(err.into()),
    };
    let tracked_artifacts = match repo.index_artifacts() {
        Ok(tracked) => tracked,
        Err(graft::repo::RepoErr::UnresolvedConflicts) => return Ok(status),
        Err(err) => return Err(err.into()),
    };
    for change in &mut status.unstaged_changes {
        if change.path == current_key || repo_key_volume_id(runtime, repo, &change.path)?.is_some()
        {
            change.kind = RepoTrackedPathKind::SqliteDatabase;
            change.storage = RepoPathStorage::SqliteSnapshot;
        }
    }
    let current_staged_deleted = status.staged_changes.iter().any(|change| {
        change.path == current_key && change.change == graft::repo::RepoFileChange::Deleted
    });
    status.unstaged_changes.retain(|change| {
        if change.path == current_key
            && change.change == RepoWorktreeChangeKind::Untracked
            && current_staged_deleted
        {
            return false;
        }

        change.path == current_key
            || tracked.contains_key(&change.path)
            || tracked_artifacts.contains_key(&change.path)
            || change.change == RepoWorktreeChangeKind::Untracked
    });
    for (key, expected_state) in tracked {
        if key == current_key
            || status
                .unstaged_changes
                .iter()
                .any(|change| change.path == key)
        {
            continue;
        }

        if let Some(state) = repo_file_state_for_key(runtime, repo, &key)? {
            if !repo_file_state_content_eq(runtime, &state, &expected_state)? {
                status
                    .unstaged_changes
                    .push(graft::repo::RepoWorktreeChange {
                        path: key,
                        change: RepoWorktreeChangeKind::Modified,
                        kind: RepoTrackedPathKind::SqliteDatabase,
                        storage: RepoPathStorage::SqliteSnapshot,
                    });
            }
            continue;
        }

        let physical_path = repo.worktree().join(&key);
        let change = match std::fs::symlink_metadata(&physical_path) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() {
                    continue;
                }
                if !is_sqlite_database_path(&physical_path)? {
                    continue;
                }
                let state = import_physical_sqlite_file_state(runtime, &physical_path)?;
                if repo_file_state_content_eq(runtime, &state, &expected_state)? {
                    None
                } else {
                    Some(RepoWorktreeChangeKind::Modified)
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Some(RepoWorktreeChangeKind::Deleted)
            }
            Err(err) => return Err(err.into()),
        };

        if let Some(change) = change {
            status
                .unstaged_changes
                .push(graft::repo::RepoWorktreeChange {
                    path: key,
                    change,
                    kind: RepoTrackedPathKind::SqliteDatabase,
                    storage: RepoPathStorage::SqliteSnapshot,
                });
        }
    }
    status.unstaged_changes.sort_by(|a, b| a.path.cmp(&b.path));
    status.unstaged = status
        .unstaged_changes
        .iter()
        .map(|change| change.path.clone())
        .collect();
    status.refresh_summary_flags();
    Ok(status)
}

pub(super) fn repo_has_work_in_progress_for_file(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
) -> Result<bool, ErrCtx> {
    let status = repo_status_for_file(runtime, file, repo)?;
    let current_key = repo.file_key(&file.tag)?;
    let has_blocking_unstaged = status.unstaged_changes.iter().any(|change| {
        change.change != RepoWorktreeChangeKind::Untracked || change.path == current_key
    });
    Ok(has_blocking_unstaged
        || !status.staged.is_empty()
        || !status.conflicted.is_empty()
        || status.merge_head.is_some())
}

pub(super) fn ensure_checkout_plan_preserves_untracked_paths(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    plan: &CheckoutPlan,
) -> Result<(), ErrCtx> {
    let keys = plan
        .files
        .keys()
        .chain(plan.artifacts.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    ensure_checkout_keys_preserve_untracked_paths(runtime, file, repo, &keys)
}

pub(super) fn ensure_checkout_key_preserves_untracked_path(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    key: &str,
) -> Result<(), ErrCtx> {
    let keys = BTreeSet::from([key.to_string()]);
    ensure_checkout_keys_preserve_untracked_paths(runtime, file, repo, &keys)
}

pub(super) fn ensure_checkout_keys_preserve_untracked_paths(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    keys: &BTreeSet<String>,
) -> Result<(), ErrCtx> {
    if keys.is_empty() {
        return Ok(());
    }
    let status = repo_status_for_file(runtime, file, repo)?;
    let current_key = repo.file_key(&file.tag)?;
    let overwritten = status
        .unstaged_changes
        .iter()
        .filter(|change| {
            change.change == RepoWorktreeChangeKind::Untracked
                && change.path != current_key
                && keys.contains(&change.path)
        })
        .map(|change| change.path.clone())
        .collect::<Vec<_>>();

    if overwritten.is_empty() {
        return Ok(());
    }

    pragma_err!(format!(
        "cannot checkout because untracked paths would be overwritten: {}",
        overwritten.join(", ")
    ))
}

pub(super) fn repo_file_state_content_eq(
    runtime: &Runtime,
    left: &CommitFileState,
    right: &CommitFileState,
) -> Result<bool, ErrCtx> {
    if left.snapshot.page_count != right.snapshot.page_count {
        return Ok(false);
    }
    let left_snapshot = left.snapshot.to_snapshot();
    let right_snapshot = right.snapshot.to_snapshot();
    Ok(runtime.snapshot_checksum(&left_snapshot)? == runtime.snapshot_checksum(&right_snapshot)?)
}

pub(super) fn staged_commit_table_summary(
    runtime: &Runtime,
    repo: &Repository,
) -> Result<Vec<CommitTableSummary>, ErrCtx> {
    let diff = repo.diff_staged(None)?;
    let mut by_name = BTreeMap::<String, CommitTableSummary>::new();
    for file in &diff.files {
        let summaries = repo_file_table_summary(runtime, file)?;
        for summary in summaries {
            merge_table_summary(&mut by_name, summary);
        }
    }
    Ok(by_name.into_values().collect())
}

pub(super) fn repo_file_table_summary(
    runtime: &Runtime,
    file: &graft::repo::RepoFileDiff,
) -> Result<Vec<CommitTableSummary>, ErrCtx> {
    match (&file.from, &file.to) {
        (Some(from), Some(to)) => {
            let from_snapshot = from.snapshot.to_snapshot();
            let to_snapshot = to.snapshot.to_snapshot();
            if from_snapshot.is_empty() {
                return snapshot_table_summary(
                    runtime,
                    &to_snapshot,
                    SnapshotSummaryMode::Inserted,
                );
            }
            if to_snapshot.is_empty() {
                return snapshot_table_summary(
                    runtime,
                    &from_snapshot,
                    SnapshotSummaryMode::Deleted,
                );
            }
            let diff = crate::row_level_diff::row_level_diff_snapshots(
                runtime,
                &from_snapshot,
                &to_snapshot,
            )
            .map_err(|e| ErrCtx::PragmaErr(format!("Diff error: {e:?}").into()))?;
            Ok(diff
                .table_changes
                .iter()
                .filter_map(|table| {
                    let (inserts, deletes, updates) = count_changes_json(&table.changes);
                    table_summary(table.table_name.clone(), inserts, deletes, updates)
                })
                .collect())
        }
        (None, Some(to)) => snapshot_table_summary(
            runtime,
            &to.snapshot.to_snapshot(),
            SnapshotSummaryMode::Inserted,
        ),
        (Some(from), None) => snapshot_table_summary(
            runtime,
            &from.snapshot.to_snapshot(),
            SnapshotSummaryMode::Deleted,
        ),
        (None, None) => Ok(Vec::new()),
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum SnapshotSummaryMode {
    Inserted,
    Deleted,
}

pub(super) fn snapshot_table_summary(
    runtime: &Runtime,
    snapshot: &graft::snapshot::Snapshot,
    mode: SnapshotSummaryMode,
) -> Result<Vec<CommitTableSummary>, ErrCtx> {
    if snapshot.is_empty() {
        return Ok(Vec::new());
    }

    let volume = runtime.volume_from_snapshot(snapshot)?;
    let vid = volume.vid.clone();
    let result = snapshot_table_summary_checked_out(runtime, &vid, mode);
    let _ = runtime.volume_delete(&vid);
    result
}

pub(super) fn snapshot_table_summary_checked_out(
    runtime: &Runtime,
    vid: &VolumeId,
    mode: SnapshotSummaryMode,
) -> Result<Vec<CommitTableSummary>, ErrCtx> {
    let reader = runtime.volume_reader(vid.clone())?;
    let scanner = crate::sqlite_parse::TableScanner::new(&reader)
        .map_err(|e| ErrCtx::PragmaErr(format!("Parse error: {e:?}").into()))?;
    let master = scanner
        .read_master_table()
        .map_err(|e| ErrCtx::PragmaErr(format!("Schema error: {e:?}").into()))?;
    let mut summaries = Vec::new();
    let ignored_tables = crate::row_level_diff::ignored_row_diff_tables(&master, &[]);

    for entry in master {
        if !crate::row_level_diff::is_diffable_table(&entry, &ignored_tables) {
            continue;
        }
        let row_count = crate::sqlite_parse::read_all_rows(&reader, entry.root_page)
            .map_err(|e| ErrCtx::PragmaErr(format!("Table read error: {e:?}").into()))?
            .len();
        let summary = match mode {
            SnapshotSummaryMode::Inserted => table_summary(entry.name, row_count, 0, 0),
            SnapshotSummaryMode::Deleted => table_summary(entry.name, 0, row_count, 0),
        };
        if let Some(summary) = summary {
            summaries.push(summary);
        }
    }

    Ok(summaries)
}

pub(super) fn table_summary(
    name: String,
    inserts: usize,
    deletes: usize,
    updates: usize,
) -> Option<CommitTableSummary> {
    if name.is_empty() || inserts + deletes + updates == 0 {
        None
    } else {
        Some(CommitTableSummary { name, inserts, deletes, updates })
    }
}

pub(super) fn merge_table_summary(
    by_name: &mut BTreeMap<String, CommitTableSummary>,
    summary: CommitTableSummary,
) {
    by_name
        .entry(summary.name.clone())
        .and_modify(|entry| {
            entry.inserts += summary.inserts;
            entry.deletes += summary.deletes;
            entry.updates += summary.updates;
        })
        .or_insert(summary);
}

pub(super) fn is_sqlite_database_path(path: &Path) -> Result<bool, ErrCtx> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    let mut magic = [0_u8; SQLITE_DATABASE_MAGIC.len()];
    match file.read_exact(&mut magic) {
        Ok(()) => Ok(&magic == SQLITE_DATABASE_MAGIC),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(err.into()),
    }
}

pub(super) fn is_sqlite_sidecar_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.ends_with("-wal") || name.ends_with("-shm") || name.ends_with("-journal")
        })
}

pub(super) fn current_repo_file_state(
    runtime: &Runtime,
    file: &VolFile,
) -> Result<CommitFileState, ErrCtx> {
    let snapshot = file.snapshot_or_latest()?;
    Ok(CommitFileState {
        volume: file.vid.clone(),
        snapshot: repo_snapshot_with_commit_hashes(runtime, &snapshot)?,
    })
}

pub(super) fn repo_file_state_for_key(
    runtime: &Runtime,
    repo: &Repository,
    key: &str,
) -> Result<Option<CommitFileState>, ErrCtx> {
    let Some(volume) = repo_key_volume_id(runtime, repo, key)? else {
        return Ok(None);
    };
    let snapshot = runtime.volume_snapshot(&volume)?;
    Ok(Some(CommitFileState {
        volume,
        snapshot: repo_snapshot_with_commit_hashes(runtime, &snapshot)?,
    }))
}

pub(super) fn repo_key_volume_id(
    runtime: &Runtime,
    repo: &Repository,
    key: &str,
) -> Result<Option<VolumeId>, ErrCtx> {
    let tag = repo.worktree().join(key);
    Ok(runtime.tag_get(&tag.to_string_lossy())?)
}

pub(super) fn repo_snapshot_with_commit_hashes(
    runtime: &Runtime,
    snapshot: &graft::snapshot::Snapshot,
) -> Result<RepoSnapshot, ErrCtx> {
    let mut ranges = Vec::new();
    for range in snapshot.iter() {
        let mut commits = Vec::new();
        for lsn in range.lsns.iter() {
            let commit_hash =
                repo_storage_commit_hash(runtime, &range.log, lsn)?.ok_or_else(|| {
                    ErrCtx::PragmaErr(
                        format!(
                            "snapshot references missing storage commit {:?}/{}",
                            range.log, lsn
                        )
                        .into(),
                    )
                })?;
            commits.push(RepoStorageCommit { lsn, commit_hash });
        }
        ranges.push(RepoLogRange {
            log: range.log.clone(),
            start: *range.lsns.start(),
            end: *range.lsns.end(),
            commits,
        });
    }
    Ok(RepoSnapshot { page_count: snapshot.page_count, ranges })
}
