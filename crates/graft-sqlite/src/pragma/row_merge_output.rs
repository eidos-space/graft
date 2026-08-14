use super::*;

pub(super) fn append_row_merge_analysis(
    output: &mut String,
    runtime: &Runtime,
    file: &RepositorySessionContext,
    repo: &Repository,
    outcome: &MergeOutcome,
    remote: Option<Arc<Remote>>,
) -> Result<(), ErrCtx> {
    let MergeOutcome::Merged { conflicted, .. } = outcome else {
        return Ok(());
    };
    let Some(key) = selected_repository_database_key(file, repo)? else {
        return Ok(());
    };
    if !conflicted.iter().any(|path| path == &key) {
        return Ok(());
    }

    if !output.ends_with('\n') {
        output.push('\n');
    }
    match format_current_file_row_merge_analysis(runtime, file, repo, &key, remote) {
        Ok(Some(analysis)) => output.push_str(&analysis),
        Ok(None) => {}
        Err(err) => {
            writeln!(output, "Row-level analysis for {key} unavailable: {err}")?;
        }
    }
    Ok(())
}

pub(super) fn format_current_file_row_merge_analysis(
    runtime: &Runtime,
    file: &RepositorySessionContext,
    repo: &Repository,
    key: &str,
    remote: Option<Arc<Remote>>,
) -> Result<Option<String>, ErrCtx> {
    let index = repo.read_index()?;
    let mut base = None;
    let mut ours = None;
    let mut theirs = None;

    for entry in index.entries.iter().filter(|entry| entry.path == key) {
        match entry.stage {
            graft::repo::index::IndexStage::Base => base = entry.file.as_ref(),
            graft::repo::index::IndexStage::Ours => ours = entry.file.as_ref(),
            graft::repo::index::IndexStage::Theirs => theirs = entry.file.as_ref(),
            graft::repo::index::IndexStage::Normal => {}
        }
    }

    let (Some(base), Some(ours), Some(theirs)) = (base, ours, theirs) else {
        return Ok(Some(formatdoc!(
            "
            Row-level analysis for {key}:
              unavailable: merge involves add/delete of this tracked path.
            "
        )));
    };

    hydrate_repo_file_state_for(runtime, base, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, ours, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, theirs, remote, RepoSnapshotPurpose::Merge)?;
    let plan = plan_repo_snapshot_merge(runtime, file, repo, base, ours, theirs)?;
    let analysis = &plan.analysis;
    let mut f = String::new();
    writeln!(&mut f, "Row-level analysis for {key}:")?;
    writeln!(&mut f, "  ours: {} row change(s)", analysis.ours_changes)?;
    writeln!(
        &mut f,
        "  theirs: {} row change(s)",
        analysis.theirs_changes
    )?;
    if !plan.resolved_opaque_changes().is_empty() {
        writeln!(
            &mut f,
            "  resolved opaque change(s): {}",
            plan.resolved_opaque_changes().len()
        )?;
    }
    if plan.has_opaque_changes() {
        writeln!(
            &mut f,
            "  unresolved opaque change(s): {}",
            plan.opaque_changes()
        )?;
    }
    if !plan.schema_conflicts().is_empty() {
        writeln!(
            &mut f,
            "  schema conflict(s): {}",
            plan.schema_conflicts().len()
        )?;
    }
    if analysis.has_conflicts() {
        writeln!(&mut f, "  Row conflicts:")?;
        for conflict in &analysis.conflicts {
            writeln!(
                &mut f,
                "    {} {} (ours {}, theirs {})",
                conflict.table,
                row_identity_label(&conflict.identity),
                row_change_kind_label(conflict.ours),
                row_change_kind_label(conflict.theirs)
            )?;
        }
    } else if !plan.has_opaque_changes() && plan.schema_conflicts().is_empty() {
        writeln!(
            &mut f,
            "  No row conflicts detected; row-level auto-merge candidate."
        )?;
    } else {
        writeln!(&mut f, "  No row conflicts detected.")?;
    }
    Ok(Some(f))
}

pub(super) fn current_file_status_row_merge_analysis(
    runtime: &Runtime,
    file: &RepositorySessionContext,
    repo: &Repository,
    remote: Option<Arc<Remote>>,
) -> Result<Option<JsonRowMergeAnalysis>, ErrCtx> {
    let Some(key) = selected_repository_database_key(file, repo)? else {
        return Ok(None);
    };
    current_file_row_merge_analysis(runtime, file, repo, &key, remote)
}

pub(super) fn current_file_status_row_merge_analysis_lossy(
    runtime: &Runtime,
    file: &RepositorySessionContext,
    repo: &Repository,
    remote: Option<Arc<Remote>>,
) -> Option<JsonRowMergeAnalysis> {
    match current_file_status_row_merge_analysis(runtime, file, repo, remote) {
        Ok(analysis) => analysis,
        Err(err) => {
            let path = selected_repository_database_key(file, repo)
                .ok()
                .flatten()
                .unwrap_or_else(|| "db.sqlite3".to_string());
            Some(JsonRowMergeAnalysis {
                path,
                available: false,
                can_auto_merge: false,
                ours_changes: 0,
                theirs_changes: 0,
                apply_changes: 0,
                opaque_changes: 0,
                resolved_opaque_changes: 0,
                resolved_opaque_change_details: vec![],
                apply_policy: row_merge_apply_policy(&crate::row_merge::RowMergePolicy::default()),
                limitations: vec![],
                blocked_reasons: vec!["analysis_error"],
                row_conflicts: vec![],
                schema_conflicts: vec![],
                message: Some(format!("row-level analysis unavailable: {err}")),
            })
        }
    }
}

