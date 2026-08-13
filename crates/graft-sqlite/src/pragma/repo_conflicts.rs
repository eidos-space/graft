use super::*;

pub(crate) fn prepare_repo_semantic_merge_seed(
    runtime: &Runtime,
    file: &RepositorySessionContext,
    repo: &Repository,
    path: &Path,
    candidate_path: &Path,
    managed_tables: &BTreeSet<String>,
) -> Result<(bool, usize), ErrCtx> {
    let (key, _) = repo_physical_path_arg(repo, path)?;
    let Some((base, ours, theirs)) = current_file_conflict_states(repo, &key)? else {
        return Err(ErrCtx::InvalidCommand(
            format!("path `{key}` has no three-way SQLite conflict state").into(),
        ));
    };
    let remote = repo_default_remote_store(repo);
    hydrate_repo_file_state_for(runtime, &base, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &ours, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &theirs, remote, RepoSnapshotPurpose::Merge)?;
    let plan = plan_repo_snapshot_merge(runtime, file, repo, &base, &ours, &theirs)?;
    if !plan.schema_conflicts().is_empty()
        || plan.has_schema_additions()
        || plan.has_opaque_changes()
        || !plan.limitations().is_empty()
        || plan.requires_validation()
    {
        return Err(ErrCtx::InvalidCommand(
            "semantic provider seed is blocked by schema, opaque, limited, or recomputation-required SQLite changes"
                .into(),
        ));
    }
    let unmanaged_conflicts = plan.conflict_count_outside(managed_tables);
    if unmanaged_conflicts > 0 {
        return Err(ErrCtx::InvalidCommand(
            format!(
                "semantic provider seed has {unmanaged_conflicts} unresolved conflict(s) outside provider-managed tables"
            )
            .into(),
        ));
    }
    if candidate_path.exists() {
        return Err(ErrCtx::InvalidCommand(
            "semantic provider result path already exists before seed construction".into(),
        ));
    }
    write_repo_file_state_to_path(runtime, &ours, candidate_path)?;
    let sql = plan.theirs_apply_sql_excluding(managed_tables);
    let applied_sql = !sql.trim().is_empty();
    if applied_sql && let Err(error) = apply_row_merge_sql_to_path(candidate_path, &sql) {
        let _ = std::fs::remove_file(candidate_path);
        return Err(error);
    }
    Ok((applied_sql, plan.conflict_count_inside(managed_tables)))
}

pub(super) fn conflict_side_state(
    repo: &Repository,
    key: &str,
    side: ResolveSide,
    resolution_state: &RowConflictResolutionState,
) -> Result<RepoConflictSideState, ErrCtx> {
    let Some(stage) = side.index_stage() else {
        return Err(ErrCtx::InvalidCommand(
            "manual resolution does not have an index conflict stage".into(),
        ));
    };
    let entries = original_conflict_entries(repo, resolution_state, key)?;
    if entries.is_empty() {
        return Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotConflicted(
            key.to_string(),
        )));
    }
    let Some(entry) = entries
        .iter()
        .find(|entry| entry.path == key && entry.stage == stage)
    else {
        return Ok(RepoConflictSideState::Deleted);
    };
    if let Some(file) = &entry.file {
        Ok(RepoConflictSideState::SqliteDatabase(file.clone()))
    } else if let Some(artifact) = &entry.artifact {
        Ok(RepoConflictSideState::Artifact(artifact.clone()))
    } else {
        Ok(RepoConflictSideState::Deleted)
    }
}

