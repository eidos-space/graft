use super::*;

pub(super) fn conflict_side_state(
    repo: &Repository,
    key: &str,
    side: ResolveSide,
) -> Result<RepoConflictSideState, ErrCtx> {
    let Some(stage) = side.index_stage() else {
        return Err(ErrCtx::PragmaErr(
            "manual resolution does not have an index conflict stage".into(),
        ));
    };
    let index = repo.read_index()?;
    if !index
        .entries
        .iter()
        .any(|entry| entry.path == key && entry.stage != graft::repo::index::IndexStage::Normal)
    {
        return Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotConflicted(
            key.to_string(),
        )));
    }
    let Some(entry) = index
        .entries
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
    file: &mut VolFile,
    repo: &Repository,
    spec: RepoResolveSpec,
) -> Result<RepoResolveConflictOutcome, ErrCtx> {
    let path = spec.path.unwrap_or_else(|| PathBuf::from(&file.tag));
    let (key, physical_path) = repo_physical_path_arg(repo, &path)?;
    let (path_kind, path_storage) = conflict_path_descriptor(repo, &key)?;
    let current_key = repo.file_key(&file.tag)?;
    if let Some(row) = spec.row.as_ref() {
        let path = resolve_repo_row_conflict(
            runtime,
            file,
            repo,
            &key,
            &physical_path,
            &current_key,
            spec.side,
            row,
        )?;
        return Ok(RepoResolveConflictOutcome { path, path_kind, path_storage });
    }
    if matches!(spec.side, ResolveSide::Ours | ResolveSide::Theirs) {
        if let Some(state) = row_resolved_conflict_file_state(runtime, repo, &key, spec.side)? {
            if key == current_key {
                checkout_repo_file_state(runtime, file, &state, None)?;
            } else {
                checkout_repo_file_state_to_path(runtime, repo, &state, &physical_path, None)?;
            }
            let entry = repo.resolve_file_conflict(&physical_path, Some(state))?;
            clear_row_conflict_resolution_state(repo)?;
            return Ok(RepoResolveConflictOutcome {
                path: entry.path,
                path_kind,
                path_storage,
            });
        }
    }
    let state = match spec.side {
        ResolveSide::Ours | ResolveSide::Theirs => {
            match conflict_side_state(repo, &key, spec.side)? {
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
                    let entry = repo.resolve_artifact_conflict(&physical_path, Some(state))?;
                    clear_row_conflict_resolution_state(repo)?;
                    return Ok(RepoResolveConflictOutcome {
                        path: entry.path,
                        path_kind,
                        path_storage,
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
            clear_row_conflict_resolution_state(repo)?;
            return Ok(RepoResolveConflictOutcome {
                path: entry.path,
                path_kind,
                path_storage,
            });
        }
        ResolveSide::Manual if physical_path.exists() => {
            Some(import_physical_sqlite_file_state(runtime, &physical_path)?)
        }
        ResolveSide::Manual => None,
    };
    let entry = repo.resolve_file_conflict(&physical_path, state)?;
    clear_row_conflict_resolution_state(repo)?;
    Ok(RepoResolveConflictOutcome {
        path: entry.path,
        path_kind,
        path_storage,
    })
}

pub(super) fn resolve_repo_row_conflict(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    key: &str,
    physical_path: &Path,
    current_key: &str,
    side: ResolveSide,
    row: &RepoResolveRowSpec,
) -> Result<String, ErrCtx> {
    if side == ResolveSide::Manual {
        return Err(ErrCtx::PragmaErr(
            "row conflict resolution requires `--ours` or `--theirs`".into(),
        ));
    }

    let status = repo.status()?;
    let mut resolution_state =
        read_row_conflict_resolution_state(repo, status.merge_head.as_deref())?;
    let Some((base, ours, theirs)) = current_file_conflict_states(repo, key)? else {
        return Err(ErrCtx::PragmaErr(
            format!("path `{key}` has no row conflict stages").into(),
        ));
    };
    let remote = repo_default_remote_store(repo);
    hydrate_repo_file_state_for(runtime, &base, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &ours, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &theirs, remote, RepoSnapshotPurpose::Merge)?;

    let plan = plan_repo_snapshot_merge(runtime, repo, &base, &ours, &theirs)?;
    if !plan.schema_conflicts().is_empty() || plan.has_opaque_changes() {
        return Err(ErrCtx::PragmaErr(
            "row conflict resolution is not available with schema or opaque conflicts".into(),
        ));
    }
    let requested_conflict = plan
        .analysis
        .conflicts
        .iter()
        .find(|conflict| conflict.table == row.table && conflict.rowid == row.rowid);
    let Some(requested_conflict) = requested_conflict else {
        return Err(ErrCtx::PragmaErr(
            format!(
                "path `{key}` has no row conflict for {} rowid={}",
                row.table, row.rowid
            )
            .into(),
        ));
    };
    if requested_conflict.reason == crate::row_merge::RowMergeConflictReason::SemanticKey {
        return Err(ErrCtx::PragmaErr(
            format!(
                "semantic key conflict for {} rowid={} requires manual file resolution",
                row.table, row.rowid
            )
            .into(),
        ));
    }

    resolution_state.rows.insert(
        row_conflict_resolution_key(key, &row.table, row.rowid),
        side.label().to_string(),
    );
    let merged = materialize_row_conflict_resolution_state(
        runtime,
        repo,
        key,
        &ours,
        &plan,
        &resolution_state,
    )?;
    if key == current_key {
        checkout_repo_file_state(runtime, file, &merged, None)?;
    } else {
        checkout_repo_file_state_to_path(runtime, repo, &merged, physical_path, None)?;
    }

    let unresolved = unresolved_row_conflict_count(key, &plan, &resolution_state);
    if unresolved == 0 {
        let entry = repo.resolve_file_conflict(physical_path, Some(merged))?;
        clear_row_conflict_resolution_state(repo)?;
        return Ok(entry.path);
    }

    write_row_conflict_resolution_state(repo, &resolution_state)?;
    Ok(key.to_string())
}

pub(super) fn row_resolved_conflict_file_state(
    runtime: &Runtime,
    repo: &Repository,
    key: &str,
    side: ResolveSide,
) -> Result<Option<CommitFileState>, ErrCtx> {
    let Some((base, ours, theirs)) = current_file_conflict_states(repo, key)? else {
        return Ok(None);
    };
    let remote = repo_default_remote_store(repo);
    hydrate_repo_file_state_for(runtime, &base, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &ours, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &theirs, remote, RepoSnapshotPurpose::Merge)?;

    let plan = plan_repo_snapshot_merge(runtime, repo, &base, &ours, &theirs)?;
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
        let selection_key = row_conflict_resolution_key(key, &conflict.table, conflict.rowid);
        let Some(selection) = resolution_state.rows.get(&selection_key) else {
            continue;
        };
        let Some(side) = row_merge_side_from_label(selection) else {
            continue;
        };
        let Some(row_sql) = plan.conflict_apply_sql(side, &conflict.table, conflict.rowid) else {
            return Err(ErrCtx::PragmaErr(
                format!(
                    "could not generate row resolution for {} rowid={}",
                    conflict.table, conflict.rowid
                )
                .into(),
            ));
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
            !resolution_state
                .rows
                .contains_key(&row_conflict_resolution_key(
                    key,
                    &conflict.table,
                    conflict.rowid,
                ))
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
    let config = repo.config()?;
    let mut policy = crate::row_merge::RowMergePolicy::default();
    policy.default_semantic_keys = config.merge.default_semantic_keys;
    policy.semantic_keys = config.merge.semantic_keys;
    for (subject, resolver) in config.merge.internal_resolvers {
        let Some(resolver) = crate::row_merge::RowMergeInternalResolver::from_str(&resolver) else {
            continue;
        };
        if internal_resolver_allowed_for_subject(&subject, resolver) {
            policy.internal_resolvers.insert(subject, resolver);
        }
    }
    for (operation, resolver) in config.merge.schema_resolvers {
        if let Some(resolver) = crate::row_merge::RowMergeSchemaResolver::from_str(&resolver) {
            policy.schema_resolvers.insert(operation, resolver);
        }
    }
    policy.generated_columns = config.merge.generated_columns;
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
    repo: &Repository,
    base: &CommitFileState,
    ours: &CommitFileState,
    theirs: &CommitFileState,
) -> Result<crate::row_merge::RowMergePlan, ErrCtx> {
    let policy = row_merge_policy_for_repo(repo)?;
    Ok(crate::row_merge::plan_snapshot_merge_with_policy(
        runtime, base, ours, theirs, &policy,
    )?)
}

pub(super) fn row_conflict_resolution_key(path: &str, table: &str, rowid: i64) -> String {
    format!("{path}\u{1f}{table}\u{1f}{rowid}")
}

pub(super) fn row_conflict_resolution_state_path(repo: &Repository) -> PathBuf {
    repo.worktree()
        .join(".graft")
        .join("row-conflict-resolutions.json")
}

pub(super) fn read_row_conflict_resolution_state(
    repo: &Repository,
    merge_head: Option<&str>,
) -> Result<RowConflictResolutionState, ErrCtx> {
    let path = row_conflict_resolution_state_path(repo);
    let state = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str::<RowConflictResolutionState>(&raw).map_err(|err| {
            ErrCtx::PragmaErr(
                format!(
                    "could not parse row conflict resolution state `{}`: {err}",
                    path.display()
                )
                .into(),
            )
        })?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            RowConflictResolutionState::default()
        }
        Err(err) => return Err(err.into()),
    };
    let merge_head = merge_head.map(str::to_string);
    if state.merge_head == merge_head {
        Ok(state)
    } else {
        Ok(RowConflictResolutionState { merge_head, rows: BTreeMap::new() })
    }
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
        ErrCtx::PragmaErr(format!("could not encode row conflict resolution state: {err}").into())
    })?;
    std::fs::write(path, raw)?;
    Ok(())
}

pub(super) fn clear_row_conflict_resolution_state(repo: &Repository) -> Result<(), ErrCtx> {
    match std::fs::remove_file(row_conflict_resolution_state_path(repo)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}