pub(super) fn current_file_row_merge_analysis(
    runtime: &Runtime,
    file: &RepositorySessionContext,
    repo: &Repository,
    key: &str,
    remote: Option<Arc<Remote>>,
) -> Result<Option<JsonRowMergeAnalysis>, ErrCtx> {
    let index = repo.read_index()?;
    if !index.conflicted_paths().iter().any(|path| path == key) {
        return Ok(None);
    }

    let Some((base, ours, theirs)) = current_file_conflict_states(repo, key)? else {
        return Ok(Some(JsonRowMergeAnalysis {
            path: key.to_string(),
            available: false,
            can_auto_merge: false,
            ours_changes: 0,
            theirs_changes: 0,
            apply_changes: 0,
            opaque_changes: 0,
            resolved_opaque_changes: 0,
            resolved_opaque_change_details: vec![],
            apply_policy: row_merge_apply_policy(&crate::row_merge::RowMergePolicy::default()),
            limitations: vec![],
            blocked_reasons: vec!["add_delete_conflict"],
            row_conflicts: vec![],
            schema_conflicts: vec![],
            message: Some("merge involves add/delete of this tracked path".to_string()),
        }));
    };

    hydrate_repo_file_state_for(runtime, &base, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &ours, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &theirs, remote, RepoSnapshotPurpose::Merge)?;

    let plan = plan_repo_snapshot_merge(runtime, file, repo, &base, &ours, &theirs)?;
    let row_conflicts: Vec<JsonRowMergeConflict> = plan
        .analysis
        .conflicts
        .iter()
        .map(|conflict| {
            let (rowid, key) = json_row_identity(&conflict.identity);
            let (ours_rowid, ours_key) = json_row_identity(&conflict.ours_identity);
            let (theirs_rowid, theirs_key) = json_row_identity(&conflict.theirs_identity);
            JsonRowMergeConflict {
                reason: conflict.reason.as_str(),
                table: conflict.table.clone(),
                columns: conflict.columns.clone(),
                rowid,
                key,
                ours_rowid,
                theirs_rowid,
                ours_key,
                theirs_key,
                semantic_key: conflict.semantic_key.clone(),
                semantic_key_collations: json_semantic_key_collations(
                    conflict.semantic_key_collations.as_deref(),
                ),
                cells: json_cell_conflicts(&conflict.cell_conflicts),
                ours: row_change_kind_label(conflict.ours),
                theirs: row_change_kind_label(conflict.theirs),
                base_row: json_record_values_opt(conflict.base_row.as_ref()),
                ours_row: json_record_values_opt(conflict.ours_row.as_ref()),
                theirs_row: json_record_values_opt(conflict.theirs_row.as_ref()),
            }
        })
        .collect();
    let schema_conflicts: Vec<JsonSchemaMergeConflict> = plan
        .schema_conflicts()
        .iter()
        .map(|conflict| JsonSchemaMergeConflict {
            reason: conflict.reason.as_str(),
            name: conflict.name.clone(),
            entry_type: conflict.entry_type.clone(),
            ours: conflict.ours.map(schema_change_kind_label),
            theirs: conflict.theirs.map(schema_change_kind_label),
            column_changes: json_schema_column_changes(&conflict.column_changes),
            message: conflict.message,
        })
        .collect();
    let apply_changes = plan.apply_change_count();
    let mut blocked_reasons = Vec::new();
    if !row_conflicts.is_empty() {
        blocked_reasons.push("row_conflicts");
    }
    if !schema_conflicts.is_empty() {
        blocked_reasons.push("schema_conflicts");
    }
    if plan.opaque_changes() > 0 {
        blocked_reasons.push("opaque_changes");
    }
    if !plan.limitations().is_empty() {
        blocked_reasons.push("analysis_limitations");
    }
    if apply_changes == 0 && !plan.can_resolve_to_ours_without_apply() {
        blocked_reasons.push("no_applicable_changes");
    }
    let can_auto_merge = blocked_reasons.is_empty();

    Ok(Some(JsonRowMergeAnalysis {
        path: key.to_string(),
        available: true,
        can_auto_merge,
        ours_changes: plan.analysis.ours_changes,
        theirs_changes: plan.analysis.theirs_changes,
        apply_changes,
        opaque_changes: plan.opaque_changes(),
        resolved_opaque_changes: plan.resolved_opaque_changes().len(),
        resolved_opaque_change_details: json_resolved_opaque_changes(
            plan.resolved_opaque_changes(),
        ),
        apply_policy: row_merge_apply_policy(plan.policy()),
        limitations: json_limitations(&plan.limitations()),
        blocked_reasons,
        row_conflicts,
        schema_conflicts,
        message: None,
    }))
}

pub(super) fn row_merge_apply_policy(
    policy: &crate::row_merge::RowMergePolicy,
) -> JsonRowMergeApplyPolicy {
    JsonRowMergeApplyPolicy {
        foreign_keys: "disabled_during_apply_checked_after",
        triggers: "disabled_during_apply",
        validation: vec!["integrity_check", "foreign_key_check"],
        same_row_merge: policy.same_row_merge,
        default_semantic_keys: policy.default_semantic_keys.clone(),
        semantic_keys: policy.semantic_keys.clone(),
        semantic_key_collations: policy.semantic_key_collations.clone(),
        internal_resolvers: json_internal_resolvers(policy),
        schema_resolvers: policy
            .schema_resolvers
            .iter()
            .map(|(operation, resolver)| JsonRowMergeSchemaResolver {
                operation: operation.clone(),
                resolver: resolver.as_str(),
            })
            .collect(),
        generated_columns: policy
            .generated_columns
            .iter()
            .map(|(table, columns)| JsonRowMergeGeneratedColumns {
                table: table.clone(),
                columns: columns.clone(),
            })
            .collect(),
        column_resolvers: policy.column_resolvers.clone(),
    }
}

pub(super) fn json_internal_resolvers(
    policy: &crate::row_merge::RowMergePolicy,
) -> Vec<JsonRowMergeInternalResolver> {
    policy
        .internal_resolvers
        .iter()
        .map(|(table, resolver)| JsonRowMergeInternalResolver {
            table: table.clone(),
            resolver: resolver.as_str(),
        })
        .collect()
}

pub(super) fn repo_conflict_artifacts(
    runtime: &Runtime,
    file: &RepositorySessionContext,
    repo: &Repository,
    remote: Option<Arc<Remote>>,
) -> Result<JsonConflictList, ErrCtx> {
    let status = repo.status()?;
    let resolution_state = read_row_conflict_resolution_state(repo, &status)?;
    let mut conflicts = Vec::new();
    let mut conflict_paths = status.conflicted.iter().cloned().collect::<BTreeSet<_>>();
    conflict_paths.extend(resolution_state.paths.keys().cloned());
    for path in &conflict_paths {
        conflicts.extend(repo_path_conflict_artifacts(
            runtime,
            file,
            repo,
            path,
            remote.clone(),
            &resolution_state,
        )?);
    }
    let paths = json_conflict_paths(&conflicts);
    let current_head = status.head_target.clone();
    let current_branch = repo.current_branch()?;
    Ok(JsonConflictList {
        current_head,
        current_branch,
        merge_head: status.merge_head,
        paths,
        conflicts,
    })
}

pub(super) fn json_conflict_paths(conflicts: &[JsonConflictArtifact]) -> Vec<JsonConflictPath> {
    #[derive(Clone, Copy)]
    struct Counts {
        kind: &'static str,
        storage: &'static str,
        total: usize,
        unresolved: usize,
        resolved: usize,
    }

    let mut by_path = BTreeMap::<String, Counts>::new();
    for conflict in conflicts {
        let entry = by_path.entry(conflict.path.clone()).or_insert(Counts {
            kind: conflict.path_kind,
            storage: conflict.storage,
            total: 0,
            unresolved: 0,
            resolved: 0,
        });
        entry.kind = conflict.path_kind;
        entry.storage = conflict.storage;
        entry.total += 1;
        if conflict.status == "resolved" {
            entry.resolved += 1;
        } else {
            entry.unresolved += 1;
        }
    }

    by_path
        .into_iter()
        .map(|(path, counts)| JsonConflictPath {
            path,
            kind: counts.kind,
            storage: counts.storage,
            status: if counts.unresolved == 0 {
                "resolved"
            } else {
                "unresolved"
            },
            total: counts.total,
            unresolved: counts.unresolved,
            resolved: counts.resolved,
        })
        .collect()
}

