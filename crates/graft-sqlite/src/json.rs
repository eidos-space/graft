//! JSON output types for graft pragmas.
//! These mirror the internal types but use only serde-serializable primitives,
//! avoiding the need to add Serialize to every core graft type.

use serde::Serialize;

/// Commit log entry (for `graft_json_log`)
#[derive(Debug, Clone, Serialize)]
pub struct JsonCommit {
    pub lsn: u64,
    pub page_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub segment: Option<String>,
    pub is_checkpoint: bool,
    pub changed_pages: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Table summary in a diff (for `graft_json_diff`)
#[derive(Debug, Clone, Serialize)]
pub struct JsonTableSummary {
    pub name: String,
    pub inserts: usize,
    pub deletes: usize,
    pub updates: usize,
}

/// Diff result (for `graft_json_diff`, default mode)
#[derive(Debug, Clone, Serialize)]
pub struct JsonDiffResult {
    pub from_lsn: u64,
    pub to_lsn: u64,
    pub tables: Vec<JsonTableSummary>,
}

/// A single row change (for `graft_json_diff`, rows mode)
#[derive(Debug, Clone, Serialize)]
pub struct JsonRowChange {
    pub op: String, // "insert", "delete", "update"
    pub rowid: i64,
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
    pub changes: Vec<JsonRowChange>,
}

/// Row-level diff result (for `graft_json_diff`, rows mode)
#[derive(Debug, Clone, Serialize)]
pub struct JsonRowDiffResult {
    pub from_lsn: u64,
    pub to_lsn: u64,
    pub tables: Vec<JsonTableChanges>,
}

/// Table entry in show output (for `graft_json_show`)
#[derive(Debug, Clone, Serialize)]
pub struct JsonTableEntry {
    #[serde(rename = "type")]
    pub entry_type: String,
    pub name: String,
    pub root_page: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<usize>,
}

/// Commit detail (for `graft_json_show`)
#[derive(Debug, Clone, Serialize)]
pub struct JsonShowResult {
    pub lsn: u64,
    pub page_count: u32,
    pub is_checkpoint: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub segment: Option<String>,
    pub changed_pages: usize,
    pub tables: Vec<JsonTableEntry>,
}

/// Debug Volume info (for `graft_debug_volume_json_info`)
#[derive(Debug, Clone, Serialize)]
pub struct JsonVolumeInfo {
    pub vid: String,
    pub local: String,
    pub remote: String,
    pub page_count: u32,
    pub snapshot_size_bytes: u64,
    pub snapshot_pages: u32,
}

/// Debug table log entry (for `graft_debug_volume_json_table_log`)
#[derive(Debug, Clone, Serialize)]
pub struct JsonTableLogEntry {
    pub lsn: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp_ms: Option<u64>,
    pub summary: String,
    pub detail: String,
}

// ============================================================
// Conversion helpers
// ============================================================

impl JsonCommit {
    pub fn from_commit_info(ci: &graft::CommitInfo) -> Self {
        Self {
            lsn: ci.lsn.to_u64(),
            page_count: ci.page_count.to_u32(),
            segment: ci.segment_id.as_ref().map(|s| s.short()),
            is_checkpoint: ci.is_checkpoint,
            changed_pages: ci.changed_pages,
            timestamp_ms: ci.timestamp,
            message: ci.message.clone(),
        }
    }
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
}
