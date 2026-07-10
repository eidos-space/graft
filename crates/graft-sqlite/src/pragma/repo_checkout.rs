use super::*;

pub(super) fn run_repo_checkout(
    runtime: &Runtime,
    file: &mut VolFile,
    spec: RepoCheckoutSpec,
) -> Result<JsonCheckoutOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot checkout while there is an open transaction");
    }
    let repo = repo_for_file(file)?;
    match spec {
        RepoCheckoutSpec::Detach { rev, force } => {
            let plan = repo.plan_detach(&rev)?;
            if repo_has_work_in_progress_for_file(runtime, file, &repo)? {
                if force {
                    repo.discard_work_in_progress()?;
                } else {
                    return pragma_err!("cannot checkout with staged or unstaged changes");
                }
            }
            if !force {
                ensure_checkout_plan_preserves_untracked_paths(runtime, file, &repo, &plan)?;
            }
            verify_repo_checkout_plan(runtime, &plan, None)?;
            let previous_files = current_repo_files_for_checkout(&repo)?;
            let previous_artifacts = current_repo_artifacts_for_checkout(&repo)?;
            let id = repo.apply_detach_plan(&rev, &plan)?;
            checkout_repo_plan(
                runtime,
                file,
                &repo,
                &plan,
                &previous_files,
                &previous_artifacts,
                None,
            )?;
            let (current_head, current_branch) = repo_head_and_branch(&repo)?;
            Ok(JsonCheckoutOutcome {
                operation: "checkout",
                current_head: current_head.clone(),
                current_branch: current_branch.clone(),
                head: current_head,
                branch: current_branch,
                target: id,
                path: None,
                paths: Vec::new(),
                path_details: Vec::new(),
            })
        }
        RepoCheckoutSpec::Path { rev, path } => {
            let path = repo_path_arg(&repo, &path)?;
            checkout_repo_path_from_revision(runtime, file, &repo, &rev, &path)
        }
    }
}

pub(super) fn checkout_repo_path_from_revision(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    rev: &str,
    path: &str,
) -> Result<JsonCheckoutOutcome, ErrCtx> {
    match checkout_repo_key_from_revision(runtime, file, repo, rev, path) {
        Ok((target, path_detail)) => {
            let path = path_detail.path.clone();
            let (current_head, current_branch) = repo_head_and_branch(repo)?;
            Ok(JsonCheckoutOutcome {
                operation: "checkout",
                current_head: current_head.clone(),
                current_branch: current_branch.clone(),
                head: current_head,
                branch: current_branch,
                target,
                path: Some(path),
                paths: Vec::new(),
                path_details: vec![path_detail],
            })
        }
        Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotFoundInRevision { .. })) => {
            let keys = checkout_keys_for_revision_pathspec(repo, rev, path)?;
            if keys.is_empty() {
                return Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotFoundInRevision {
                    path: path.to_string(),
                    rev: rev.to_string(),
                }));
            }
            let target = repo.resolve_revision(rev)?;
            let mut checkout_keys = BTreeSet::new();
            for key in &keys {
                if revision_has_repo_key(repo, &target, key)? {
                    checkout_keys.insert(key.clone());
                }
            }
            ensure_checkout_keys_preserve_untracked_paths(runtime, file, repo, &checkout_keys)?;
            let mut checked_out = Vec::with_capacity(keys.len());
            let mut path_details = Vec::with_capacity(keys.len());
            for key in keys {
                if revision_has_repo_key(repo, &target, &key)? {
                    let (_, path_detail) =
                        checkout_repo_key_from_revision(runtime, file, repo, rev, &key)?;
                    checked_out.push(path_detail.path.clone());
                    path_details.push(path_detail);
                } else {
                    let path_detail = current_key_path_detail(repo, &key)?;
                    stage_checkout_deletion_for_key(runtime, file, repo, &key)?;
                    checked_out.push(path_detail.path.clone());
                    path_details.push(path_detail);
                }
            }
            let (current_head, current_branch) = repo_head_and_branch(repo)?;
            Ok(JsonCheckoutOutcome {
                operation: "checkout",
                current_head: current_head.clone(),
                current_branch: current_branch.clone(),
                head: current_head,
                branch: current_branch,
                target,
                path: None,
                paths: checked_out,
                path_details,
            })
        }
        Err(err) => Err(err.into()),
    }
}

pub(super) fn checkout_repo_key_from_revision(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    rev: &str,
    key: &str,
) -> Result<(String, JsonPathDetail), ErrCtx> {
    let current_key = repo.file_key(&file.tag)?;
    match repo.plan_checkout_file_key_from_revision(rev, key.to_string()) {
        Ok(plan) => {
            ensure_checkout_key_preserves_untracked_path(runtime, file, repo, &plan.path)?;
            hydrate_repo_file_state(runtime, &plan.state, None)?;
            let outcome = repo.apply_checkout_file_plan(&plan)?;
            if outcome.path == current_key {
                checkout_repo_file_state(runtime, file, &outcome.state, None)?;
            } else {
                checkout_repo_file_state_to_key(
                    runtime,
                    repo,
                    &outcome.path,
                    &outcome.state,
                    None,
                )?;
            }
            Ok((
                outcome.target,
                json_path_detail(
                    outcome.path,
                    RepoTrackedPathKind::SqliteDatabase,
                    RepoPathStorage::SqliteSnapshot,
                ),
            ))
        }
        Err(graft::repo::RepoErr::PathNotFoundInRevision { .. }) => {
            let plan = repo.plan_checkout_artifact_key_from_revision(rev, key.to_string())?;
            ensure_checkout_key_preserves_untracked_path(runtime, file, repo, &plan.path)?;
            let outcome = repo.apply_checkout_artifact_plan(&plan)?;
            if outcome.path == current_key {
                let volume = runtime.volume_open(None, None, None)?;
                file.switch_volume(&volume.vid)?;
            }
            repo.materialize_artifact_key(&outcome.path, &outcome.state)?;
            Ok((
                outcome.target,
                json_path_detail(
                    outcome.path,
                    artifact_checkout_path_kind(&outcome.state),
                    artifact_checkout_path_storage(&outcome.state),
                ),
            ))
        }
        Err(err) => Err(err.into()),
    }
}

