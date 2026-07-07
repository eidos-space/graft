use super::*;

pub(super) fn append_row_merge_analysis(
    output: &mut String,
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    outcome: &MergeOutcome,
    remote: Option<Arc<Remote>>,
) -> Result<(), ErrCtx> {
    let MergeOutcome::Merged { conflicted, .. } = outcome else {
        return Ok(());
    };
    let key = repo.file_key(&file.tag)?;
    if !conflicted.iter().any(|path| path == &key) {
        return Ok(());
    }

    if !output.ends_with('\n') {
        output.push('\n');
    }
    match format_current_file_row_merge_analysis(runtime, repo, &key, remote) {
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
    let plan = plan_repo_snapshot_merge(runtime, repo, base, ours, theirs)?;
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
                "    {} rowid={} (ours {}, theirs {})",
                conflict.table,
                conflict.rowid,
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
    file: &VolFile,
    repo: &Repository,
    remote: Option<Arc<Remote>>,
) -> Result<Option<JsonRowMergeAnalysis>, ErrCtx> {
    let key = repo.file_key(&file.tag)?;
    current_file_row_merge_analysis(runtime, repo, &key, remote)
}

pub(super) fn current_file_status_row_merge_analysis_lossy(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    remote: Option<Arc<Remote>>,
) -> Option<JsonRowMergeAnalysis> {
    match current_file_status_row_merge_analysis(runtime, file, repo, remote) {
        Ok(analysis) => analysis,
        Err(err) => {
            let path = repo
                .file_key(&file.tag)
                .unwrap_or_else(|_| "db.sqlite3".to_string());
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

    let plan = plan_repo_snapshot_merge(runtime, repo, &base, &ours, &theirs)?;
    let row_conflicts: Vec<JsonRowMergeConflict> = plan
        .analysis
        .conflicts
        .iter()
        .map(|conflict| JsonRowMergeConflict {
            reason: conflict.reason.as_str(),
            table: conflict.table.clone(),
            columns: conflict.columns.clone(),
            rowid: conflict.rowid,
            ours_rowid: (conflict.ours_rowid != conflict.rowid).then_some(conflict.ours_rowid),
            theirs_rowid: (conflict.theirs_rowid != conflict.rowid)
                .then_some(conflict.theirs_rowid),
            semantic_key: conflict.semantic_key.clone(),
            ours: row_change_kind_label(conflict.ours),
            theirs: row_change_kind_label(conflict.theirs),
            base_row: json_record_values_opt(conflict.base_row.as_ref()),
            ours_row: json_record_values_opt(conflict.ours_row.as_ref()),
            theirs_row: json_record_values_opt(conflict.theirs_row.as_ref()),
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
    if apply_changes == 0 {
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
        default_semantic_keys: policy.default_semantic_keys.clone(),
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
    repo: &Repository,
    remote: Option<Arc<Remote>>,
) -> Result<JsonConflictList, ErrCtx> {
    let status = repo.status()?;
    let resolution_state = read_row_conflict_resolution_state(repo, status.merge_head.as_deref())?;
    let mut conflicts = Vec::new();
    for path in &status.conflicted {
        conflicts.extend(repo_path_conflict_artifacts(
            runtime,
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
    repo: &Repository,
    remote: Option<Arc<Remote>>,
) -> Result<usize, ErrCtx> {
    Ok(repo_conflict_artifacts(runtime, repo, remote)?
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
    repo: &Repository,
    key: &str,
    remote: Option<Arc<Remote>>,
    resolution_state: &RowConflictResolutionState,
) -> Result<Vec<JsonConflictArtifact>, ErrCtx> {
    let (path_kind, path_storage) = conflict_path_descriptor(repo, key)?;
    let path_kind_label = repo_tracked_path_kind_json_label(path_kind);
    let path_storage_label = repo_path_storage_json_label(path_storage);
    let Some((base, ours, theirs)) = current_file_conflict_states(repo, key)? else {
        return Ok(vec![file_conflict_artifact(
            key,
            path_kind_label,
            path_storage_label,
            "file",
            "add_delete_conflict",
            Some("merge involves add/delete of this tracked path".to_string()),
        )]);
    };

    let result = (|| {
        hydrate_repo_file_state_for(runtime, &base, None, RepoSnapshotPurpose::Merge)?;
        hydrate_repo_file_state_for(runtime, &ours, None, RepoSnapshotPurpose::Merge)?;
        hydrate_repo_file_state_for(runtime, &theirs, remote, RepoSnapshotPurpose::Merge)?;
        let plan = plan_repo_snapshot_merge(runtime, repo, &base, &ours, &theirs)?;
        let mut artifacts = Vec::new();

        for conflict in &plan.analysis.conflicts {
            let resolution = resolution_state
                .rows
                .get(&row_conflict_resolution_key(
                    key,
                    &conflict.table,
                    conflict.rowid,
                ))
                .and_then(|label| match label.as_str() {
                    "ours" => Some("ours"),
                    "theirs" => Some("theirs"),
                    _ => None,
                });
            artifacts.push(JsonConflictArtifact {
                id: format!("{}:row:{}:{}", key, conflict.table, conflict.rowid),
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
                table: Some(conflict.table.clone()),
                columns: Some(conflict.columns.clone()).filter(|columns| !columns.is_empty()),
                rowid: Some(conflict.rowid),
                ours_rowid: (conflict.ours_rowid != conflict.rowid).then_some(conflict.ours_rowid),
                theirs_rowid: (conflict.theirs_rowid != conflict.rowid)
                    .then_some(conflict.theirs_rowid),
                semantic_key: conflict.semantic_key.clone(),
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
            });
        }

        for conflict in plan.schema_conflicts() {
            artifacts.push(JsonConflictArtifact {
                id: format!("{}:schema:{}:{}", key, conflict.entry_type, conflict.name),
                path: key.to_string(),
                path_kind: "sqlite_database",
                storage: path_storage_label,
                kind: "schema",
                reason: conflict.reason.as_str(),
                status: "unresolved",
                resolution: None,
                table: None,
                columns: None,
                rowid: None,
                ours_rowid: None,
                theirs_rowid: None,
                semantic_key: None,
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
            });
        }

        for change in plan.unresolved_opaque_changes() {
            artifacts.push(JsonConflictArtifact {
                id: format!("{}:opaque:{}:{}", key, change.reason.as_str(), change.name),
                path: key.to_string(),
                path_kind: "sqlite_database",
                storage: path_storage_label,
                kind: "opaque",
                reason: change.reason.as_str(),
                status: "unresolved",
                resolution: None,
                table: None,
                columns: None,
                rowid: None,
                ours_rowid: None,
                theirs_rowid: None,
                semantic_key: None,
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
            });
        }

        if artifacts.is_empty() && plan.apply_change_count() == 0 {
            artifacts.push(file_conflict_artifact(
                key,
                path_kind_label,
                path_storage_label,
                "file",
                "no_applicable_changes",
                Some("no row or schema conflict details were produced".to_string()),
            ));
        }

        Ok::<_, ErrCtx>(artifacts)
    })();

    match result {
        Ok(artifacts) => Ok(artifacts),
        Err(err) => Ok(vec![file_conflict_artifact(
            key,
            path_kind_label,
            path_storage_label,
            "file",
            "analysis_error",
            Some(format!("row-level conflict analysis unavailable: {err}")),
        )]),
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
        table: None,
        columns: None,
        rowid: None,
        ours_rowid: None,
        theirs_rowid: None,
        semantic_key: None,
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
        crate::row_level_diff::OpaqueChangeReason::WithoutRowidTable => {
            "WITHOUT ROWID table changes are outside row-level merge support"
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
}

pub(super) fn try_row_auto_merge_current_file_conflict(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    outcome: &MergeOutcome,
    remote: Option<Arc<Remote>>,
) -> Result<Option<RowAutoMergeResult>, ErrCtx> {
    let MergeOutcome::Merged { conflicted, .. } = outcome else {
        return Ok(None);
    };
    let key = repo.file_key(&file.tag)?;
    if !conflicted.iter().any(|path| path == &key) {
        return Ok(None);
    }

    try_row_merge_current_file_status_conflict(runtime, file, repo, remote, true)
}

pub(super) fn try_row_auto_merge_current_file_status_conflict(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    remote: Option<Arc<Remote>>,
) -> Result<Option<RowAutoMergeResult>, ErrCtx> {
    try_row_merge_current_file_status_conflict(runtime, file, repo, remote, false)
}

pub(super) fn try_row_merge_current_file_status_conflict(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    remote: Option<Arc<Remote>>,
    allow_partial: bool,
) -> Result<Option<RowAutoMergeResult>, ErrCtx> {
    let key = repo.file_key(&file.tag)?;
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

    let plan = plan_repo_snapshot_merge(runtime, repo, &base, &ours, &theirs)?;
    if plan.has_opaque_changes()
        || !plan.schema_conflicts().is_empty()
        || plan.apply_change_count() == 0
    {
        return Ok(None);
    }
    if plan.analysis.has_conflicts() && !allow_partial {
        return Ok(None);
    }

    let applied_changes = plan.apply_change_count();
    let sql = plan.theirs_apply_sql();
    let merged = materialize_row_auto_merge_state(runtime, repo, &key, &ours, &sql)?;
    checkout_repo_file_state(runtime, file, &merged, None)?;
    if plan.analysis.has_conflicts() {
        return Ok(None);
    }
    repo.resolve_file_conflict(&file.tag, Some(merged))?;

    Ok(Some(RowAutoMergeResult {
        key,
        applied_changes,
        ours_changes: plan.analysis.ours_changes,
        theirs_changes: plan.analysis.theirs_changes,
    }))
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
    let temp_path = row_auto_merge_temp_path(repo, key)?;
    let result = (|| {
        write_repo_file_state_to_path(runtime, ours, &temp_path)?;
        apply_row_merge_sql_to_path(&temp_path, sql)?;
        import_physical_sqlite_file_state(runtime, &temp_path)
    })();
    let cleanup = std::fs::remove_file(&temp_path);
    match (result, cleanup) {
        (Ok(state), Ok(()) | Err(_)) => Ok(state),
        (Err(err), Ok(()) | Err(_)) => Err(err),
    }
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
    conn.execute_batch("PRAGMA foreign_keys = OFF;")
        .map_err(|err| row_auto_merge_sqlite_err(path, "disable foreign keys", err))?;
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
    let mut integrity_stmt = conn
        .prepare("PRAGMA integrity_check;")
        .map_err(|err| row_auto_merge_sqlite_err(path, "prepare integrity_check", err))?;
    let integrity_rows = integrity_stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|err| row_auto_merge_sqlite_err(path, "run integrity_check", err))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|err| row_auto_merge_sqlite_err(path, "read integrity_check", err))?;
    if integrity_rows.is_empty() || integrity_rows.iter().any(|row| row != "ok") {
        return Err(ErrCtx::PragmaErr(
            format!(
                "row-level auto-merge failed integrity_check at `{}`: {}",
                path.display(),
                integrity_rows.join("; ")
            )
            .into(),
        ));
    }

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
        return Err(ErrCtx::PragmaErr(
            format!(
                "row-level auto-merge failed foreign_key_check at `{}`: table={table}, rowid={}, parent={parent}, fkid={fkid}",
                path.display(),
                rowid
                    .map(|rowid| rowid.to_string())
                    .unwrap_or_else(|| "NULL".to_string())
            )
            .into(),
        ));
    }

    Ok(())
}

pub(super) fn row_auto_merge_sqlite_err(path: &Path, action: &str, err: rusqlite::Error) -> ErrCtx {
    ErrCtx::PragmaErr(
        format!(
            "could not {action} for row-level auto-merge at `{}`: {err}",
            path.display()
        )
        .into(),
    )
}