pub(super) fn resolve_repo_conflict_for_file(
    runtime: &Runtime,
    file: &mut RepositorySessionContext,
    repo: &Repository,
    spec: RepoResolveSpec,
) -> Result<RepoResolveConflictOutcome, ErrCtx> {
    let path = spec.path.unwrap_or_else(|| PathBuf::from(&file.tag));
    let (key, physical_path) = repo_physical_path_arg(repo, &path)?;
    let current_key = repo.file_key(&file.tag)?;
    let status = repo.status()?;
    let resolution_state = read_row_conflict_resolution_state(repo, &status)?;
    if !resolution_state.paths.contains_key(&key) {
        return Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotConflicted(key)));
    }
    let original_entries = original_conflict_entries(repo, &resolution_state, &key)?;
    let (path_kind, path_storage) =
        conflict_path_descriptor_from_original_entries(&original_entries);
    if let Some(row) = spec.row.as_ref() {
        let (path, materialized) = resolve_repo_row_conflict(
            runtime,
            file,
            repo,
            &key,
            &physical_path,
            &current_key,
            spec.side,
            row,
        )?;
        return Ok(RepoResolveConflictOutcome {
            path,
            path_kind,
            path_storage,
            materialized,
        });
    }
    if matches!(spec.side, ResolveSide::Ours | ResolveSide::Theirs)
        && let Some(state) = row_resolved_conflict_file_state(
            runtime,
            file,
            repo,
            &key,
            spec.side,
            &resolution_state,
        )?
    {
        if key == current_key {
            checkout_repo_file_state(runtime, file, &state, None)?;
        } else {
            checkout_repo_file_state_to_path(runtime, repo, &state, &physical_path, None)?;
        }
        let entry = stage_resolved_file_state(repo, &physical_path, state)?;
        set_merge_path_resolution(repo, &key, Some(spec.side.label()))?;
        return Ok(RepoResolveConflictOutcome {
            path: entry.path,
            path_kind,
            path_storage,
            materialized: true,
        });
    }
    let state = match spec.side {
        ResolveSide::Ours | ResolveSide::Theirs => {
            match conflict_side_state(repo, &key, spec.side, &resolution_state)? {
                RepoConflictSideState::SqliteDatabase(state) => {
                    if key == current_key {
                        checkout_repo_file_state(runtime, file, &state, None)?;
                    } else {
                        checkout_repo_file_state_to_path(
                            runtime,
                            repo,
                            &state,
                            &physical_path,
                            None,
                        )?;
                    }
                    Some(state)
                }
                RepoConflictSideState::Artifact(state) => {
                    if key == current_key {
                        let volume = runtime.volume_open(None, None, None)?;
                        file.switch_volume(&volume.vid)?;
                    }
                    repo.materialize_artifact_key(&key, &state)?;
                    let entry = stage_resolved_artifact_state(repo, &physical_path, state)?;
                    set_merge_path_resolution(repo, &key, Some(spec.side.label()))?;
                    return Ok(RepoResolveConflictOutcome {
                        path: entry.path,
                        path_kind,
                        path_storage,
                        materialized: true,
                    });
                }
                RepoConflictSideState::Deleted => {
                    if key == current_key {
                        let volume = runtime.volume_open(None, None, None)?;
                        file.switch_volume(&volume.vid)?;
                    } else {
                        remove_materialized_repo_file(repo, &key)?;
                    }
                    None
                }
            }
        }
        ResolveSide::Manual if key == current_key => Some(current_repo_file_state(runtime, file)?),
        ResolveSide::Manual
            if physical_path.exists() && !is_sqlite_database_path(&physical_path)? =>
        {
            let entry = repo.resolve_artifact_conflict_from_path(&physical_path)?;
            set_merge_path_resolution(repo, &key, Some("manual"))?;
            return Ok(RepoResolveConflictOutcome {
                path: entry.path,
                path_kind,
                path_storage,
                materialized: false,
            });
        }
        ResolveSide::Manual if physical_path.exists() => Some(import_physical_sqlite_file_state(
            runtime,
            &physical_path,
            None,
        )?),
        ResolveSide::Manual => None,
    };
    let entry = if let Some(state) = state {
        stage_resolved_file_state(repo, &physical_path, state)?
    } else {
        stage_resolved_deletion(repo, &physical_path)?
    };
    set_merge_path_resolution(repo, &key, Some(spec.side.label()))?;
    Ok(RepoResolveConflictOutcome {
        path: entry.path,
        path_kind,
        path_storage,
        materialized: false,
    })
}

pub(super) fn resolve_repo_row_conflict(
    runtime: &Runtime,
    file: &mut RepositorySessionContext,
    repo: &Repository,
    key: &str,
    physical_path: &Path,
    current_key: &str,
    side: ResolveSide,
    row: &RepoResolveRowSpec,
) -> Result<(String, bool), ErrCtx> {
    if side == ResolveSide::Manual {
        return Err(ErrCtx::InvalidCommand(
            "row conflict resolution requires `--ours` or `--theirs`".into(),
        ));
    }

    let status = repo.status()?;
    let mut resolution_state = read_row_conflict_resolution_state(repo, &status)?;
    let Some((base, ours, theirs)) = original_file_conflict_states(repo, &resolution_state, key)?
    else {
        return Err(ErrCtx::InvalidCommand(
            format!("path `{key}` has no row conflict stages").into(),
        ));
    };
    let remote = repo_default_remote_store(repo);
    hydrate_repo_file_state_for(runtime, &base, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &ours, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &theirs, remote, RepoSnapshotPurpose::Merge)?;

    let plan = plan_repo_snapshot_merge(runtime, file, repo, &base, &ours, &theirs)?;
    if !plan.schema_conflicts().is_empty() || plan.has_opaque_changes() {
        return Err(ErrCtx::InvalidCommand(
            "row conflict resolution is not available with schema or opaque conflicts".into(),
        ));
    }
    let requested_conflict = plan.analysis.conflicts.iter().find(|conflict| {
        conflict.table == row.table && row_identities_match(&conflict.identity, &row.identity)
    });
    let Some(requested_conflict) = requested_conflict else {
        return Err(ErrCtx::InvalidCommand(
            format!(
                "path `{key}` has no row conflict for {} {}",
                row.table,
                row_identity_label(&row.identity)
            )
            .into(),
        ));
    };
    if requested_conflict.reason == crate::row_merge::RowMergeConflictReason::SemanticKey {
        return Err(ErrCtx::InvalidCommand(
            format!(
                "semantic key conflict for {} {} requires manual file resolution",
                row.table,
                row_identity_label(&row.identity)
            )
            .into(),
        ));
    }

    resolution_state.rows.insert(
        row_conflict_resolution_key(key, &row.table, &requested_conflict.identity),
        side.label().to_string(),
    );
    if let Some(path) = resolution_state.paths.get_mut(key) {
        path.resolution = None;
    }
    let unresolved = unresolved_row_conflict_count(key, &plan, &resolution_state);
    if unresolved > 0 {
        write_row_conflict_resolution_state(repo, &resolution_state)?;
        return Ok((key.to_string(), false));
    }

    let merged = materialize_row_conflict_resolution_state(
        runtime,
        repo,
        key,
        &ours,
        &plan,
        &resolution_state,
    )?;
    let materialized = merged != ours;
    if materialized {
        if key == current_key {
            checkout_repo_file_state(runtime, file, &merged, None)?;
        } else {
            checkout_repo_file_state_to_path(runtime, repo, &merged, physical_path, None)?;
        }
    }

    if !plan.requires_validation() {
        let entry = stage_resolved_file_state(repo, physical_path, merged)?;
        write_row_conflict_resolution_state(repo, &resolution_state)?;
        return Ok((entry.path, materialized));
    }

    write_row_conflict_resolution_state(repo, &resolution_state)?;
    Ok((key.to_string(), materialized))
}

