use super::*;

pub(super) fn run_repo_add(
    runtime: &Runtime,
    file: &mut VolFile,
    spec: &RepoAddSpec,
) -> Result<Vec<graft::repo::index::IndexEntry>, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot add while there is an open transaction");
    }
    let repo = repo_for_file(file)?;
    if spec.all {
        stage_repo_add_all(runtime, file, &repo, spec.kind)
    } else if let Some(path) = spec.path.as_deref() {
        stage_repo_add_path(runtime, file, &repo, path, spec.force)
    } else {
        let state = current_repo_file_state(runtime, file)?;
        Ok(vec![repo.stage_file_state_path(&file.tag, state)?])
    }
}

pub(super) fn json_staged_entry_paths(
    repo: &Repository,
    entries: &[graft::repo::index::IndexEntry],
) -> Result<Vec<crate::json::JsonRepoPathDiff>, ErrCtx> {
    let head = repo_head_commit(repo)?;
    entries
        .iter()
        .map(|entry| {
            let (kind, storage, change) =
                staged_entry_kind_storage_and_change(head.as_ref(), entry);
            Ok(crate::json::JsonRepoPathDiff {
                path: entry.path.clone(),
                change: repo_file_change_label(change).to_string(),
                kind: repo_tracked_path_kind_json_label(kind).to_string(),
                storage: repo_path_storage_json_label(storage).to_string(),
            })
        })
        .collect()
}

pub(super) fn filter_tracked_paths_by_kind(
    paths: Vec<RepoTrackedPath>,
    kind: Option<RepoTrackedPathKind>,
) -> Vec<RepoTrackedPath> {
    match kind {
        Some(kind) => paths.into_iter().filter(|path| path.kind == kind).collect(),
        None => paths,
    }
}

pub(super) fn filter_tracked_path_details_by_kind(
    paths: Vec<RepoTrackedPathDetail>,
    kind: Option<RepoTrackedPathKind>,
) -> Vec<RepoTrackedPathDetail> {
    match kind {
        Some(kind) => paths.into_iter().filter(|path| path.kind == kind).collect(),
        None => paths,
    }
}

pub(super) fn filter_tracked_path_entries_by_kind(
    paths: Vec<RepoTrackedPathEntry>,
    kind: Option<RepoTrackedPathKind>,
) -> Vec<RepoTrackedPathEntry> {
    match kind {
        Some(kind) => paths.into_iter().filter(|path| path.kind == kind).collect(),
        None => paths,
    }
}

pub(super) fn filter_repo_status_by_kind(
    mut status: RepoStatus,
    kind: Option<RepoTrackedPathKind>,
) -> RepoStatus {
    let Some(kind) = kind else {
        return status;
    };
    status.unstaged_changes.retain(|change| change.kind == kind);
    status.staged_changes.retain(|change| change.kind == kind);
    status
        .conflicted_changes
        .retain(|change| change.kind == kind);
    status.unstaged = status
        .unstaged_changes
        .iter()
        .map(|change| change.path.clone())
        .collect();
    status.staged = status
        .staged_changes
        .iter()
        .map(|change| change.path.clone())
        .collect();
    status.conflicted = status
        .conflicted_changes
        .iter()
        .map(|change| change.path.clone())
        .collect();
    status.refresh_summary_flags();
    status
}