pub(super) fn unresolved_conflict_artifact_count(
    runtime: &Runtime,
    file: &RepositorySessionContext,
    repo: &Repository,
    remote: Option<Arc<Remote>>,
) -> Result<usize, ErrCtx> {
    Ok(repo_conflict_artifacts(runtime, file, repo, remote)?
        .conflicts
        .iter()
        .filter(|conflict| conflict.status == "unresolved")
        .count())
}

pub(super) fn conflict_path_kind(
    repo: &Repository,
    key: &str,
) -> Result<RepoTrackedPathKind, ErrCtx> {
    conflict_path_descriptor(repo, key).map(|(kind, _)| kind)
}

pub(super) fn conflict_path_storage(
    repo: &Repository,
    key: &str,
) -> Result<RepoPathStorage, ErrCtx> {
    conflict_path_descriptor(repo, key).map(|(_, storage)| storage)
}

pub(super) fn conflict_path_descriptor(
    repo: &Repository,
    key: &str,
) -> Result<(RepoTrackedPathKind, RepoPathStorage), ErrCtx> {
    let index = repo.read_index()?;
    for entry in index.entries.iter().filter(|entry| entry.path == key) {
        if entry.file.is_some() {
            return Ok((
                RepoTrackedPathKind::SqliteDatabase,
                RepoPathStorage::SqliteSnapshot,
            ));
        }
        if let Some(artifact) = &entry.artifact {
            return Ok((
                artifact_checkout_path_kind(artifact),
                artifact_checkout_path_storage(artifact),
            ));
        }
    }
    Ok((RepoTrackedPathKind::BinaryFile, RepoPathStorage::Inline))
}