pub(crate) fn resolve_repo_cell_conflict(
    runtime: &Runtime,
    file: &mut RepositorySessionContext,
    repo: &Repository,
    path: &Path,
    side: ResolveSide,
    cell: &RepoResolveCellSpec,
) -> Result<(String, bool), ErrCtx> {
    if side == ResolveSide::Manual {
        return Err(ErrCtx::InvalidCommand(
            "cell conflict resolution requires `ours` or `theirs`".into(),
        ));
    }
    let (key, physical_path) = repo_physical_path_arg(repo, path)?;
    let status = repo.status()?;
    let mut resolution_state = read_row_conflict_resolution_state(repo, &status)?;
    let Some((base, ours, theirs)) = original_file_conflict_states(repo, &resolution_state, &key)?
    else {
        return Err(ErrCtx::InvalidCommand(
            format!("path `{key}` has no SQLite row conflict stages").into(),
        ));
    };
    let remote = repo_default_remote_store(repo);
    hydrate_repo_file_state_for(runtime, &base, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &ours, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &theirs, remote, RepoSnapshotPurpose::Merge)?;
    let plan = plan_repo_snapshot_merge(runtime, file, repo, &base, &ours, &theirs)?;
    let requested = plan.analysis.conflicts.iter().find(|conflict| {
        conflict.reason == crate::row_merge::RowMergeConflictReason::Cell
            && conflict.table == cell.table
            && row_identities_match(&conflict.identity, &cell.identity)
            && conflict
                .cell_conflicts
                .iter()
                .any(|conflict| conflict.column.eq_ignore_ascii_case(&cell.column))
    });
    let Some(requested) = requested else {
        return Err(ErrCtx::InvalidCommand(
            format!(
                "path `{key}` has no cell conflict for {} {} column `{}`",
                cell.table,
                row_identity_label(&cell.identity),
                cell.column
            )
            .into(),
        ));
    };
    let canonical_column = requested
        .cell_conflicts
        .iter()
        .find(|conflict| conflict.column.eq_ignore_ascii_case(&cell.column))
        .map(|conflict| conflict.column.clone())
        .expect("requested cell conflict was found");
    let selection_key =
        cell_conflict_resolution_key(&key, &cell.table, &requested.identity, &canonical_column);
    if resolution_state
        .cells
        .get(&selection_key)
        .map(String::as_str)
        == Some(side.label())
    {
        return Ok((key, false));
    }
    resolution_state.rows.remove(&row_conflict_resolution_key(
        &key,
        &cell.table,
        &requested.identity,
    ));
    resolution_state
        .cells
        .insert(selection_key, side.label().to_string());
    if let Some(path) = resolution_state.paths.get_mut(&key) {
        path.resolution = None;
    }
    let unresolved = unresolved_row_conflict_count(&key, &plan, &resolution_state);
    if unresolved > 0 {
        write_row_conflict_resolution_state(repo, &resolution_state)?;
        return Ok((key, false));
    }

    let merged = materialize_row_conflict_resolution_state(
        runtime,
        repo,
        &key,
        &ours,
        &plan,
        &resolution_state,
    )?;
    let materialized = merged != ours;
    if materialized {
        let current_key = repo.file_key(&file.tag)?;
        if key == current_key {
            checkout_repo_file_state(runtime, file, &merged, None)?;
        } else {
            checkout_repo_file_state_to_path(runtime, repo, &merged, &physical_path, None)?;
        }
    }
    if !plan.requires_validation() {
        stage_resolved_file_state(repo, &physical_path, merged)?;
    }
    write_row_conflict_resolution_state(repo, &resolution_state)?;
    Ok((key, materialized))
}

pub(crate) fn stage_repo_worktree_sqlite_result(
    runtime: &Runtime,
    repo: &Repository,
    path: &Path,
) -> Result<String, ErrCtx> {
    let (key, physical_path) = repo_physical_path_arg(repo, path)?;
    let status = repo.status()?;
    let resolution_state = read_row_conflict_resolution_state(repo, &status)?;
    if !resolution_state.paths.contains_key(&key) {
        return Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotConflicted(key)));
    }
    let entries = original_conflict_entries(repo, &resolution_state, &key)?;
    if !entries.iter().any(|entry| entry.file.is_some()) {
        return Err(ErrCtx::InvalidCommand(
            format!("path `{key}` is not a tracked SQLite merge path").into(),
        ));
    }
    if !physical_path.exists() || !is_sqlite_database_path(&physical_path)? {
        return Err(ErrCtx::InvalidCommand(
            format!("path `{key}` is not a readable SQLite worktree result").into(),
        ));
    }

    // Capture first, then validate that exact immutable state in a private temporary database.
    // No index, worktree, or resolution-journal state changes before all checks succeed.
    let captured = import_physical_sqlite_file_state(runtime, &physical_path, None)?;
    let validated = materialize_row_auto_merge_state(
        runtime,
        repo,
        &key,
        &captured,
        "BEGIN TRANSACTION;\nCOMMIT;\n",
    )?;
    stage_resolved_file_state(repo, &physical_path, validated)?;
    set_merge_path_resolution(repo, &key, Some("edited"))?;
    Ok(key)
}