pub(super) fn repo_head_commit(repo: &Repository) -> Result<Option<CommitObject>, ErrCtx> {
    match repo.show_revision("HEAD") {
        Ok(commit) => Ok(Some(commit)),
        Err(graft::repo::RepoErr::UnbornHead) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

pub(super) fn repo_head_and_branch(
    repo: &Repository,
) -> Result<(Option<String>, Option<String>), ErrCtx> {
    let status = repo.status()?;
    let branch = repo.current_branch()?;
    Ok((status.head_target, branch))
}

pub(super) fn staged_entry_kind_storage_and_change(
    head: Option<&CommitObject>,
    entry: &graft::repo::index::IndexEntry,
) -> (RepoTrackedPathKind, RepoPathStorage, RepoFileChange) {
    if entry.file.is_some() {
        let change = if head.is_some_and(|commit| commit.files.contains_key(&entry.path)) {
            RepoFileChange::Modified
        } else {
            RepoFileChange::Added
        };
        return (
            RepoTrackedPathKind::SqliteDatabase,
            RepoPathStorage::SqliteSnapshot,
            change,
        );
    }

    if let Some(artifact) = &entry.artifact {
        let change = if head.is_some_and(|commit| commit.artifacts.contains_key(&entry.path)) {
            RepoFileChange::Modified
        } else {
            RepoFileChange::Added
        };
        return (
            artifact_checkout_path_kind(artifact),
            artifact_checkout_path_storage(artifact),
            change,
        );
    }

    if head.is_some_and(|commit| commit.files.contains_key(&entry.path)) {
        return (
            RepoTrackedPathKind::SqliteDatabase,
            RepoPathStorage::SqliteSnapshot,
            RepoFileChange::Deleted,
        );
    }

    if let Some(artifact) = head.and_then(|commit| commit.artifacts.get(&entry.path)) {
        return (
            artifact_checkout_path_kind(artifact),
            artifact_checkout_path_storage(artifact),
            RepoFileChange::Deleted,
        );
    }

    (
        RepoTrackedPathKind::BinaryFile,
        RepoPathStorage::Inline,
        RepoFileChange::Deleted,
    )
}

pub(super) fn stage_repo_add_path(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    path: &Path,
    force: bool,
) -> Result<Vec<graft::repo::index::IndexEntry>, ErrCtx> {
    if !path.is_absolute()
        && let Some(path) = path.to_str()
    {
        graft::repo::validate_repo_path_identity(path)?;
    }
    let physical_path = repo_input_path(repo, path);
    let metadata = match std::fs::metadata(&physical_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let (key, _) = repo_restore_path_arg(repo, path)?;
            let tracked = tracked_repo_keys_under_directory(repo, &key)?;
            if tracked.is_empty() {
                return Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotTracked(key)));
            }
            let mut entries = Vec::with_capacity(tracked.len());
            for tracked_key in tracked {
                if let Some(entry) = stage_repo_add_deletion(repo, &tracked_key)? {
                    entries.push(entry);
                }
            }
            return Ok(entries);
        }
        Err(err) => return Err(err.into()),
    };
    let current_key = repo.file_key(&file.tag)?;

    if metadata.is_dir() {
        let directory = std::fs::canonicalize(&physical_path)?;
        if !directory.starts_with(repo.worktree()) {
            return Err(ErrCtx::Repo(graft::repo::RepoErr::PathOutsideWorktree {
                path: directory,
                worktree: repo.worktree().to_path_buf(),
            }));
        }
        if !force && repo.is_ignored_worktree_path(&directory)? {
            return ignored_add_path_error(repo, &directory);
        }

        let directory_key = repo_directory_key(repo, &directory)?;
        if !force {
            let status = repo_status_for_file(runtime, file, repo)?;
            let changes = status
                .unstaged_changes
                .into_iter()
                .filter(|change| repo_key_is_under_directory(&change.path, &directory_key))
                .collect();
            return stage_repo_add_changes(runtime, file, repo, changes);
        }

        let mut paths = BTreeSet::new();
        collect_repo_add_directory_files(repo, &directory, force, &mut paths)?;
        let tracked = tracked_repo_keys_under_directory(repo, &directory_key)?;
        let mut entries = Vec::with_capacity(paths.len() + tracked.len());
        for key in tracked {
            if !repo.worktree().join(&key).is_file()
                && let Some(entry) = stage_repo_add_deletion(repo, &key)?
            {
                entries.push(entry);
            }
        }
        for key in paths {
            let physical_path = repo.worktree().join(&key);
            entries.extend(stage_repo_add_topology_removals(repo, &key)?);
            entries.push(stage_repo_add_file(
                runtime,
                file,
                repo,
                &current_key,
                &key,
                &physical_path,
            )?);
        }
        return Ok(entries);
    }

    if !metadata.is_file() {
        return Err(ErrCtx::PragmaErr(
            format!(
                "path `{}` is not a regular file or directory",
                physical_path.display()
            )
            .into(),
        ));
    }

    let (key, physical_path) = repo_physical_path_arg(repo, path)?;
    if !force && repo.is_ignored_worktree_path(&physical_path)? {
        return ignored_add_path_error(repo, &physical_path);
    }
    let mut entries = stage_repo_add_topology_removals(repo, &key)?;
    entries.push(stage_repo_add_file(
        runtime,
        file,
        repo,
        &current_key,
        &key,
        &physical_path,
    )?);
    Ok(entries)
}