pub(super) fn repo_path_conflict_artifacts(
    runtime: &Runtime,
    file: &RepositorySessionContext,
    repo: &Repository,
    key: &str,
    remote: Option<Arc<Remote>>,
    resolution_state: &RowConflictResolutionState,
) -> Result<Vec<JsonConflictArtifact>, ErrCtx> {
    let original_entries = original_conflict_entries(repo, resolution_state, key)?;
    let (path_kind, path_storage) = conflict_path_descriptor_from_entries(&original_entries);
    let path_kind_label = repo_tracked_path_kind_json_label(path_kind);
    let path_storage_label = repo_path_storage_json_label(path_storage);
    let path_resolution = resolution_state
        .paths
        .get(key)
        .and_then(|path| path.resolution.as_deref())
        .and_then(merge_resolution_label);
    let path_is_unmerged = repo
        .read_index()?
        .conflicted_paths()
        .iter()
        .any(|path| path == key);
    if path_is_unmerged && let Some(error) = resolution_state.analysis_errors.get(key) {
        let mut artifact = file_conflict_artifact(
            key,
            path_kind_label,
            path_storage_label,
            "validation",
            "candidate_validation_failed",
            Some(error.clone()),
        );
        artifact.auto_resolvable = Some(false);
        artifact.recommended_action = Some("inspect_candidate_constraints");
        return Ok(vec![artifact]);
    }
    let Some((base, ours, theirs)) = original_file_conflict_states(repo, resolution_state, key)?
    else {
        let mut artifact = file_conflict_artifact(
            key,
            path_kind_label,
            path_storage_label,
            "file",
            "add_delete_conflict",
            Some("merge involves add/delete of this tracked path".to_string()),
        );
        apply_path_resolution(&mut artifact, path_resolution);
        return Ok(vec![artifact]);
    };

    let result = (|| {
        hydrate_repo_file_state_for(runtime, &base, None, RepoSnapshotPurpose::Merge)?;
        hydrate_repo_file_state_for(runtime, &ours, None, RepoSnapshotPurpose::Merge)?;
        hydrate_repo_file_state_for(runtime, &theirs, remote, RepoSnapshotPurpose::Merge)?;
        let plan = plan_repo_snapshot_merge(runtime, file, repo, &base, &ours, &theirs)?;
        let mut artifacts = Vec::new();

        for conflict in &plan.analysis.conflicts {
            let row_resolution = path_resolution.or_else(|| {
                resolution_state
                    .rows
                    .get(&row_conflict_resolution_key(
                        key,
                        &conflict.table,
                        &conflict.identity,
                    ))
                    .and_then(|label| match label.as_str() {
                        "ours" => Some("ours"),
                        "theirs" => Some("theirs"),
                        _ => None,
                    })
            });
            let cells = conflict
                .cell_conflicts
                .iter()
                .map(|cell| JsonCellConflict {
                    column: cell.column.clone(),
                    base: crate::json::JsonRowChange::value_to_json(&cell.base),
                    ours: crate::json::JsonRowChange::value_to_json(&cell.ours),
                    theirs: crate::json::JsonRowChange::value_to_json(&cell.theirs),
                    resolution: resolution_state
                        .cells
                        .get(&cell_conflict_resolution_key(
                            key,
                            &conflict.table,
                            &conflict.identity,
                            &cell.column,
                        ))
                        .and_then(|selection| merge_resolution_label(selection)),
                })
                .collect::<Vec<_>>();
            let resolution = row_resolution.or_else(|| {
                (!cells.is_empty() && cells.iter().all(|cell| cell.resolution.is_some()))
                    .then_some("cells")
            });
            let artifact = JsonConflictArtifact {
                id: format!(
                    "{}:row:{}:{}",
                    key,
                    conflict.table,
                    row_identity_label(&conflict.identity)
                ),
                path: key.to_string(),
                path_kind: "sqlite_database",
                storage: path_storage_label,
                kind: "row",
                reason: conflict.reason.as_str(),
                status: if resolution.is_some() {
                    "resolved"
                } else {
                    "unresolved"
                },
                resolution,
                auto_resolvable: None,
                recommended_result: None,
                recommended_action: None,
                table: Some(conflict.table.clone()),
                columns: Some(conflict.columns.clone()).filter(|columns| !columns.is_empty()),
                rowid: conflict.identity.rowid(),
                ours_rowid: conflict.ours_identity.rowid(),
                theirs_rowid: conflict.theirs_identity.rowid(),
                key: json_row_identity(&conflict.identity).1,
                ours_key: json_row_identity(&conflict.ours_identity).1,
                theirs_key: json_row_identity(&conflict.theirs_identity).1,
                semantic_key: conflict.semantic_key.clone(),
                semantic_key_collations: json_semantic_key_collations(
                    conflict.semantic_key_collations.as_deref(),
                ),
                cells,
                name: None,
                entry_type: None,
                column_changes: Vec::new(),
                change: None,
                owner: None,
                ours_op: Some(row_change_kind_label(conflict.ours)),
                theirs_op: Some(row_change_kind_label(conflict.theirs)),
                base_row: json_record_values_opt(conflict.base_row.as_ref()),
                ours_row: json_record_values_opt(conflict.ours_row.as_ref()),
                theirs_row: json_record_values_opt(conflict.theirs_row.as_ref()),
                message: None,
            };
            artifacts.push(artifact);
        }

        for conflict in plan.schema_conflicts() {
            let mut artifact = JsonConflictArtifact {
                id: format!("{}:schema:{}:{}", key, conflict.entry_type, conflict.name),
                path: key.to_string(),
                path_kind: "sqlite_database",
                storage: path_storage_label,
                kind: "schema",
                reason: conflict.reason.as_str(),
                status: "unresolved",
                resolution: None,
                auto_resolvable: None,
                recommended_result: None,
                recommended_action: None,
                table: None,
                columns: None,
                rowid: None,
                ours_rowid: None,
                theirs_rowid: None,
                key: None,
                ours_key: None,
                theirs_key: None,
                semantic_key: None,
                semantic_key_collations: None,
                cells: Vec::new(),
                name: Some(conflict.name.clone()),
                entry_type: Some(conflict.entry_type.clone()),
                column_changes: json_schema_column_changes(&conflict.column_changes),
                change: None,
                owner: None,
                ours_op: conflict.ours.map(schema_change_kind_label),
                theirs_op: conflict.theirs.map(schema_change_kind_label),
                base_row: None,
                ours_row: None,
                theirs_row: None,
                message: Some(conflict.message.to_string()),
            };
            apply_path_resolution(&mut artifact, path_resolution);
            artifacts.push(artifact);
        }

        for change in plan.unresolved_opaque_changes() {
            let mut artifact = JsonConflictArtifact {
                id: format!("{}:opaque:{}:{}", key, change.reason.as_str(), change.name),
                path: key.to_string(),
                path_kind: "sqlite_database",
                storage: path_storage_label,
                kind: "opaque",
                reason: change.reason.as_str(),
                status: "unresolved",
                resolution: None,
                auto_resolvable: None,
                recommended_result: None,
                recommended_action: None,
                table: None,
                columns: None,
                rowid: None,
                ours_rowid: None,
                theirs_rowid: None,
                key: None,
                ours_key: None,
                theirs_key: None,
                semantic_key: None,
                semantic_key_collations: None,
                cells: Vec::new(),
                name: Some(change.name.clone()),
                entry_type: None,
                column_changes: Vec::new(),
                change: Some(change.change.as_str()),
                owner: change.owner.clone(),
                ours_op: None,
                theirs_op: None,
                base_row: None,
                ours_row: None,
                theirs_row: None,
                message: Some(opaque_conflict_message(change).to_string()),
            };
            apply_path_resolution(&mut artifact, path_resolution);
            artifacts.push(artifact);
        }

        for pending in plan.pending_recomputations() {
            let mut artifact = file_conflict_artifact(
                key,
                path_kind_label,
                path_storage_label,
                "validation",
                "recompute_required",
                Some(
                    "the merge candidate is materialized but these managed columns must be recomputed and the worktree SQLite result staged explicitly"
                        .to_string(),
                ),
            );
            artifact.id = format!(
                "{}:validation:{}:{}",
                key,
                pending.table,
                row_identity_label(&pending.identity)
            );
            artifact.table = Some(pending.table.clone());
            artifact.columns = Some(pending.columns.clone());
            artifact.rowid = pending.identity.rowid();
            artifact.key = json_row_identity(&pending.identity).1;
            artifact.recommended_action = Some("stage_worktree_result");
            apply_path_resolution(&mut artifact, path_resolution);
            artifacts.push(artifact);
        }

        if artifacts.is_empty() {
            if plan.can_resolve_to_ours_without_apply() {
                if path_is_unmerged || path_resolution.is_some() {
                    let (reason, message) = if plan.theirs_logical_status()
                        == crate::row_level_diff::LogicalDiffStatus::FileChangedNoSupportedLogicalChanges
                    {
                        (
                            "theirs_logically_equivalent_to_base",
                            "theirs has no supported logical SQLite changes relative to base; keeping ours preserves the complete supported merge result",
                        )
                    } else {
                        (
                            "merge_result_matches_ours",
                            "all supported changes from theirs are already represented by ours; no SQLite changes need to be applied",
                        )
                    };
                    let mut artifact = file_conflict_artifact(
                        key,
                        path_kind_label,
                        path_storage_label,
                        "file",
                        reason,
                        Some(message.to_string()),
                    );
                    artifact.auto_resolvable = Some(true);
                    artifact.recommended_result = Some("ours");
                    artifact.recommended_action = Some("apply_merge");
                    apply_path_resolution(&mut artifact, path_resolution);
                    artifacts.push(artifact);
                }
            } else if plan.apply_change_count() > 0 && path_is_unmerged {
                let mut artifact = file_conflict_artifact(
                    key,
                    path_kind_label,
                    path_storage_label,
                    "file",
                    "automatic_merge_available",
                    Some(
                        "a validated SQLite candidate can be materialized by applyMerge or continueMerge"
                            .to_string(),
                    ),
                );
                artifact.auto_resolvable = Some(true);
                artifact.recommended_result = Some("merged");
                artifact.recommended_action = Some("apply_merge");
                artifacts.push(artifact);
            } else {
                let mut artifact = file_conflict_artifact(
                    key,
                    path_kind_label,
                    path_storage_label,
                    "file",
                    "no_applicable_changes",
                    Some("no row or schema conflict details were produced".to_string()),
                );
                apply_path_resolution(&mut artifact, path_resolution);
                artifacts.push(artifact);
            }
        }

        Ok::<_, ErrCtx>(artifacts)
    })();

    match result {
        Ok(artifacts) => Ok(artifacts),
        Err(err) => {
            let mut artifact = file_conflict_artifact(
                key,
                path_kind_label,
                path_storage_label,
                "file",
                "analysis_error",
                Some(format!("row-level conflict analysis unavailable: {err}")),
            );
            apply_path_resolution(&mut artifact, path_resolution);
            Ok(vec![artifact])
        }
    }
}

fn conflict_path_descriptor_from_entries(
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

fn merge_resolution_label(label: &str) -> Option<&'static str> {
    match label {
        "ours" => Some("ours"),
        "theirs" => Some("theirs"),
        "manual" => Some("manual"),
        "edited" => Some("edited"),
        "cells" => Some("cells"),
        _ => None,
    }
}

fn apply_path_resolution(artifact: &mut JsonConflictArtifact, resolution: Option<&'static str>) {
    if resolution.is_some() {
        artifact.status = "resolved";
        artifact.resolution = resolution;
    }
}

pub(super) fn file_conflict_artifact(
    key: &str,
    path_kind: &'static str,
    path_storage: &'static str,
    kind: &'static str,
    reason: &'static str,
    message: Option<String>,
) -> JsonConflictArtifact {
    JsonConflictArtifact {
        id: format!("{key}:{kind}:{reason}"),
        path: key.to_string(),
        path_kind,
        storage: path_storage,
        kind,
        reason,
        status: "unresolved",
        resolution: None,
        auto_resolvable: None,
        recommended_result: None,
        recommended_action: None,
        table: None,
        columns: None,
        rowid: None,
        ours_rowid: None,
        theirs_rowid: None,
        key: None,
        ours_key: None,
        theirs_key: None,
        semantic_key: None,
        semantic_key_collations: None,
        cells: Vec::new(),
        name: None,
        entry_type: None,
        column_changes: Vec::new(),
        change: None,
        owner: None,
        ours_op: None,
        theirs_op: None,
        base_row: None,
        ours_row: None,
        theirs_row: None,
        message,
    }
}

