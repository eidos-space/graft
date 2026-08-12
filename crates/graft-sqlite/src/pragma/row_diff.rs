use super::*;

pub(super) fn json_opaque_changes(
    changes: &[crate::row_level_diff::OpaqueChange],
) -> Vec<crate::json::JsonOpaqueChange> {
    changes
        .iter()
        .map(|change| crate::json::JsonOpaqueChange {
            name: change.name.clone(),
            change: change.change.as_str().to_string(),
            reason: change.reason.as_str().to_string(),
            owner: change.owner.clone(),
        })
        .collect()
}

pub(super) fn json_schema_changes(
    changes: &[crate::row_level_diff::SchemaChange],
) -> Vec<crate::json::JsonSchemaChange> {
    changes
        .iter()
        .map(|change| crate::json::JsonSchemaChange {
            name: change.name.clone(),
            entry_type: change.entry_type.clone(),
            op: change.kind.as_str().to_string(),
            sql: change.sql.clone(),
            old_sql: change.old_sql.clone(),
        })
        .collect()
}

pub(super) fn json_diff_capabilities(diff: &crate::row_level_diff::RowLevelDiff) -> Vec<String> {
    diff.analysis
        .capabilities
        .iter()
        .map(|capability| capability.as_str().to_string())
        .collect()
}

pub(super) fn json_diff_limitations(
    diff: &crate::row_level_diff::RowLevelDiff,
) -> Vec<crate::json::JsonDiffLimitation> {
    json_limitations(&diff.analysis.limitations)
}

pub(super) fn json_limitations(
    limitations: &[crate::row_level_diff::RowLevelDiffLimitation],
) -> Vec<crate::json::JsonDiffLimitation> {
    limitations
        .iter()
        .map(|limitation| crate::json::JsonDiffLimitation {
            kind: limitation.kind.as_str().to_string(),
            subject: limitation.subject.clone(),
        })
        .collect()
}

pub(super) fn json_repo_row_diff(
    runtime: &Runtime,
    repo: &Repository,
    diff: &RepoDiff,
    table: Option<&str>,
) -> Result<crate::json::JsonRepoRowDiffResult, ErrCtx> {
    let paths = diff
        .paths
        .iter()
        .map(|path| crate::json::JsonRepoPathDiff {
            path: path.path.clone(),
            previous_path: path.previous_path.clone(),
            change: repo_file_change_label(path.change).to_string(),
            kind: repo_tracked_path_kind_json_label(path.kind).to_string(),
            storage: repo_path_storage_json_label(path.storage).to_string(),
        })
        .collect();
    let files = diff
        .files
        .iter()
        .map(|file| {
            let change = repo_file_change_label(file.change).to_string();
            let kind = repo_tracked_path_kind_json_label(file.kind).to_string();
            let storage = repo_path_storage_json_label(file.storage).to_string();
            match repo_file_row_diff(runtime, repo, file, table) {
                Ok(Some(row_diff)) => Ok(crate::json::JsonRepoRowDiffFile {
                    path: file.path.clone(),
                    previous_path: file.previous_path.clone(),
                    change,
                    kind,
                    storage,
                    row_diff_available: true,
                    logical_status: row_diff.logical_status().as_str().to_string(),
                    capabilities: json_diff_capabilities(&row_diff),
                    limitations: json_diff_limitations(&row_diff),
                    message: None,
                    schema_changes: json_schema_changes(&row_diff.schema_changes),
                    tables: json_table_changes(&row_diff.table_changes),
                    opaque_changes: json_opaque_changes(&row_diff.opaque_changes),
                    telemetry: crate::json::JsonRowDiffTelemetry {
                        requested_table: row_diff.telemetry.requested_table,
                        tables_considered: row_diff.telemetry.tables_considered,
                        tables_scanned: row_diff.telemetry.tables_scanned,
                    },
                }),
                Ok(None) => Ok(crate::json::JsonRepoRowDiffFile {
                    path: file.path.clone(),
                    previous_path: file.previous_path.clone(),
                    change: change.clone(),
                    kind,
                    storage,
                    row_diff_available: false,
                    logical_status: "row_diff_unavailable".to_string(),
                    capabilities: Vec::new(),
                    limitations: Vec::new(),
                    message: Some(format!(
                        "row diff unavailable for {change} database snapshots"
                    )),
                    schema_changes: Vec::new(),
                    tables: Vec::new(),
                    opaque_changes: Vec::new(),
                    telemetry: crate::json::JsonRowDiffTelemetry::default(),
                }),
                Err(err) => Ok(crate::json::JsonRepoRowDiffFile {
                    path: file.path.clone(),
                    previous_path: file.previous_path.clone(),
                    change: change.clone(),
                    kind,
                    storage,
                    row_diff_available: false,
                    logical_status: "row_diff_unavailable".to_string(),
                    capabilities: Vec::new(),
                    limitations: Vec::new(),
                    message: Some(format!(
                        "row diff unavailable for {change} database snapshots: {err}"
                    )),
                    schema_changes: Vec::new(),
                    tables: Vec::new(),
                    opaque_changes: Vec::new(),
                    telemetry: crate::json::JsonRowDiffTelemetry::default(),
                }),
            }
        })
        .collect::<Result<Vec<_>, ErrCtx>>()?;

    Ok(crate::json::JsonRepoRowDiffResult {
        from: diff.from.clone(),
        to: diff.to.clone(),
        paths,
        files,
    })
}