pub(super) fn stage_repo_add_topology_removals(
    repo: &Repository,
    key: &str,
) -> Result<Vec<graft::repo::index::IndexEntry>, ErrCtx> {
    let mut effective_keys = BTreeSet::new();
    effective_keys.extend(repo.index_files()?.into_keys());
    effective_keys.extend(repo.index_artifacts()?.into_keys());
    let head = repo_head_commit(repo)?;
    let mut removals = Vec::new();

    for conflict in effective_keys.into_iter().filter(|candidate| {
        candidate != key
            && (repo_key_is_under_directory(candidate, key)
                || repo_key_is_under_directory(key, candidate))
    }) {
        let tracked_at_head = head.as_ref().is_some_and(|commit| {
            commit.files.contains_key(&conflict) || commit.artifacts.contains_key(&conflict)
        });
        if tracked_at_head {
            removals.push(repo.stage_file_removal_key(conflict)?);
        } else if repo.index_has_key(&conflict)? {
            repo.restore_index_key_from_head(conflict)?;
        }
    }

    Ok(removals)
}

pub(super) fn stage_repo_add_all(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    kind: Option<RepoTrackedPathKind>,
) -> Result<Vec<graft::repo::index::IndexEntry>, ErrCtx> {
    let status = repo_status_for_file(runtime, file, repo)?;
    let changes = status
        .unstaged_changes
        .into_iter()
        .filter(|change| kind.is_none_or(|kind| change.kind == kind))
        .collect();
    stage_repo_add_changes(runtime, file, repo, changes)
}

pub(super) fn stage_repo_add_changes(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    changes: Vec<graft::repo::RepoWorktreeChange>,
) -> Result<Vec<graft::repo::index::IndexEntry>, ErrCtx> {
    let current_key = repo.file_key(&file.tag)?;
    let mut entries = Vec::with_capacity(changes.len());

    for change in changes {
        match change.change {
            RepoWorktreeChangeKind::Modified | RepoWorktreeChangeKind::Untracked => {
                let physical_path = repo.worktree().join(&change.path);
                entries.push(stage_repo_add_file(
                    runtime,
                    file,
                    repo,
                    &current_key,
                    &change.path,
                    &physical_path,
                )?);
            }
            RepoWorktreeChangeKind::Deleted => {
                let entry = stage_repo_add_deletion(repo, &change.path)?;
                if change.path == current_key {
                    let volume = runtime.volume_open(None, None, None)?;
                    file.switch_volume(&volume.vid)?;
                }
                if let Some(entry) = entry {
                    entries.push(entry);
                }
            }
        }
    }

    Ok(entries)
}

pub(super) fn stage_repo_add_deletion(
    repo: &Repository,
    key: &str,
) -> Result<Option<graft::repo::index::IndexEntry>, ErrCtx> {
    let head = repo_head_commit(repo)?;
    if head
        .as_ref()
        .is_some_and(|commit| commit.files.contains_key(key) || commit.artifacts.contains_key(key))
    {
        return Ok(Some(repo.stage_file_removal_key(key)?));
    }
    if repo.index_has_key(key)? {
        repo.restore_index_key_from_head(key)?;
        repo.clear_dirty_key(key)?;
        return Ok(None);
    }
    Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotTracked(
        key.to_string(),
    )))
}

pub(super) fn stage_repo_add_file(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    current_key: &str,
    key: &str,
    physical_path: &Path,
) -> Result<graft::repo::index::IndexEntry, ErrCtx> {
    if key == current_key {
        let state = current_repo_file_state(runtime, file)?;
        repo.stage_file_state_path(&file.tag, state)
            .map_err(Into::into)
    } else if let Some(state) = repo_file_state_for_key(runtime, repo, key)? {
        repo.stage_file_state_path(repo.worktree().join(key), state)
            .map_err(Into::into)
    } else if is_sqlite_database_path(physical_path)? {
        stage_physical_sqlite_file(runtime, repo, key, physical_path)
    } else {
        repo.stage_artifact_path(physical_path).map_err(Into::into)
    }
}