pub(super) fn json_path_detail(
    path: String,
    kind: RepoTrackedPathKind,
    storage: RepoPathStorage,
) -> JsonPathDetail {
    JsonPathDetail {
        path,
        kind: repo_tracked_path_kind_json_label(kind),
        storage: repo_path_storage_json_label(storage),
    }
}

pub(super) fn artifact_checkout_path_kind(state: &CommitArtifactState) -> RepoTrackedPathKind {
    match state {
        CommitArtifactState::File { kind, .. } | CommitArtifactState::LargeFile { kind, .. } => {
            *kind
        }
    }
}

pub(super) fn artifact_checkout_path_storage(state: &CommitArtifactState) -> RepoPathStorage {
    match state {
        CommitArtifactState::File { .. } => RepoPathStorage::Inline,
        CommitArtifactState::LargeFile { .. } => RepoPathStorage::External,
    }
}

pub(super) fn current_key_path_detail(
    repo: &Repository,
    key: &str,
) -> Result<JsonPathDetail, ErrCtx> {
    if repo.index_files()?.contains_key(key) {
        return Ok(json_path_detail(
            key.to_string(),
            RepoTrackedPathKind::SqliteDatabase,
            RepoPathStorage::SqliteSnapshot,
        ));
    }

    if let Some(state) = repo.index_artifacts()?.get(key) {
        return Ok(json_path_detail(
            key.to_string(),
            artifact_checkout_path_kind(state),
            artifact_checkout_path_storage(state),
        ));
    }
    match repo.show_revision("HEAD") {
        Ok(commit) => {
            if commit.files.contains_key(key) {
                return Ok(json_path_detail(
                    key.to_string(),
                    RepoTrackedPathKind::SqliteDatabase,
                    RepoPathStorage::SqliteSnapshot,
                ));
            }
            if let Some(state) = commit.artifacts.get(key) {
                return Ok(json_path_detail(
                    key.to_string(),
                    artifact_checkout_path_kind(state),
                    artifact_checkout_path_storage(state),
                ));
            }
        }
        Err(graft::repo::RepoErr::UnbornHead) => {}
        Err(err) => return Err(err.into()),
    }

    Ok(json_path_detail(
        key.to_string(),
        RepoTrackedPathKind::BinaryFile,
        RepoPathStorage::Inline,
    ))
}

pub(super) fn checkout_keys_for_revision_pathspec(
    repo: &Repository,
    rev: &str,
    filter: &str,
) -> Result<Vec<String>, ErrCtx> {
    let target = repo.resolve_revision(rev)?;
    let commit = repo.read_commit(&target)?;
    let mut keys = BTreeSet::new();
    keys.extend(
        commit
            .files
            .keys()
            .chain(commit.artifacts.keys())
            .filter(|key| repo_key_matches_filter(key, filter))
            .cloned(),
    );
    keys.extend(
        repo.index_files()?
            .keys()
            .chain(repo.index_artifacts()?.keys())
            .filter(|key| repo_key_matches_filter(key, filter))
            .cloned(),
    );
    Ok(keys.into_iter().collect())
}

pub(super) fn revision_has_repo_key(
    repo: &Repository,
    target: &str,
    key: &str,
) -> Result<bool, ErrCtx> {
    let commit = repo.read_commit(target)?;
    Ok(commit.files.contains_key(key) || commit.artifacts.contains_key(key))
}

pub(super) fn stage_checkout_deletion_for_key(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    key: &str,
) -> Result<(), ErrCtx> {
    let current_key = repo.file_key(&file.tag)?;
    if key == current_key {
        if head_has_repo_key(repo, key)? {
            repo.stage_file_removal_key(key)?;
        } else if repo.index_has_key(key)? {
            repo.restore_index_key_from_head(key)?;
        } else {
            repo.stage_file_removal_key(key)?;
        }
        let volume = runtime.volume_open(None, None, None)?;
        file.switch_volume(&volume.vid)?;
    } else {
        remove_materialized_repo_file(repo, key)?;
        if head_has_repo_key(repo, key)? {
            repo.stage_file_removal_key(key)?;
        } else if repo.index_has_key(key)? {
            repo.restore_index_key_from_head(key)?;
        } else {
            repo.stage_file_removal_key(key)?;
        }
    }
    Ok(())
}

pub(super) fn head_has_repo_key(repo: &Repository, key: &str) -> Result<bool, ErrCtx> {
    match repo.show_revision("HEAD") {
        Ok(commit) => Ok(commit.files.contains_key(key) || commit.artifacts.contains_key(key)),
        Err(graft::repo::RepoErr::UnbornHead) => Ok(false),
        Err(err) => Err(err.into()),
    }
}

pub(super) fn format_checkout_outcome(outcome: &JsonCheckoutOutcome) -> String {
    match &outcome.path {
        Some(path) => format!(
            "Checked out {} from {}",
            path,
            &outcome.target[..outcome.target.len().min(12)]
        ),
        None if !outcome.paths.is_empty() => {
            let mut output = format!(
                "Checked out {} paths from {}",
                outcome.paths.len(),
                &outcome.target[..outcome.target.len().min(12)]
            );
            for path in &outcome.paths {
                output.push_str("\n  ");
                output.push_str(path);
            }
            output
        }
        None => format!(
            "HEAD detached at {}",
            &outcome.target[..outcome.target.len().min(12)]
        ),
    }
}