pub(crate) fn stage_repo_external_sqlite_result(
    runtime: &Runtime,
    repo: &Repository,
    path: &Path,
    candidate_path: &Path,
) -> Result<String, ErrCtx> {
    let (key, physical_path) = repo_physical_path_arg(repo, path)?;
    let status = repo.status()?;
    let resolution_state = read_row_conflict_resolution_state(repo, &status)?;
    if !resolution_state.paths.contains_key(&key) {
        return Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotConflicted(key)));
    }
    let entries = original_conflict_entries(repo, &resolution_state, &key)?;
    if !entries.iter().any(|entry| entry.file.is_some()) {
        return Err(ErrCtx::InvalidCommand(
            format!("path `{key}` is not a tracked SQLite merge path").into(),
        ));
    }
    if !candidate_path.is_file() || !is_sqlite_database_path(candidate_path)? {
        return Err(ErrCtx::InvalidCommand(
            format!(
                "semantic merge result `{}` is not a readable SQLite database",
                candidate_path.display()
            )
            .into(),
        ));
    }

    // Capture and validate the provider-owned file before touching the application worktree.
    let captured = import_physical_sqlite_file_state(runtime, candidate_path, None)?;
    let validated = materialize_row_auto_merge_state(
        runtime,
        repo,
        &key,
        &captured,
        "BEGIN TRANSACTION;\nCOMMIT;\n",
    )?;

    // Replacement uses the normal SQLite checkout boundary: it writes a private temporary file,
    // validates the destination kind, and renames only after the complete snapshot is available.
    checkout_repo_file_state_to_path(runtime, repo, &validated, &physical_path, None)?;
    stage_resolved_file_state(repo, &physical_path, validated)?;
    set_merge_path_resolution(repo, &key, Some("semantic_provider"))?;
    Ok(key)
}

pub(super) fn resolve_repo_table_conflicts(
    runtime: &Runtime,
    file: &mut RepositorySessionContext,
    repo: &Repository,
    path: &Path,
    table: &str,
    side: ResolveSide,
) -> Result<RepoResolveConflictOutcome, ErrCtx> {
    if side == ResolveSide::Manual {
        return Err(ErrCtx::InvalidCommand(
            "table conflict resolution requires `ours` or `theirs`".into(),
        ));
    }
    let (key, physical_path) = repo_physical_path_arg(repo, path)?;
    let status = repo.status()?;
    let mut resolution_state = read_row_conflict_resolution_state(repo, &status)?;
    let Some((base, ours, theirs)) = original_file_conflict_states(repo, &resolution_state, &key)?
    else {
        return Err(ErrCtx::InvalidCommand(
            format!("path `{key}` has no SQLite row conflict stages").into(),
        ));
    };
    let remote = repo_default_remote_store(repo);
    hydrate_repo_file_state_for(runtime, &base, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &ours, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &theirs, remote, RepoSnapshotPurpose::Merge)?;
    let plan = plan_repo_snapshot_merge(runtime, file, repo, &base, &ours, &theirs)?;
    if !plan.schema_conflicts().is_empty() {
        return Err(ErrCtx::InvalidCommand(
            format!(
                "cannot resolve table `{table}` for `{key}`: the SQLite merge has schema conflicts"
            )
            .into(),
        ));
    }
    if plan.has_opaque_changes() {
        return Err(ErrCtx::InvalidCommand(
            format!(
                "cannot resolve table `{table}` for `{key}`: the SQLite merge has opaque conflicts"
            )
            .into(),
        ));
    }
    let table_conflicts = plan
        .analysis
        .conflicts
        .iter()
        .filter(|conflict| conflict.table == table)
        .collect::<Vec<_>>();
    if table_conflicts.is_empty() {
        return Err(ErrCtx::InvalidCommand(
            format!("path `{key}` has no row conflicts in table `{table}`").into(),
        ));
    }
    if let Some(conflict) = table_conflicts
        .iter()
        .find(|conflict| conflict.reason == crate::row_merge::RowMergeConflictReason::SemanticKey)
    {
        return Err(ErrCtx::InvalidCommand(
            format!(
                "cannot resolve table `{table}` for `{key}`: semantic key conflict {} requires manual file resolution",
                row_identity_label(&conflict.identity)
            )
            .into(),
        ));
    }

    for conflict in table_conflicts {
        resolution_state.rows.insert(
            row_conflict_resolution_key(&key, table, &conflict.identity),
            side.label().to_string(),
        );
    }
    if let Some(path) = resolution_state.paths.get_mut(&key) {
        path.resolution = None;
    }
    let unresolved = unresolved_row_conflict_count(&key, &plan, &resolution_state);
    if unresolved > 0 {
        write_row_conflict_resolution_state(repo, &resolution_state)?;
        let (path_kind, path_storage) = conflict_path_descriptor(repo, &key)?;
        return Ok(RepoResolveConflictOutcome {
            path: key,
            path_kind,
            path_storage,
            materialized: false,
        });
    }

    let merged = materialize_row_conflict_resolution_state(
        runtime,
        repo,
        &key,
        &ours,
        &plan,
        &resolution_state,
    )?;
    graft::repo::cancellation_checkpoint()?;
    let materialized = merged != ours;
    if materialized {
        let current_key = repo.file_key(&file.tag)?;
        if key == current_key {
            checkout_repo_file_state(runtime, file, &merged, None)?;
        } else {
            checkout_repo_file_state_to_path(runtime, repo, &merged, &physical_path, None)?;
        }
    }
    if !plan.requires_validation() {
        stage_resolved_file_state(repo, &physical_path, merged)?;
    }
    write_row_conflict_resolution_state(repo, &resolution_state)?;
    let (path_kind, path_storage) = conflict_path_descriptor(repo, &key)?;
    Ok(RepoResolveConflictOutcome {
        path: key,
        path_kind,
        path_storage,
        materialized,
    })
}