pub(super) fn collect_repo_add_directory_files(
    repo: &Repository,
    dir: &Path,
    force: bool,
    out: &mut BTreeSet<String>,
) -> Result<(), ErrCtx> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if entry.file_name() == graft::repo::GRAFT_DIR {
                continue;
            }
            if !force && repo.is_ignored_worktree_path(&path)? {
                continue;
            }
            collect_repo_add_directory_files(repo, &path, force, out)?;
        } else if file_type.is_file()
            && (force || !repo.is_ignored_worktree_path(&path)?)
            && !is_sqlite_sidecar_path(&path)
        {
            out.insert(repo.file_key(&path)?);
        }
    }
    Ok(())
}

pub(super) fn ignored_add_path_error<T>(repo: &Repository, path: &Path) -> Result<T, ErrCtx> {
    let key = repo.file_key(path)?;
    Err(ErrCtx::PragmaErr(
        format!("path `{key}` is ignored; use `--force` to add it").into(),
    ))
}

pub(super) fn format_added_entries(entries: &[graft::repo::index::IndexEntry]) -> String {
    match entries {
        [entry] => format!("Added {}", entry.path),
        entries => {
            let mut output = format!("Added {} paths", entries.len());
            for entry in entries {
                output.push_str("\n  ");
                output.push_str(&entry.path);
            }
            output
        }
    }
}

pub(super) fn run_repo_remove(
    runtime: &Runtime,
    file: &mut VolFile,
    spec: &RepoRemoveSpec,
) -> Result<Vec<JsonPathAction>, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot remove while there is an open transaction");
    }
    let repo = repo_for_file(file)?;
    if let Some(path) = spec.path.as_deref() {
        stage_repo_remove_path(runtime, file, &repo, path, spec.cached)
    } else {
        let key = repo.file_key(&file.tag)?;
        Ok(vec![stage_repo_remove_key(
            runtime,
            file,
            &repo,
            &key,
            spec.cached,
        )?])
    }
}

pub(super) fn stage_repo_remove_path(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    path: &Path,
    cached: bool,
) -> Result<Vec<JsonPathAction>, ErrCtx> {
    let physical_path = repo_input_path(repo, path);
    match std::fs::symlink_metadata(&physical_path) {
        Ok(metadata) if metadata.is_dir() => {
            let directory_key = repo_directory_key(repo, &physical_path)?;
            let removed = stage_repo_remove_directory(runtime, file, repo, &directory_key, cached)?;
            if removed.is_empty() {
                Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotTracked(
                    if directory_key.is_empty() {
                        ".".to_string()
                    } else {
                        directory_key
                    },
                )))
            } else {
                Ok(removed)
            }
        }
        Ok(metadata) if metadata.is_file() => {
            let (key, _) = repo_physical_path_arg(repo, path)?;
            Ok(vec![stage_repo_remove_key(
                runtime, file, repo, &key, cached,
            )?])
        }
        Ok(_) => Err(ErrCtx::PragmaErr(
            format!(
                "path `{}` is not a regular file or directory",
                physical_path.display()
            )
            .into(),
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let key = repo.file_key(&physical_path)?;
            Ok(vec![stage_repo_remove_key(
                runtime, file, repo, &key, cached,
            )?])
        }
        Err(err) => Err(err.into()),
    }
}

pub(super) fn stage_repo_remove_directory(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    directory_key: &str,
    cached: bool,
) -> Result<Vec<JsonPathAction>, ErrCtx> {
    let keys = tracked_repo_keys_under_directory(repo, directory_key)?;
    let mut removed = Vec::with_capacity(keys.len());
    for key in keys {
        removed.push(stage_repo_remove_key(runtime, file, repo, &key, cached)?);
    }
    Ok(removed)
}

pub(super) fn tracked_repo_keys_under_directory(
    repo: &Repository,
    directory_key: &str,
) -> Result<Vec<String>, ErrCtx> {
    let mut keys = BTreeSet::new();
    for key in repo
        .index_files()?
        .keys()
        .chain(repo.index_artifacts()?.keys())
    {
        if repo_key_is_under_directory(key, directory_key) {
            keys.insert(key.clone());
        }
    }
    Ok(keys.into_iter().collect())
}