pub(super) fn run_repo_reset(
    runtime: &Runtime,
    file: &mut VolFile,
    rev: &str,
    mode: ResetMode,
) -> Result<RepoResetCommandOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot reset while there is an open transaction");
    }

    let repo = repo_for_file(file)?;
    let current_state = current_repo_file_state(runtime, file)?;
    let old_head_state = repo.head_file(&file.tag)?;
    let had_staged_changes = repo.has_staged_changes()?;
    let plan = repo.plan_reset(rev, mode)?;
    let plan = if matches!(mode, ResetMode::Hard) {
        let mut plan = plan;
        plan.checkout = prepare_repo_checkout_plan_with_hash_policy(
            runtime,
            &plan.checkout,
            None,
            SnapshotHashPolicy::AllowHydratedMismatch,
        )?;
        plan
    } else {
        plan
    };
    if matches!(mode, ResetMode::Hard) {
        verify_repo_checkout_plan(runtime, &plan.checkout, None)?;
    }
    let previous_files = if matches!(mode, ResetMode::Hard) {
        current_repo_files_for_checkout(&repo)?
    } else {
        BTreeMap::new()
    };
    let previous_artifacts = if matches!(mode, ResetMode::Hard) {
        current_repo_artifacts_for_checkout(&repo)?
    } else {
        BTreeMap::new()
    };
    let reset_paths = if matches!(mode, ResetMode::Hard) {
        checkout_plan_path_actions(&plan.checkout, &previous_files, &previous_artifacts)
    } else {
        Vec::new()
    };
    let outcome = repo.apply_reset_plan(&plan)?;

    match mode {
        ResetMode::Soft => {
            if !had_staged_changes && let Some(old_head_state) = &old_head_state {
                let target_state = repo.head_file(&file.tag)?;
                if target_state.as_ref() != Some(old_head_state) {
                    repo.stage_file_state_path(&file.tag, old_head_state.clone())?;
                }
            }
            if !had_staged_changes
                && old_head_state
                    .as_ref()
                    .is_some_and(|old_head_state| &current_state != old_head_state)
            {
                repo.mark_dirty_path(&file.tag)?;
            }
        }
        ResetMode::Mixed => {
            let target_state = repo.head_file(&file.tag)?;
            if target_state.as_ref() == Some(&current_state) {
                repo.clear_dirty_path(&file.tag)?;
            } else {
                repo.mark_dirty_path(&file.tag)?;
            }
        }
        ResetMode::Hard => {
            checkout_repo_plan(
                runtime,
                file,
                &repo,
                &plan.checkout,
                &previous_files,
                &previous_artifacts,
                None,
            )?;
        }
    }

    let branch = repo.current_branch()?;
    Ok(RepoResetCommandOutcome { outcome, branch, paths: reset_paths })
}

pub(super) fn checkout_plan_path_actions(
    plan: &CheckoutPlan,
    previous_files: &BTreeMap<String, CommitFileState>,
    previous_artifacts: &BTreeMap<String, graft::repo::CommitArtifactState>,
) -> Vec<JsonPathAction> {
    let mut paths = BTreeMap::new();
    for path in plan.files.keys() {
        paths.insert(
            path.clone(),
            json_path_action(
                path.clone(),
                RepoTrackedPathKind::SqliteDatabase,
                RepoPathStorage::SqliteSnapshot,
                "checked_out",
            ),
        );
    }
    for (path, state) in &plan.artifacts {
        paths.insert(
            path.clone(),
            json_path_action(
                path.clone(),
                artifact_checkout_path_kind(state),
                artifact_checkout_path_storage(state),
                "checked_out",
            ),
        );
    }
    for path in previous_files.keys() {
        if plan.files.contains_key(path) || plan.artifacts.contains_key(path) {
            continue;
        }
        paths.insert(
            path.clone(),
            json_path_action(
                path.clone(),
                RepoTrackedPathKind::SqliteDatabase,
                RepoPathStorage::SqliteSnapshot,
                "removed",
            ),
        );
    }
    for (path, state) in previous_artifacts {
        if plan.files.contains_key(path) || plan.artifacts.contains_key(path) {
            continue;
        }
        paths.insert(
            path.clone(),
            json_path_action(
                path.clone(),
                artifact_checkout_path_kind(state),
                artifact_checkout_path_storage(state),
                "removed",
            ),
        );
    }
    paths.into_values().collect()
}

pub(super) fn merge_path_actions(
    repo: &Repository,
    outcome: &MergeOutcome,
    fast_forward_plan: Option<&CheckoutPlan>,
    previous_files: &BTreeMap<String, CommitFileState>,
    previous_artifacts: &BTreeMap<String, graft::repo::CommitArtifactState>,
) -> Result<Vec<JsonPathAction>, ErrCtx> {
    let mut paths = BTreeMap::new();
    match outcome {
        MergeOutcome::FastForward { .. } => {
            if let Some(plan) = fast_forward_plan {
                return Ok(checkout_plan_path_actions(
                    plan,
                    previous_files,
                    previous_artifacts,
                ));
            }
        }
        MergeOutcome::Merged { staged, conflicted, .. } => {
            let materialized = conflicted.is_empty();
            let index = repo.read_index()?;
            let stage0_entries = index
                .stage0_entries()
                .map(|entry| (entry.path.clone(), entry.clone()))
                .collect::<BTreeMap<_, _>>();
            for path in staged {
                let Some(entry) = stage0_entries.get(path) else {
                    continue;
                };
                let action = if materialized {
                    "checked_out"
                } else {
                    "staged"
                };
                let path_action = if entry.file.is_some() {
                    json_path_action(
                        path.clone(),
                        RepoTrackedPathKind::SqliteDatabase,
                        RepoPathStorage::SqliteSnapshot,
                        action,
                    )
                } else if let Some(state) = &entry.artifact {
                    json_path_action(
                        path.clone(),
                        artifact_checkout_path_kind(state),
                        artifact_checkout_path_storage(state),
                        action,
                    )
                } else {
                    let (kind, storage) =
                        previous_path_descriptor(path, previous_files, previous_artifacts);
                    json_path_action(
                        path.clone(),
                        kind,
                        storage,
                        if materialized { "removed" } else { "staged" },
                    )
                };
                paths.insert(path.clone(), path_action);
            }
            for path in conflicted {
                paths.insert(
                    path.clone(),
                    json_path_action(
                        path.clone(),
                        conflict_path_kind(repo, path)?,
                        conflict_path_storage(repo, path)?,
                        "conflicted",
                    ),
                );
            }
        }
        MergeOutcome::AlreadyUpToDate { .. } => {}
    }
    Ok(paths.into_values().collect())
}