pub(super) fn json_record_values_opt(
    record: Option<&crate::sqlite_parse::Record>,
) -> Option<Vec<serde_json::Value>> {
    record.map(|record| {
        record
            .values
            .iter()
            .map(crate::json::JsonRowChange::value_to_json)
            .collect()
    })
}

fn json_semantic_key_collations(
    collations: Option<&[graft::repo::SemanticKeyCollation]>,
) -> Option<Vec<&'static str>> {
    collations.map(|collations| {
        collations
            .iter()
            .map(|collation| match collation {
                graft::repo::SemanticKeyCollation::Binary => "binary",
                graft::repo::SemanticKeyCollation::NoCase => "nocase",
            })
            .collect()
    })
}

fn json_cell_conflicts(
    conflicts: &[crate::row_merge::RowMergeCellConflict],
) -> Vec<JsonCellConflict> {
    conflicts
        .iter()
        .map(|cell| JsonCellConflict {
            column: cell.column.clone(),
            base: crate::json::JsonRowChange::value_to_json(&cell.base),
            ours: crate::json::JsonRowChange::value_to_json(&cell.ours),
            theirs: crate::json::JsonRowChange::value_to_json(&cell.theirs),
            resolution: None,
        })
        .collect()
}

fn json_row_identity(
    identity: &crate::row_level_diff::RowIdentity,
) -> (Option<i64>, Option<BTreeMap<String, serde_json::Value>>) {
    match identity {
        crate::row_level_diff::RowIdentity::Rowid(rowid) => (Some(*rowid), None),
        crate::row_level_diff::RowIdentity::PrimaryKey(key) => (
            None,
            Some(
                key.iter()
                    .map(|part| {
                        (
                            part.column.clone(),
                            crate::json::JsonRowChange::primary_key_value_to_json(&part.value),
                        )
                    })
                    .collect(),
            ),
        ),
    }
}

pub(super) fn json_schema_column_changes(
    changes: &[crate::row_merge::SchemaMergeColumnChange],
) -> Vec<JsonSchemaColumnChange> {
    changes
        .iter()
        .map(|change| JsonSchemaColumnChange {
            side: change.side.as_str(),
            operation: change.operation.as_str(),
            from: change.from.clone(),
            to: change.to.clone(),
        })
        .collect()
}

pub(super) fn json_resolved_opaque_changes(
    changes: &[crate::row_merge::RowMergeResolvedOpaqueChange],
) -> Vec<JsonResolvedOpaqueChange> {
    changes
        .iter()
        .map(|change| JsonResolvedOpaqueChange {
            name: change.name.clone(),
            reason: change.reason.as_str(),
            resolver: change.resolver.as_str(),
        })
        .collect()
}

pub(super) fn opaque_conflict_message(
    change: &crate::row_level_diff::OpaqueChange,
) -> &'static str {
    match change.reason {
        crate::row_level_diff::OpaqueChangeReason::VirtualTable => {
            "virtual table changes require application-specific resolution"
        }
        crate::row_level_diff::OpaqueChangeReason::FtsShadowTable => {
            "FTS shadow table changes must be rebuilt or resolved with their owner table"
        }
        crate::row_level_diff::OpaqueChangeReason::SqliteInternalTable => {
            "SQLite internal table changes require an explicit resolver policy"
        }
        crate::row_level_diff::OpaqueChangeReason::IndexBtree => {
            "SQLite index B-tree changes require an explicit resolver policy"
        }
    }
}

pub(super) fn row_change_kind_label(kind: crate::row_merge::RowChangeKind) -> &'static str {
    match kind {
        crate::row_merge::RowChangeKind::Insert => "insert",
        crate::row_merge::RowChangeKind::Delete => "delete",
        crate::row_merge::RowChangeKind::Update => "update",
    }
}

pub(super) fn schema_change_kind_label(
    kind: crate::row_level_diff::SchemaChangeKind,
) -> &'static str {
    match kind {
        crate::row_level_diff::SchemaChangeKind::Added => "added",
        crate::row_level_diff::SchemaChangeKind::Deleted => "deleted",
        crate::row_level_diff::SchemaChangeKind::Modified => "modified",
    }
}

#[derive(Debug)]
pub(super) struct RowAutoMergeResult {
    pub(super) key: String,
    pub(super) applied_changes: usize,
    pub(super) ours_changes: usize,
    pub(super) theirs_changes: usize,
    pub(super) resolved: bool,
    pub(super) requires_validation: bool,
}

#[derive(Debug)]
struct PreparedRowMergeFile {
    path: PathBuf,
}

const MAX_ROW_MERGE_INTEGRITY_PROOFS: usize = 256;

/// Successful full-database checks for immutable Graft states in this process.
///
/// The cache is only a performance proof: clearing it causes another authoritative
/// `integrity_check`, and entries cannot be supplied by repository-controlled files.
static ROW_MERGE_INTEGRITY_PROOFS: OnceLock<
    Mutex<std::collections::HashSet<(VolumeId, RepoSnapshot)>>,
> = OnceLock::new();

impl Drop for PreparedRowMergeFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(super) fn try_row_auto_merge_current_file_conflict(
    runtime: &Runtime,
    file: &mut RepositorySessionContext,
    repo: &Repository,
    outcome: &MergeOutcome,
    remote: Option<Arc<Remote>>,
    physical_replacement_prepared: bool,
) -> Result<Option<RowAutoMergeResult>, ErrCtx> {
    let MergeOutcome::Merged { conflicted, .. } = outcome else {
        return Ok(None);
    };
    let Some(key) = selected_repository_database_key(file, repo)? else {
        return Ok(None);
    };
    if !conflicted.iter().any(|path| path == &key) {
        return Ok(None);
    }

    try_row_merge_current_file_status_conflict(
        runtime,
        file,
        repo,
        remote,
        true,
        physical_replacement_prepared,
    )
}

/// Resolves every conflict-free `SQLite` candidate in a merge outcome.
///
/// Directory-backed SDK sessions do not have one selected database file. This pass therefore
/// plans every conflicted `SQLite` path and materializes any required SQL into private temporary
/// databases before changing the index or worktree.
pub(super) fn try_row_auto_merge_conflicts(
    runtime: &Runtime,
    file: &mut RepositorySessionContext,
    repo: &Repository,
    outcome: &MergeOutcome,
    remote: Option<Arc<Remote>>,
    physical_replacement_prepared: bool,
) -> Result<Vec<RowAutoMergeResult>, ErrCtx> {
    let MergeOutcome::Merged { conflicted, .. } = outcome else {
        return Ok(Vec::new());
    };
    try_row_auto_merge_paths(
        runtime,
        file,
        repo,
        conflicted,
        remote,
        physical_replacement_prepared,
    )
}