pub(super) fn repo_key_is_under_directory(key: &str, directory_key: &str) -> bool {
    directory_key.is_empty()
        || key == directory_key
        || key
            .strip_prefix(directory_key)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

pub(super) fn repo_directory_key(repo: &Repository, path: &Path) -> Result<String, ErrCtx> {
    let directory = std::fs::canonicalize(path)?;
    if !directory.starts_with(repo.worktree()) {
        return Err(ErrCtx::Repo(graft::repo::RepoErr::PathOutsideWorktree {
            path: directory,
            worktree: repo.worktree().to_path_buf(),
        }));
    }
    let relative = directory.strip_prefix(repo.worktree()).map_err(|_| {
        ErrCtx::Repo(graft::repo::RepoErr::PathOutsideWorktree {
            path: directory.clone(),
            worktree: repo.worktree().to_path_buf(),
        })
    })?;
    relative
        .to_str()
        .map(|path| path.replace('\\', "/"))
        .ok_or_else(|| ErrCtx::Repo(graft::repo::RepoErr::NonUtf8Path(relative.to_path_buf())))
}

pub(super) fn stage_repo_remove_key(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    key: &str,
    cached: bool,
) -> Result<JsonPathAction, ErrCtx> {
    if cached {
        return stage_repo_remove_cached_key(repo, key);
    }

    let current_key = repo.file_key(&file.tag)?;
    if key == current_key {
        let physical_path = repo.worktree().join(key);
        let action = if repo.head_file(&file.tag)?.is_some() {
            let entry = repo.stage_file_removal(&file.tag)?;
            json_path_action(
                entry.path,
                RepoTrackedPathKind::SqliteDatabase,
                RepoPathStorage::SqliteSnapshot,
                "staged",
            )
        } else if let Some(artifact) = repo.head_artifact(&file.tag)? {
            let entry = repo.stage_file_removal(&file.tag)?;
            json_path_action(
                entry.path,
                artifact_checkout_path_kind(&artifact),
                artifact_checkout_path_storage(&artifact),
                "staged",
            )
        } else if repo.index_has_entry(&physical_path)? {
            let (kind, storage) = index_path_descriptor(repo, key)?;
            let path = repo.restore_index_path_from_head(&physical_path)?;
            json_path_action(path, kind, storage, "removed")
        } else {
            return Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotTracked(
                key.to_string(),
            )));
        };
        let volume = runtime.volume_open(None, None, None)?;
        file.switch_volume(&volume.vid)?;
        repo.clear_dirty_key(key)?;
        return Ok(action);
    }

    let physical_path = repo.worktree().join(key);
    if repo.head_file(&physical_path)?.is_some() {
        remove_physical_sqlite_file(repo, key, &physical_path)?;
        let entry = repo.stage_file_removal(&physical_path)?;
        return Ok(json_path_action(
            entry.path,
            RepoTrackedPathKind::SqliteDatabase,
            RepoPathStorage::SqliteSnapshot,
            "staged",
        ));
    }
    if let Some(artifact) = repo.head_artifact(&physical_path)? {
        remove_physical_artifact_file(&physical_path)?;
        let entry = repo.stage_file_removal(&physical_path)?;
        return Ok(json_path_action(
            entry.path,
            artifact_checkout_path_kind(&artifact),
            artifact_checkout_path_storage(&artifact),
            "staged",
        ));
    }
    if repo.index_has_entry(&physical_path)? {
        let (kind, storage) = index_path_descriptor(repo, key)?;
        remove_physical_artifact_file(&physical_path)?;
        let path = repo.restore_index_path_from_head(&physical_path)?;
        return Ok(json_path_action(path, kind, storage, "removed"));
    }

    Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotTracked(
        key.to_string(),
    )))
}

pub(super) fn stage_repo_remove_cached_key(
    repo: &Repository,
    key: &str,
) -> Result<JsonPathAction, ErrCtx> {
    let physical_path = repo.worktree().join(key);
    if repo.head_file(&physical_path)?.is_some() {
        let entry = repo.stage_file_removal_key(key)?;
        return Ok(json_path_action(
            entry.path,
            RepoTrackedPathKind::SqliteDatabase,
            RepoPathStorage::SqliteSnapshot,
            "staged",
        ));
    }

    if let Some(artifact) = repo.head_artifact(&physical_path)? {
        let entry = repo.stage_file_removal_key(key)?;
        return Ok(json_path_action(
            entry.path,
            artifact_checkout_path_kind(&artifact),
            artifact_checkout_path_storage(&artifact),
            "staged",
        ));
    }

    if repo.index_has_key(key)? {
        let (kind, storage) = index_path_descriptor(repo, key)?;
        let path = repo.restore_index_key_from_head(key)?;
        return Ok(json_path_action(path, kind, storage, "removed"));
    }

    Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotTracked(
        key.to_string(),
    )))
}