pub(super) fn unresolve_repo_conflict_for_file(
    runtime: &Runtime,
    file: &mut RepositorySessionContext,
    repo: &Repository,
    path: &Path,
) -> Result<RepoResolveConflictOutcome, ErrCtx> {
    let (key, physical_path) = repo_physical_path_arg(repo, path)?;
    let status = repo.status()?;
    let mut resolution_state = read_row_conflict_resolution_state(repo, &status)?;
    let entries = original_conflict_entries(repo, &resolution_state, &key)?;
    if entries.is_empty() {
        return Err(ErrCtx::InvalidCommand(
            format!("path `{key}` has no resolve-undo record in the active merge").into(),
        ));
    }
    let (path_kind, path_storage) = conflict_path_descriptor_from_original_entries(&entries);
    let current_key = repo.file_key(&file.tag)?;
    let ours = entries
        .iter()
        .find(|entry| entry.stage == graft::repo::index::IndexStage::Ours);
    graft::repo::cancellation_checkpoint()?;
    if entries.iter().any(|entry| entry.file.is_some()) {
        let candidate = row_resolved_conflict_file_state(
            runtime,
            file,
            repo,
            &key,
            ResolveSide::Ours,
            &resolution_state,
        )?
        .or_else(|| ours.and_then(|entry| entry.file.clone()));
        if let Some(candidate) = candidate {
            if key == current_key {
                checkout_repo_file_state(runtime, file, &candidate, None)?;
            } else {
                checkout_repo_file_state_to_path(runtime, repo, &candidate, &physical_path, None)?;
            }
        } else if key == current_key {
            let volume = runtime.volume_open(None, None, None)?;
            file.switch_volume(&volume.vid)?;
        } else {
            remove_materialized_repo_file(repo, &key)?;
        }
    } else if let Some(artifact) = ours.and_then(|entry| entry.artifact.as_ref()) {
        if key == current_key {
            let volume = runtime.volume_open(None, None, None)?;
            file.switch_volume(&volume.vid)?;
        }
        repo.materialize_artifact_key(&key, artifact)?;
    } else if key == current_key {
        let volume = runtime.volume_open(None, None, None)?;
        file.switch_volume(&volume.vid)?;
    } else {
        remove_materialized_repo_file(repo, &key)?;
    }

    repo.restore_merge_conflict_stages(&physical_path, &entries)?;
    clear_path_row_resolutions(&mut resolution_state, &key);
    if let Some(path) = resolution_state.paths.get_mut(&key) {
        path.resolution = None;
    }
    write_row_conflict_resolution_state(repo, &resolution_state)?;
    Ok(RepoResolveConflictOutcome {
        path: key,
        path_kind,
        path_storage,
        materialized: true,
    })
}

fn conflict_path_descriptor_from_original_entries(
    entries: &[graft::repo::index::IndexEntry],
) -> (RepoTrackedPathKind, RepoPathStorage) {
    for entry in entries {
        if entry.file.is_some() {
            return (
                RepoTrackedPathKind::SqliteDatabase,
                RepoPathStorage::SqliteSnapshot,
            );
        }
        if let Some(artifact) = &entry.artifact {
            return (
                artifact_checkout_path_kind(artifact),
                artifact_checkout_path_storage(artifact),
            );
        }
    }
    (RepoTrackedPathKind::BinaryFile, RepoPathStorage::Inline)
}

fn stage_resolved_file_state(
    repo: &Repository,
    physical_path: &Path,
    state: CommitFileState,
) -> Result<graft::repo::index::IndexEntry, ErrCtx> {
    let key = repo.file_key(physical_path)?;
    if repo
        .read_index()?
        .conflicted_paths()
        .iter()
        .any(|path| path == &key)
    {
        Ok(repo.resolve_file_conflict(physical_path, Some(state))?)
    } else {
        Ok(repo.stage_file_state_path(physical_path, state)?)
    }
}

fn stage_resolved_artifact_state(
    repo: &Repository,
    physical_path: &Path,
    state: CommitArtifactState,
) -> Result<graft::repo::index::IndexEntry, ErrCtx> {
    let key = repo.file_key(physical_path)?;
    if repo
        .read_index()?
        .conflicted_paths()
        .iter()
        .any(|path| path == &key)
    {
        Ok(repo.resolve_artifact_conflict(physical_path, Some(state))?)
    } else {
        let entry = graft::repo::index::IndexEntry {
            path: key,
            mode: Some(graft::repo::object::TreeEntryMode::Regular),
            oid: Some(state.oid().clone()),
            stage: graft::repo::index::IndexStage::Normal,
            file: None,
            artifact: Some(state),
        };
        repo.stage_index_entries(std::slice::from_ref(&entry))?;
        Ok(entry)
    }
}

fn stage_resolved_deletion(
    repo: &Repository,
    physical_path: &Path,
) -> Result<graft::repo::index::IndexEntry, ErrCtx> {
    let key = repo.file_key(physical_path)?;
    if repo
        .read_index()?
        .conflicted_paths()
        .iter()
        .any(|path| path == &key)
    {
        Ok(repo.resolve_file_conflict(physical_path, None)?)
    } else {
        let entry = graft::repo::index::IndexEntry {
            path: key,
            mode: None,
            oid: None,
            stage: graft::repo::index::IndexStage::Normal,
            file: None,
            artifact: None,
        };
        repo.stage_index_entries(std::slice::from_ref(&entry))?;
        Ok(entry)
    }
}