/// Resolves the safe `SQLite` subset of `conflicted` after preparing every candidate first.
///
/// Planning first ensures that hydration or analysis failures cannot occur after an earlier path
/// has already been staged. Filesystem or index I/O may still fail while applying a prepared
/// result, just like an ordinary multi-path checkout.
pub(super) fn try_row_auto_merge_paths(
    runtime: &Runtime,
    file: &mut RepositorySessionContext,
    repo: &Repository,
    conflicted: &[String],
    remote: Option<Arc<Remote>>,
    physical_replacement_prepared: bool,
) -> Result<Vec<RowAutoMergeResult>, ErrCtx> {
    type PreparedCandidate = (
        CommitFileState,
        Option<PreparedRowMergeFile>,
        usize,
        usize,
        usize,
        bool,
    );

    let mut candidates = Vec::new();
    let mut analyzed = BTreeSet::new();
    let mut failures = BTreeMap::new();
    for key in conflicted {
        let prepared = (|| -> Result<Option<PreparedCandidate>, ErrCtx> {
            let Some((base, ours, theirs)) = current_file_conflict_states(repo, key)? else {
                return Ok(None);
            };
            hydrate_repo_file_state_for(runtime, &base, None, RepoSnapshotPurpose::Merge)?;
            hydrate_repo_file_state_for(runtime, &ours, None, RepoSnapshotPurpose::Merge)?;
            hydrate_repo_file_state_for(
                runtime,
                &theirs,
                remote.clone(),
                RepoSnapshotPurpose::Merge,
            )?;
            let plan = plan_repo_snapshot_merge(runtime, file, repo, &base, &ours, &theirs)?;
            if plan.has_conflicts() || plan.has_opaque_changes() || !plan.limitations().is_empty() {
                return Ok(None);
            }
            let applied_changes = plan.apply_change_count();
            let tables = plan
                .theirs_apply_table_summaries()
                .into_iter()
                .filter_map(|(name, inserts, deletes, updates)| {
                    table_summary(name, inserts, deletes, updates)
                })
                .collect::<Vec<_>>();
            let summary_from = ours.clone();
            let (merged, materialized) = if plan.can_resolve_to_ours_without_apply() {
                (ours, None)
            } else if applied_changes > 0 {
                let (merged, path) = materialize_row_auto_merge_candidate(
                    runtime,
                    repo,
                    key,
                    &ours,
                    &plan.theirs_apply_sql(),
                    physical_replacement_prepared
                        .then(|| repo.worktree().join(key))
                        .as_deref(),
                )?;
                (merged, Some(PreparedRowMergeFile { path }))
            } else {
                return Ok(None);
            };
            file.cache_row_merge_table_summaries(key.clone(), summary_from, merged.clone(), tables);
            Ok(Some((
                merged,
                materialized,
                applied_changes,
                plan.analysis.ours_changes,
                plan.analysis.theirs_changes,
                plan.requires_validation(),
            )))
        })();
        match prepared {
            Ok(Some((
                merged,
                materialized,
                applied_changes,
                ours_changes,
                theirs_changes,
                requires_validation,
            ))) => {
                analyzed.insert(key.clone());
                candidates.push((
                    key.clone(),
                    merged,
                    materialized,
                    applied_changes,
                    ours_changes,
                    theirs_changes,
                    requires_validation,
                ));
            }
            Ok(None) => {
                analyzed.insert(key.clone());
            }
            Err(error) => {
                failures.insert(key.clone(), error.to_string());
            }
        };
    }

    let status = repo.status()?;
    if status.merge_head.is_some() {
        let mut state = read_row_conflict_resolution_state(repo, &status)?;
        let mut changed = false;
        for key in analyzed {
            changed |= state.analysis_errors.remove(&key).is_some();
        }
        for (key, error) in failures {
            changed |= state.analysis_errors.get(&key) != Some(&error);
            state.analysis_errors.insert(key, error);
        }
        if changed {
            write_row_conflict_resolution_state(repo, &state)?;
        }
    }

    let mut resolved = Vec::new();
    for (
        key,
        merged,
        materialized,
        applied_changes,
        ours_changes,
        theirs_changes,
        requires_validation,
    ) in candidates
    {
        if let Some(materialized) = materialized
            && repo.file_key(&file.tag)? != key
        {
            install_materialized_repo_file_state(
                runtime,
                repo,
                &key,
                &merged,
                &materialized.path,
                physical_replacement_prepared,
            )?;
        } else {
            checkout_selected_repository_database(
                runtime,
                file,
                repo,
                &key,
                &merged,
                physical_replacement_prepared,
            )?;
        }
        if !requires_validation {
            repo.resolve_file_conflict(repo.worktree().join(&key), Some(merged))?;
        }
        resolved.push(RowAutoMergeResult {
            key,
            applied_changes,
            ours_changes,
            theirs_changes,
            resolved: !requires_validation,
            requires_validation,
        });
    }
    Ok(resolved)
}

pub(super) fn try_row_auto_merge_current_file_status_conflict(
    runtime: &Runtime,
    file: &mut RepositorySessionContext,
    repo: &Repository,
    remote: Option<Arc<Remote>>,
) -> Result<Option<RowAutoMergeResult>, ErrCtx> {
    try_row_merge_current_file_status_conflict(runtime, file, repo, remote, false, false)
}