pub(super) fn index_path_descriptor(
    repo: &Repository,
    key: &str,
) -> Result<(RepoTrackedPathKind, RepoPathStorage), ErrCtx> {
    if repo.index_files()?.contains_key(key) {
        return Ok((
            RepoTrackedPathKind::SqliteDatabase,
            RepoPathStorage::SqliteSnapshot,
        ));
    }
    if let Some(artifact) = repo.index_artifacts()?.get(key) {
        return Ok((
            artifact_checkout_path_kind(artifact),
            artifact_checkout_path_storage(artifact),
        ));
    }
    Ok((RepoTrackedPathKind::BinaryFile, RepoPathStorage::Inline))
}

pub(super) fn format_removed_paths(paths: &[JsonPathAction]) -> String {
    match paths {
        [path] => format!("Removed {}", path.path),
        paths => {
            let mut output = format!("Removed {} paths", paths.len());
            for path in paths {
                output.push_str("\n  ");
                output.push_str(&path.path);
            }
            output
        }
    }
}

pub(super) fn remove_physical_sqlite_file(
    repo: &Repository,
    key: &str,
    path: &Path,
) -> Result<(), ErrCtx> {
    if repo.head_file(path)?.is_none() {
        return Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotTracked(
            key.to_string(),
        )));
    }

    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(ErrCtx::PragmaErr(
                    format!(
                        "path `{}` is not a regular SQLite database file",
                        path.display()
                    )
                    .into(),
                ));
            }
            std::fs::remove_file(path)?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }

    Ok(())
}

pub(super) fn remove_physical_artifact_file(path: &Path) -> Result<(), ErrCtx> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(ErrCtx::PragmaErr(
                    format!("path `{}` is not a regular file", path.display()).into(),
                ));
            }
            std::fs::remove_file(path)?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }

    Ok(())
}

pub(super) fn stage_physical_sqlite_file(
    runtime: &Runtime,
    repo: &Repository,
    key: &str,
    path: &Path,
) -> Result<graft::repo::index::IndexEntry, ErrCtx> {
    let state = import_physical_sqlite_file_state(runtime, path)?;
    let entry = repo.stage_file_state_path(repo.worktree().join(key), state)?;
    Ok(entry)
}

pub(super) struct PhysicalSqliteReader {
    input: Mutex<File>,
    path: PathBuf,
    snapshot: graft::snapshot::Snapshot,
}

impl PhysicalSqliteReader {
    pub(super) fn open(path: &Path) -> Result<Self, ErrCtx> {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() {
            return Err(ErrCtx::PragmaErr(
                format!(
                    "path `{}` is not a regular SQLite database file",
                    path.display()
                )
                .into(),
            ));
        }

        if metadata.len() < 100 {
            return Err(ErrCtx::PragmaErr(
                format!("path `{}` is not a SQLite database", path.display()).into(),
            ));
        }

        let mut input = File::open(path)?;
        let mut header = [0_u8; 100];
        input.read_exact(&mut header)?;
        if &header[..SQLITE_DATABASE_MAGIC.len()] != SQLITE_DATABASE_MAGIC {
            return Err(ErrCtx::PragmaErr(
                format!("path `{}` is not a SQLite database", path.display()).into(),
            ));
        }

        let sqlite_page_size = sqlite_page_size_from_header(&header);
        let graft_page_size = PAGESIZE.as_usize() as u32;
        if sqlite_page_size != graft_page_size {
            return Err(ErrCtx::PragmaErr(format!(
                "can only read SQLite databases with {graft_page_size}-byte pages directly; \
                 `{}` uses {sqlite_page_size}-byte pages. Use VACUUM INTO with the Graft VFS to import it.",
                path.display()
            ).into()));
        }

        let page_size = PAGESIZE.as_usize();
        if metadata.len() % page_size as u64 != 0 {
            return Err(ErrCtx::PragmaErr(
                format!(
                    "SQLite database `{}` is not an even multiple of {page_size} bytes",
                    path.display()
                )
                .into(),
            ));
        }

        let page_count = metadata.len() / page_size as u64;
        let page_count = u32::try_from(page_count).map_err(|_| {
            ErrCtx::PragmaErr(
                format!("SQLite database `{}` has too many pages", path.display()).into(),
            )
        })?;
        let mut snapshot = graft::snapshot::Snapshot::empty();
        snapshot.page_count = PageCount::new(page_count);
        Ok(Self {
            input: Mutex::new(input),
            path: path.to_path_buf(),
            snapshot,
        })
    }

