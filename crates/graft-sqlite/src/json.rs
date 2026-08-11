//! JSON output types for repository commands.
//! These mirror the internal types but use only serde-serializable primitives,
//! avoiding the need to add Serialize to every core graft type.

use std::collections::BTreeMap;

use serde::Serialize;

/// Table summary in a diff (for `graft_json_diff`)
#[derive(Debug, Clone, Serialize)]
pub struct JsonTableSummary {
    pub name: String,
    pub inserts: usize,
    pub deletes: usize,
    pub updates: usize,
}

/// A table-level change that cannot be expanded into ordinary row changes.
#[derive(Debug, Clone, Serialize)]
pub struct JsonOpaqueChange {
    pub name: String,
    pub change: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

/// A row-diff semantic limitation or unsupported `SQLite` surface.
#[derive(Debug, Clone, Serialize)]
pub struct JsonDiffLimitation {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

/// A single row change (for `graft_json_diff`, rows mode)
#[derive(Debug, Clone, Serialize)]
pub struct JsonRowChange {
    pub op: String, // "insert", "delete", "update"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rowid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<BTreeMap<String, serde_json::Value>>,
    pub values: Vec<serde_json::Value>,
    /// Old values (only present for "update" operations)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_values: Option<Vec<serde_json::Value>>,
}

/// Table changes with row details (for `graft_json_diff`, rows mode)
#[derive(Debug, Clone, Serialize)]
pub struct JsonTableChanges {
    pub name: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub primary_key_columns: Vec<String>,
    pub changes: Vec<JsonRowChange>,
}

/// Row-level repository diff result (for `graft_json_diff --rows`)
#[derive(Debug, Clone, Serialize)]
pub struct JsonRepoRowDiffResult {
    pub from: String,
    pub to: String,
    pub paths: Vec<JsonRepoPathDiff>,
    pub files: Vec<JsonRepoRowDiffFile>,
}

/// Memory-bounded `SQLite` repository diff result.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRepoBoundedDiffResult {
    pub from: String,
    pub to: String,
    pub paths: Vec<JsonRepoPathDiff>,
    pub files: Vec<JsonRepoBoundedDiffFile>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRepoBoundedDiffFile {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_path: Option<String>,
    pub change: String,
    pub kind: String,
    pub storage: String,
    pub row_diff_available: bool,
    pub mode: String,
    pub logical_status: String,
    pub capabilities: Vec<String>,
    pub limitations: Vec<JsonDiffLimitation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub summaries: Vec<JsonTableSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tables: Vec<JsonTableChanges>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub opaque_changes: Vec<JsonOpaqueChange>,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub telemetry: JsonBoundedRowDiffTelemetry,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct JsonBoundedRowDiffTelemetry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_table: Option<String>,
    pub tables_considered: usize,
    pub tables_scanned: usize,
    pub rows_scanned: usize,
    pub rows_returned: usize,
    pub truncated: bool,
    pub response_scope: String,
}

/// Path-level repository diff summary shared by default and row diff surfaces.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRepoPathDiff {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_path: Option<String>,
    pub change: String,
    pub kind: String,
    pub storage: String,
}

/// Row-level changes for one repository file.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRepoRowDiffFile {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_path: Option<String>,
    pub change: String,
    pub kind: String,
    pub storage: String,
    pub row_diff_available: bool,
    pub logical_status: String,
    pub capabilities: Vec<String>,
    pub limitations: Vec<JsonDiffLimitation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tables: Vec<JsonTableChanges>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub opaque_changes: Vec<JsonOpaqueChange>,
    pub telemetry: JsonRowDiffTelemetry,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct JsonRowDiffTelemetry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_table: Option<String>,
    pub tables_considered: usize,
    pub tables_scanned: usize,
}

impl JsonRowChange {
    pub fn value_to_json(v: &crate::sqlite_parse::Value) -> serde_json::Value {
        match v {
            crate::sqlite_parse::Value::Null => serde_json::Value::Null,
            crate::sqlite_parse::Value::Integer(i) => serde_json::Value::Number((*i).into()),
            crate::sqlite_parse::Value::Real(f) => {
                serde_json::json!(*f)
            }
            crate::sqlite_parse::Value::Text(s) => serde_json::Value::String(s.clone()),
            crate::sqlite_parse::Value::Blob(b) => {
                let hex: String = b.iter().map(|byte| format!("{byte:02x}")).collect();
                serde_json::Value::String(hex)
            }
        }
    }

    /// Encode a primary-key value in the same shape accepted by `graft resolve --row`.
    ///
    /// BLOBs need an explicit type marker because a bare JSON string represents `SQLite` TEXT.
    pub fn primary_key_value_to_json(
        value: &crate::row_level_diff::PrimaryKeyValue,
    ) -> serde_json::Value {
        match value {
            crate::row_level_diff::PrimaryKeyValue::Blob(bytes) => {
                let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
                serde_json::json!({ "$blob": hex })
            }
            _ => Self::value_to_json(&value.to_value()),
        }
    }
}