pub(super) fn try_row_merge_current_file_status_conflict(
    runtime: &Runtime,
    file: &mut RepositorySessionContext,
    repo: &Repository,
    remote: Option<Arc<Remote>>,
    allow_partial: bool,
    physical_replacement_prepared: bool,
) -> Result<Option<RowAutoMergeResult>, ErrCtx> {
    let Some(key) = selected_repository_database_key(file, repo)? else {
        return Ok(None);
    };
    let index = repo.read_index()?;
    if !index.conflicted_paths().iter().any(|path| path == &key) {
        return Ok(None);
    }

    let Some((base, ours, theirs)) = current_file_conflict_states(repo, &key)? else {
        return Ok(None);
    };

    hydrate_repo_file_state_for(runtime, &base, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &ours, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &theirs, remote, RepoSnapshotPurpose::Merge)?;

    let plan = plan_repo_snapshot_merge(runtime, file, repo, &base, &ours, &theirs)?;
    if plan.has_opaque_changes()
        || !plan.schema_conflicts().is_empty()
        || !plan.limitations().is_empty()
    {
        return Ok(None);
    }
    if plan.analysis.has_conflicts() && !allow_partial {
        return Ok(None);
    }

    let applied_changes = plan.apply_change_count();
    if plan.can_resolve_to_ours_without_apply() {
        checkout_selected_repository_database(
            runtime,
            file,
            repo,
            &key,
            &ours,
            physical_replacement_prepared,
        )?;
        repo.resolve_file_conflict(repo.worktree().join(&key), Some(ours))?;
        return Ok(Some(RowAutoMergeResult {
            key,
            applied_changes,
            ours_changes: plan.analysis.ours_changes,
            theirs_changes: plan.analysis.theirs_changes,
            resolved: true,
            requires_validation: false,
        }));
    }
    if applied_changes == 0 {
        return Ok(None);
    }
    let sql = plan.theirs_apply_sql();
    let (merged, materialized) =
        materialize_row_auto_merge_candidate(runtime, repo, &key, &ours, &sql, None)?;
    let checkout = if repo.file_key(&file.tag)? == key {
        checkout_repo_file_state(runtime, file, &merged, None)
    } else {
        install_materialized_repo_file_state(
            runtime,
            repo,
            &key,
            &merged,
            &materialized,
            physical_replacement_prepared,
        )
    };
    let cleanup = std::fs::remove_file(&materialized);
    match (checkout, cleanup) {
        (Err(error), _) => return Err(error),
        (Ok(()), Ok(()) | Err(_)) => {}
    }
    if plan.analysis.has_conflicts() {
        return Ok(None);
    }
    let requires_validation = plan.requires_validation();
    if !requires_validation {
        repo.resolve_file_conflict(repo.worktree().join(&key), Some(merged))?;
    }

    Ok(Some(RowAutoMergeResult {
        key,
        applied_changes,
        ours_changes: plan.analysis.ours_changes,
        theirs_changes: plan.analysis.theirs_changes,
        resolved: !requires_validation,
        requires_validation,
    }))
}

fn selected_repository_database_key(
    file: &RepositorySessionContext,
    repo: &Repository,
) -> Result<Option<String>, ErrCtx> {
    file.repository_database_path()
        .map(|path| repo.file_key(path).map_err(ErrCtx::from))
        .transpose()
}

fn checkout_selected_repository_database(
    runtime: &Runtime,
    file: &mut RepositorySessionContext,
    repo: &Repository,
    key: &str,
    state: &CommitFileState,
    physical_replacement_prepared: bool,
) -> Result<(), ErrCtx> {
    if repo.file_key(&file.tag)? == key {
        checkout_repo_file_state(runtime, file, state, None)
    } else if physical_replacement_prepared {
        checkout_repo_file_state_to_prepared_key(runtime, repo, key, state, None)
    } else {
        checkout_repo_file_state_to_key(runtime, repo, key, state, None)
    }
}

pub(super) fn current_file_conflict_states(
    repo: &Repository,
    key: &str,
) -> Result<Option<(CommitFileState, CommitFileState, CommitFileState)>, ErrCtx> {
    let index = repo.read_index()?;
    let mut base = None;
    let mut ours = None;
    let mut theirs = None;

    for entry in index.entries.iter().filter(|entry| entry.path == key) {
        match entry.stage {
            graft::repo::index::IndexStage::Base => base = entry.file.clone(),
            graft::repo::index::IndexStage::Ours => ours = entry.file.clone(),
            graft::repo::index::IndexStage::Theirs => theirs = entry.file.clone(),
            graft::repo::index::IndexStage::Normal => {}
        }
    }

    Ok(match (base, ours, theirs) {
        (Some(base), Some(ours), Some(theirs)) => Some((base, ours, theirs)),
        _ => None,
    })
}

pub(super) fn materialize_row_auto_merge_state(
    runtime: &Runtime,
    repo: &Repository,
    key: &str,
    ours: &CommitFileState,
    sql: &str,
) -> Result<CommitFileState, ErrCtx> {
    if sql.trim().is_empty() {
        return Ok(ours.clone());
    }
    let (state, path) = materialize_row_auto_merge_candidate(runtime, repo, key, ours, sql, None)?;
    let cleanup = std::fs::remove_file(path);
    match cleanup {
        Ok(()) | Err(_) => Ok(state),
    }
}

fn materialize_row_auto_merge_candidate(
    runtime: &Runtime,
    repo: &Repository,
    key: &str,
    ours: &CommitFileState,
    sql: &str,
    worktree_seed: Option<&Path>,
) -> Result<(CommitFileState, PathBuf), ErrCtx> {
    if sql.trim().is_empty() {
        return Err(ErrCtx::InvalidCommand(
            "cannot materialize an empty row merge candidate".into(),
        ));
    }
    let temp_path = row_auto_merge_temp_path(repo, key)?;
    let result = (|| {
        let cloned = if row_merge_integrity_proven(ours) {
            worktree_seed
                .map(|seed| try_clone_sqlite_merge_seed(seed, &temp_path))
                .transpose()?
                .unwrap_or(false)
        } else {
            false
        };
        if !cloned {
            write_repo_file_state_to_path(runtime, ours, &temp_path)?;
        }
        validate_row_merge_base_integrity(&temp_path, ours)?;
        apply_row_merge_sql_to_path(&temp_path, sql)?;
        // Keep the merged snapshot in the ancestry of ours. A row merge changes
        // only a handful of SQLite pages; importing with no base turned every
        // candidate into a new full-size Volume and made the next push upload the
        // compressed database again.
        import_stable_sqlite_file_state(runtime, &temp_path, Some(ours))
    })();
    match result {
        Ok(state) => Ok((state, temp_path)),
        Err(error) => {
            let _ = std::fs::remove_file(&temp_path);
            Err(error)
        }
    }
}

/// Clone a proof-backed, exclusively locked worktree seed without copying its data blocks.
///
/// Failure is an ordinary cache miss: callers fall back to materializing the authoritative Graft
/// state. Other platforms intentionally return `false` instead of paying for a full byte copy.
fn try_clone_sqlite_merge_seed(source: &Path, destination: &Path) -> Result<bool, ErrCtx> {
    #[cfg(target_os = "macos")]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};

        if let (Ok(source_c), Ok(destination_c)) = (
            CString::new(source.as_os_str().as_bytes()),
            CString::new(destination.as_os_str().as_bytes()),
        ) {
            // SAFETY: both C strings live through the call and are NUL-terminated. The private
            // destination does not exist; clonefile either creates an independent CoW file or
            // reports failure without changing the authoritative source.
            if unsafe { libc::clonefile(source_c.as_ptr(), destination_c.as_ptr(), 0) } == 0 {
                return Ok(true);
            }
            match std::fs::remove_file(destination) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    let _ = (source, destination);

    Ok(false)
}

pub(super) fn row_auto_merge_temp_path(repo: &Repository, key: &str) -> Result<PathBuf, ErrCtx> {
    let dir = repo.worktree().join(".graft").join("tmp");
    std::fs::create_dir_all(&dir)?;
    let id = NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed);
    let key = key
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    Ok(dir.join(format!("row-merge-{}-{id}-{key}.db", std::process::id())))
}