pub(super) fn original_file_conflict_states(
    repo: &Repository,
    resolution_state: &RowConflictResolutionState,
    key: &str,
) -> Result<Option<(CommitFileState, CommitFileState, CommitFileState)>, ErrCtx> {
    let entries = original_conflict_entries(repo, resolution_state, key)?;
    let mut base = None;
    let mut ours = None;
    let mut theirs = None;
    for entry in entries {
        match entry.stage {
            graft::repo::index::IndexStage::Base => base = entry.file,
            graft::repo::index::IndexStage::Ours => ours = entry.file,
            graft::repo::index::IndexStage::Theirs => theirs = entry.file,
            graft::repo::index::IndexStage::Normal => {}
        }
    }
    Ok(match (base, ours, theirs) {
        (Some(base), Some(ours), Some(theirs)) => Some((base, ours, theirs)),
        _ => None,
    })
}

fn row_identities_match(
    left: &crate::row_level_diff::RowIdentity,
    right: &crate::row_level_diff::RowIdentity,
) -> bool {
    match (left, right) {
        (
            crate::row_level_diff::RowIdentity::Rowid(left),
            crate::row_level_diff::RowIdentity::Rowid(right),
        ) => left == right,
        (
            crate::row_level_diff::RowIdentity::PrimaryKey(left),
            crate::row_level_diff::RowIdentity::PrimaryKey(right),
        ) => {
            left.len() == right.len()
                && left.iter().all(|left_part| {
                    right.iter().any(|right_part| {
                        left_part.column == right_part.column && left_part.value == right_part.value
                    })
                })
        }
        _ => false,
    }
}

pub(super) fn row_resolved_conflict_file_state(
    runtime: &Runtime,
    file: &RepositorySessionContext,
    repo: &Repository,
    key: &str,
    side: ResolveSide,
    resolution_state: &RowConflictResolutionState,
) -> Result<Option<CommitFileState>, ErrCtx> {
    let Some((base, ours, theirs)) = original_file_conflict_states(repo, resolution_state, key)?
    else {
        return Ok(None);
    };
    let remote = repo_default_remote_store(repo);
    hydrate_repo_file_state_for(runtime, &base, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &ours, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &theirs, remote, RepoSnapshotPurpose::Merge)?;

    let plan = plan_repo_snapshot_merge(runtime, file, repo, &base, &ours, &theirs)?;
    if !plan.analysis.has_conflicts()
        || !plan.schema_conflicts().is_empty()
        || plan.has_opaque_changes()
    {
        return Ok(None);
    }

    let (base_state, sql) = match side {
        ResolveSide::Ours => (&ours, plan.theirs_apply_sql()),
        ResolveSide::Theirs => (&theirs, plan.ours_apply_sql()),
        ResolveSide::Manual => return Ok(None),
    };
    materialize_row_auto_merge_state(runtime, repo, key, base_state, &sql).map(Some)
}

pub(super) fn materialize_row_conflict_resolution_state(
    runtime: &Runtime,
    repo: &Repository,
    key: &str,
    ours: &CommitFileState,
    plan: &crate::row_merge::RowMergePlan,
    resolution_state: &RowConflictResolutionState,
) -> Result<CommitFileState, ErrCtx> {
    let mut sql = plan.theirs_apply_sql();
    for conflict in &plan.analysis.conflicts {
        let selection_key = row_conflict_resolution_key(key, &conflict.table, &conflict.identity);
        let row_sql = if let Some(selection) = resolution_state.rows.get(&selection_key) {
            let Some(side) = row_merge_side_from_label(selection) else {
                continue;
            };
            if side == crate::row_merge::RowMergeSide::Ours {
                None
            } else {
                plan.conflict_apply_sql(side, &conflict.table, &conflict.identity)
            }
        } else if conflict.reason == crate::row_merge::RowMergeConflictReason::Cell {
            let selections = conflict
                .cell_conflicts
                .iter()
                .filter_map(|cell| {
                    let key = cell_conflict_resolution_key(
                        key,
                        &conflict.table,
                        &conflict.identity,
                        &cell.column,
                    );
                    resolution_state
                        .cells
                        .get(&key)
                        .and_then(|selection| row_merge_side_from_label(selection))
                        .map(|side| (cell.column.clone(), side))
                })
                .collect::<BTreeMap<_, _>>();
            if selections
                .values()
                .all(|side| *side == crate::row_merge::RowMergeSide::Ours)
            {
                None
            } else {
                plan.cell_resolution_apply_sql(&conflict.table, &conflict.identity, &selections)
            }
        } else {
            None
        };
        let Some(row_sql) = row_sql else {
            continue;
        };
        sql.push('\n');
        sql.push_str(&row_sql);
    }
    materialize_row_auto_merge_state(runtime, repo, key, ours, &sql)
}

pub(super) fn unresolved_row_conflict_count(
    key: &str,
    plan: &crate::row_merge::RowMergePlan,
    resolution_state: &RowConflictResolutionState,
) -> usize {
    plan.analysis
        .conflicts
        .iter()
        .filter(|conflict| {
            let row_key = row_conflict_resolution_key(key, &conflict.table, &conflict.identity);
            if resolution_state.rows.contains_key(&row_key) {
                return false;
            }
            conflict.reason != crate::row_merge::RowMergeConflictReason::Cell
                || conflict.cell_conflicts.iter().any(|cell| {
                    !resolution_state
                        .cells
                        .contains_key(&cell_conflict_resolution_key(
                            key,
                            &conflict.table,
                            &conflict.identity,
                            &cell.column,
                        ))
                })
        })
        .count()
}