pub(super) fn previous_path_descriptor(
    path: &str,
    previous_files: &BTreeMap<String, CommitFileState>,
    previous_artifacts: &BTreeMap<String, graft::repo::CommitArtifactState>,
) -> (RepoTrackedPathKind, RepoPathStorage) {
    if previous_files.contains_key(path) {
        (
            RepoTrackedPathKind::SqliteDatabase,
            RepoPathStorage::SqliteSnapshot,
        )
    } else if let Some(state) = previous_artifacts.get(path) {
        (
            artifact_checkout_path_kind(state),
            artifact_checkout_path_storage(state),
        )
    } else {
        (RepoTrackedPathKind::BinaryFile, RepoPathStorage::Inline)
    }
}

pub(super) fn json_path_action(
    path: String,
    kind: RepoTrackedPathKind,
    storage: RepoPathStorage,
    action: &'static str,
) -> JsonPathAction {
    JsonPathAction {
        path,
        kind: repo_tracked_path_kind_json_label(kind),
        storage: repo_path_storage_json_label(storage),
        action,
    }
}

pub(super) fn checkout_repo_head(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    remote: Option<Arc<Remote>>,
) -> Result<(), ErrCtx> {
    if let Some(state) = repo.head_file(&file.tag)? {
        checkout_repo_file_state(runtime, file, &state, remote)?;
    } else {
        let volume = runtime.volume_open(None, None, None)?;
        file.switch_volume(&volume.vid)?;
    }
    Ok(())
}

pub(super) fn checkout_repo_plan(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    plan: &CheckoutPlan,
    previous_files: &BTreeMap<String, CommitFileState>,
    previous_artifacts: &BTreeMap<String, graft::repo::CommitArtifactState>,
    remote: Option<Arc<Remote>>,
) -> Result<(), ErrCtx> {
    let key = repo.file_key(&file.tag)?;
    if let Some(state) = plan.files.get(&key) {
        checkout_repo_file_state(runtime, file, state, remote.clone())?;
    } else {
        let volume = runtime.volume_open(None, None, None)?;
        file.switch_volume(&volume.vid)?;
    }
    for (path, state) in &plan.files {
        if path == &key {
            continue;
        }
        checkout_repo_file_state_to_path(
            runtime,
            repo,
            state,
            &repo.worktree().join(path),
            remote.clone(),
        )?;
    }
    repo.materialize_artifact_checkout(&plan.artifacts, previous_artifacts, &plan.files)?;
    for path in previous_files.keys() {
        if path == &key || plan.files.contains_key(path) || plan.artifacts.contains_key(path) {
            continue;
        }
        remove_materialized_repo_file(repo, path)?;
    }
    Ok(())
}

pub(super) fn current_repo_files_for_checkout(
    repo: &Repository,
) -> Result<BTreeMap<String, CommitFileState>, ErrCtx> {
    match repo.index_files() {
        Ok(files) => Ok(files),
        Err(graft::repo::RepoErr::UnresolvedConflicts) => Ok(BTreeMap::new()),
        Err(err) => Err(err.into()),
    }
}

pub(super) fn current_repo_artifacts_for_checkout(
    repo: &Repository,
) -> Result<BTreeMap<String, graft::repo::CommitArtifactState>, ErrCtx> {
    match repo.index_artifacts() {
        Ok(artifacts) => Ok(artifacts),
        Err(graft::repo::RepoErr::UnresolvedConflicts) => Ok(BTreeMap::new()),
        Err(err) => Err(err.into()),
    }
}

pub(super) fn remove_materialized_repo_file(repo: &Repository, key: &str) -> Result<(), ErrCtx> {
    let path = repo.worktree().join(key);
    match std::fs::symlink_metadata(&path) {
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

#[derive(Clone)]
pub(super) enum RestoredRepoPathState {
    File(CommitFileState),
    Artifact(CommitArtifactState),
}

pub(super) struct RepoRestoreKeyPlan {
    key: String,
    restored: Option<RestoredRepoPathState>,
    path_detail: JsonPathDetail,
}

pub(super) fn restore_repo_path(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    spec: &RepoRestoreSpec,
) -> Result<JsonRestoreOutcome, ErrCtx> {
    if repo.read_index()?.has_conflicts() {
        return Err(ErrCtx::Repo(graft::repo::RepoErr::UnresolvedConflicts));
    }
    if spec.all {
        return restore_repo_staged_all(runtime, file, repo, spec);
    }

    let path = spec.path.as_deref().ok_or_else(|| {
        ErrCtx::PragmaErr("restore requires a path unless --staged --all is used".into())
    })?;
    let (key, physical_path) = repo_restore_path_arg(repo, path)?;
    let is_directory = std::fs::symlink_metadata(&physical_path)
        .map(|metadata| metadata.file_type().is_dir())
        .unwrap_or(false);
    if is_directory {
        return restore_repo_directory(runtime, file, repo, spec, &key);
    }

    match restore_repo_key(runtime, file, repo, spec, &key) {
        Ok(restored) => json_restore_outcome(repo, spec, vec![restored]),
        Err(ErrCtx::PragmaErr(_)) | Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotTracked(_))) => {
            let keys = restore_keys_for_pathspec(repo, spec, &key)?;
            if keys.is_empty() {
                restore_repo_key(runtime, file, repo, spec, &key)
                    .and_then(|restored| json_restore_outcome(repo, spec, vec![restored]))
            } else {
                restore_repo_keys(runtime, file, repo, spec, keys)
            }
        }
        Err(err) => Err(err),
    }
}

pub(super) fn restore_repo_staged_all(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    spec: &RepoRestoreSpec,
) -> Result<JsonRestoreOutcome, ErrCtx> {
    let status = repo_status_for_file(runtime, file, repo)?;
    let keys = status
        .staged_changes
        .into_iter()
        .filter(|change| spec.kind.is_none_or(|kind| change.kind == kind))
        .map(|change| change.path)
        .collect();
    restore_repo_keys(runtime, file, repo, spec, keys)
}

pub(super) fn restore_repo_directory(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    spec: &RepoRestoreSpec,
    key: &str,
) -> Result<JsonRestoreOutcome, ErrCtx> {
    let keys = restore_keys_for_pathspec(repo, spec, key)?;
    if keys.is_empty() {
        return Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotTracked(
            if key.is_empty() {
                ".".to_string()
            } else {
                key.to_string()
            },
        )));
    }
    restore_repo_keys(runtime, file, repo, spec, keys)
}

