//! Row-level Diff - Built-in Implementation
//!
//! Parses `SQLite` B-tree directly to compare row data between versions

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::File,
    io::Write,
};

use crate::sqlite_parse::{
    ColumnInfo, GeneratedColumnKind, IndexRowStream, KeyConstraintKind, MasterEntry, ParseError,
    Record, TableRowStream, TableScanner, Value, parse_create_table_column_definitions,
    parse_create_table_items, read_all_rows,
};
use graft::core::{PageIdx, VolumeId, lsn::LSN, page::PAGESIZE};
use graft::rt::runtime::Runtime;
use graft::snapshot::Snapshot;
use graft::volume_reader::VolumeRead;
use rusqlite::{Connection, OpenFlags, types::ValueRef};

/// Coarse logical status for a SQLite snapshot diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalDiffStatus {
    LogicalChanges,
    UnsupportedLogicalSurface,
    FileChangedNoSupportedLogicalChanges,
}

impl LogicalDiffStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LogicalChanges => "logical_changes",
            Self::UnsupportedLogicalSurface => "unsupported_logical_surface",
            Self::FileChangedNoSupportedLogicalChanges => {
                "file_changed_no_supported_logical_changes"
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowLevelDiffCapability {
    RowidTableRows,
    PrimaryKeyTableRows,
    SchemaEntries,
    OpaqueTableDetection,
    SemanticInsertKeys,
}

impl RowLevelDiffCapability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RowidTableRows => "rowid_table_rows",
            Self::PrimaryKeyTableRows => "primary_key_table_rows",
            Self::SchemaEntries => "schema_entries",
            Self::OpaqueTableDetection => "opaque_table_detection",
            Self::SemanticInsertKeys => "semantic_insert_keys",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowLevelDiffLimitationKind {
    VirtualTable,
    FtsShadowTable,
    SqliteInternalTable,
    IndexBtree,
    Utf16TextEncoding,
    GeneratedColumns,
}

impl RowLevelDiffLimitationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VirtualTable => "virtual_table",
            Self::FtsShadowTable => "fts_shadow_table",
            Self::SqliteInternalTable => "sqlite_internal_table",
            Self::IndexBtree => "index_btree",
            Self::Utf16TextEncoding => "utf16_text_encoding",
            Self::GeneratedColumns => "generated_columns",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowLevelDiffLimitation {
    pub kind: RowLevelDiffLimitationKind,
    pub subject: Option<String>,
}

impl RowLevelDiffLimitation {
    fn new(kind: RowLevelDiffLimitationKind, subject: Option<String>) -> Self {
        Self { kind, subject }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowLevelDiffAnalysis {
    pub capabilities: Vec<RowLevelDiffCapability>,
    pub limitations: Vec<RowLevelDiffLimitation>,
}

impl Default for RowLevelDiffAnalysis {
    fn default() -> Self {
        Self {
            capabilities: vec![
                RowLevelDiffCapability::RowidTableRows,
                RowLevelDiffCapability::PrimaryKeyTableRows,
                RowLevelDiffCapability::SchemaEntries,
                RowLevelDiffCapability::OpaqueTableDetection,
                RowLevelDiffCapability::SemanticInsertKeys,
            ],
            limitations: Vec::new(),
        }
    }
}

/// Type of row change
#[derive(Debug, Clone, PartialEq)]
pub enum RowChange {
    Insert {
        rowid: i64,
        row: Record,
    },
    Delete {
        rowid: i64,
        row: Record,
    },
    Update {
        rowid: i64,
        old_row: Record,
        new_row: Record,
    },
    PrimaryKeyInsert {
        key: Vec<PrimaryKeyPart>,
        row: Record,
    },
    PrimaryKeyDelete {
        key: Vec<PrimaryKeyPart>,
        row: Record,
    },
    PrimaryKeyUpdate {
        key: Vec<PrimaryKeyPart>,
        old_row: Record,
        new_row: Record,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RowIdentity {
    Rowid(i64),
    PrimaryKey(Vec<PrimaryKeyPart>),
}

impl RowIdentity {
    pub fn rowid(&self) -> Option<i64> {
        match self {
            Self::Rowid(rowid) => Some(*rowid),
            Self::PrimaryKey(_) => None,
        }
    }

    pub fn primary_key(&self) -> Option<&[PrimaryKeyPart]> {
        match self {
            Self::Rowid(_) => None,
            Self::PrimaryKey(key) => Some(key),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PrimaryKeyPart {
    pub column: String,
    pub value: PrimaryKeyValue,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum PrimaryKeyValue {
    Null,
    Integer(i64),
    Real(u64),
    Text(String),
    Blob(Vec<u8>),
}

impl PrimaryKeyValue {
    fn from_value(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Integer(value) => Self::Integer(*value),
            Value::Real(value) => {
                let normalized = if *value == 0.0 { 0.0 } else { *value };
                Self::Real(normalized.to_bits())
            }
            Value::Text(value) => Self::Text(value.clone()),
            Value::Blob(value) => Self::Blob(value.clone()),
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Integer(value) => Value::Integer(*value),
            Self::Real(bits) => Value::Real(f64::from_bits(*bits)),
            Self::Text(value) => Value::Text(value.clone()),
            Self::Blob(value) => Value::Blob(value.clone()),
        }
    }

    fn to_sql(&self) -> String {
        self.to_value().to_sql()
    }
}

impl RowChange {
    pub fn identity(&self) -> RowIdentity {
        match self {
            Self::Insert { rowid, .. }
            | Self::Delete { rowid, .. }
            | Self::Update { rowid, .. } => RowIdentity::Rowid(*rowid),
            Self::PrimaryKeyInsert { key, .. }
            | Self::PrimaryKeyDelete { key, .. }
            | Self::PrimaryKeyUpdate { key, .. } => RowIdentity::PrimaryKey(key.clone()),
        }
    }

    pub fn rowid(&self) -> Option<i64> {
        match self.identity() {
            RowIdentity::Rowid(rowid) => Some(rowid),
            RowIdentity::PrimaryKey(_) => None,
        }
    }

    pub fn primary_key(&self) -> Option<&[PrimaryKeyPart]> {
        match self {
            Self::PrimaryKeyInsert { key, .. }
            | Self::PrimaryKeyDelete { key, .. }
            | Self::PrimaryKeyUpdate { key, .. } => Some(key),
            Self::Insert { .. } | Self::Delete { .. } | Self::Update { .. } => None,
        }
    }
}

/// Changes for a single table
#[derive(Debug, Clone, PartialEq)]
pub struct TableChanges {
    pub table_name: String,
    pub columns: Vec<String>,
    pub rowid_alias: Option<String>,
    pub generated_columns: BTreeMap<String, GeneratedColumnKind>,
    pub semantic_key_columns: Vec<String>,
    pub primary_key_columns: Vec<String>,
    pub changes: Vec<RowChange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertRowidMode {
    Preserve,
    Omit,
}

impl TableChanges {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Generate SQL statements using actual column names
    pub fn to_sql(&self) -> String {
        self.to_sql_filtered(|_| true)
    }

    pub fn to_sql_filtered(&self, include: impl FnMut(&RowChange) -> bool) -> String {
        self.to_sql_filtered_with_insert_rowid(include, |_| InsertRowidMode::Preserve)
    }

    pub fn to_sql_filtered_with_insert_rowid(
        &self,
        include: impl FnMut(&RowChange) -> bool,
        insert_rowid_mode: impl FnMut(&RowChange) -> InsertRowidMode,
    ) -> String {
        self.to_sql_filtered_with_insert_rowid_and_generated(
            &self.generated_columns,
            include,
            insert_rowid_mode,
        )
    }

    pub fn to_sql_filtered_with_insert_rowid_and_generated(
        &self,
        generated_columns: &BTreeMap<String, GeneratedColumnKind>,
        mut include: impl FnMut(&RowChange) -> bool,
        mut insert_rowid_mode: impl FnMut(&RowChange) -> InsertRowidMode,
    ) -> String {
        let mut sql = String::new();

        for change in &self.changes {
            if !include(change) {
                continue;
            }
            match change {
                RowChange::Insert { rowid, row } => {
                    let rowid = match insert_rowid_mode(change) {
                        InsertRowidMode::Preserve => Some(*rowid),
                        InsertRowidMode::Omit => None,
                    };
                    sql.push_str(&format_sql_insert(
                        &self.table_name,
                        &self.columns,
                        self.rowid_alias.as_deref(),
                        generated_columns,
                        rowid,
                        row,
                    ));
                }
                RowChange::Delete { rowid, .. } => {
                    sql.push_str(&format_sql_delete(&self.table_name, *rowid));
                }
                RowChange::Update { rowid, new_row, .. } => {
                    sql.push_str(&format_sql_update(
                        &self.table_name,
                        &self.columns,
                        self.rowid_alias.as_deref(),
                        generated_columns,
                        *rowid,
                        new_row,
                    ));
                }
                RowChange::PrimaryKeyInsert { row, .. } => {
                    sql.push_str(&format_sql_insert(
                        &self.table_name,
                        &self.columns,
                        self.rowid_alias.as_deref(),
                        generated_columns,
                        None,
                        row,
                    ));
                }
                RowChange::PrimaryKeyDelete { key, .. } => {
                    sql.push_str(&format_sql_delete_by_primary_key(&self.table_name, key));
                }
                RowChange::PrimaryKeyUpdate { key, new_row, .. } => {
                    sql.push_str(&format_sql_update_by_primary_key(
                        &self.table_name,
                        &self.columns,
                        generated_columns,
                        key,
                        new_row,
                    ));
                }
            }
            if !sql.ends_with('\n') && !sql.is_empty() {
                sql.push('\n');
            }
        }

        sql
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaChange {
    pub name: String,
    pub entry_type: String,
    pub sql: String,
    pub old_sql: Option<String>,
    pub kind: SchemaChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaChangeKind {
    Added,
    Deleted,
    Modified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpaqueChange {
    pub name: String,
    pub change: OpaqueChangeKind,
    pub reason: OpaqueChangeReason,
    pub owner: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpaqueChangeKind {
    Added,
    Deleted,
    Modified,
}

impl OpaqueChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Deleted => "deleted",
            Self::Modified => "modified",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpaqueChangeReason {
    VirtualTable,
    FtsShadowTable,
    SqliteInternalTable,
    IndexBtree,
}

impl OpaqueChangeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VirtualTable => "virtual_table",
            Self::FtsShadowTable => "fts_shadow_table",
            Self::SqliteInternalTable => "sqlite_internal_table",
            Self::IndexBtree => "index_btree",
        }
    }

    fn limitation_kind(self) -> RowLevelDiffLimitationKind {
        match self {
            Self::VirtualTable => RowLevelDiffLimitationKind::VirtualTable,
            Self::FtsShadowTable => RowLevelDiffLimitationKind::FtsShadowTable,
            Self::SqliteInternalTable => RowLevelDiffLimitationKind::SqliteInternalTable,
            Self::IndexBtree => RowLevelDiffLimitationKind::IndexBtree,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IgnoredTable {
    pub name: String,
    pub reason: OpaqueChangeReason,
    pub owner: Option<String>,
}

/// Row-level diff result
#[derive(Debug)]
pub struct RowLevelDiff {
    pub from_lsn: LSN,
    pub to_lsn: LSN,
    pub analysis: RowLevelDiffAnalysis,
    pub schema_changes: Vec<SchemaChange>,
    pub table_changes: Vec<TableChanges>,
    pub opaque_changes: Vec<OpaqueChange>,
    pub telemetry: RowLevelDiffTelemetry,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RowLevelDiffTelemetry {
    pub requested_table: Option<String>,
    pub tables_considered: usize,
    pub tables_scanned: usize,
}

/// Summary for one table without row payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedTableSummary {
    pub table_name: String,
    pub inserts: usize,
    pub deletes: usize,
    pub updates: usize,
}

/// Requested response shape for the bounded `SQLite` diff surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundedRowDiffMode {
    Summary,
    Rows {
        table: String,
        limit: usize,
        offset: usize,
    },
}

/// Memory-bounded logical diff used by SDK consumers.
#[derive(Debug)]
pub struct BoundedRowLevelDiff {
    pub from_lsn: LSN,
    pub to_lsn: LSN,
    pub analysis: RowLevelDiffAnalysis,
    pub schema_changes: Vec<SchemaChange>,
    pub summaries: Vec<BoundedTableSummary>,
    pub table_changes: Vec<TableChanges>,
    pub opaque_changes: Vec<OpaqueChange>,
    pub telemetry: BoundedRowDiffTelemetry,
    pub has_more: bool,
    pub next_offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BoundedRowDiffTelemetry {
    pub requested_table: Option<String>,
    pub tables_considered: usize,
    pub tables_scanned: usize,
    pub rows_scanned: usize,
    pub rows_returned: usize,
    pub truncated: bool,
    pub response_scope: &'static str,
}

impl BoundedRowLevelDiff {
    pub fn logical_status(&self) -> LogicalDiffStatus {
        if !self.schema_changes.is_empty()
            || !self.summaries.is_empty()
            || !self.table_changes.is_empty()
        {
            LogicalDiffStatus::LogicalChanges
        } else if !self.opaque_changes.is_empty() {
            LogicalDiffStatus::UnsupportedLogicalSurface
        } else {
            LogicalDiffStatus::FileChangedNoSupportedLogicalChanges
        }
    }
}

impl RowLevelDiff {
    pub fn logical_status(&self) -> LogicalDiffStatus {
        if !self.schema_changes.is_empty() || !self.table_changes.is_empty() {
            LogicalDiffStatus::LogicalChanges
        } else if !self.opaque_changes.is_empty() {
            LogicalDiffStatus::UnsupportedLogicalSurface
        } else {
            LogicalDiffStatus::FileChangedNoSupportedLogicalChanges
        }
    }

    /// Generate complete SQL diff
    pub fn to_sql(&self) -> String {
        let mut sql = format!(
            "-- Row-level Diff: LSN {} -> {}\n",
            self.from_lsn, self.to_lsn
        );
        sql.push_str("BEGIN TRANSACTION;\n\n");

        for change in &self.schema_changes {
            match change.kind {
                SchemaChangeKind::Added if !change.sql.trim().is_empty() => {
                    sql.push_str(&change.sql);
                    if !change.sql.trim_end().ends_with(';') {
                        sql.push(';');
                    }
                    sql.push('\n');
                }
                SchemaChangeKind::Deleted
                | SchemaChangeKind::Modified
                | SchemaChangeKind::Added => {
                    sql.push_str(&format!(
                        "-- Schema change: {} {}\n",
                        change_kind_label(change.kind),
                        change.name
                    ));
                }
            }
        }

        if !self.schema_changes.is_empty() {
            sql.push('\n');
        }

        for table in &self.table_changes {
            if !table.is_empty() {
                sql.push_str(&format!("-- Table: {}\n", table.table_name));
                sql.push_str(&table.to_sql());
                sql.push('\n');
            }
        }

        for change in &self.opaque_changes {
            sql.push_str(&format!(
                "-- Opaque change: {} {} ({})\n",
                change.change.as_str(),
                change.name,
                change.reason.as_str()
            ));
        }

        sql.push_str("COMMIT;\n");
        sql
    }

    /// Generate human-readable report
    pub fn to_report(&self) -> String {
        let mut report = format!("Diff LSN {} -> {}\n", self.from_lsn, self.to_lsn);
        report.push_str("============================\n\n");

        if self.table_changes.is_empty()
            && self.schema_changes.is_empty()
            && self.opaque_changes.is_empty()
        {
            report.push_str("No row changes.\n\n");
            return report;
        }

        if !self.schema_changes.is_empty() {
            report.push_str("Schema changes:\n");
            for change in &self.schema_changes {
                report.push_str(&format!(
                    "  {} {} ({})\n",
                    change_kind_label(change.kind),
                    change.name,
                    change.entry_type
                ));
            }
            report.push('\n');
        }

        for table in &self.table_changes {
            if table.is_empty() {
                continue;
            }

            let (inserts, deletes, updates) = count_changes(&table.changes);

            report.push_str(&format!("Table '{}': ", table.table_name));
            if inserts > 0 {
                report.push_str(&format!("+{inserts} inserts "));
            }
            if deletes > 0 {
                report.push_str(&format!("-{deletes} deletes "));
            }
            if updates > 0 {
                report.push_str(&format!("~{updates} updates"));
            }
            report.push('\n');

            // Show detailed changes
            for change in &table.changes {
                match change {
                    RowChange::Insert { rowid, row } => {
                        report.push_str(&format!("  + rowid {}: {:?}\n", rowid, row.values));
                    }
                    RowChange::Delete { rowid, row } => {
                        report.push_str(&format!("  - rowid {}: {:?}\n", rowid, row.values));
                    }
                    RowChange::Update { rowid, old_row, new_row } => {
                        report.push_str(&format!("  ~ rowid {rowid}:\n"));
                        report.push_str(&format!("    old: {:?}\n", old_row.values));
                        report.push_str(&format!("    new: {:?}\n", new_row.values));
                    }
                    RowChange::PrimaryKeyInsert { key, row } => {
                        report.push_str(&format!(
                            "  + key {}: {:?}\n",
                            format_primary_key(key),
                            row.values
                        ));
                    }
                    RowChange::PrimaryKeyDelete { key, row } => {
                        report.push_str(&format!(
                            "  - key {}: {:?}\n",
                            format_primary_key(key),
                            row.values
                        ));
                    }
                    RowChange::PrimaryKeyUpdate { key, old_row, new_row } => {
                        report.push_str(&format!("  ~ key {}:\n", format_primary_key(key)));
                        report.push_str(&format!("    old: {:?}\n", old_row.values));
                        report.push_str(&format!("    new: {:?}\n", new_row.values));
                    }
                }
            }
            report.push('\n');
        }

        if !self.opaque_changes.is_empty() {
            report.push_str("Opaque changes:\n");
            for change in &self.opaque_changes {
                report.push_str(&format!(
                    "  {} {} ({})\n",
                    change.change.as_str(),
                    change.name,
                    change.reason.as_str()
                ));
            }
            report.push('\n');
        }

        report
    }
}

/// Calculate change statistics
fn count_changes(changes: &[RowChange]) -> (usize, usize, usize) {
    let mut inserts = 0;
    let mut deletes = 0;
    let mut updates = 0;

    for change in changes {
        match change {
            RowChange::Insert { .. } | RowChange::PrimaryKeyInsert { .. } => inserts += 1,
            RowChange::Delete { .. } | RowChange::PrimaryKeyDelete { .. } => deletes += 1,
            RowChange::Update { .. } | RowChange::PrimaryKeyUpdate { .. } => updates += 1,
        }
    }

    (inserts, deletes, updates)
}

/// Execute row-level diff
pub fn row_level_diff(
    runtime: &Runtime,
    vid: &VolumeId,
    from_lsn: LSN,
    to_lsn: LSN,
) -> Result<RowLevelDiff, graft::err::GraftErr> {
    // Checkout both versions
    let from_vol = runtime.volume_checkout(vid, from_lsn)?;
    let to_vol = match runtime.volume_checkout(vid, to_lsn) {
        Ok(to_vol) => to_vol,
        Err(err) => {
            let _ = runtime.volume_delete(&from_vol.vid);
            return Err(err);
        }
    };

    let from_vid = from_vol.vid.clone();
    let to_vid = to_vol.vid.clone();

    tracing::debug!("row_level_diff: from_vid={}, to_vid={}", from_vid, to_vid);

    let result = row_level_diff_checked_out(runtime, &from_vid, &to_vid, from_lsn, to_lsn);
    let _ = runtime.volume_delete(&from_vol.vid);
    let _ = runtime.volume_delete(&to_vol.vid);

    result
}

pub fn row_level_diff_snapshots(
    runtime: &Runtime,
    from_snapshot: &Snapshot,
    to_snapshot: &Snapshot,
) -> Result<RowLevelDiff, graft::err::GraftErr> {
    row_level_diff_snapshots_for_table(runtime, from_snapshot, to_snapshot, None)
}

pub fn row_level_diff_snapshots_for_table(
    runtime: &Runtime,
    from_snapshot: &Snapshot,
    to_snapshot: &Snapshot,
    table: Option<&str>,
) -> Result<RowLevelDiff, graft::err::GraftErr> {
    let from_reader = runtime.snapshot_reader(from_snapshot.clone());
    let to_reader = runtime.snapshot_reader(to_snapshot.clone());
    let from_lsn = from_snapshot.head().map_or(LSN::FIRST, |(_, lsn)| lsn);
    let to_lsn = to_snapshot.head().map_or(LSN::FIRST, |(_, lsn)| lsn);
    row_level_diff_readers_for_table(&from_reader, &to_reader, from_lsn, to_lsn, table)
}

pub fn row_level_diff_readers(
    from_reader: &dyn VolumeRead,
    to_reader: &dyn VolumeRead,
    from_lsn: LSN,
    to_lsn: LSN,
) -> Result<RowLevelDiff, graft::err::GraftErr> {
    row_level_diff_readers_for_table(from_reader, to_reader, from_lsn, to_lsn, None)
}

pub fn row_level_diff_readers_for_table(
    from_reader: &dyn VolumeRead,
    to_reader: &dyn VolumeRead,
    from_lsn: LSN,
    to_lsn: LSN,
    table: Option<&str>,
) -> Result<RowLevelDiff, graft::err::GraftErr> {
    row_level_diff_from_readers(from_reader, to_reader, from_lsn, to_lsn, table)
}

/// Compute either an all-table summary or one bounded page of row details.
///
/// Ordinary rowid tables are merged in rowid order one leaf page at a time.
/// The compatibility path for non-native page sizes and `WITHOUT ROWID`
/// tables may materialize snapshots, but the returned row payload remains
/// bounded by the requested page size.
pub fn bounded_row_level_diff_readers(
    from_reader: &dyn VolumeRead,
    to_reader: &dyn VolumeRead,
    from_lsn: LSN,
    to_lsn: LSN,
    mode: &BoundedRowDiffMode,
) -> Result<BoundedRowLevelDiff, graft::err::GraftErr> {
    bounded_row_diff_from_readers(from_reader, to_reader, from_lsn, to_lsn, mode, None)
}

/// Computes a summary after a worktree page comparison has already narrowed the candidate tables.
/// Schema changes are still included even when their table no longer owns a page in the worktree.
pub fn bounded_row_level_diff_readers_for_summary_tables(
    from_reader: &dyn VolumeRead,
    to_reader: &dyn VolumeRead,
    from_lsn: LSN,
    to_lsn: LSN,
    tables: &BTreeSet<String>,
) -> Result<BoundedRowLevelDiff, graft::err::GraftErr> {
    bounded_row_diff_from_readers(
        from_reader,
        to_reader,
        from_lsn,
        to_lsn,
        &BoundedRowDiffMode::Summary,
        Some(tables),
    )
}

/// Uses the existing page-aware rowid diff when every candidate table has an ordinary rowid
/// layout. For these tables it is faster to decode only rows on changed B-tree pages than to merge
/// the complete rowid stream. `WITHOUT ROWID` and schema-changing tables return `None` so callers
/// retain the bounded primary-key path that avoids materializing large Eidos tables.
pub fn rowid_table_summaries_for_tables(
    from_reader: &dyn VolumeRead,
    to_reader: &dyn VolumeRead,
    from_lsn: LSN,
    to_lsn: LSN,
    tables: &BTreeSet<String>,
) -> Result<Option<Vec<BoundedTableSummary>>, graft::err::GraftErr> {
    if sqlite_page_size(from_reader)? != PAGESIZE.as_u32()
        || sqlite_page_size(to_reader)? != PAGESIZE.as_u32()
    {
        return Ok(None);
    }
    let from_scanner = TableScanner::new(from_reader).map_err(|error| {
        graft::err::LogicalErr::Other(format!("Failed to parse source B-tree: {error:?}"))
    })?;
    let to_scanner = TableScanner::new(to_reader).map_err(|error| {
        graft::err::LogicalErr::Other(format!("Failed to parse target B-tree: {error:?}"))
    })?;
    let from_master = from_scanner.read_master_table().map_err(|error| {
        graft::err::LogicalErr::Other(format!("Failed to read source schema: {error:?}"))
    })?;
    let to_master = to_scanner.read_master_table().map_err(|error| {
        graft::err::LogicalErr::Other(format!("Failed to read target schema: {error:?}"))
    })?;
    for table in tables {
        let from_entry = from_master.iter().find(|entry| entry.name == *table);
        let to_entry = to_master.iter().find(|entry| entry.name == *table);
        let stable_rowid_layout = match (from_entry, to_entry) {
            (Some(from), Some(to)) => {
                !is_without_rowid_table(from) && !is_without_rowid_table(to) && from.sql == to.sql
            }
            _ => false,
        };
        if !stable_rowid_layout {
            return Ok(None);
        }
    }

    let mut summaries = Vec::new();
    for table in tables {
        let diff = row_level_diff_readers_for_table(
            from_reader,
            to_reader,
            from_lsn,
            to_lsn,
            Some(table),
        )?;
        for changes in diff.table_changes {
            let (inserts, deletes, updates) = count_changes(&changes.changes);
            if inserts + deletes + updates > 0 {
                summaries.push(BoundedTableSummary {
                    table_name: changes.table_name,
                    inserts,
                    deletes,
                    updates,
                });
            }
        }
    }
    Ok(Some(summaries))
}

fn bounded_row_diff_from_readers(
    from_reader: &dyn VolumeRead,
    to_reader: &dyn VolumeRead,
    from_lsn: LSN,
    to_lsn: LSN,
    mode: &BoundedRowDiffMode,
    summary_tables: Option<&BTreeSet<String>>,
) -> Result<BoundedRowLevelDiff, graft::err::GraftErr> {
    let native_page_size = PAGESIZE.as_u32();
    let needs_materialized_schema = sqlite_page_size(from_reader)? != native_page_size
        || sqlite_page_size(to_reader)? != native_page_size;
    let from_scanner = TableScanner::new(from_reader).map_err(|error| {
        graft::err::LogicalErr::Other(format!("Failed to parse source B-tree: {error:?}"))
    })?;
    let to_scanner = TableScanner::new(to_reader).map_err(|error| {
        graft::err::LogicalErr::Other(format!("Failed to parse target B-tree: {error:?}"))
    })?;
    let mut materialized = needs_materialized_schema
        .then(|| MaterializedPair::new(from_reader, to_reader))
        .transpose()?;
    let (from_master, to_master) =
        bounded_master_tables(&from_scanner, &to_scanner, materialized.as_ref())?;
    let requested_table = match mode {
        BoundedRowDiffMode::Summary => None,
        BoundedRowDiffMode::Rows { table, .. } => Some(table.as_str()),
    };
    let mut schema_changes = diff_schema_entries(&from_master, &to_master);
    if let Some(table) = requested_table {
        schema_changes.retain(|change| change.name == table);
    }
    let ignored_table_infos = ignored_row_diff_table_infos(&from_master, &to_master);
    let ignored_tables: HashSet<String> = ignored_table_infos.keys().cloned().collect();
    let mut limitations = diff_parser_limitations(&from_scanner, &to_scanner);
    limitations.extend(ignored_table_infos.values().map(|table| {
        RowLevelDiffLimitation::new(table.reason.limitation_kind(), Some(table.name.clone()))
    }));
    limitations.extend(generated_column_limitations(&from_master, &to_master));
    dedupe_limitations(&mut limitations);
    let opaque_changes = diff_opaque_tables_bounded(
        from_reader,
        to_reader,
        &from_master,
        &to_master,
        &ignored_table_infos,
        requested_table,
    );

    let mut table_names = BTreeSet::new();
    for entry in from_master.iter().chain(&to_master) {
        if is_diffable_table(entry, &ignored_tables)
            && requested_table.is_none_or(|table| entry.name == table)
            && summary_tables.is_none_or(|tables| {
                tables.contains(&entry.name)
                    || schema_changes
                        .iter()
                        .any(|change| change.name == entry.name)
            })
        {
            table_names.insert(entry.name.clone());
        }
    }
    let tables_considered = table_names.len();
    let mut summaries = Vec::new();
    let mut table_changes = Vec::new();
    let mut rows_scanned = 0_usize;
    let mut has_more = false;
    let mut next_offset = None;
    let mut used_materialized_compat = false;
    let mut used_rowid_stream = false;
    let mut used_primary_key_stream = false;

    for table_name in table_names {
        bounded_cancellation_checkpoint()?;
        let from_entry = from_master.iter().find(|entry| entry.name == table_name);
        let to_entry = to_master.iter().find(|entry| entry.name == table_name);
        let entry = to_entry
            .or(from_entry)
            .expect("bounded table exists in one side");
        let column_infos = entry.parse_columns();
        let columns = column_infos
            .iter()
            .map(|column| column.name.clone())
            .collect();
        let rowid_alias = rowid_alias_column(&column_infos);
        let generated_columns = generated_columns(&column_infos);
        let without_rowid = from_entry.is_some_and(is_without_rowid_table)
            || to_entry.is_some_and(is_without_rowid_table);
        let direct_primary_key = !needs_materialized_schema
            && without_rowid
            && compatible_without_rowid_layouts(from_entry, to_entry).is_some();
        let needs_sqlite_rows = needs_materialized_schema || (without_rowid && !direct_primary_key);
        let page = if needs_sqlite_rows {
            used_materialized_compat = true;
            if materialized.is_none() {
                materialized = Some(MaterializedPair::new(from_reader, to_reader)?);
            }
            bounded_materialized_table(
                materialized
                    .as_ref()
                    .expect("materialized pair initialized"),
                from_entry,
                to_entry,
                mode,
            )?
        } else if direct_primary_key {
            used_primary_key_stream = true;
            bounded_without_rowid_table(from_reader, to_reader, from_entry, to_entry, mode)?
        } else {
            used_rowid_stream = true;
            bounded_rowid_table(from_reader, to_reader, from_entry, to_entry, mode)?
        };
        rows_scanned = rows_scanned.saturating_add(page.rows_scanned);
        if matches!(mode, BoundedRowDiffMode::Summary)
            && page.inserts + page.deletes + page.updates > 0
        {
            summaries.push(BoundedTableSummary {
                table_name: table_name.clone(),
                inserts: page.inserts,
                deletes: page.deletes,
                updates: page.updates,
            });
        }
        if !page.changes.is_empty() {
            table_changes.push(TableChanges {
                table_name,
                columns,
                rowid_alias,
                generated_columns,
                semantic_key_columns: Vec::new(),
                primary_key_columns: page.primary_key_columns,
                changes: page.changes,
            });
        }
        has_more |= page.has_more;
        next_offset = page.next_offset.or(next_offset);
    }
    let rows_returned = table_changes.iter().map(|table| table.changes.len()).sum();
    Ok(BoundedRowLevelDiff {
        from_lsn,
        to_lsn,
        analysis: RowLevelDiffAnalysis {
            limitations,
            ..RowLevelDiffAnalysis::default()
        },
        schema_changes,
        summaries,
        table_changes,
        opaque_changes,
        telemetry: BoundedRowDiffTelemetry {
            requested_table: requested_table.map(str::to_owned),
            tables_considered,
            tables_scanned: tables_considered,
            rows_scanned,
            rows_returned,
            truncated: has_more,
            response_scope: if used_materialized_compat {
                "materialized_compat"
            } else if used_primary_key_stream && used_rowid_stream {
                "streaming_btree"
            } else if used_primary_key_stream {
                "streaming_primary_key"
            } else {
                "streaming_rowid"
            },
        },
        has_more,
        next_offset,
    })
}

fn bounded_master_tables(
    from_scanner: &TableScanner<'_>,
    to_scanner: &TableScanner<'_>,
    materialized: Option<&MaterializedPair>,
) -> Result<(Vec<MasterEntry>, Vec<MasterEntry>), graft::err::GraftErr> {
    if let Some(pair) = materialized {
        return Ok((
            read_master_table_sqlite(&pair.from.connection, "source")?,
            read_master_table_sqlite(&pair.to.connection, "target")?,
        ));
    }
    Ok((
        from_scanner.read_master_table().map_err(|error| {
            graft::err::LogicalErr::Other(format!("Failed to read source schema: {error:?}"))
        })?,
        to_scanner.read_master_table().map_err(|error| {
            graft::err::LogicalErr::Other(format!("Failed to read target schema: {error:?}"))
        })?,
    ))
}

fn diff_opaque_tables_bounded(
    from_reader: &dyn VolumeRead,
    to_reader: &dyn VolumeRead,
    from_master: &[MasterEntry],
    to_master: &[MasterEntry],
    ignored_tables: &BTreeMap<String, IgnoredTable>,
    table_filter: Option<&str>,
) -> Vec<OpaqueChange> {
    let mut changes = Vec::new();
    for (name, info) in ignored_tables {
        if table_filter.is_some_and(|table| name != table && info.owner.as_deref() != Some(table)) {
            continue;
        }
        let from_entry = from_master.iter().find(|entry| entry.name == *name);
        let to_entry = to_master.iter().find(|entry| entry.name == *name);
        let change = match (from_entry, to_entry) {
            (None, None) => None,
            (None, Some(_)) => Some(OpaqueChangeKind::Added),
            (Some(_), None) => Some(OpaqueChangeKind::Deleted),
            (Some(from), Some(to))
                if from.entry_type != to.entry_type
                    || from.table_name != to.table_name
                    || from.sql != to.sql =>
            {
                Some(OpaqueChangeKind::Modified)
            }
            (Some(from), Some(to)) if from.root_page == 0 || to.root_page == 0 => None,
            (Some(from), Some(to)) => {
                let changed = (|| {
                    let from_scanner = TableScanner::new(from_reader).map_err(|error| {
                        graft::err::LogicalErr::Other(format!(
                            "Failed to scan opaque source table: {error}"
                        ))
                    })?;
                    let to_scanner = TableScanner::new(to_reader).map_err(|error| {
                        graft::err::LogicalErr::Other(format!(
                            "Failed to scan opaque target table: {error}"
                        ))
                    })?;
                    changed_table_leaf_pages(
                        from_reader,
                        to_reader,
                        &from_scanner,
                        &to_scanner,
                        from,
                        to,
                    )
                })();
                match changed {
                    Ok(Some(pages)) => (!pages.is_empty()).then_some(OpaqueChangeKind::Modified),
                    Ok(None) | Err(_) => Some(OpaqueChangeKind::Modified),
                }
            }
        };
        if let Some(change) = change {
            changes.push(OpaqueChange {
                name: info.name.clone(),
                change,
                reason: info.reason,
                owner: info.owner.clone(),
            });
        }
    }
    changes
}

struct BoundedTablePage {
    inserts: usize,
    deletes: usize,
    updates: usize,
    rows_scanned: usize,
    primary_key_columns: Vec<String>,
    changes: Vec<RowChange>,
    has_more: bool,
    next_offset: Option<usize>,
}

#[derive(Debug, Clone)]
struct WithoutRowidLayout {
    columns: Vec<String>,
    primary_key_columns: Vec<String>,
    physical_to_declared: Vec<usize>,
}

impl WithoutRowidLayout {
    fn from_entry(entry: &MasterEntry) -> Option<Self> {
        let normalized_sql = entry.sql.to_ascii_uppercase();
        // The direct B-tree merge compares decoded primary-key values. Keep this fast path to the
        // ordering guarantees used by Eidos tables: STRICT values, ascending keys, and SQLite's
        // BINARY collation. Other legal layouts continue through the SQLite compatibility
        // reader so a custom collation or descending key cannot be silently misordered.
        if !normalized_sql.contains("STRICT") {
            return None;
        }
        let column_infos = entry.parse_columns();
        if column_infos.is_empty() || column_infos.iter().any(|column| column.generated.is_some()) {
            return None;
        }
        let columns = column_infos
            .iter()
            .map(|column| column.name.clone())
            .collect::<Vec<_>>();
        let primary_key_columns = entry
            .parse_key_constraints()
            .into_iter()
            .find(|constraint| constraint.kind == KeyConstraintKind::PrimaryKey)
            .map_or_else(
                || {
                    column_infos
                        .iter()
                        .filter(|column| column.pk)
                        .map(|column| column.name.clone())
                        .collect()
                },
                |constraint| constraint.columns,
            );
        if primary_key_columns.is_empty()
            || primary_key_columns.iter().any(|primary_key| {
                column_infos
                    .iter()
                    .find(|column| column.name == *primary_key)
                    .is_none_or(|column| {
                        !column.ctype.eq_ignore_ascii_case("TEXT")
                            && !column.ctype.eq_ignore_ascii_case("INTEGER")
                    })
            })
        {
            return None;
        }
        if !supports_direct_primary_key_order(entry, &primary_key_columns) {
            return None;
        }
        let physical_columns = primary_key_columns
            .iter()
            .cloned()
            .chain(
                columns
                    .iter()
                    .filter(|column| !primary_key_columns.contains(column))
                    .cloned(),
            )
            .collect::<Vec<_>>();
        let physical_to_declared = physical_columns
            .iter()
            .map(|physical| columns.iter().position(|column| column == physical))
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            columns,
            primary_key_columns,
            physical_to_declared,
        })
    }

    fn decode(
        &self,
        physical: Record,
        output_columns: &[String],
    ) -> Result<(RowIdentity, Record), graft::err::GraftErr> {
        // SQLite does not rewrite existing records after `ALTER TABLE ADD COLUMN`, so records in
        // the newer B-tree may legitimately omit trailing nullable columns from the newer schema.
        if physical.values.len() > self.physical_to_declared.len() {
            return Err(graft::err::LogicalErr::Other(format!(
                "WITHOUT ROWID record contains {} values, expected at most {}",
                physical.values.len(),
                self.physical_to_declared.len()
            ))
            .into());
        }
        let mut values = vec![Value::Null; self.columns.len()];
        for (physical_value, declared_index) in physical
            .values
            .into_iter()
            .zip(self.physical_to_declared.iter().copied())
        {
            values[declared_index] = physical_value;
        }
        let key = self
            .primary_key_columns
            .iter()
            .map(|column| {
                let index = self
                    .columns
                    .iter()
                    .position(|candidate| candidate == column)
                    .expect("validated primary key column exists");
                PrimaryKeyPart {
                    column: column.clone(),
                    value: PrimaryKeyValue::from_value(&values[index]),
                }
            })
            .collect();
        let output_values = output_columns
            .iter()
            .map(|column| {
                self.columns
                    .iter()
                    .position(|candidate| candidate == column)
                    .map_or(Value::Null, |index| values[index].clone())
            })
            .collect();
        Ok((
            RowIdentity::PrimaryKey(key),
            Record { values: output_values },
        ))
    }
}

#[derive(Debug, Clone)]
struct WithoutRowidPairLayout {
    from: Option<WithoutRowidLayout>,
    to: Option<WithoutRowidLayout>,
    output_columns: Vec<String>,
    primary_key_columns: Vec<String>,
}

fn supports_direct_primary_key_order(entry: &MasterEntry, primary_key_columns: &[String]) -> bool {
    let column_definitions = parse_create_table_column_definitions(&entry.sql);
    let definitions_are_supported = primary_key_columns.iter().all(|primary_key| {
        column_definitions
            .iter()
            .find(|definition| definition.name == *primary_key)
            .is_some_and(|definition| ordering_fragment_is_supported(&definition.sql))
    });
    if !definitions_are_supported {
        return false;
    }
    parse_create_table_items(&entry.sql)
        .into_iter()
        .filter(|item| item.to_ascii_uppercase().contains("PRIMARY KEY"))
        .all(|item| ordering_fragment_is_supported(&item))
}

fn ordering_fragment_is_supported(fragment: &str) -> bool {
    let tokens = fragment
        .split_ascii_whitespace()
        .map(|token| {
            token
                .trim_matches(|character: char| {
                    matches!(
                        character,
                        '(' | ')' | ',' | ';' | '\'' | '"' | '`' | '[' | ']'
                    )
                })
                .to_ascii_uppercase()
        })
        .collect::<Vec<_>>();
    if tokens.iter().any(|token| token == "DESC") {
        return false;
    }
    tokens.iter().enumerate().all(|(index, token)| {
        token != "COLLATE"
            || tokens
                .get(index + 1)
                .is_some_and(|collation| collation == "BINARY")
    })
}

fn compatible_without_rowid_layouts(
    from_entry: Option<&MasterEntry>,
    to_entry: Option<&MasterEntry>,
) -> Option<WithoutRowidPairLayout> {
    let from = match from_entry {
        Some(entry) => Some(WithoutRowidLayout::from_entry(entry)?),
        None => None,
    };
    let to = match to_entry {
        Some(entry) => Some(WithoutRowidLayout::from_entry(entry)?),
        None => None,
    };
    let primary_key_columns = to
        .as_ref()
        .or(from.as_ref())
        .map(|layout| layout.primary_key_columns.clone())?;
    if from.as_ref().zip(to.as_ref()).is_some_and(|(from, to)| {
        from.primary_key_columns != to.primary_key_columns
            || !supports_nullable_appended_transition(from_entry, to_entry, from, to)
    }) {
        return None;
    }
    let output_columns = to
        .as_ref()
        .or(from.as_ref())
        .map(|layout| layout.columns.clone())?;
    Some(WithoutRowidPairLayout {
        from,
        to,
        output_columns,
        primary_key_columns,
    })
}

fn supports_nullable_appended_transition(
    from_entry: Option<&MasterEntry>,
    to_entry: Option<&MasterEntry>,
    from: &WithoutRowidLayout,
    to: &WithoutRowidLayout,
) -> bool {
    if from.columns == to.columns {
        return true;
    }
    let (shorter, longer, longer_entry) = if from.columns.len() < to.columns.len() {
        (from, to, to_entry)
    } else {
        (to, from, from_entry)
    };
    if !longer.columns.starts_with(&shorter.columns) {
        return false;
    }
    let Some(longer_entry) = longer_entry else {
        return false;
    };
    let definitions = parse_create_table_column_definitions(&longer_entry.sql);
    longer.columns[shorter.columns.len()..]
        .iter()
        .all(|column| {
            definitions
                .iter()
                .find(|definition| definition.name == *column)
                .is_some_and(|definition| {
                    let sql = definition.sql.to_ascii_uppercase();
                    !sql.contains("NOT NULL") && !sql.contains("DEFAULT")
                })
        })
}

fn bounded_without_rowid_table(
    from_reader: &dyn VolumeRead,
    to_reader: &dyn VolumeRead,
    from_entry: Option<&MasterEntry>,
    to_entry: Option<&MasterEntry>,
    mode: &BoundedRowDiffMode,
) -> Result<BoundedTablePage, graft::err::GraftErr> {
    let layouts = compatible_without_rowid_layouts(from_entry, to_entry).ok_or_else(|| {
        graft::err::LogicalErr::Other("unsupported WITHOUT ROWID primary-key layout".into())
    })?;
    let from_count = from_entry
        .map(|entry| TableScanner::new(from_reader)?.count_index_rows(entry.root_page))
        .transpose()
        .map_err(|error| {
            graft::err::LogicalErr::Other(format!(
                "Failed to count source WITHOUT ROWID rows: {error}"
            ))
        })?
        .unwrap_or(0);
    let to_count = to_entry
        .map(|entry| TableScanner::new(to_reader)?.count_index_rows(entry.root_page))
        .transpose()
        .map_err(|error| {
            graft::err::LogicalErr::Other(format!(
                "Failed to count target WITHOUT ROWID rows: {error}"
            ))
        })?
        .unwrap_or(0);
    if matches!(mode, BoundedRowDiffMode::Summary)
        && (from_entry.is_none() || to_entry.is_none() || from_count == 0 || to_count == 0)
    {
        return Ok(BoundedTablePage {
            inserts: to_count,
            deletes: from_count,
            updates: 0,
            rows_scanned: 0,
            primary_key_columns: layouts.primary_key_columns,
            changes: Vec::new(),
            has_more: false,
            next_offset: None,
        });
    }
    if from_count > 0
        && to_count > 0
        && let (Some(from_entry), Some(to_entry)) = (from_entry, to_entry)
    {
        return bounded_without_rowid_changed_pages(
            from_reader,
            to_reader,
            from_entry,
            to_entry,
            &layouts,
            mode,
        );
    }
    let mut from_stream = from_entry
        .map(|entry| TableScanner::new(from_reader)?.into_index_row_stream(entry.root_page))
        .transpose()
        .map_err(|error| {
            graft::err::LogicalErr::Other(format!(
                "Failed to stream source WITHOUT ROWID rows: {error}"
            ))
        })?;
    let mut to_stream = to_entry
        .map(|entry| TableScanner::new(to_reader)?.into_index_row_stream(entry.root_page))
        .transpose()
        .map_err(|error| {
            graft::err::LogicalErr::Other(format!(
                "Failed to stream target WITHOUT ROWID rows: {error}"
            ))
        })?;
    let mut from_row = next_bounded_primary_key_row(
        &mut from_stream,
        layouts.from.as_ref(),
        &layouts.output_columns,
    )?;
    let mut to_row =
        next_bounded_primary_key_row(&mut to_stream, layouts.to.as_ref(), &layouts.output_columns)?;
    let mut page = BoundedTablePage {
        inserts: 0,
        deletes: 0,
        updates: 0,
        rows_scanned: 0,
        primary_key_columns: layouts.primary_key_columns.clone(),
        changes: Vec::new(),
        has_more: false,
        next_offset: None,
    };
    let (offset, limit) = match mode {
        BoundedRowDiffMode::Summary => (usize::MAX, 0),
        BoundedRowDiffMode::Rows { offset, limit, .. } => (*offset, *limit),
    };
    let mut change_index = 0_usize;
    while from_row.is_some() || to_row.is_some() {
        if page.rows_scanned.is_multiple_of(1_024) {
            bounded_cancellation_checkpoint()?;
        }
        let change = match (from_row.take(), to_row.take()) {
            (Some((from_identity, old_row)), Some((to_identity, new_row)))
                if from_identity == to_identity =>
            {
                page.rows_scanned = page.rows_scanned.saturating_add(2);
                from_row = next_bounded_primary_key_row(
                    &mut from_stream,
                    layouts.from.as_ref(),
                    &layouts.output_columns,
                )?;
                to_row = next_bounded_primary_key_row(
                    &mut to_stream,
                    layouts.to.as_ref(),
                    &layouts.output_columns,
                )?;
                (old_row != new_row).then(|| match from_identity {
                    RowIdentity::PrimaryKey(key) => {
                        RowChange::PrimaryKeyUpdate { key, old_row, new_row }
                    }
                    RowIdentity::Rowid(_) => {
                        unreachable!("WITHOUT ROWID identity is a primary key")
                    }
                })
            }
            (Some((from_identity, old_row)), Some((to_identity, new_row)))
                if from_identity < to_identity =>
            {
                page.rows_scanned = page.rows_scanned.saturating_add(1);
                from_row = next_bounded_primary_key_row(
                    &mut from_stream,
                    layouts.from.as_ref(),
                    &layouts.output_columns,
                )?;
                to_row = Some((to_identity, new_row));
                match from_identity {
                    RowIdentity::PrimaryKey(key) => {
                        Some(RowChange::PrimaryKeyDelete { key, row: old_row })
                    }
                    RowIdentity::Rowid(_) => {
                        unreachable!("WITHOUT ROWID identity is a primary key")
                    }
                }
            }
            (Some((from_identity, old_row)), Some((to_identity, new_row))) => {
                page.rows_scanned = page.rows_scanned.saturating_add(1);
                from_row = Some((from_identity, old_row));
                to_row = next_bounded_primary_key_row(
                    &mut to_stream,
                    layouts.to.as_ref(),
                    &layouts.output_columns,
                )?;
                match to_identity {
                    RowIdentity::PrimaryKey(key) => {
                        Some(RowChange::PrimaryKeyInsert { key, row: new_row })
                    }
                    RowIdentity::Rowid(_) => {
                        unreachable!("WITHOUT ROWID identity is a primary key")
                    }
                }
            }
            (Some((identity, row)), None) => {
                page.rows_scanned = page.rows_scanned.saturating_add(1);
                from_row = next_bounded_primary_key_row(
                    &mut from_stream,
                    layouts.from.as_ref(),
                    &layouts.output_columns,
                )?;
                match identity {
                    RowIdentity::PrimaryKey(key) => Some(RowChange::PrimaryKeyDelete { key, row }),
                    RowIdentity::Rowid(_) => {
                        unreachable!("WITHOUT ROWID identity is a primary key")
                    }
                }
            }
            (None, Some((identity, row))) => {
                page.rows_scanned = page.rows_scanned.saturating_add(1);
                to_row = next_bounded_primary_key_row(
                    &mut to_stream,
                    layouts.to.as_ref(),
                    &layouts.output_columns,
                )?;
                match identity {
                    RowIdentity::PrimaryKey(key) => Some(RowChange::PrimaryKeyInsert { key, row }),
                    RowIdentity::Rowid(_) => {
                        unreachable!("WITHOUT ROWID identity is a primary key")
                    }
                }
            }
            (None, None) => None,
        };
        let Some(change) = change else { continue };
        match &change {
            RowChange::PrimaryKeyInsert { .. } => page.inserts += 1,
            RowChange::PrimaryKeyDelete { .. } => page.deletes += 1,
            RowChange::PrimaryKeyUpdate { .. } => page.updates += 1,
            _ => unreachable!("WITHOUT ROWID stream only produces primary-key changes"),
        }
        if !matches!(mode, BoundedRowDiffMode::Summary) && change_index >= offset {
            if page.changes.len() == limit {
                page.has_more = true;
                page.next_offset = Some(offset.saturating_add(limit));
                break;
            }
            page.changes.push(change);
        }
        change_index = change_index.saturating_add(1);
    }
    Ok(page)
}

fn bounded_without_rowid_changed_pages(
    from_reader: &dyn VolumeRead,
    to_reader: &dyn VolumeRead,
    from_entry: &MasterEntry,
    to_entry: &MasterEntry,
    layouts: &WithoutRowidPairLayout,
    mode: &BoundedRowDiffMode,
) -> Result<BoundedTablePage, graft::err::GraftErr> {
    let from_scanner = TableScanner::new(from_reader).map_err(|error| {
        graft::err::LogicalErr::Other(format!(
            "Failed to inspect source WITHOUT ROWID pages: {error}"
        ))
    })?;
    let to_scanner = TableScanner::new(to_reader).map_err(|error| {
        graft::err::LogicalErr::Other(format!(
            "Failed to inspect target WITHOUT ROWID pages: {error}"
        ))
    })?;
    let from_pages = from_scanner
        .index_btree_pages(from_entry.root_page)
        .map_err(|error| {
            graft::err::LogicalErr::Other(format!(
                "Failed to enumerate source WITHOUT ROWID pages: {error}"
            ))
        })?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let to_pages = to_scanner
        .index_btree_pages(to_entry.root_page)
        .map_err(|error| {
            graft::err::LogicalErr::Other(format!(
                "Failed to enumerate target WITHOUT ROWID pages: {error}"
            ))
        })?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let all_pages = from_pages.union(&to_pages).copied().collect::<Vec<_>>();
    let mut changed_pages = Vec::new();
    for (index, page_number) in all_pages.into_iter().enumerate() {
        if index.is_multiple_of(1_024) {
            bounded_cancellation_checkpoint()?;
        }
        if !from_pages.contains(&page_number) || !to_pages.contains(&page_number) {
            changed_pages.push(page_number);
            continue;
        }
        let page_idx = PageIdx::try_new(page_number).ok_or_else(|| {
            graft::err::LogicalErr::Other(format!("Invalid WITHOUT ROWID page index {page_number}"))
        })?;
        let from_page = from_reader.read_page(page_idx)?;
        let to_page = to_reader.read_page(page_idx)?;
        if from_page != to_page
            || from_scanner
                .index_page_has_overflow(page_number)
                .map_err(|error| {
                    graft::err::LogicalErr::Other(format!(
                        "Failed to inspect source WITHOUT ROWID overflow pages: {error}"
                    ))
                })?
            || to_scanner
                .index_page_has_overflow(page_number)
                .map_err(|error| {
                    graft::err::LogicalErr::Other(format!(
                        "Failed to inspect target WITHOUT ROWID overflow pages: {error}"
                    ))
                })?
        {
            changed_pages.push(page_number);
        }
    }

    let from_rows = changed_without_rowid_records(
        &from_scanner,
        &from_pages,
        &changed_pages,
        layouts
            .from
            .as_ref()
            .expect("source WITHOUT ROWID layout exists"),
        &layouts.output_columns,
        "source",
    )?;
    let to_rows = changed_without_rowid_records(
        &to_scanner,
        &to_pages,
        &changed_pages,
        layouts
            .to
            .as_ref()
            .expect("target WITHOUT ROWID layout exists"),
        &layouts.output_columns,
        "target",
    )?;
    let rows_scanned = from_rows.len().saturating_add(to_rows.len());
    let all_changes = diff_sqlite_rows(from_rows, to_rows);
    let inserts = all_changes
        .iter()
        .filter(|change| matches!(change, RowChange::PrimaryKeyInsert { .. }))
        .count();
    let deletes = all_changes
        .iter()
        .filter(|change| matches!(change, RowChange::PrimaryKeyDelete { .. }))
        .count();
    let updates = all_changes
        .iter()
        .filter(|change| matches!(change, RowChange::PrimaryKeyUpdate { .. }))
        .count();
    let (changes, has_more, next_offset) = match mode {
        BoundedRowDiffMode::Summary => (Vec::new(), false, None),
        BoundedRowDiffMode::Rows { offset, limit, .. } => {
            let has_more = all_changes.len() > offset.saturating_add(*limit);
            let changes = all_changes.into_iter().skip(*offset).take(*limit).collect();
            (
                changes,
                has_more,
                has_more.then_some(offset.saturating_add(*limit)),
            )
        }
    };
    Ok(BoundedTablePage {
        inserts,
        deletes,
        updates,
        rows_scanned,
        primary_key_columns: layouts.primary_key_columns.clone(),
        changes,
        has_more,
        next_offset,
    })
}

fn changed_without_rowid_records(
    scanner: &TableScanner<'_>,
    owned_pages: &BTreeSet<u32>,
    changed_pages: &[u32],
    layout: &WithoutRowidLayout,
    output_columns: &[String],
    side: &str,
) -> Result<BTreeMap<RowIdentity, Record>, graft::err::GraftErr> {
    let mut rows = BTreeMap::new();
    for page_number in changed_pages
        .iter()
        .copied()
        .filter(|page_number| owned_pages.contains(page_number))
    {
        let records = scanner
            .read_index_records_from_page(page_number)
            .map_err(|error| {
                graft::err::LogicalErr::Other(format!(
                    "Failed to decode {side} WITHOUT ROWID page {page_number}: {error}"
                ))
            })?;
        for record in records {
            let (identity, record) = layout.decode(record, output_columns)?;
            rows.insert(identity, record);
        }
    }
    Ok(rows)
}

fn next_bounded_primary_key_row(
    stream: &mut Option<IndexRowStream<'_>>,
    layout: Option<&WithoutRowidLayout>,
    output_columns: &[String],
) -> Result<Option<(RowIdentity, Record)>, graft::err::GraftErr> {
    stream
        .as_mut()
        .map(|stream| stream.next_record())
        .transpose()
        .map_err(|error| {
            graft::err::LogicalErr::Other(format!(
                "Failed to stream WITHOUT ROWID table rows: {error}"
            ))
        })?
        .flatten()
        .map(|record| {
            layout
                .ok_or_else(|| {
                    graft::err::LogicalErr::Other(
                        "WITHOUT ROWID stream is missing its schema layout".into(),
                    )
                })?
                .decode(record, output_columns)
        })
        .transpose()
}

fn bounded_rowid_table(
    from_reader: &dyn VolumeRead,
    to_reader: &dyn VolumeRead,
    from_entry: Option<&MasterEntry>,
    to_entry: Option<&MasterEntry>,
    mode: &BoundedRowDiffMode,
) -> Result<BoundedTablePage, graft::err::GraftErr> {
    if matches!(mode, BoundedRowDiffMode::Summary) {
        let from_count = from_entry
            .map(|entry| TableScanner::new(from_reader)?.count_table_rows(entry.root_page))
            .transpose()
            .map_err(|error| {
                graft::err::LogicalErr::Other(format!("Failed to count source rows: {error}"))
            })?
            .unwrap_or(0);
        let to_count = to_entry
            .map(|entry| TableScanner::new(to_reader)?.count_table_rows(entry.root_page))
            .transpose()
            .map_err(|error| {
                graft::err::LogicalErr::Other(format!("Failed to count target rows: {error}"))
            })?
            .unwrap_or(0);
        if from_count == 0 || to_count == 0 {
            return Ok(BoundedTablePage {
                inserts: to_count,
                deletes: from_count,
                updates: 0,
                rows_scanned: 0,
                primary_key_columns: Vec::new(),
                changes: Vec::new(),
                has_more: false,
                next_offset: None,
            });
        }
    }
    let mut from_stream = from_entry
        .map(|entry| TableScanner::new(from_reader)?.into_row_stream(entry.root_page))
        .transpose()
        .map_err(|error| {
            graft::err::LogicalErr::Other(format!("Failed to stream source rows: {error}"))
        })?;
    let mut to_stream = to_entry
        .map(|entry| TableScanner::new(to_reader)?.into_row_stream(entry.root_page))
        .transpose()
        .map_err(|error| {
            graft::err::LogicalErr::Other(format!("Failed to stream target rows: {error}"))
        })?;
    let mut from_row = next_bounded_row(&mut from_stream)?;
    let mut to_row = next_bounded_row(&mut to_stream)?;
    let mut page = BoundedTablePage {
        inserts: 0,
        deletes: 0,
        updates: 0,
        rows_scanned: 0,
        primary_key_columns: Vec::new(),
        changes: Vec::new(),
        has_more: false,
        next_offset: None,
    };
    let (offset, limit) = match mode {
        BoundedRowDiffMode::Summary => (usize::MAX, 0),
        BoundedRowDiffMode::Rows { offset, limit, .. } => (*offset, *limit),
    };
    let mut change_index = 0_usize;
    while from_row.is_some() || to_row.is_some() {
        if page.rows_scanned.is_multiple_of(1_024) {
            bounded_cancellation_checkpoint()?;
        }
        let change = match (from_row.take(), to_row.take()) {
            (Some((from_rowid, old_row)), Some((to_rowid, new_row))) if from_rowid == to_rowid => {
                page.rows_scanned = page.rows_scanned.saturating_add(2);
                from_row = next_bounded_row(&mut from_stream)?;
                to_row = next_bounded_row(&mut to_stream)?;
                (old_row != new_row).then_some(RowChange::Update {
                    rowid: from_rowid,
                    old_row,
                    new_row,
                })
            }
            (Some((from_rowid, old_row)), Some((to_rowid, new_row))) if from_rowid < to_rowid => {
                page.rows_scanned = page.rows_scanned.saturating_add(1);
                from_row = next_bounded_row(&mut from_stream)?;
                to_row = Some((to_rowid, new_row));
                Some(RowChange::Delete { rowid: from_rowid, row: old_row })
            }
            (Some((from_rowid, old_row)), Some((to_rowid, new_row))) => {
                page.rows_scanned = page.rows_scanned.saturating_add(1);
                from_row = Some((from_rowid, old_row));
                to_row = next_bounded_row(&mut to_stream)?;
                Some(RowChange::Insert { rowid: to_rowid, row: new_row })
            }
            (Some((rowid, row)), None) => {
                page.rows_scanned = page.rows_scanned.saturating_add(1);
                from_row = next_bounded_row(&mut from_stream)?;
                Some(RowChange::Delete { rowid, row })
            }
            (None, Some((rowid, row))) => {
                page.rows_scanned = page.rows_scanned.saturating_add(1);
                to_row = next_bounded_row(&mut to_stream)?;
                Some(RowChange::Insert { rowid, row })
            }
            (None, None) => None,
        };
        let Some(change) = change else { continue };
        match &change {
            RowChange::Insert { .. } => page.inserts += 1,
            RowChange::Delete { .. } => page.deletes += 1,
            RowChange::Update { .. } => page.updates += 1,
            _ => unreachable!("rowid stream only produces rowid changes"),
        }
        if !matches!(mode, BoundedRowDiffMode::Summary) && change_index >= offset {
            if page.changes.len() == limit {
                page.has_more = true;
                page.next_offset = Some(offset.saturating_add(limit));
                break;
            }
            page.changes.push(change);
        }
        change_index = change_index.saturating_add(1);
    }
    Ok(page)
}

fn next_bounded_row(
    stream: &mut Option<TableRowStream<'_>>,
) -> Result<Option<(i64, Record)>, graft::err::GraftErr> {
    stream
        .as_mut()
        .map(|stream| stream.next_row())
        .transpose()
        .map(Option::flatten)
        .map_err(|error| {
            graft::err::LogicalErr::Other(format!("Failed to stream SQLite table rows: {error}"))
                .into()
        })
}

fn bounded_cancellation_checkpoint() -> Result<(), graft::err::GraftErr> {
    graft::repo::cancellation_checkpoint().map_err(|error| {
        graft::err::LogicalErr::Other(format!("bounded SQLite diff cancelled: {error}")).into()
    })
}

fn bounded_materialized_table(
    pair: &MaterializedPair,
    from_entry: Option<&MasterEntry>,
    to_entry: Option<&MasterEntry>,
    mode: &BoundedRowDiffMode,
) -> Result<BoundedTablePage, graft::err::GraftErr> {
    if matches!(mode, BoundedRowDiffMode::Summary) {
        let from_count = materialized_table_count(&pair.from.connection, from_entry, "source")?;
        let to_count = materialized_table_count(&pair.to.connection, to_entry, "target")?;
        if from_count == 0 || to_count == 0 {
            return Ok(BoundedTablePage {
                inserts: to_count,
                deletes: from_count,
                updates: 0,
                rows_scanned: 0,
                primary_key_columns: Vec::new(),
                changes: Vec::new(),
                has_more: false,
                next_offset: None,
            });
        }
    }
    let from_rows = from_entry
        .map(|entry| read_sqlite_table_rows(&pair.from.connection, entry, "source"))
        .transpose()?;
    let to_rows = to_entry
        .map(|entry| read_sqlite_table_rows(&pair.to.connection, entry, "target"))
        .transpose()?;
    let primary_key_columns = to_rows
        .as_ref()
        .or(from_rows.as_ref())
        .map(|rows| rows.primary_key_columns.clone())
        .unwrap_or_default();
    let mut from_rows = from_rows.map(|rows| rows.rows).unwrap_or_default();
    let mut to_rows = to_rows.map(|rows| rows.rows).unwrap_or_default();
    let mut identities = BTreeSet::new();
    identities.extend(from_rows.keys().cloned());
    identities.extend(to_rows.keys().cloned());
    let (offset, limit) = match mode {
        BoundedRowDiffMode::Summary => (usize::MAX, 0),
        BoundedRowDiffMode::Rows { offset, limit, .. } => (*offset, *limit),
    };
    let mut inserts = 0_usize;
    let mut deletes = 0_usize;
    let mut updates = 0_usize;
    let mut rows_scanned = 0_usize;
    let mut change_index = 0_usize;
    let mut changes = Vec::with_capacity(limit);
    let mut has_more = false;
    for identity in identities {
        if rows_scanned.is_multiple_of(1_024) {
            bounded_cancellation_checkpoint()?;
        }
        let old_row = from_rows.remove(&identity);
        let new_row = to_rows.remove(&identity);
        rows_scanned += usize::from(old_row.is_some()) + usize::from(new_row.is_some());
        let change = match (old_row, new_row, identity) {
            (Some(old_row), Some(new_row), RowIdentity::Rowid(rowid)) if old_row != new_row => {
                Some(RowChange::Update { rowid, old_row, new_row })
            }
            (Some(old_row), Some(new_row), RowIdentity::PrimaryKey(key)) if old_row != new_row => {
                Some(RowChange::PrimaryKeyUpdate { key, old_row, new_row })
            }
            (Some(row), None, RowIdentity::Rowid(rowid)) => Some(RowChange::Delete { rowid, row }),
            (Some(row), None, RowIdentity::PrimaryKey(key)) => {
                Some(RowChange::PrimaryKeyDelete { key, row })
            }
            (None, Some(row), RowIdentity::Rowid(rowid)) => Some(RowChange::Insert { rowid, row }),
            (None, Some(row), RowIdentity::PrimaryKey(key)) => {
                Some(RowChange::PrimaryKeyInsert { key, row })
            }
            _ => None,
        };
        let Some(change) = change else { continue };
        match &change {
            RowChange::Insert { .. } | RowChange::PrimaryKeyInsert { .. } => inserts += 1,
            RowChange::Delete { .. } | RowChange::PrimaryKeyDelete { .. } => deletes += 1,
            RowChange::Update { .. } | RowChange::PrimaryKeyUpdate { .. } => updates += 1,
        }
        if !matches!(mode, BoundedRowDiffMode::Summary) && change_index >= offset {
            if changes.len() == limit {
                has_more = true;
                break;
            }
            changes.push(change);
        }
        change_index = change_index.saturating_add(1);
    }
    Ok(BoundedTablePage {
        inserts,
        deletes,
        updates,
        rows_scanned,
        primary_key_columns,
        changes,
        has_more,
        next_offset: has_more.then_some(offset.saturating_add(limit)),
    })
}

fn materialized_table_count(
    connection: &Connection,
    entry: Option<&MasterEntry>,
    side: &str,
) -> Result<usize, graft::err::GraftErr> {
    let Some(entry) = entry else { return Ok(0) };
    let count = connection
        .query_row(
            &format!("SELECT count(*) FROM {}", quote_identifier(&entry.name)),
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| sqlite_snapshot_err(side, "count table rows in", error))?;
    usize::try_from(count).map_err(|_| {
        graft::err::LogicalErr::Other(format!(
            "Invalid row count for {side} table '{}'",
            entry.name
        ))
        .into()
    })
}

fn row_level_diff_checked_out(
    runtime: &Runtime,
    from_vid: &VolumeId,
    to_vid: &VolumeId,
    from_lsn: LSN,
    to_lsn: LSN,
) -> Result<RowLevelDiff, graft::err::GraftErr> {
    // Get readers
    let from_reader = runtime.volume_reader(from_vid.clone()).map_err(|e| {
        tracing::error!("Failed to create from_reader for {}: {:?}", from_vid, e);
        graft::err::LogicalErr::Other(format!("Failed to create reader for {from_vid}: {e:?}"))
    })?;
    let to_reader = runtime.volume_reader(to_vid.clone()).map_err(|e| {
        tracing::error!("Failed to create to_reader for {}: {:?}", to_vid, e);
        graft::err::LogicalErr::Other(format!("Failed to create reader for {to_vid}: {e:?}"))
    })?;

    row_level_diff_from_readers(&from_reader, &to_reader, from_lsn, to_lsn, None)
}

pub(crate) struct MaterializedSnapshot {
    _directory: tempfile::TempDir,
    connection: Connection,
}

impl MaterializedSnapshot {
    pub(crate) fn from_reader(
        reader: &dyn VolumeRead,
        label: &str,
    ) -> Result<Self, graft::err::GraftErr> {
        let directory = tempfile::tempdir().map_err(|error| {
            graft::err::LogicalErr::Other(format!(
                "Failed to create temporary {label} SQLite snapshot: {error}"
            ))
        })?;
        let path = directory.path().join("snapshot.sqlite");
        let mut file = File::create(&path).map_err(|error| {
            graft::err::LogicalErr::Other(format!(
                "Failed to create temporary {label} SQLite file: {error}"
            ))
        })?;
        for page_number in 1..=reader.page_count().to_u32() {
            if page_number.is_multiple_of(1_024) {
                bounded_cancellation_checkpoint()?;
            }
            let page_idx = PageIdx::try_new(page_number).ok_or_else(|| {
                graft::err::LogicalErr::Other(format!(
                    "Invalid page {page_number} while materializing {label} SQLite snapshot"
                ))
            })?;
            let page = reader.read_page(page_idx).map_err(|error| {
                graft::err::LogicalErr::Other(format!(
                    "Failed to read page {page_number} from {label} SQLite snapshot: {error}"
                ))
            })?;
            file.write_all(page.as_ref()).map_err(|error| {
                graft::err::LogicalErr::Other(format!(
                    "Failed to materialize {label} SQLite snapshot: {error}"
                ))
            })?;
        }
        file.flush().map_err(|error| {
            graft::err::LogicalErr::Other(format!(
                "Failed to flush temporary {label} SQLite snapshot: {error}"
            ))
        })?;
        drop(file);

        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| sqlite_snapshot_err(label, "open", error))?;
        connection
            .pragma_update(None, "trusted_schema", false)
            .map_err(|error| sqlite_snapshot_err(label, "disable trusted schema", error))?;
        Ok(Self { _directory: directory, connection })
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }
}

struct MaterializedPair {
    from: MaterializedSnapshot,
    to: MaterializedSnapshot,
}

impl MaterializedPair {
    fn new(
        from_reader: &dyn VolumeRead,
        to_reader: &dyn VolumeRead,
    ) -> Result<Self, graft::err::GraftErr> {
        Ok(Self {
            from: MaterializedSnapshot::from_reader(from_reader, "source")?,
            to: MaterializedSnapshot::from_reader(to_reader, "target")?,
        })
    }
}

fn sqlite_snapshot_err(side: &str, action: &str, error: rusqlite::Error) -> graft::err::LogicalErr {
    graft::err::LogicalErr::Other(format!(
        "Failed to {action} {side} SQLite snapshot: {error}"
    ))
}

fn sqlite_page_size(reader: &dyn VolumeRead) -> Result<u32, graft::err::LogicalErr> {
    let page = reader.read_page(PageIdx::FIRST).map_err(|error| {
        graft::err::LogicalErr::Other(format!("Failed to read SQLite header: {error}"))
    })?;
    if page.len() < 18 || &page[..16] != b"SQLite format 3\x00" {
        return Err(graft::err::LogicalErr::Other(
            "Invalid SQLite header while reading row diff".into(),
        ));
    }
    let bytes = page.as_ref();
    let raw = u16::from_be_bytes([bytes[16], bytes[17]]);
    Ok(if raw == 1 { 65_536 } else { u32::from(raw) })
}

pub(crate) fn read_master_table_sqlite(
    connection: &Connection,
    side: &str,
) -> Result<Vec<MasterEntry>, graft::err::LogicalErr> {
    let mut statement = connection
        .prepare(
            "SELECT type, name, tbl_name, rootpage, coalesce(sql, '') FROM sqlite_schema ORDER BY rowid",
        )
        .map_err(|error| sqlite_snapshot_err(side, "prepare sqlite_schema query for", error))?;
    statement
        .query_map([], |row| {
            Ok(MasterEntry {
                entry_type: row.get(0)?,
                name: row.get(1)?,
                table_name: row.get(2)?,
                root_page: row.get(3)?,
                sql: row.get(4)?,
            })
        })
        .map_err(|error| sqlite_snapshot_err(side, "query sqlite_schema from", error))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| sqlite_snapshot_err(side, "read sqlite_schema from", error))
}

fn row_level_diff_from_readers(
    from_reader: &dyn VolumeRead,
    to_reader: &dyn VolumeRead,
    from_lsn: LSN,
    to_lsn: LSN,
    table_filter: Option<&str>,
) -> Result<RowLevelDiff, graft::err::GraftErr> {
    let native_page_size = PAGESIZE.as_u32();
    let needs_materialized_schema = sqlite_page_size(from_reader)? != native_page_size
        || sqlite_page_size(to_reader)? != native_page_size;

    // The direct parser is the fast path for Graft-sized SQLite pages. SQLite itself is the
    // compatibility path for other legal page sizes and index-organized WITHOUT ROWID tables.
    let from_scanner = TableScanner::new(from_reader).map_err(|e| {
        tracing::error!("Failed to create source scanner: {:?}", e);
        graft::err::LogicalErr::Other(format!("Failed to parse source B-tree: {e:?}"))
    })?;
    let to_scanner = TableScanner::new(to_reader).map_err(|e| {
        graft::err::LogicalErr::Other(format!("Failed to parse target B-tree: {e:?}"))
    })?;

    let mut materialized = needs_materialized_schema
        .then(|| MaterializedPair::new(from_reader, to_reader))
        .transpose()?;
    let (from_master, to_master) = if let Some(pair) = materialized.as_ref() {
        (
            read_master_table_sqlite(&pair.from.connection, "source")?,
            read_master_table_sqlite(&pair.to.connection, "target")?,
        )
    } else {
        (
            from_scanner.read_master_table().map_err(|e| {
                graft::err::LogicalErr::Other(format!("Failed to read source schema: {e:?}"))
            })?,
            to_scanner.read_master_table().map_err(|e| {
                graft::err::LogicalErr::Other(format!("Failed to read target schema: {e:?}"))
            })?,
        )
    };

    // Compare schema and tables
    let mut schema_changes = diff_schema_entries(&from_master, &to_master);
    if let Some(table) = table_filter {
        schema_changes.retain(|change| {
            change.name == table
                || from_master
                    .iter()
                    .chain(to_master.iter())
                    .any(|entry| entry.name == change.name && entry.table_name == table)
        });
    }
    let mut table_changes = Vec::new();
    let mut limitations = diff_parser_limitations(&from_scanner, &to_scanner);

    // Collect all table names
    let mut ignored_table_infos = ignored_row_diff_table_infos(&from_master, &to_master);
    if let Some(table) = table_filter {
        ignored_table_infos.retain(|name, info| {
            name == table || info.owner.as_deref().is_some_and(|owner| owner == table)
        });
    }
    limitations.extend(ignored_table_infos.values().map(|table| {
        RowLevelDiffLimitation::new(table.reason.limitation_kind(), Some(table.name.clone()))
    }));
    limitations.extend(
        generated_column_limitations(&from_master, &to_master)
            .into_iter()
            .filter(|limitation| {
                table_filter.is_none()
                    || limitation
                        .subject
                        .as_deref()
                        .is_some_and(|subject| table_filter.is_some_and(|table| subject == table))
            }),
    );
    dedupe_limitations(&mut limitations);
    let ignored_tables: HashSet<String> = ignored_table_infos.keys().cloned().collect();
    let opaque_changes = diff_opaque_tables(
        from_reader,
        to_reader,
        &from_master,
        &to_master,
        &ignored_table_infos,
    );
    let index_btree_changes = if needs_materialized_schema {
        Vec::new()
    } else {
        diff_index_btrees(
            from_reader,
            to_reader,
            &from_master,
            &to_master,
            table_filter,
        )
    };
    limitations.extend(index_btree_changes.iter().map(|change| {
        RowLevelDiffLimitation::new(change.reason.limitation_kind(), Some(change.name.clone()))
    }));
    dedupe_limitations(&mut limitations);
    let opaque_changes = opaque_changes
        .into_iter()
        .chain(index_btree_changes)
        .collect();
    let mut all_tables: HashSet<String> = HashSet::new();
    for entry in &from_master {
        if is_diffable_table(entry, &ignored_tables)
            && table_filter.is_none_or(|table| entry.name == table)
        {
            all_tables.insert(entry.name.clone());
        }
    }
    for entry in &to_master {
        if is_diffable_table(entry, &ignored_tables)
            && table_filter.is_none_or(|table| entry.name == table)
        {
            all_tables.insert(entry.name.clone());
        }
    }
    let tables_considered = all_tables.len();
    let mut tables_scanned = 0;

    // Compare each table
    for table_name in all_tables {
        tables_scanned += 1;
        let from_entry = from_master.iter().find(|e| e.name == table_name);
        let to_entry = to_master.iter().find(|e| e.name == table_name);

        // Get columns from schema (prefer to-entry, fallback to from-entry)
        let column_infos: Vec<ColumnInfo> = to_entry
            .or(from_entry)
            .map(MasterEntry::parse_columns)
            .unwrap_or_default();
        let rowid_alias = rowid_alias_column(&column_infos);
        let semantic_key_columns = semantic_key_columns(
            to_entry.or(from_entry),
            &column_infos,
            rowid_alias.as_deref(),
        );
        let generated_columns = generated_columns(&column_infos);
        let parsed_columns: Vec<String> = column_infos.into_iter().map(|c| c.name).collect();
        let needs_sqlite_rows = needs_materialized_schema
            || from_entry.is_some_and(is_without_rowid_table)
            || to_entry.is_some_and(is_without_rowid_table);

        let (columns, primary_key_columns, changes) = if needs_sqlite_rows {
            if materialized.is_none() {
                materialized = Some(MaterializedPair::new(from_reader, to_reader)?);
            }
            let pair = materialized
                .as_ref()
                .expect("materialized pair initialized");
            let from_rows = from_entry
                .map(|entry| read_sqlite_table_rows(&pair.from.connection, entry, "source"))
                .transpose()?;
            let to_rows = to_entry
                .map(|entry| read_sqlite_table_rows(&pair.to.connection, entry, "target"))
                .transpose()?;
            let columns = to_rows
                .as_ref()
                .or(from_rows.as_ref())
                .map_or(parsed_columns, |rows| rows.columns.clone());
            let primary_key_columns = to_rows
                .as_ref()
                .or(from_rows.as_ref())
                .map(|rows| rows.primary_key_columns.clone())
                .unwrap_or_default();
            let changes = diff_sqlite_rows(
                from_rows.map(|rows| rows.rows).unwrap_or_default(),
                to_rows.map(|rows| rows.rows).unwrap_or_default(),
            );
            (columns, primary_key_columns, changes)
        } else {
            let changes = match (from_entry, to_entry) {
                (Some(from), Some(to)) => {
                    diff_table_rows(from_reader, to_reader, &from_scanner, &to_scanner, from, to)?
                }
                (Some(from), None) => read_all_rows(from_reader, from.root_page)
                    .map_err(|e| table_read_err("from", from, e))?
                    .into_iter()
                    .map(|(rowid, row)| RowChange::Delete { rowid, row })
                    .collect(),
                (None, Some(to)) => read_all_rows(to_reader, to.root_page)
                    .map_err(|e| table_read_err("to", to, e))?
                    .into_iter()
                    .map(|(rowid, row)| RowChange::Insert { rowid, row })
                    .collect(),
                (None, None) => vec![],
            };
            (parsed_columns, Vec::new(), changes)
        };

        if !changes.is_empty() {
            table_changes.push(TableChanges {
                table_name,
                columns,
                rowid_alias,
                generated_columns,
                semantic_key_columns,
                primary_key_columns,
                changes,
            });
        }
    }

    Ok(RowLevelDiff {
        from_lsn,
        to_lsn,
        analysis: RowLevelDiffAnalysis {
            limitations,
            ..RowLevelDiffAnalysis::default()
        },
        schema_changes,
        table_changes,
        opaque_changes,
        telemetry: RowLevelDiffTelemetry {
            requested_table: table_filter.map(str::to_owned),
            tables_considered,
            tables_scanned,
        },
    })
}

fn diff_schema_entries(
    from_master: &[MasterEntry],
    to_master: &[MasterEntry],
) -> Vec<SchemaChange> {
    let mut changes = Vec::new();
    let mut names: HashSet<String> = HashSet::new();
    for entry in from_master.iter().chain(to_master.iter()) {
        if is_schema_diffable_entry(entry) {
            names.insert(entry.name.clone());
        }
    }

    let mut names: Vec<_> = names.into_iter().collect();
    names.sort_by(|a, b| {
        let a_entry = to_master
            .iter()
            .chain(from_master.iter())
            .find(|entry| entry.name == *a);
        let b_entry = to_master
            .iter()
            .chain(from_master.iter())
            .find(|entry| entry.name == *b);
        schema_entry_priority(a_entry)
            .cmp(&schema_entry_priority(b_entry))
            .then(a.cmp(b))
    });

    for name in names {
        let from_entry = from_master.iter().find(|entry| entry.name == name);
        let to_entry = to_master.iter().find(|entry| entry.name == name);
        let Some(change) = (match (from_entry, to_entry) {
            (None, Some(to)) => Some(SchemaChange {
                name: to.name.clone(),
                entry_type: to.entry_type.clone(),
                sql: to.sql.clone(),
                old_sql: None,
                kind: SchemaChangeKind::Added,
            }),
            (Some(from), None) => Some(SchemaChange {
                name: from.name.clone(),
                entry_type: from.entry_type.clone(),
                sql: from.sql.clone(),
                old_sql: Some(from.sql.clone()),
                kind: SchemaChangeKind::Deleted,
            }),
            (Some(from), Some(to))
                if from.entry_type != to.entry_type
                    || from.table_name != to.table_name
                    || from.sql != to.sql =>
            {
                Some(SchemaChange {
                    name: to.name.clone(),
                    entry_type: to.entry_type.clone(),
                    sql: to.sql.clone(),
                    old_sql: Some(from.sql.clone()),
                    kind: SchemaChangeKind::Modified,
                })
            }
            _ => None,
        }) else {
            continue;
        };
        changes.push(change);
    }

    changes
}

fn is_schema_diffable_entry(entry: &MasterEntry) -> bool {
    !entry.name.starts_with("sqlite_") && !entry.sql.trim().is_empty()
}

fn schema_entry_priority(entry: Option<&MasterEntry>) -> u8 {
    match entry.map(|entry| entry.entry_type.as_str()) {
        Some("table") => 0,
        Some("view") => 1,
        Some("index") => 2,
        Some("trigger") => 3,
        _ => 4,
    }
}

fn change_kind_label(kind: SchemaChangeKind) -> &'static str {
    match kind {
        SchemaChangeKind::Added => "added",
        SchemaChangeKind::Deleted => "deleted",
        SchemaChangeKind::Modified => "modified",
    }
}

/// Diff rows for a single table
fn diff_table_rows(
    from_reader: &dyn VolumeRead,
    to_reader: &dyn VolumeRead,
    from_scanner: &TableScanner<'_>,
    to_scanner: &TableScanner<'_>,
    from_entry: &MasterEntry,
    to_entry: &MasterEntry,
) -> Result<Vec<RowChange>, graft::err::LogicalErr> {
    let changed_leaf_pages = changed_table_leaf_pages(
        from_reader,
        to_reader,
        from_scanner,
        to_scanner,
        from_entry,
        to_entry,
    )?;
    let (from_rows, to_rows) = if let Some(leaf_pages) = changed_leaf_pages {
        let from_rows = from_scanner
            .read_rows_from_leaf_pages(&leaf_pages)
            .map_err(|e| table_read_err("from", from_entry, e))?;
        let to_rows = to_scanner
            .read_rows_from_leaf_pages(&leaf_pages)
            .map_err(|e| table_read_err("to", to_entry, e))?;
        (from_rows, to_rows)
    } else {
        let from_rows = read_all_rows(from_reader, from_entry.root_page)
            .map_err(|e| table_read_err("from", from_entry, e))?;
        let to_rows = read_all_rows(to_reader, to_entry.root_page)
            .map_err(|e| table_read_err("to", to_entry, e))?;
        (from_rows, to_rows)
    };

    let mut changes = Vec::new();

    // Find all rowids
    let mut all_rowids: std::collections::HashSet<i64> = std::collections::HashSet::new();
    all_rowids.extend(from_rows.keys());
    all_rowids.extend(to_rows.keys());

    for rowid in all_rowids {
        match (from_rows.get(&rowid), to_rows.get(&rowid)) {
            (Some(old_row), Some(new_row)) => {
                // Row exists, check if modified
                if old_row != new_row {
                    changes.push(RowChange::Update {
                        rowid,
                        old_row: old_row.clone(),
                        new_row: new_row.clone(),
                    });
                }
            }
            (Some(row), None) => {
                // Row deleted
                changes.push(RowChange::Delete { rowid, row: row.clone() });
            }
            (None, Some(row)) => {
                // New row
                changes.push(RowChange::Insert { rowid, row: row.clone() });
            }
            (None, None) => {}
        }
    }

    Ok(changes)
}

struct SqliteTableRows {
    columns: Vec<String>,
    primary_key_columns: Vec<String>,
    rows: BTreeMap<RowIdentity, Record>,
}

fn read_sqlite_table_rows(
    connection: &Connection,
    entry: &MasterEntry,
    side: &str,
) -> Result<SqliteTableRows, graft::err::LogicalErr> {
    let mut metadata = connection
        .prepare("SELECT cid, name, pk, hidden FROM pragma_table_xinfo(?1) ORDER BY cid")
        .map_err(|error| sqlite_snapshot_err(side, "prepare table metadata query for", error))?;
    let column_metadata = metadata
        .query_map([entry.name.as_str()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(|error| sqlite_snapshot_err(side, "query table metadata from", error))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| sqlite_snapshot_err(side, "read table metadata from", error))?;
    let columns: Vec<String> = column_metadata
        .iter()
        .filter(|(_, _, _, hidden)| *hidden != 1)
        .map(|(_, name, _, _)| name.clone())
        .collect();
    let mut primary_key_metadata: Vec<_> = column_metadata
        .iter()
        .filter(|(_, _, ordinal, _)| *ordinal > 0)
        .map(|(_, name, ordinal, _)| (*ordinal, name.clone()))
        .collect();
    primary_key_metadata.sort_by_key(|(ordinal, _)| *ordinal);
    let mut primary_key_columns: Vec<String> = primary_key_metadata
        .into_iter()
        .map(|(_, name)| name)
        .collect();
    let without_rowid = is_without_rowid_table(entry);
    if without_rowid && primary_key_columns.is_empty() {
        return Err(graft::err::LogicalErr::Other(format!(
            "WITHOUT ROWID table '{}' has no readable primary key",
            entry.name
        )));
    }
    if !without_rowid {
        primary_key_columns.clear();
    }

    let projection = columns
        .iter()
        .map(|column| quote_identifier(column))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = if without_rowid {
        format!("SELECT {projection} FROM {}", quote_identifier(&entry.name))
    } else {
        format!(
            "SELECT rowid, {projection} FROM {}",
            quote_identifier(&entry.name)
        )
    };
    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| sqlite_snapshot_err(side, "prepare table row query for", error))?;
    let mut query = statement
        .query([])
        .map_err(|error| sqlite_snapshot_err(side, "query table rows from", error))?;
    let mut rows = BTreeMap::new();
    while let Some(row) = query
        .next()
        .map_err(|error| sqlite_snapshot_err(side, "read table rows from", error))?
    {
        let value_offset = if without_rowid { 0 } else { 1 };
        let values = (0..columns.len())
            .map(|index| sqlite_value(row.get_ref(index + value_offset)?))
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| sqlite_snapshot_err(side, "decode table rows from", error))?;
        let record = Record { values };
        let identity = if without_rowid {
            let mut key = Vec::with_capacity(primary_key_columns.len());
            for column in &primary_key_columns {
                let index = columns
                    .iter()
                    .position(|candidate| candidate == column)
                    .ok_or_else(|| {
                        graft::err::LogicalErr::Other(format!(
                            "Primary key column '{column}' is missing from table '{}'",
                            entry.name
                        ))
                    })?;
                key.push(PrimaryKeyPart {
                    column: column.clone(),
                    value: PrimaryKeyValue::from_value(&record.values[index]),
                });
            }
            RowIdentity::PrimaryKey(key)
        } else {
            RowIdentity::Rowid(
                row.get(0)
                    .map_err(|error| sqlite_snapshot_err(side, "read rowid from", error))?,
            )
        };
        rows.insert(identity, record);
    }

    Ok(SqliteTableRows { columns, primary_key_columns, rows })
}

fn sqlite_value(value: ValueRef<'_>) -> rusqlite::Result<Value> {
    Ok(match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => Value::Integer(value),
        ValueRef::Real(value) => Value::Real(value),
        ValueRef::Text(value) => Value::Text(String::from_utf8_lossy(value).into_owned()),
        ValueRef::Blob(value) => Value::Blob(value.to_vec()),
    })
}

fn diff_sqlite_rows(
    from_rows: BTreeMap<RowIdentity, Record>,
    to_rows: BTreeMap<RowIdentity, Record>,
) -> Vec<RowChange> {
    let mut identities = std::collections::BTreeSet::new();
    identities.extend(from_rows.keys().cloned());
    identities.extend(to_rows.keys().cloned());
    let mut changes = Vec::new();
    for identity in identities {
        match (from_rows.get(&identity), to_rows.get(&identity)) {
            (Some(old_row), Some(new_row)) if old_row != new_row => match identity {
                RowIdentity::Rowid(rowid) => changes.push(RowChange::Update {
                    rowid,
                    old_row: old_row.clone(),
                    new_row: new_row.clone(),
                }),
                RowIdentity::PrimaryKey(key) => changes.push(RowChange::PrimaryKeyUpdate {
                    key,
                    old_row: old_row.clone(),
                    new_row: new_row.clone(),
                }),
            },
            (Some(row), None) => match identity {
                RowIdentity::Rowid(rowid) => {
                    changes.push(RowChange::Delete { rowid, row: row.clone() });
                }
                RowIdentity::PrimaryKey(key) => {
                    changes.push(RowChange::PrimaryKeyDelete { key, row: row.clone() })
                }
            },
            (None, Some(row)) => match identity {
                RowIdentity::Rowid(rowid) => {
                    changes.push(RowChange::Insert { rowid, row: row.clone() });
                }
                RowIdentity::PrimaryKey(key) => {
                    changes.push(RowChange::PrimaryKeyInsert { key, row: row.clone() })
                }
            },
            _ => {}
        }
    }
    changes
}

fn changed_table_leaf_pages(
    from_reader: &dyn VolumeRead,
    to_reader: &dyn VolumeRead,
    from_scanner: &TableScanner<'_>,
    to_scanner: &TableScanner<'_>,
    from_entry: &MasterEntry,
    to_entry: &MasterEntry,
) -> Result<Option<Vec<u32>>, graft::err::LogicalErr> {
    let from_pages = from_scanner
        .table_leaf_pages(from_entry.root_page)
        .map_err(|e| table_read_err("from", from_entry, e))?;
    let to_pages = to_scanner
        .table_leaf_pages(to_entry.root_page)
        .map_err(|e| table_read_err("to", to_entry, e))?;
    if from_pages != to_pages {
        return Ok(None);
    }

    let leaf_pages: HashSet<u32> = from_pages.iter().copied().collect();
    let non_leaf_pages_changed =
        table_non_leaf_pages_changed(from_reader, to_reader, &leaf_pages, from_entry, to_entry)?;
    let mut changed_pages = Vec::new();
    for page_num in from_pages {
        let page_idx = PageIdx::try_new(page_num)
            .ok_or_else(|| table_read_err("from", from_entry, ParseError::InvalidPageNumber))?;
        let from_page = from_reader
            .read_page(page_idx)
            .map_err(|_| table_read_err("from", from_entry, ParseError::ReadError))?;
        let to_page = to_reader
            .read_page(page_idx)
            .map_err(|_| table_read_err("to", to_entry, ParseError::ReadError))?;
        if from_page != to_page
            || (non_leaf_pages_changed
                && from_scanner
                    .leaf_page_has_overflow(page_num, from_page.as_ref())
                    .map_err(|e| table_read_err("from", from_entry, e))?)
        {
            changed_pages.push(page_num);
        }
    }
    Ok(Some(changed_pages))
}

fn table_non_leaf_pages_changed(
    from_reader: &dyn VolumeRead,
    to_reader: &dyn VolumeRead,
    leaf_pages: &HashSet<u32>,
    from_entry: &MasterEntry,
    to_entry: &MasterEntry,
) -> Result<bool, graft::err::LogicalErr> {
    if from_reader.page_count() != to_reader.page_count() {
        return Ok(true);
    }
    for page_num in 2..=from_reader.page_count().to_u32() {
        if leaf_pages.contains(&page_num) {
            continue;
        }
        let page_idx = PageIdx::try_new(page_num)
            .ok_or_else(|| table_read_err("from", from_entry, ParseError::InvalidPageNumber))?;
        let from_page = from_reader
            .read_page(page_idx)
            .map_err(|_| table_read_err("from", from_entry, ParseError::ReadError))?;
        let to_page = to_reader
            .read_page(page_idx)
            .map_err(|_| table_read_err("to", to_entry, ParseError::ReadError))?;
        if from_page != to_page {
            return Ok(true);
        }
    }
    Ok(false)
}

fn table_read_err(side: &str, entry: &MasterEntry, err: ParseError) -> graft::err::LogicalErr {
    graft::err::LogicalErr::Other(format!(
        "Failed to read {side} table '{}' at root page {}: {err}",
        entry.name, entry.root_page
    ))
}

fn diff_parser_limitations(
    from_scanner: &TableScanner<'_>,
    to_scanner: &TableScanner<'_>,
) -> Vec<RowLevelDiffLimitation> {
    let mut limitations = Vec::new();
    if from_scanner.get_header().text_encoding != crate::sqlite_parse::TextEncoding::Utf8
        || to_scanner.get_header().text_encoding != crate::sqlite_parse::TextEncoding::Utf8
    {
        limitations.push(RowLevelDiffLimitation::new(
            RowLevelDiffLimitationKind::Utf16TextEncoding,
            None,
        ));
    }
    limitations
}

fn generated_column_limitations(
    from_master: &[MasterEntry],
    to_master: &[MasterEntry],
) -> Vec<RowLevelDiffLimitation> {
    let mut limitations = Vec::new();
    let mut seen = HashSet::new();
    for entry in from_master.iter().chain(to_master.iter()) {
        if entry.entry_type != "table" || !has_generated_columns(entry) {
            continue;
        }
        if seen.insert(entry.name.clone()) {
            limitations.push(RowLevelDiffLimitation::new(
                RowLevelDiffLimitationKind::GeneratedColumns,
                Some(entry.name.clone()),
            ));
        }
    }
    limitations
}

fn has_generated_columns(entry: &MasterEntry) -> bool {
    let sql = entry.sql.to_ascii_lowercase();
    sql.contains(" generated always ")
        || sql.contains(" generated\n")
        || sql.contains(" generated\t")
}

fn dedupe_limitations(limitations: &mut Vec<RowLevelDiffLimitation>) {
    let mut seen = HashSet::new();
    limitations.retain(|limitation| {
        seen.insert((
            limitation.kind.as_str(),
            limitation.subject.clone().unwrap_or_default(),
        ))
    });
}

const FTS_SHADOW_SUFFIXES: &[&str] = &[
    "_content",
    "_data",
    "_docsize",
    "_idx",
    "_segdir",
    "_segments",
    "_stat",
    "_config",
];

pub(crate) fn ignored_row_diff_tables(
    from_master: &[MasterEntry],
    to_master: &[MasterEntry],
) -> HashSet<String> {
    ignored_row_diff_table_infos(from_master, to_master)
        .into_keys()
        .collect()
}

pub(crate) fn ignored_row_diff_table_infos(
    from_master: &[MasterEntry],
    to_master: &[MasterEntry],
) -> BTreeMap<String, IgnoredTable> {
    let mut ignored = BTreeMap::new();

    for entry in from_master.iter().chain(to_master.iter()) {
        if is_sqlite_internal_table(entry) {
            ignored
                .entry(entry.name.clone())
                .or_insert_with(|| IgnoredTable {
                    name: entry.name.clone(),
                    reason: OpaqueChangeReason::SqliteInternalTable,
                    owner: None,
                });
            continue;
        }

        if !is_virtual_table(entry) {
            continue;
        }

        ignored
            .entry(entry.name.clone())
            .or_insert_with(|| IgnoredTable {
                name: entry.name.clone(),
                reason: OpaqueChangeReason::VirtualTable,
                owner: None,
            });

        if is_fts_virtual_table(entry) {
            for suffix in FTS_SHADOW_SUFFIXES {
                let name = format!("{}{}", entry.name, suffix);
                ignored.entry(name.clone()).or_insert_with(|| IgnoredTable {
                    name,
                    reason: OpaqueChangeReason::FtsShadowTable,
                    owner: Some(entry.name.clone()),
                });
            }
        }
    }

    ignored
}

fn diff_opaque_tables(
    from_reader: &dyn VolumeRead,
    to_reader: &dyn VolumeRead,
    from_master: &[MasterEntry],
    to_master: &[MasterEntry],
    ignored_tables: &BTreeMap<String, IgnoredTable>,
) -> Vec<OpaqueChange> {
    let mut changes = Vec::new();

    for (name, info) in ignored_tables {
        let from_entry = from_master.iter().find(|entry| entry.name == *name);
        let to_entry = to_master.iter().find(|entry| entry.name == *name);
        let change = match (from_entry, to_entry) {
            (None, None) => None,
            (None, Some(_)) => Some(OpaqueChangeKind::Added),
            (Some(_), None) => Some(OpaqueChangeKind::Deleted),
            (Some(from), Some(to)) => {
                opaque_table_change_kind(from_reader, to_reader, from, to, info.reason)
            }
        };

        if let Some(change) = change {
            changes.push(OpaqueChange {
                name: info.name.clone(),
                change,
                reason: info.reason,
                owner: info.owner.clone(),
            });
        }
    }

    changes
}

fn diff_index_btrees(
    from_reader: &dyn VolumeRead,
    to_reader: &dyn VolumeRead,
    from_master: &[MasterEntry],
    to_master: &[MasterEntry],
    table_filter: Option<&str>,
) -> Vec<OpaqueChange> {
    let mut changes = Vec::new();
    let mut names = HashSet::new();
    for entry in from_master.iter().chain(to_master.iter()) {
        if is_index_btree(entry) && table_filter.is_none_or(|table| entry.table_name == table) {
            names.insert(entry.name.clone());
        }
    }
    for name in names {
        let from_entry = from_master.iter().find(|entry| entry.name == name);
        let to_entry = to_master.iter().find(|entry| entry.name == name);
        let change = match (from_entry, to_entry) {
            (Some(from), Some(to)) => {
                opaque_root_page_change_kind(from_reader, to_reader, from, to)
            }
            _ => None,
        };
        if let Some(change) = change {
            changes.push(OpaqueChange {
                name,
                change,
                reason: OpaqueChangeReason::IndexBtree,
                owner: None,
            });
        }
    }
    changes
}

fn opaque_table_change_kind(
    from_reader: &dyn VolumeRead,
    to_reader: &dyn VolumeRead,
    from: &MasterEntry,
    to: &MasterEntry,
    reason: OpaqueChangeReason,
) -> Option<OpaqueChangeKind> {
    if from.entry_type != to.entry_type || from.table_name != to.table_name || from.sql != to.sql {
        return Some(OpaqueChangeKind::Modified);
    }

    if from.root_page == 0 || to.root_page == 0 {
        return None;
    }

    if matches!(reason, OpaqueChangeReason::SqliteInternalTable) {
        return opaque_root_page_change_kind(from_reader, to_reader, from, to);
    }

    let changes = (|| {
        let from_scanner =
            TableScanner::new(from_reader).map_err(|e| table_read_err("from", from, e))?;
        let to_scanner = TableScanner::new(to_reader).map_err(|e| table_read_err("to", to, e))?;
        diff_table_rows(from_reader, to_reader, &from_scanner, &to_scanner, from, to)
    })();
    match changes {
        Ok(changes) => (!changes.is_empty()).then_some(OpaqueChangeKind::Modified),
        Err(err) => {
            tracing::warn!(
                "Could not expand opaque table '{}' while detecting opaque diff: {:?}",
                from.name,
                err
            );
            Some(OpaqueChangeKind::Modified)
        }
    }
}

fn opaque_root_page_change_kind(
    from_reader: &dyn VolumeRead,
    to_reader: &dyn VolumeRead,
    from: &MasterEntry,
    to: &MasterEntry,
) -> Option<OpaqueChangeKind> {
    if from.root_page != to.root_page {
        return Some(OpaqueChangeKind::Modified);
    }
    let Some(page_idx) = PageIdx::try_new(from.root_page) else {
        return Some(OpaqueChangeKind::Modified);
    };
    let from_page = from_reader.read_page(page_idx);
    let to_page = to_reader.read_page(page_idx);
    match (from_page, to_page) {
        (Ok(from_page), Ok(to_page)) => {
            (from_page.as_ref() != to_page.as_ref()).then_some(OpaqueChangeKind::Modified)
        }
        _ => Some(OpaqueChangeKind::Modified),
    }
}

pub(crate) fn is_diffable_table(entry: &MasterEntry, ignored_tables: &HashSet<String>) -> bool {
    entry.entry_type == "table"
        && !entry.name.starts_with("sqlite_")
        && entry.root_page != 0
        && !ignored_tables.contains(&entry.name)
}

fn is_sqlite_internal_table(entry: &MasterEntry) -> bool {
    entry.entry_type == "table" && entry.name.starts_with("sqlite_") && entry.root_page != 0
}

fn is_index_btree(entry: &MasterEntry) -> bool {
    entry.entry_type == "index" && entry.root_page != 0
}

fn is_virtual_table(entry: &MasterEntry) -> bool {
    entry.entry_type == "table"
        && (entry.root_page == 0
            || entry
                .sql
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("create virtual table"))
}

fn is_without_rowid_table(entry: &MasterEntry) -> bool {
    entry.entry_type == "table" && entry.sql.to_ascii_lowercase().contains("without rowid")
}

fn is_fts_virtual_table(entry: &MasterEntry) -> bool {
    if !is_virtual_table(entry) {
        return false;
    }

    let sql = entry.sql.to_ascii_lowercase();
    sql.contains(" using fts3")
        || sql.contains(" using fts4")
        || sql.contains(" using fts5")
        || sql.contains(" using \"fts")
        || sql.contains(" using 'fts")
        || sql.contains(" using [fts")
}

fn rowid_alias_column(columns: &[ColumnInfo]) -> Option<String> {
    columns
        .iter()
        .find(|column| column.pk && column.ctype.eq_ignore_ascii_case("INTEGER"))
        .map(|column| column.name.clone())
}

fn generated_columns(columns: &[ColumnInfo]) -> BTreeMap<String, GeneratedColumnKind> {
    columns
        .iter()
        .filter_map(|column| column.generated.map(|kind| (column.name.clone(), kind)))
        .collect()
}

fn semantic_key_columns(
    entry: Option<&MasterEntry>,
    columns: &[ColumnInfo],
    rowid_alias: Option<&str>,
) -> Vec<String> {
    let constraints = entry
        .map(MasterEntry::parse_key_constraints)
        .unwrap_or_default();

    for constraint in constraints
        .iter()
        .filter(|constraint| constraint.kind == KeyConstraintKind::PrimaryKey)
    {
        if let Some(columns) = resolve_key_columns(&constraint.columns, columns, rowid_alias) {
            return columns;
        }
    }

    for column in columns {
        if column.pk
            && rowid_alias != Some(column.name.as_str())
            && !column.ctype.eq_ignore_ascii_case("INTEGER")
        {
            return vec![column.name.clone()];
        }
    }

    for constraint in constraints
        .iter()
        .filter(|constraint| constraint.kind == KeyConstraintKind::Unique)
    {
        if let Some(columns) = resolve_key_columns(&constraint.columns, columns, rowid_alias) {
            return columns;
        }
    }

    for column in columns {
        if column.unique && rowid_alias != Some(column.name.as_str()) {
            return vec![column.name.clone()];
        }
    }

    Vec::new()
}

fn resolve_key_columns(
    key_columns: &[String],
    columns: &[ColumnInfo],
    rowid_alias: Option<&str>,
) -> Option<Vec<String>> {
    let mut resolved = Vec::with_capacity(key_columns.len());
    for key_column in key_columns {
        let column = columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(key_column))?;
        if rowid_alias == Some(column.name.as_str()) {
            return None;
        }
        resolved.push(column.name.clone());
    }
    Some(resolved)
}

/// Format SQL INSERT while preserving the SQLite rowid.
fn format_sql_insert(
    table: &str,
    columns: &[String],
    rowid_alias: Option<&str>,
    generated_columns: &BTreeMap<String, GeneratedColumnKind>,
    rowid: Option<i64>,
    row: &Record,
) -> String {
    let mut insert_columns = Vec::with_capacity(columns.len() + 1);
    let mut values = Vec::with_capacity(row.values.len() + 1);
    if let Some(rowid) = rowid {
        insert_columns.push(quote_identifier("rowid"));
        values.push(rowid.to_string());
    }
    for (column, value) in writable_column_values(columns, generated_columns, row) {
        if rowid.is_some() && rowid_alias == Some(column.as_str()) {
            continue;
        }
        insert_columns.push(quote_identifier(column));
        values.push(value.to_sql());
    }
    format!(
        "INSERT INTO {} ({}) VALUES ({});",
        quote_identifier(table),
        insert_columns.join(", "),
        values.join(", ")
    )
}

/// Format SQL DELETE by rowid
fn format_sql_delete(table: &str, rowid: i64) -> String {
    format!(
        "DELETE FROM {} WHERE rowid = {};",
        quote_identifier(table),
        rowid
    )
}

fn format_sql_delete_by_primary_key(table: &str, key: &[PrimaryKeyPart]) -> String {
    format!(
        "DELETE FROM {} WHERE {};",
        quote_identifier(table),
        primary_key_predicate(key)
    )
}

/// Format SQL UPDATE using column names and rowid
fn format_sql_update(
    table: &str,
    columns: &[String],
    rowid_alias: Option<&str>,
    generated_columns: &BTreeMap<String, GeneratedColumnKind>,
    rowid: i64,
    row: &Record,
) -> String {
    let set_clause: Vec<_> = writable_column_values(columns, generated_columns, row)
        .into_iter()
        .filter(|(col, _)| rowid_alias != Some(col.as_str()))
        .map(|(col, val)| format!("{} = {}", quote_identifier(col), val.to_sql()))
        .collect();
    if set_clause.is_empty() {
        return String::new();
    }

    format!(
        "UPDATE {} SET {} WHERE rowid = {};",
        quote_identifier(table),
        set_clause.join(", "),
        rowid
    )
}

fn format_sql_update_by_primary_key(
    table: &str,
    columns: &[String],
    generated_columns: &BTreeMap<String, GeneratedColumnKind>,
    key: &[PrimaryKeyPart],
    row: &Record,
) -> String {
    let key_columns: HashSet<&str> = key.iter().map(|part| part.column.as_str()).collect();
    let set_clause: Vec<_> = writable_column_values(columns, generated_columns, row)
        .into_iter()
        .filter(|(column, _)| !key_columns.contains(column.as_str()))
        .map(|(column, value)| format!("{} = {}", quote_identifier(column), value.to_sql()))
        .collect();
    if set_clause.is_empty() {
        return String::new();
    }
    format!(
        "UPDATE {} SET {} WHERE {};",
        quote_identifier(table),
        set_clause.join(", "),
        primary_key_predicate(key)
    )
}

fn primary_key_predicate(key: &[PrimaryKeyPart]) -> String {
    key.iter()
        .map(|part| match part.value {
            PrimaryKeyValue::Null => format!("{} IS NULL", quote_identifier(&part.column)),
            _ => format!(
                "{} = {}",
                quote_identifier(&part.column),
                part.value.to_sql()
            ),
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn format_primary_key(key: &[PrimaryKeyPart]) -> String {
    key.iter()
        .map(|part| format!("{}={}", quote_identifier(&part.column), part.value.to_sql()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn writable_column_values<'a>(
    columns: &'a [String],
    generated_columns: &BTreeMap<String, GeneratedColumnKind>,
    row: &'a Record,
) -> Vec<(&'a String, &'a Value)> {
    let mut values = Vec::new();
    let mut value_index = 0;
    for column in columns {
        match generated_columns.get(column) {
            Some(GeneratedColumnKind::Virtual) => continue,
            Some(GeneratedColumnKind::Stored) => {
                value_index += 1;
                continue;
            }
            None => {}
        }
        let Some(value) = row.values.get(value_index) else {
            break;
        };
        value_index += 1;
        values.push((column, value));
    }
    values
}

/// Escape SQL identifier
pub(crate) fn quote_identifier(id: &str) -> String {
    format!("\"{}\"", id.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_record(vals: Vec<Value>) -> Record {
        Record { values: vals }
    }

    #[test]
    fn test_row_level_diff_insert_only() {
        let diff = RowLevelDiff {
            from_lsn: graft::core::lsn::LSN::new(1),
            to_lsn: graft::core::lsn::LSN::new(2),
            analysis: RowLevelDiffAnalysis::default(),
            schema_changes: vec![],
            table_changes: vec![TableChanges {
                table_name: "users".into(),
                columns: vec!["id".into(), "name".into()],
                rowid_alias: Some("id".into()),
                generated_columns: BTreeMap::new(),
                semantic_key_columns: vec![],
                primary_key_columns: vec![],
                changes: vec![
                    RowChange::Insert {
                        rowid: 1,
                        row: make_record(vec![Value::Integer(1), Value::Text("Alice".into())]),
                    },
                    RowChange::Insert {
                        rowid: 2,
                        row: make_record(vec![Value::Integer(2), Value::Text("Bob".into())]),
                    },
                ],
            }],
            opaque_changes: vec![],
            telemetry: RowLevelDiffTelemetry::default(),
        };

        let sql = diff.to_sql();
        assert!(sql.contains("INSERT INTO \"users\" (\"rowid\", \"name\") VALUES (1, 'Alice')"));
        assert!(sql.contains("'Alice'"));
        assert!(sql.contains("'Bob'"));
        assert!(sql.contains("COMMIT"));
    }

    #[test]
    fn test_row_level_diff_delete_only() {
        let diff = RowLevelDiff {
            from_lsn: graft::core::lsn::LSN::new(1),
            to_lsn: graft::core::lsn::LSN::new(2),
            analysis: RowLevelDiffAnalysis::default(),
            schema_changes: vec![],
            table_changes: vec![TableChanges {
                table_name: "users".into(),
                columns: vec!["id".into(), "name".into()],
                rowid_alias: Some("id".into()),
                generated_columns: BTreeMap::new(),
                semantic_key_columns: vec![],
                primary_key_columns: vec![],
                changes: vec![RowChange::Delete {
                    rowid: 1,
                    row: make_record(vec![Value::Integer(1), Value::Text("Alice".into())]),
                }],
            }],
            opaque_changes: vec![],
            telemetry: RowLevelDiffTelemetry::default(),
        };

        let sql = diff.to_sql();
        assert!(sql.contains("DELETE FROM \"users\" WHERE rowid = 1"));
    }

    #[test]
    fn test_row_level_diff_update_only() {
        let diff = RowLevelDiff {
            from_lsn: graft::core::lsn::LSN::new(1),
            to_lsn: graft::core::lsn::LSN::new(2),
            analysis: RowLevelDiffAnalysis::default(),
            schema_changes: vec![],
            table_changes: vec![TableChanges {
                table_name: "users".into(),
                columns: vec!["id".into(), "name".into()],
                rowid_alias: Some("id".into()),
                generated_columns: BTreeMap::new(),
                semantic_key_columns: vec![],
                primary_key_columns: vec![],
                changes: vec![RowChange::Update {
                    rowid: 1,
                    old_row: make_record(vec![Value::Integer(1), Value::Text("Alice".into())]),
                    new_row: make_record(vec![Value::Integer(1), Value::Text("Alicia".into())]),
                }],
            }],
            opaque_changes: vec![],
            telemetry: RowLevelDiffTelemetry::default(),
        };

        let sql = diff.to_sql();
        assert!(sql.contains("UPDATE \"users\" SET"));
        assert!(sql.contains("'Alicia'"));
        assert!(sql.contains("rowid = 1"));
        assert!(!sql.contains("SET \"id\" ="));
    }

    #[test]
    fn test_row_level_diff_empty() {
        let diff = RowLevelDiff {
            from_lsn: graft::core::lsn::LSN::new(1),
            to_lsn: graft::core::lsn::LSN::new(2),
            analysis: RowLevelDiffAnalysis::default(),
            schema_changes: vec![],
            table_changes: vec![],
            opaque_changes: vec![],
            telemetry: RowLevelDiffTelemetry::default(),
        };

        let sql = diff.to_sql();
        assert!(sql.contains("COMMIT"));

        let report = diff.to_report();
        assert!(report.contains("Diff LSN"));
    }

    #[test]
    fn test_table_changes_to_sql_mixed() {
        let tc = TableChanges {
            table_name: "orders".into(),
            columns: vec!["id".into(), "amount".into()],
            rowid_alias: Some("id".into()),
            generated_columns: BTreeMap::new(),
            semantic_key_columns: vec![],
            primary_key_columns: vec![],
            changes: vec![
                RowChange::Insert {
                    rowid: 1,
                    row: make_record(vec![Value::Integer(1), Value::Real(99.99)]),
                },
                RowChange::Delete {
                    rowid: 2,
                    row: make_record(vec![Value::Integer(2), Value::Real(50.0)]),
                },
                RowChange::Update {
                    rowid: 3,
                    old_row: make_record(vec![Value::Integer(3), Value::Real(25.0)]),
                    new_row: make_record(vec![Value::Integer(3), Value::Real(30.0)]),
                },
            ],
        };

        let sql = tc.to_sql();
        assert!(sql.contains("INSERT"));
        assert!(sql.contains("DELETE"));
        assert!(sql.contains("UPDATE"));
        assert!(sql.contains("1"));
    }

    #[test]
    fn test_sql_insert_format() {
        let row = make_record(vec![Value::Null, Value::Text("test".into())]);
        let sql = format_sql_insert(
            "users",
            &["id".into(), "name".into()],
            Some("id"),
            &BTreeMap::new(),
            Some(7),
            &row,
        );
        assert_eq!(
            sql,
            "INSERT INTO \"users\" (\"rowid\", \"name\") VALUES (7, 'test');"
        );
    }

    #[test]
    fn test_sql_insert_format_preserves_hidden_rowid() {
        let row = make_record(vec![Value::Text("test".into())]);
        let sql = format_sql_insert(
            "users",
            &["name".into()],
            None,
            &BTreeMap::new(),
            Some(7),
            &row,
        );
        assert_eq!(
            sql,
            "INSERT INTO \"users\" (\"rowid\", \"name\") VALUES (7, 'test');"
        );
    }

    #[test]
    fn test_sql_insert_format_can_omit_hidden_rowid() {
        let row = make_record(vec![Value::Text("test".into())]);
        let sql = format_sql_insert(
            "users",
            &["name".into()],
            None,
            &BTreeMap::new(),
            None,
            &row,
        );
        assert_eq!(sql, "INSERT INTO \"users\" (\"name\") VALUES ('test');");
    }

    #[test]
    fn test_sql_insert_skips_stored_generated_columns() {
        let row = make_record(vec![
            Value::Integer(1),
            Value::Text("alpha".into()),
            Value::Text("ALPHA".into()),
        ]);
        let generated = BTreeMap::from([("body_upper".to_string(), GeneratedColumnKind::Stored)]);
        let sql = format_sql_insert(
            "docs",
            &["id".into(), "body".into(), "body_upper".into()],
            Some("id"),
            &generated,
            Some(1),
            &row,
        );
        assert_eq!(
            sql,
            "INSERT INTO \"docs\" (\"rowid\", \"body\") VALUES (1, 'alpha');"
        );
    }

    #[test]
    fn test_sql_update_skips_virtual_generated_columns_without_consuming_values() {
        let row = make_record(vec![Value::Integer(1), Value::Text("alpha".into())]);
        let generated = BTreeMap::from([("body_len".to_string(), GeneratedColumnKind::Virtual)]);
        let sql = format_sql_update(
            "docs",
            &["id".into(), "body_len".into(), "body".into()],
            Some("id"),
            &generated,
            1,
            &row,
        );
        assert_eq!(
            sql,
            "UPDATE \"docs\" SET \"body\" = 'alpha' WHERE rowid = 1;"
        );
    }

    #[test]
    fn test_sql_delete_format() {
        let sql = format_sql_delete("users", 42);
        assert_eq!(sql, "DELETE FROM \"users\" WHERE rowid = 42;");
    }

    #[test]
    fn test_sql_update_format() {
        let row = make_record(vec![Value::Null, Value::Text("new_name".into())]);
        let sql = format_sql_update(
            "users",
            &["id".into(), "name".into()],
            Some("id"),
            &BTreeMap::new(),
            1,
            &row,
        );
        assert!(sql.contains("UPDATE \"users\" SET"));
        assert!(sql.contains("\"name\" = 'new_name'"));
        assert!(!sql.contains("SET \"id\" ="));
    }

    #[test]
    fn test_quote_identifier_quotes_simple_names_and_keywords() {
        assert_eq!(quote_identifier("users"), "\"users\"");
        assert_eq!(quote_identifier("my_table"), "\"my_table\"");
        assert_eq!(quote_identifier("_col"), "\"_col\"");
        assert_eq!(quote_identifier("Table"), "\"Table\"");
    }

    #[test]
    fn test_quote_identifier_special() {
        assert_eq!(quote_identifier("my table"), "\"my table\"");
        assert_eq!(quote_identifier("123col"), "\"123col\"");
        assert_eq!(quote_identifier("col-name"), "\"col-name\"");
    }

    #[test]
    fn test_count_changes() {
        let changes = vec![
            RowChange::Insert { rowid: 1, row: make_record(vec![]) },
            RowChange::Insert { rowid: 2, row: make_record(vec![]) },
            RowChange::Delete { rowid: 3, row: make_record(vec![]) },
            RowChange::Update {
                rowid: 4,
                old_row: make_record(vec![]),
                new_row: make_record(vec![]),
            },
        ];
        let (inserts, deletes, updates) = count_changes(&changes);
        assert_eq!(inserts, 2);
        assert_eq!(deletes, 1);
        assert_eq!(updates, 1);
    }

    #[test]
    fn direct_without_rowid_order_accepts_eidos_binary_primary_keys() {
        let entry = MasterEntry {
            entry_type: "table".into(),
            name: "records".into(),
            table_name: "records".into(),
            root_page: 2,
            sql: "CREATE TABLE records (id TEXT PRIMARY KEY COLLATE BINARY, name TEXT COLLATE NOCASE) STRICT, WITHOUT ROWID".into(),
        };

        let layout = WithoutRowidLayout::from_entry(&entry).expect("Eidos layout should stream");
        assert_eq!(layout.primary_key_columns, ["id"]);
    }

    #[test]
    fn direct_without_rowid_order_rejects_custom_or_descending_primary_keys() {
        for sql in [
            "CREATE TABLE records (id TEXT PRIMARY KEY COLLATE NOCASE) STRICT, WITHOUT ROWID",
            "CREATE TABLE records (id TEXT PRIMARY KEY DESC) STRICT, WITHOUT ROWID",
            "CREATE TABLE records (id TEXT, PRIMARY KEY (id DESC)) STRICT, WITHOUT ROWID",
            "CREATE TABLE records (id TEXT PRIMARY KEY) WITHOUT ROWID",
        ] {
            let entry = MasterEntry {
                entry_type: "table".into(),
                name: "records".into(),
                table_name: "records".into(),
                root_page: 2,
                sql: sql.into(),
            };
            assert!(
                WithoutRowidLayout::from_entry(&entry).is_none(),
                "accepted unsupported layout: {sql}"
            );
        }
    }
}