pub(super) fn apply_row_merge_sql_to_path(path: &Path, sql: &str) -> Result<(), ErrCtx> {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|err| row_auto_merge_sqlite_err(path, "open temporary database", err))?;
    // A full integrity proof for this immutable source state was established
    // before mutation. Let SQLite validate every B-tree cell touched by the
    // delta while it transactionally maintains indexes and constraints.
    conn.execute_batch("PRAGMA cell_size_check = ON; PRAGMA foreign_keys = OFF;")
        .map_err(|err| row_auto_merge_sqlite_err(path, "configure merge validation", err))?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER, false)
        .map_err(|err| row_auto_merge_sqlite_err(path, "disable triggers", err))?;
    conn.execute_batch(sql)
        .map_err(|err| row_auto_merge_sqlite_err(path, "apply row changes", err))?;
    validate_row_merge_sqlite(path, &conn)?;
    Ok(())
}

pub(super) fn validate_row_merge_sqlite(
    path: &Path,
    conn: &rusqlite::Connection,
) -> Result<(), ErrCtx> {
    validate_row_merge_foreign_keys(path, conn)
}

pub(super) fn validate_row_merge_base_integrity(
    path: &Path,
    state: &CommitFileState,
) -> Result<(), ErrCtx> {
    let key = (state.volume.clone(), state.snapshot.clone());
    let proofs = ROW_MERGE_INTEGRITY_PROOFS.get_or_init(Default::default);
    if proofs.lock().contains(&key) {
        return Ok(());
    }

    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|err| row_auto_merge_sqlite_err(path, "open merge base for validation", err))?;
    validate_row_merge_integrity(path, &conn)?;

    let mut proofs = proofs.lock();
    if proofs.len() >= MAX_ROW_MERGE_INTEGRITY_PROOFS {
        proofs.clear();
    }
    proofs.insert(key);
    Ok(())
}

fn row_merge_integrity_proven(state: &CommitFileState) -> bool {
    let key = (state.volume.clone(), state.snapshot.clone());
    ROW_MERGE_INTEGRITY_PROOFS
        .get()
        .is_some_and(|proofs| proofs.lock().contains(&key))
}

fn validate_row_merge_integrity(path: &Path, conn: &rusqlite::Connection) -> Result<(), ErrCtx> {
    let mut integrity_stmt = conn
        .prepare("PRAGMA integrity_check;")
        .map_err(|err| row_auto_merge_sqlite_err(path, "prepare integrity_check", err))?;
    let integrity_rows = integrity_stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|err| row_auto_merge_sqlite_err(path, "run integrity_check", err))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|err| row_auto_merge_sqlite_err(path, "read integrity_check", err))?;
    if integrity_rows.is_empty() || integrity_rows.iter().any(|row| row != "ok") {
        return Err(ErrCtx::InvalidCommand(
            format!(
                "row-level auto-merge failed integrity_check: {}",
                integrity_rows.join("; ")
            )
            .into(),
        ));
    }

    Ok(())
}

fn validate_row_merge_foreign_keys(path: &Path, conn: &rusqlite::Connection) -> Result<(), ErrCtx> {
    let mut fk_stmt = conn
        .prepare("PRAGMA foreign_key_check;")
        .map_err(|err| row_auto_merge_sqlite_err(path, "prepare foreign_key_check", err))?;
    let mut fk_rows = fk_stmt
        .query([])
        .map_err(|err| row_auto_merge_sqlite_err(path, "run foreign_key_check", err))?;
    if let Some(row) = fk_rows
        .next()
        .map_err(|err| row_auto_merge_sqlite_err(path, "read foreign_key_check", err))?
    {
        let table = row
            .get::<_, String>(0)
            .unwrap_or_else(|_| "<unknown>".into());
        let rowid = row.get::<_, Option<i64>>(1).unwrap_or(None);
        let parent = row
            .get::<_, String>(2)
            .unwrap_or_else(|_| "<unknown>".into());
        let fkid = row.get::<_, i64>(3).unwrap_or_default();
        return Err(ErrCtx::InvalidCommand(
            format!(
                "row-level auto-merge failed foreign_key_check: table={table}, rowid={}, parent={parent}, fkid={fkid}",
                rowid
                    .map(|rowid| rowid.to_string())
                    .unwrap_or_else(|| "NULL".to_string())
            )
            .into(),
        ));
    }

    Ok(())
}

pub(super) fn row_auto_merge_sqlite_err(
    _path: &Path,
    action: &str,
    err: rusqlite::Error,
) -> ErrCtx {
    ErrCtx::InvalidCommand(format!("could not {action} for row-level auto-merge: {err}").into())
}

#[cfg(test)]
mod validation_tests {
    use super::*;

    fn test_state() -> CommitFileState {
        CommitFileState {
            volume: VolumeId::random(),
            snapshot: RepoSnapshot {
                page_count: PageCount::new(4),
                ranges: Vec::new(),
            },
        }
    }

    #[test]
    fn immutable_integrity_proofs_are_exact_and_corrupt_states_still_fail() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("integrity.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA page_size = 4096;\
             CREATE TABLE items(id INTEGER PRIMARY KEY, payload BLOB NOT NULL);\
             INSERT INTO items VALUES(1, zeroblob(12000));",
        )
        .unwrap();
        drop(conn);

        validate_row_merge_base_integrity(&path, &test_state()).unwrap();

        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(4096)).unwrap();
        std::io::Write::write_all(&mut file, &[0; 4096]).unwrap();
        file.sync_all().unwrap();
        drop(file);

        let error = validate_row_merge_base_integrity(&path, &test_state()).unwrap_err();
        assert!(error.to_string().contains("integrity_check"));
    }

    #[test]
    fn row_merge_delta_keeps_sqlite_constraints_enabled() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("constraints.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);\
             INSERT INTO items VALUES(1, 'existing');",
        )
        .unwrap();
        drop(conn);

        let error = apply_row_merge_sql_to_path(
            &path,
            "BEGIN TRANSACTION; INSERT INTO items VALUES(2, 'existing'); COMMIT;",
        )
        .unwrap_err();
        assert!(error.to_string().contains("UNIQUE constraint failed"));
    }

    #[test]
    fn row_merge_delta_checks_foreign_keys_after_the_transaction() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("foreign-keys.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE parents(id INTEGER PRIMARY KEY);\
             CREATE TABLE children(\
               id INTEGER PRIMARY KEY,\
               parent_id INTEGER NOT NULL REFERENCES parents(id)\
             );",
        )
        .unwrap();
        drop(conn);

        let error = apply_row_merge_sql_to_path(
            &path,
            "BEGIN TRANSACTION; INSERT INTO children VALUES(1, 99); COMMIT;",
        )
        .unwrap_err();
        assert!(error.to_string().contains("failed foreign_key_check"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_merge_seed_clone_is_an_independent_file() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.db");
        let destination = temp.path().join("candidate.db");
        std::fs::write(&source, b"immutable source").unwrap();

        assert!(try_clone_sqlite_merge_seed(&source, &destination).unwrap());
        std::fs::write(&destination, b"changed candidate").unwrap();

        assert_eq!(std::fs::read(&source).unwrap(), b"immutable source");
        assert_eq!(std::fs::read(&destination).unwrap(), b"changed candidate");
    }
}