pub(super) fn row_merge_side_from_label(label: &str) -> Option<crate::row_merge::RowMergeSide> {
    match label {
        "ours" => Some(crate::row_merge::RowMergeSide::Ours),
        "theirs" => Some(crate::row_merge::RowMergeSide::Theirs),
        _ => None,
    }
}

pub(super) fn row_merge_policy_for_repo(
    repo: &Repository,
) -> Result<crate::row_merge::RowMergePolicy, ErrCtx> {
    let status = repo.status()?;
    let merge = if status.merge_head.is_some() {
        read_row_conflict_resolution_state(repo, &status)?.merge_policy
    } else {
        repo.config()?.merge.effective()
    };
    row_merge_policy_from_config(&merge)
}

pub(crate) fn active_merge_policy(
    repo: &Repository,
) -> Result<Option<(graft::repo::MergeConfig, String, u32)>, ErrCtx> {
    let status = repo.status()?;
    if status.merge_head.is_none() {
        return Ok(None);
    }
    let state = read_row_conflict_resolution_state(repo, &status)?;
    Ok(Some((
        state.merge_policy,
        state.policy_token,
        state.policy_version,
    )))
}

fn row_merge_policy_from_config(
    merge: &graft::repo::MergeConfig,
) -> Result<crate::row_merge::RowMergePolicy, ErrCtx> {
    merge.validate()?;
    let mut policy = crate::row_merge::RowMergePolicy {
        same_row_merge: merge.same_row_merge,
        default_semantic_keys: merge.default_semantic_keys.clone(),
        semantic_keys: merge.semantic_keys.clone(),
        semantic_key_collations: merge.semantic_key_collations.clone(),
        column_resolvers: merge.column_resolvers.clone(),
        generated_columns: merge.generated_columns.clone(),
        ..Default::default()
    };
    for (subject, resolver) in &merge.internal_resolvers {
        let Some(resolver) = crate::row_merge::RowMergeInternalResolver::from_str(resolver) else {
            continue;
        };
        if internal_resolver_allowed_for_subject(subject, resolver) {
            policy.internal_resolvers.insert(subject.clone(), resolver);
        }
    }
    for (operation, resolver) in &merge.schema_resolvers {
        if let Some(resolver) = crate::row_merge::RowMergeSchemaResolver::from_str(resolver) {
            policy.schema_resolvers.insert(operation.clone(), resolver);
        }
    }
    Ok(policy)
}

pub(super) fn internal_resolver_allowed_for_subject(
    subject: &str,
    resolver: crate::row_merge::RowMergeInternalResolver,
) -> bool {
    match subject {
        "sqlite_sequence" => resolver == crate::row_merge::RowMergeInternalResolver::SequenceMax,
        "sqlite_stat1" | "sqlite_stat2" | "sqlite_stat3" | "sqlite_stat4" => {
            resolver == crate::row_merge::RowMergeInternalResolver::Rebuild
        }
        "index_btree" => resolver == crate::row_merge::RowMergeInternalResolver::Reindex,
        _ => false,
    }
}

pub(super) fn plan_repo_snapshot_merge(
    runtime: &Runtime,
    file: &RepositorySessionContext,
    repo: &Repository,
    base: &CommitFileState,
    ours: &CommitFileState,
    theirs: &CommitFileState,
) -> Result<Arc<crate::row_merge::RowMergePlan>, ErrCtx> {
    let policy = row_merge_policy_for_repo(repo)?;
    let cache_key = format!(
        "{}\u{1f}{policy:?}",
        serde_json::to_string(&(base, ours, theirs))
            .map_err(|error| ErrCtx::InvalidCommand(error.to_string().into()))?
    );
    if let Some(plan) = file.row_merge_plan(&cache_key) {
        return Ok(plan);
    }
    let plan = Arc::new(crate::row_merge::plan_snapshot_merge_with_policy(
        runtime, base, ours, theirs, &policy,
    )?);
    file.cache_row_merge_plan(cache_key, Arc::clone(&plan));
    Ok(plan)
}

pub(super) fn row_conflict_resolution_key(
    path: &str,
    table: &str,
    identity: &crate::row_level_diff::RowIdentity,
) -> String {
    format!("{path}\u{1f}{table}\u{1f}{}", row_identity_token(identity))
}

pub(super) fn cell_conflict_resolution_key(
    path: &str,
    table: &str,
    identity: &crate::row_level_diff::RowIdentity,
    column: &str,
) -> String {
    format!(
        "{}\u{1f}{column}",
        row_conflict_resolution_key(path, table, identity)
    )
}

pub(super) fn row_identity_label(identity: &crate::row_level_diff::RowIdentity) -> String {
    match identity {
        crate::row_level_diff::RowIdentity::Rowid(rowid) => format!("rowid={rowid}"),
        crate::row_level_diff::RowIdentity::PrimaryKey(key) => {
            let values = key
                .iter()
                .map(|part| format!("{}={:?}", part.column, part.value.to_value()))
                .collect::<Vec<_>>()
                .join(", ");
            format!("key=({values})")
        }
    }
}

fn row_identity_token(identity: &crate::row_level_diff::RowIdentity) -> String {
    match identity {
        crate::row_level_diff::RowIdentity::Rowid(rowid) => format!("rowid:{rowid}"),
        crate::row_level_diff::RowIdentity::PrimaryKey(key) => {
            let key = key
                .iter()
                .map(|part| {
                    (
                        part.column.clone(),
                        crate::json::JsonRowChange::primary_key_value_to_json(&part.value),
                    )
                })
                .collect::<BTreeMap<_, _>>();
            format!(
                "key:{}",
                serde_json::to_string(&key).expect("primary key serializes")
            )
        }
    }
}