pub(super) fn restore_repo_keys(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    spec: &RepoRestoreSpec,
    keys: Vec<String>,
) -> Result<JsonRestoreOutcome, ErrCtx> {
    if spec.staged {
        for key in &keys {
            restore_key_path_detail(repo, spec, key)?;
        }
        let mut restored = Vec::with_capacity(keys.len());
        for key in keys {
            restored.push(restore_repo_key(runtime, file, repo, spec, &key)?);
        }
        return json_restore_outcome(repo, spec, restored);
    }

    let plan = plan_restore_repo_keys(repo, spec, keys)?;
    preflight_restore_repo_keys(runtime, file, repo, &plan)?;
    // Individual replacements are atomic, but filesystems do not provide a cross-path
    // transaction. A new OS I/O error during apply can therefore leave a restored prefix.
    for entry in &plan {
        apply_restored_repo_key(runtime, file, repo, &entry.key, entry.restored.as_ref())?;
    }
    for entry in &plan {
        update_restored_worktree_state_key(runtime, repo, &entry.key, entry.restored.as_ref())?;
    }
    json_restore_outcome(
        repo,
        spec,
        plan.into_iter().map(|entry| entry.path_detail).collect(),
    )
}

pub(super) fn plan_restore_repo_keys(
    repo: &Repository,
    spec: &RepoRestoreSpec,
    keys: Vec<String>,
) -> Result<Vec<RepoRestoreKeyPlan>, ErrCtx> {
    let index_files = repo.index_files()?;
    let index_artifacts = repo.index_artifacts()?;
    let index_keys = repo
        .read_index()?
        .stage0_entries()
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    let (source_files, source_artifacts) = if let Some(source) = &spec.source {
        let commit = repo.show_revision(source)?;
        (commit.files, commit.artifacts)
    } else {
        (index_files.clone(), index_artifacts.clone())
    };
    let (head_files, head_artifacts) = match repo.show_revision("HEAD") {
        Ok(commit) => (commit.files, commit.artifacts),
        Err(graft::repo::RepoErr::UnbornHead) => (BTreeMap::new(), BTreeMap::new()),
        Err(err) => return Err(err.into()),
    };
    let plan_keys = keys.iter().cloned().collect::<BTreeSet<_>>();
    let restored_keys = source_files
        .keys()
        .chain(source_artifacts.keys())
        .filter(|key| plan_keys.contains(*key))
        .cloned()
        .collect::<BTreeSet<_>>();
    validate_restore_plan_path_conflicts(&restored_keys, &plan_keys)?;

    keys.into_iter()
        .map(|key| {
            let restored = source_files
                .get(&key)
                .cloned()
                .map(RestoredRepoPathState::File)
                .or_else(|| {
                    source_artifacts
                        .get(&key)
                        .cloned()
                        .map(RestoredRepoPathState::Artifact)
                });
            if restored.is_none()
                && !can_plan_restore_deletion(
                    spec,
                    &key,
                    &index_files,
                    &index_artifacts,
                    &index_keys,
                    &head_files,
                    &head_artifacts,
                )
            {
                return Err(ErrCtx::PragmaErr(
                    format!("path `{key}` is not tracked").into(),
                ));
            }
            let path_detail = restored_repo_path_detail(repo, &key, restored.as_ref())?;
            Ok(RepoRestoreKeyPlan { key, restored, path_detail })
        })
        .collect()
}