    pub(super) fn worktree_state(&self) -> RepoWorktreeFileState {
        RepoWorktreeFileState { page_count: self.page_count() }
    }

    pub(super) fn matches_state(
        &self,
        runtime: &Runtime,
        expected: &CommitFileState,
    ) -> Result<bool, ErrCtx> {
        if self.page_count() != expected.snapshot.page_count {
            return Ok(false);
        }

        let stored = runtime.snapshot_reader(expected.snapshot.to_snapshot());
        for page_number in 1..=self.page_count().to_u32() {
            let pageidx = PageIdx::try_from(page_number).map_err(|err| {
                ErrCtx::PragmaErr(format!("invalid SQLite page index {page_number}: {err}").into())
            })?;
            if self.read_page(pageidx)? != stored.read_page(pageidx)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

impl VolumeRead for PhysicalSqliteReader {
    fn snapshot(&self) -> &graft::snapshot::Snapshot {
        &self.snapshot
    }

    fn page_count(&self) -> PageCount {
        self.snapshot.page_count
    }

    fn read_page(&self, pageidx: PageIdx) -> Result<Page, graft::err::GraftErr> {
        if pageidx.to_u32() > self.page_count().to_u32() {
            return Ok(Page::EMPTY);
        }
        let offset = u64::from(pageidx.to_u32() - 1) * PAGESIZE.as_u64();
        let mut page_bytes = vec![0_u8; PAGESIZE.as_usize()];
        let mut input = self.input.lock();
        input.seek(SeekFrom::Start(offset)).map_err(|err| {
            graft::err::LogicalErr::Other(format!(
                "failed to seek SQLite database `{}`: {err}",
                self.path.display()
            ))
        })?;
        input.read_exact(&mut page_bytes).map_err(|err| {
            graft::err::LogicalErr::Other(format!(
                "failed to read SQLite database `{}`: {err}",
                self.path.display()
            ))
        })?;
        Page::try_from(page_bytes.as_slice()).map_err(|err| {
            graft::err::LogicalErr::Other(format!(
                "invalid SQLite page in `{}`: {err}",
                self.path.display()
            ))
            .into()
        })
    }
}

pub(super) fn physical_sqlite_file_matches_state(
    runtime: &Runtime,
    path: &Path,
    expected: &CommitFileState,
) -> Result<bool, ErrCtx> {
    let physical = PhysicalSqliteReader::open(path)?;
    physical.matches_state(runtime, expected)
}

pub(super) fn import_physical_sqlite_file_state(
    runtime: &Runtime,
    path: &Path,
) -> Result<CommitFileState, ErrCtx> {
    let physical = PhysicalSqliteReader::open(path)?;
    let volume = runtime.volume_open(None, None, None)?;
    let vid = volume.vid;
    let mut writer = runtime.volume_writer(vid.clone())?;
    for page_number in 1..=physical.page_count().to_u32() {
        let pageidx = PageIdx::try_from(page_number).map_err(|err| {
            ErrCtx::PragmaErr(
                format!("invalid SQLite page index in `{}`: {err}", path.display()).into(),
            )
        })?;
        writer.write_page(pageidx, physical.read_page(pageidx)?)?;
    }
    let reader = writer.commit()?;
    Ok(CommitFileState {
        volume: vid,
        snapshot: repo_snapshot_with_commit_hashes(runtime, reader.snapshot())?,
    })
}

pub(super) fn sqlite_page_size_from_header(header: &[u8; 100]) -> u32 {
    let raw = u16::from_be_bytes([header[16], header[17]]);
    if raw == 1 { 65_536 } else { raw as u32 }
}