pub(super) fn row_conflict_resolution_state_path(repo: &Repository) -> PathBuf {
    repo.worktree()
        .join(".graft")
        .join("merge-resolution-session.json")
}

pub(super) fn read_row_conflict_resolution_state(
    repo: &Repository,
    status: &RepoStatus,
) -> Result<RowConflictResolutionState, ErrCtx> {
    let path = row_conflict_resolution_state_path(repo);
    let mut state = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str::<RowConflictResolutionState>(&raw).map_err(|err| {
            ErrCtx::InvalidCommand(
                format!(
                    "could not parse row conflict resolution state `{}`: {err}",
                    path.display()
                )
                .into(),
            )
        })?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            merge_resolution_state_from_index(repo, status)?
        }
        Err(err) => return Err(err.into()),
    };
    if state.orig_head != status.orig_head || state.merge_head != status.merge_head {
        return merge_resolution_state_from_index(repo, status);
    }
    if state.schema_version == 1 {
        let policy = repo.config()?.merge.effective();
        policy.validate()?;
        state.schema_version = 2;
        state.policy_token = policy.policy_token();
        state.policy_version = graft::repo::MERGE_POLICY_VERSION;
        state.merge_policy = policy;
    }
    if state.schema_version != 2
        || state.policy_version != graft::repo::MERGE_POLICY_VERSION
        || state.policy_token != state.merge_policy.policy_token()
    {
        return Err(ErrCtx::InvalidCommand(
            "active merge has an invalid or unsupported frozen merge policy".into(),
        ));
    }
    state.merge_policy.validate()?;
    Ok(state)
}

pub(super) fn initialize_merge_resolution_state(repo: &Repository) -> Result<(), ErrCtx> {
    let status = repo.status()?;
    if status.merge_head.is_none() {
        clear_row_conflict_resolution_state(repo)?;
        return Ok(());
    }
    let state = merge_resolution_state_from_index(repo, &status)?;
    write_row_conflict_resolution_state(repo, &state)
}

fn merge_resolution_state_from_index(
    repo: &Repository,
    status: &RepoStatus,
) -> Result<RowConflictResolutionState, ErrCtx> {
    let index = repo.read_index()?;
    let mut paths = BTreeMap::<String, MergeResolutionPathState>::new();
    for entry in index
        .entries
        .iter()
        .filter(|entry| entry.stage != graft::repo::index::IndexStage::Normal)
    {
        paths
            .entry(entry.path.clone())
            .or_default()
            .original_entries
            .push(entry.clone());
    }
    let merge_policy = repo.config()?.merge.effective();
    merge_policy.validate()?;
    let policy_token = merge_policy.policy_token();
    Ok(RowConflictResolutionState {
        schema_version: 2,
        orig_head: status.orig_head.clone(),
        merge_head: status.merge_head.clone(),
        merge_policy,
        policy_token,
        policy_version: graft::repo::MERGE_POLICY_VERSION,
        paths,
        rows: BTreeMap::new(),
        cells: BTreeMap::new(),
        analysis_errors: BTreeMap::new(),
    })
}

pub(super) fn original_conflict_entries(
    repo: &Repository,
    state: &RowConflictResolutionState,
    key: &str,
) -> Result<Vec<graft::repo::index::IndexEntry>, ErrCtx> {
    if let Some(path) = state.paths.get(key) {
        return Ok(path.original_entries.clone());
    }
    Ok(repo
        .read_index()?
        .entries
        .into_iter()
        .filter(|entry| entry.path == key && entry.stage != graft::repo::index::IndexStage::Normal)
        .collect())
}

pub(super) fn clear_path_row_resolutions(state: &mut RowConflictResolutionState, key: &str) {
    let prefix = format!("{key}\u{1f}");
    state
        .rows
        .retain(|selection, _| !selection.starts_with(&prefix));
    state
        .cells
        .retain(|selection, _| !selection.starts_with(&prefix));
    state.analysis_errors.remove(key);
}

pub(super) fn set_merge_path_resolution(
    repo: &Repository,
    key: &str,
    resolution: Option<&str>,
) -> Result<(), ErrCtx> {
    let status = repo.status()?;
    let mut state = read_row_conflict_resolution_state(repo, &status)?;
    let Some(path) = state.paths.get_mut(key) else {
        return Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotConflicted(
            key.to_string(),
        )));
    };
    path.resolution = resolution.map(str::to_string);
    if resolution.is_some() {
        clear_path_row_resolutions(&mut state, key);
    }
    write_row_conflict_resolution_state(repo, &state)
}

pub(super) fn write_row_conflict_resolution_state(
    repo: &Repository,
    state: &RowConflictResolutionState,
) -> Result<(), ErrCtx> {
    let path = row_conflict_resolution_state_path(repo);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let raw = serde_json::to_string_pretty(state).map_err(|err| {
        ErrCtx::InvalidCommand(
            format!("could not encode row conflict resolution state: {err}").into(),
        )
    })?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, raw)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

pub(super) fn clear_row_conflict_resolution_state(repo: &Repository) -> Result<(), ErrCtx> {
    for path in [
        row_conflict_resolution_state_path(repo),
        repo.worktree()
            .join(".graft")
            .join("row-conflict-resolutions.json"),
    ] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}