pub(super) fn validate_restore_plan_path_conflicts(
    restored_keys: &BTreeSet<String>,
    plan_keys: &BTreeSet<String>,
) -> Result<(), ErrCtx> {
    for parent in restored_keys {
        if let Some(descendant) = plan_keys
            .iter()
            .find(|key| *key != parent && repo_key_matches_filter(key, parent))
        {
            return pragma_err!(format!(
                "cannot restore file `{parent}` together with descendant `{descendant}`"
            ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn can_plan_restore_deletion(
    spec: &RepoRestoreSpec,
    key: &str,
    index_files: &BTreeMap<String, CommitFileState>,
    index_artifacts: &BTreeMap<String, CommitArtifactState>,
    index_keys: &BTreeSet<String>,
    head_files: &BTreeMap<String, CommitFileState>,
    head_artifacts: &BTreeMap<String, CommitArtifactState>,
) -> bool {
    if spec.source.is_none() {
        return index_keys.contains(key);
    }
    index_files.contains_key(key)
        || index_artifacts.contains_key(key)
        || index_keys.contains(key)
        || head_files.contains_key(key)
        || head_artifacts.contains_key(key)
}

pub(super) fn preflight_restore_repo_keys(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    plan: &[RepoRestoreKeyPlan],
) -> Result<(), ErrCtx> {
    for entry in plan {
        preflight_restore_repo_key_path(repo, &entry.key)?;
    }
    ensure_restore_keys_preserve_untracked_paths(file, repo, plan)?;
    for entry in plan {
        match &entry.restored {
            Some(RestoredRepoPathState::File(state)) => {
                hydrate_repo_file_state(runtime, state, None)?;
            }
            Some(RestoredRepoPathState::Artifact(state)) => {
                repo.verify_artifact_state(state)?;
            }
            None => {}
        }
    }
    Ok(())
}

pub(super) fn preflight_restore_repo_key_path(repo: &Repository, key: &str) -> Result<(), ErrCtx> {
    graft::repo::validate_repo_path_identity(key)?;
    let components = Path::new(key).components().collect::<Vec<_>>();
    if components.is_empty()
        || components
            .iter()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return pragma_err!(format!("path `{key}` is not a valid repository file path"));
    }

    let mut physical_path = repo.worktree().to_path_buf();
    for (index, component) in components.iter().enumerate() {
        physical_path.push(component.as_os_str());
        let is_target = index + 1 == components.len();
        match std::fs::symlink_metadata(&physical_path) {
            Ok(metadata) if is_target && metadata.file_type().is_file() => {}
            Ok(metadata) if !is_target && metadata.file_type().is_dir() => {}
            Ok(_) if is_target => {
                return pragma_err!(format!(
                    "path `{}` is not a regular file",
                    physical_path.display()
                ));
            }
            Ok(_) => {
                return pragma_err!(format!(
                    "path ancestor `{}` is not a directory",
                    physical_path.display()
                ));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

pub(super) fn ensure_restore_keys_preserve_untracked_paths(
    file: &VolFile,
    repo: &Repository,
    plan: &[RepoRestoreKeyPlan],
) -> Result<(), ErrCtx> {
    let tracked = repo
        .index_files()?
        .into_keys()
        .chain(repo.index_artifacts()?.into_keys())
        .collect::<BTreeSet<_>>();
    let current_key = repo.file_key(&file.tag)?;
    let overwritten = plan
        .iter()
        .filter(|entry| {
            entry.key != current_key
                && !tracked.contains(&entry.key)
                && std::fs::symlink_metadata(repo.worktree().join(&entry.key))
                    .is_ok_and(|metadata| metadata.file_type().is_file())
        })
        .map(|entry| entry.key.clone())
        .collect::<Vec<_>>();
    if overwritten.is_empty() {
        return Ok(());
    }
    pragma_err!(format!(
        "cannot restore because untracked paths would be overwritten: {}",
        overwritten.join(", ")
    ))
}

pub(super) fn apply_restored_repo_key(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    key: &str,
    restored: Option<&RestoredRepoPathState>,
) -> Result<(), ErrCtx> {
    let current_key = repo.file_key(&file.tag)?;
    if key == current_key {
        if let Some(RestoredRepoPathState::File(state)) = restored {
            checkout_repo_file_state(runtime, file, state, None)?;
        } else if let Some(RestoredRepoPathState::Artifact(state)) = restored {
            let volume = runtime.volume_open(None, None, None)?;
            file.switch_volume(&volume.vid)?;
            repo.materialize_artifact_key(key, state)?;
        } else {
            let volume = runtime.volume_open(None, None, None)?;
            file.switch_volume(&volume.vid)?;
        }
    } else if let Some(RestoredRepoPathState::File(state)) = restored {
        checkout_repo_file_state_to_key(runtime, repo, key, state, None)?;
    } else if let Some(RestoredRepoPathState::Artifact(state)) = restored {
        repo.materialize_artifact_key(key, state)?;
    } else {
        remove_materialized_repo_file(repo, key)?;
    }
    Ok(())
}

pub(super) fn restore_repo_key(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    spec: &RepoRestoreSpec,
    key: &str,
) -> Result<JsonPathDetail, ErrCtx> {
    if spec.staged {
        let restored = restore_key_path_detail(repo, spec, key)?;
        if let Some(source) = &spec.source {
            repo.restore_index_key_from_revision(source, key)?;
        } else {
            repo.restore_index_key_from_head(key)?;
        }
        update_worktree_state_after_index_restore_key(runtime, file, repo, key)?;
        return Ok(restored);
    }

    let restored = if let Some(source) = &spec.source {
        let source_commit = repo.show_revision(source)?;
        if let Some(state) = source_commit.files.get(key).cloned() {
            Some(RestoredRepoPathState::File(state))
        } else {
            source_commit
                .artifacts
                .get(key)
                .cloned()
                .map(RestoredRepoPathState::Artifact)
        }
    } else {
        if let Some(state) = repo.index_files()?.get(key).cloned() {
            Some(RestoredRepoPathState::File(state))
        } else {
            repo.index_artifacts()?
                .get(key)
                .cloned()
                .map(RestoredRepoPathState::Artifact)
        }
    };

    if restored.is_none() {
        let can_restore_deletion = if spec.source.is_some() {
            repo.index_files()?.contains_key(key)
                || repo.index_artifacts()?.contains_key(key)
                || repo.index_has_key(key)?
                || head_has_repo_key(repo, key)?
        } else {
            repo.index_has_key(key)?
        };
        if !can_restore_deletion {
            return Err(ErrCtx::PragmaErr(
                format!("path `{key}` is not tracked").into(),
            ));
        }
    }

    let path_detail = restored_repo_path_detail(repo, key, restored.as_ref())?;
    apply_restored_repo_key(runtime, file, repo, key, restored.as_ref())?;
    update_restored_worktree_state_key(runtime, repo, key, restored.as_ref())?;
    Ok(path_detail)
}

pub(super) fn json_restore_outcome(
    repo: &Repository,
    spec: &RepoRestoreSpec,
    path_details: Vec<JsonPathDetail>,
) -> Result<JsonRestoreOutcome, ErrCtx> {
    let paths = path_details
        .iter()
        .map(|path| path.path.clone())
        .collect::<Vec<_>>();
    let path = match paths.as_slice() {
        [path] => Some(path.clone()),
        _ => None,
    };
    let (current_head, current_branch) = repo_head_and_branch(repo)?;
    Ok(JsonRestoreOutcome {
        operation: "restore",
        current_head,
        current_branch,
        source: spec.source.clone(),
        staged: spec.staged,
        all: spec.all,
        kind: spec.kind.map(repo_tracked_path_kind_json_label),
        path,
        paths: if path_details.len() == 1 {
            Vec::new()
        } else {
            paths
        },
        path_details,
    })
}

pub(super) fn format_restore_outcome(outcome: &JsonRestoreOutcome) -> String {
    let restored = match &outcome.path {
        Some(path) => path.clone(),
        None => format_repo_path_list(
            outcome.path_details.len(),
            outcome
                .path_details
                .iter()
                .map(|path| path.path.clone())
                .collect(),
        ),
    };
    format!("Restored {restored}")
}

pub(super) fn restored_repo_path_detail(
    repo: &Repository,
    key: &str,
    restored: Option<&RestoredRepoPathState>,
) -> Result<JsonPathDetail, ErrCtx> {
    match restored {
        Some(RestoredRepoPathState::File(_)) => Ok(json_path_detail(
            key.to_string(),
            RepoTrackedPathKind::SqliteDatabase,
            RepoPathStorage::SqliteSnapshot,
        )),
        Some(RestoredRepoPathState::Artifact(state)) => Ok(json_path_detail(
            key.to_string(),
            artifact_checkout_path_kind(state),
            artifact_checkout_path_storage(state),
        )),
        None => current_key_path_detail(repo, key),
    }
}

pub(super) fn restore_key_path_detail(
    repo: &Repository,
    spec: &RepoRestoreSpec,
    key: &str,
) -> Result<JsonPathDetail, ErrCtx> {
    if let Some(source) = &spec.source {
        let source_commit = repo.show_revision(source)?;
        if source_commit.files.contains_key(key) {
            return Ok(json_path_detail(
                key.to_string(),
                RepoTrackedPathKind::SqliteDatabase,
                RepoPathStorage::SqliteSnapshot,
            ));
        }
        if let Some(state) = source_commit.artifacts.get(key) {
            return Ok(json_path_detail(
                key.to_string(),
                artifact_checkout_path_kind(state),
                artifact_checkout_path_storage(state),
            ));
        }
        return current_key_path_detail(repo, key);
    }

    if spec.staged {
        match repo.show_revision("HEAD") {
            Ok(head) => {
                if head.files.contains_key(key) {
                    return Ok(json_path_detail(
                        key.to_string(),
                        RepoTrackedPathKind::SqliteDatabase,
                        RepoPathStorage::SqliteSnapshot,
                    ));
                }
                if let Some(state) = head.artifacts.get(key) {
                    return Ok(json_path_detail(
                        key.to_string(),
                        artifact_checkout_path_kind(state),
                        artifact_checkout_path_storage(state),
                    ));
                }
            }
            Err(graft::repo::RepoErr::UnbornHead) => {}
            Err(err) => return Err(err.into()),
        }
    }

    current_key_path_detail(repo, key)
}

pub(super) fn restore_keys_for_pathspec(
    repo: &Repository,
    spec: &RepoRestoreSpec,
    filter: &str,
) -> Result<Vec<String>, ErrCtx> {
    let mut keys = BTreeSet::new();

    if let Some(source) = &spec.source {
        let source_commit = repo.show_revision(source)?;
        keys.extend(
            source_commit
                .files
                .keys()
                .chain(source_commit.artifacts.keys())
                .filter(|key| repo_key_matches_filter(key, filter))
                .cloned(),
        );
    } else if spec.staged {
        if let Ok(head) = repo.show_revision("HEAD") {
            keys.extend(
                head.files
                    .keys()
                    .chain(head.artifacts.keys())
                    .filter(|key| repo_key_matches_filter(key, filter))
                    .cloned(),
            );
        }
    }

    keys.extend(
        repo.index_files()?
            .keys()
            .chain(repo.index_artifacts()?.keys())
            .filter(|key| repo_key_matches_filter(key, filter))
            .cloned(),
    );
    keys.extend(
        repo.read_index()?
            .stage0_entries()
            .filter(|entry| repo_key_matches_filter(&entry.path, filter))
            .map(|entry| entry.path.clone()),
    );

    Ok(keys.into_iter().collect())
}

pub(super) fn format_repo_path_list(count: usize, paths: Vec<String>) -> String {
    match paths.as_slice() {
        [path] => path.clone(),
        _ => {
            let mut output = format!("{count} paths");
            for path in paths {
                output.push_str("\n  ");
                output.push_str(&path);
            }
            output
        }
    }
}

pub(super) fn export_repo_path(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    spec: &RepoExportSpec,
) -> Result<String, ErrCtx> {
    let path = spec.path.as_deref().unwrap_or_else(|| Path::new(&file.tag));
    let (key, physical_path) = repo_physical_path_arg(repo, path)?;

    if let Some(source) = &spec.source {
        let state = repo
            .file_from_revision(source, &physical_path)?
            .ok_or_else(|| ErrCtx::Repo(graft::repo::RepoErr::PathNotTracked(key.clone())))?;
        hydrate_repo_file_state_for(runtime, &state, None, RepoSnapshotPurpose::Export)?;
        write_repo_file_state_to_path(runtime, &state, &spec.output)?;
        return Ok(key);
    }

    let current_key = repo.file_key(&file.tag)?;
    if key != current_key {
        return Err(ErrCtx::PragmaErr(
            format!(
                "exporting worktree path `{key}` requires opening that database path or passing --source"
            )
            .into(),
        ));
    }

    let reader = file.reader()?;
    write_volume_reader_to_path(&reader, &spec.output)?;
    Ok(key)
}

pub(super) fn update_worktree_state_after_index_restore_key(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    key: &str,
) -> Result<(), ErrCtx> {
    if key != repo.file_key(&file.tag)? {
        repo.clear_dirty_key(key)?;
        return Ok(());
    }

    let worktree_state = current_repo_file_state(runtime, file)?;
    let index_state = repo.index_files()?.get(key).cloned();
    let matches_index = match index_state.as_ref() {
        Some(index_state) => repo_file_state_content_eq(runtime, &worktree_state, index_state)?,
        None => false,
    };
    if matches_index {
        repo.clear_dirty_key(key)?;
    } else {
        repo.mark_dirty_key(key.to_string())?;
    }
    Ok(())
}

pub(super) fn update_restored_worktree_state_key(
    runtime: &Runtime,
    repo: &Repository,
    key: &str,
    restored: Option<&RestoredRepoPathState>,
) -> Result<(), ErrCtx> {
    let index_state = repo.index_files()?.get(key).cloned();
    let index_artifact = repo.index_artifacts()?.get(key).cloned();
    let matches_index = match (restored, index_state.as_ref(), index_artifact.as_ref()) {
        (Some(RestoredRepoPathState::File(restored)), Some(index_state), None) => {
            repo_file_state_content_eq(runtime, restored, index_state)?
        }
        (Some(RestoredRepoPathState::Artifact(restored)), None, Some(index_artifact)) => {
            restored == index_artifact
        }
        (None, None, None) => true,
        _ => false,
    };

    if matches_index {
        repo.clear_dirty_key(key)?;
    } else if restored.is_none() {
        repo.mark_deleted_key(key.to_string())?;
    } else {
        repo.mark_dirty_key(key.to_string())?;
    }
    Ok(())
}

pub(super) fn checkout_repo_file_state(
    runtime: &Runtime,
    file: &mut VolFile,
    state: &CommitFileState,
    remote: Option<Arc<Remote>>,
) -> Result<(), ErrCtx> {
    let snapshot = state.snapshot.to_snapshot();
    let volume = if snapshot.is_empty() {
        runtime.volume_open(None, None, None)?
    } else {
        hydrate_repo_file_state(runtime, state, remote)?;
        runtime.volume_from_snapshot(&snapshot)?
    };
    file.switch_volume(&volume.vid)?;
    Ok(())
}

pub(super) fn checkout_repo_file_state_to_path(
    runtime: &Runtime,
    repo: &Repository,
    state: &CommitFileState,
    path: &Path,
    remote: Option<Arc<Remote>>,
) -> Result<(), ErrCtx> {
    let key = repo.file_key(path)?;
    let path = repo.worktree().join(key);
    if let Ok(metadata) = std::fs::symlink_metadata(&path)
        && !metadata.file_type().is_file()
    {
        return Err(ErrCtx::PragmaErr(
            format!(
                "path `{}` is not a regular SQLite database file",
                path.display()
            )
            .into(),
        ));
    }

    hydrate_repo_file_state(runtime, state, remote)?;
    write_repo_file_state_to_path(runtime, state, &path)
}

pub(super) fn checkout_repo_file_state_to_key(
    runtime: &Runtime,
    repo: &Repository,
    key: &str,
    state: &CommitFileState,
    remote: Option<Arc<Remote>>,
) -> Result<(), ErrCtx> {
    let path = repo.worktree().join(key);
    if let Ok(metadata) = std::fs::symlink_metadata(&path)
        && !metadata.file_type().is_file()
    {
        return Err(ErrCtx::PragmaErr(
            format!(
                "path `{}` is not a regular SQLite database file",
                path.display()
            )
            .into(),
        ));
    }

    hydrate_repo_file_state(runtime, state, remote)?;
    write_repo_file_state_to_path(runtime, state, &path)
}

pub(super) fn write_empty_sqlite_file_to_path(path: &Path) -> Result<(), ErrCtx> {
    write_sqlite_file_to_path(path, |_| Ok(()))
}

pub(super) fn write_volume_reader_to_path<R: VolumeRead>(
    reader: &R,
    path: &Path,
) -> Result<(), ErrCtx> {
    write_sqlite_file_to_path(path, |output| {
        for page_idx in reader.page_count().iter() {
            let page = reader.read_page(page_idx)?;
            output.write_all(page.as_ref())?;
        }
        Ok(())
    })
}

pub(super) fn write_sqlite_file_to_path(
    path: &Path,
    mut write_contents: impl FnMut(&mut File) -> Result<(), ErrCtx>,
) -> Result<(), ErrCtx> {
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && !metadata.file_type().is_file()
    {
        return Err(ErrCtx::PragmaErr(
            format!(
                "path `{}` is not a regular SQLite database file",
                path.display()
            )
            .into(),
        ));
    }

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let started_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    for attempt in 0..100 {
        let tmp = parent.join(format!(
            ".graft-checkout-{}-{started_ms}-{attempt}",
            std::process::id()
        ));
        if tmp.exists() {
            continue;
        }

        let write_result = (|| -> Result<(), ErrCtx> {
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            write_contents(&mut output)?;
            output.flush()?;
            Ok(())
        })();

        match write_result.and_then(|()| {
            std::fs::rename(&tmp, path)?;
            Ok(())
        }) {
            Ok(()) => return Ok(()),
            Err(err) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(err);
            }
        }
    }

    Err(ErrCtx::PragmaErr(
        format!(
            "could not create temporary checkout file for `{}`",
            path.display()
        )
        .into(),
    ))
}

pub(super) fn write_repo_file_state_to_path(
    runtime: &Runtime,
    state: &CommitFileState,
    path: &Path,
) -> Result<(), ErrCtx> {
    let snapshot = state.snapshot.to_snapshot();
    if snapshot.is_empty() {
        return write_empty_sqlite_file_to_path(path);
    }
    let volume = runtime.volume_from_snapshot(&snapshot)?;
    let reader = runtime.volume_reader(volume.vid)?;
    write_volume_reader_to_path(&reader, path)
}

pub(super) fn checkout_merge_outcome(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    outcome: &MergeOutcome,
    fast_forward_plan: Option<&CheckoutPlan>,
    previous_files: &BTreeMap<String, CommitFileState>,
    previous_artifacts: &BTreeMap<String, graft::repo::CommitArtifactState>,
    remote: Option<Arc<Remote>>,
) -> Result<(), ErrCtx> {
    match outcome {
        MergeOutcome::FastForward { .. } => {
            if let Some(plan) = fast_forward_plan {
                checkout_repo_plan(
                    runtime,
                    file,
                    repo,
                    plan,
                    previous_files,
                    previous_artifacts,
                    remote,
                )?;
            } else {
                checkout_repo_head(runtime, file, repo, remote)?;
            }
        }
        MergeOutcome::Merged { staged, conflicted, .. } if conflicted.is_empty() => {
            let key = repo.file_key(&file.tag)?;
            let index = repo.read_index()?;
            for entry in index.stage0_entries() {
                if !staged.iter().any(|path| path == &entry.path) {
                    continue;
                }

                if entry.path == key {
                    if let Some(state) = &entry.file {
                        checkout_repo_file_state(runtime, file, state, remote.clone())?;
                    } else if let Some(state) = &entry.artifact {
                        repo.materialize_artifact_key(&entry.path, state)?;
                    } else {
                        let volume = runtime.volume_open(None, None, None)?;
                        file.switch_volume(&volume.vid)?;
                    }
                } else if let Some(state) = &entry.file {
                    checkout_repo_file_state_to_path(
                        runtime,
                        repo,
                        state,
                        &repo.worktree().join(&entry.path),
                        remote.clone(),
                    )?;
                } else if let Some(state) = &entry.artifact {
                    repo.materialize_artifact_key(&entry.path, state)?;
                } else {
                    remove_materialized_repo_file(repo, &entry.path)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}