const ROW_CURSOR_PREFIX: &str = "graft-row-v1:";

pub(super) fn bounded_row_offset(after: Option<&str>, table: &str) -> Result<usize, ErrCtx> {
    let Some(after) = after else { return Ok(0) };
    let value = after
        .strip_prefix(ROW_CURSOR_PREFIX)
        .ok_or_else(|| ErrCtx::InvalidCommand("invalid or incompatible row diff cursor".into()))?;
    let (cursor_table, value) = value
        .split_once(':')
        .ok_or_else(|| ErrCtx::InvalidCommand("invalid or incompatible row diff cursor".into()))?;
    if cursor_table != encode_cursor_table(table) {
        return Err(ErrCtx::InvalidCommand(
            "row diff cursor does not match the requested table".into(),
        ));
    }
    value
        .parse::<usize>()
        .map_err(|_| ErrCtx::InvalidCommand("invalid or incompatible row diff cursor".into()))
}

fn bounded_row_cursor(table: &str, offset: usize) -> String {
    format!("{ROW_CURSOR_PREFIX}{}:{offset}", encode_cursor_table(table))
}

fn encode_cursor_table(table: &str) -> String {
    table
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn json_repo_bounded_diff(
    runtime: &Runtime,
    repo: &Repository,
    diff: &RepoDiff,
    mode: &crate::row_level_diff::BoundedRowDiffMode,
) -> Result<crate::json::JsonRepoBoundedDiffResult, ErrCtx> {
    let paths = diff
        .paths
        .iter()
        .map(|path| crate::json::JsonRepoPathDiff {
            path: path.path.clone(),
            previous_path: path.previous_path.clone(),
            change: repo_file_change_label(path.change).to_string(),
            kind: repo_tracked_path_kind_json_label(path.kind).to_string(),
            storage: repo_path_storage_json_label(path.storage).to_string(),
        })
        .collect();
    let response_mode = match mode {
        crate::row_level_diff::BoundedRowDiffMode::Summary => "summary",
        crate::row_level_diff::BoundedRowDiffMode::Rows { .. } => "rows",
    };
    let files = diff
        .files
        .iter()
        .map(|file| {
            let change = repo_file_change_label(file.change).to_string();
            let kind = repo_tracked_path_kind_json_label(file.kind).to_string();
            let storage = repo_path_storage_json_label(file.storage).to_string();
            match repo_file_bounded_row_diff(runtime, repo, file, mode) {
                Ok(Some(row_diff)) => Ok(crate::json::JsonRepoBoundedDiffFile {
                    path: file.path.clone(),
                    previous_path: file.previous_path.clone(),
                    change,
                    kind,
                    storage,
                    row_diff_available: true,
                    mode: response_mode.to_string(),
                    logical_status: row_diff.logical_status().as_str().to_string(),
                    capabilities: row_diff
                        .analysis
                        .capabilities
                        .iter()
                        .map(|capability| capability.as_str().to_string())
                        .collect(),
                    limitations: json_limitations(&row_diff.analysis.limitations),
                    message: None,
                    summaries: row_diff
                        .summaries
                        .into_iter()
                        .map(|summary| crate::json::JsonTableSummary {
                            name: summary.table_name,
                            inserts: summary.inserts,
                            deletes: summary.deletes,
                            updates: summary.updates,
                        })
                        .collect(),
                    schema_changes: json_schema_changes(&row_diff.schema_changes),
                    tables: json_table_changes(&row_diff.table_changes),
                    opaque_changes: json_opaque_changes(&row_diff.opaque_changes),
                    has_more: row_diff.has_more,
                    next_cursor: row_diff.next_offset.map(|offset| {
                        let table = row_diff
                            .telemetry
                            .requested_table
                            .as_deref()
                            .expect("row pagination always has a requested table");
                        bounded_row_cursor(table, offset)
                    }),
                    telemetry: crate::json::JsonBoundedRowDiffTelemetry {
                        requested_table: row_diff.telemetry.requested_table,
                        tables_considered: row_diff.telemetry.tables_considered,
                        tables_scanned: row_diff.telemetry.tables_scanned,
                        rows_scanned: row_diff.telemetry.rows_scanned,
                        rows_returned: row_diff.telemetry.rows_returned,
                        truncated: row_diff.telemetry.truncated,
                        response_scope: row_diff.telemetry.response_scope.to_string(),
                    },
                }),
                Ok(None) => Ok(unavailable_bounded_file(
                    file,
                    &change,
                    kind,
                    storage,
                    response_mode,
                    "row diff unavailable for these database snapshots".to_string(),
                )),
                Err(error) => Ok(unavailable_bounded_file(
                    file,
                    &change,
                    kind,
                    storage,
                    response_mode,
                    format!("bounded row diff unavailable: {error}"),
                )),
            }
        })
        .collect::<Result<Vec<_>, ErrCtx>>()?;
    Ok(crate::json::JsonRepoBoundedDiffResult {
        from: diff.from.clone(),
        to: diff.to.clone(),
        paths,
        files,
    })
}

fn unavailable_bounded_file(
    file: &graft::repo::RepoFileDiff,
    change: &str,
    kind: String,
    storage: String,
    mode: &str,
    message: String,
) -> crate::json::JsonRepoBoundedDiffFile {
    crate::json::JsonRepoBoundedDiffFile {
        path: file.path.clone(),
        previous_path: file.previous_path.clone(),
        change: change.to_string(),
        kind,
        storage,
        row_diff_available: false,
        mode: mode.to_string(),
        logical_status: "row_diff_unavailable".to_string(),
        capabilities: Vec::new(),
        limitations: Vec::new(),
        message: Some(message),
        summaries: Vec::new(),
        schema_changes: Vec::new(),
        tables: Vec::new(),
        opaque_changes: Vec::new(),
        has_more: false,
        next_cursor: None,
        telemetry: crate::json::JsonBoundedRowDiffTelemetry {
            response_scope: "unavailable".to_string(),
            ..crate::json::JsonBoundedRowDiffTelemetry::default()
        },
    }
}

pub(super) fn json_table_changes(
    changes: &[crate::row_level_diff::TableChanges],
) -> Vec<crate::json::JsonTableChanges> {
    changes
        .iter()
        .map(|table| crate::json::JsonTableChanges {
            name: table.table_name.clone(),
            columns: table.columns.clone(),
            primary_key_columns: table.primary_key_columns.clone(),
            changes: table.changes.iter().map(json_row_change).collect(),
        })
        .collect()
}

pub(super) fn json_row_change(
    change: &crate::row_level_diff::RowChange,
) -> crate::json::JsonRowChange {
    match change {
        crate::row_level_diff::RowChange::Insert { rowid, row } => crate::json::JsonRowChange {
            op: "insert".into(),
            rowid: Some(*rowid),
            key: None,
            values: row
                .values
                .iter()
                .map(crate::json::JsonRowChange::value_to_json)
                .collect(),
            old_values: None,
        },
        crate::row_level_diff::RowChange::Delete { rowid, row } => crate::json::JsonRowChange {
            op: "delete".into(),
            rowid: Some(*rowid),
            key: None,
            values: row
                .values
                .iter()
                .map(crate::json::JsonRowChange::value_to_json)
                .collect(),
            old_values: None,
        },
        crate::row_level_diff::RowChange::Update { rowid, old_row, new_row } => {
            crate::json::JsonRowChange {
                op: "update".into(),
                rowid: Some(*rowid),
                key: None,
                values: new_row
                    .values
                    .iter()
                    .map(crate::json::JsonRowChange::value_to_json)
                    .collect(),
                old_values: Some(
                    old_row
                        .values
                        .iter()
                        .map(crate::json::JsonRowChange::value_to_json)
                        .collect(),
                ),
            }
        }
        crate::row_level_diff::RowChange::PrimaryKeyInsert { key, row } => {
            crate::json::JsonRowChange {
                op: "insert".into(),
                rowid: None,
                key: Some(json_primary_key(key)),
                values: row
                    .values
                    .iter()
                    .map(crate::json::JsonRowChange::value_to_json)
                    .collect(),
                old_values: None,
            }
        }
        crate::row_level_diff::RowChange::PrimaryKeyDelete { key, row } => {
            crate::json::JsonRowChange {
                op: "delete".into(),
                rowid: None,
                key: Some(json_primary_key(key)),
                values: row
                    .values
                    .iter()
                    .map(crate::json::JsonRowChange::value_to_json)
                    .collect(),
                old_values: None,
            }
        }
        crate::row_level_diff::RowChange::PrimaryKeyUpdate { key, old_row, new_row } => {
            crate::json::JsonRowChange {
                op: "update".into(),
                rowid: None,
                key: Some(json_primary_key(key)),
                values: new_row
                    .values
                    .iter()
                    .map(crate::json::JsonRowChange::value_to_json)
                    .collect(),
                old_values: Some(
                    old_row
                        .values
                        .iter()
                        .map(crate::json::JsonRowChange::value_to_json)
                        .collect(),
                ),
            }
        }
    }
}

fn json_primary_key(
    key: &[crate::row_level_diff::PrimaryKeyPart],
) -> std::collections::BTreeMap<String, serde_json::Value> {
    key.iter()
        .map(|part| {
            (
                part.column.clone(),
                crate::json::JsonRowChange::primary_key_value_to_json(&part.value),
            )
        })
        .collect()
}
