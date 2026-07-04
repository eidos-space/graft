//! Row-level Diff - Built-in Implementation
//!
//! Parses `SQLite` B-tree directly to compare row data between versions

use std::collections::{BTreeMap, HashSet};

use crate::sqlite_parse::{
    ColumnInfo, GeneratedColumnKind, KeyConstraintKind, MasterEntry, ParseError, Record,
    TableScanner, Value, read_all_rows,
};
use graft::core::{PageIdx, VolumeId, lsn::LSN};
use graft::rt::runtime::Runtime;
use graft::snapshot::Snapshot;
use graft::volume_reader::{VolumeRead, VolumeReader};

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
    SchemaEntries,
    OpaqueTableDetection,
    SemanticInsertKeys,
}

impl RowLevelDiffCapability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RowidTableRows => "rowid_table_rows",
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
    WithoutRowidTable,
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
            Self::WithoutRowidTable => "without_rowid_table",
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
}

/// Changes for a single table
#[derive(Debug, Clone, PartialEq)]
pub struct TableChanges {
    pub table_name: String,
    pub columns: Vec<String>,
    pub rowid_alias: Option<String>,
    pub generated_columns: BTreeMap<String, GeneratedColumnKind>,
    pub semantic_key_columns: Vec<String>,
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
    WithoutRowidTable,
    SqliteInternalTable,
    IndexBtree,
}

impl OpaqueChangeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VirtualTable => "virtual_table",
            Self::FtsShadowTable => "fts_shadow_table",
            Self::WithoutRowidTable => "without_rowid_table",
            Self::SqliteInternalTable => "sqlite_internal_table",
            Self::IndexBtree => "index_btree",
        }
    }

    fn limitation_kind(self) -> RowLevelDiffLimitationKind {
        match self {
            Self::VirtualTable => RowLevelDiffLimitationKind::VirtualTable,
            Self::FtsShadowTable => RowLevelDiffLimitationKind::FtsShadowTable,
            Self::WithoutRowidTable => RowLevelDiffLimitationKind::WithoutRowidTable,
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
            RowChange::Insert { .. } => inserts += 1,
            RowChange::Delete { .. } => deletes += 1,
            RowChange::Update { .. } => updates += 1,
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
    let from_vol = runtime.volume_from_snapshot(from_snapshot)?;
    let to_vol = match runtime.volume_from_snapshot(to_snapshot) {
        Ok(to_vol) => to_vol,
        Err(err) => {
            let _ = runtime.volume_delete(&from_vol.vid);
            return Err(err);
        }
    };

    let from_vid = from_vol.vid.clone();
    let to_vid = to_vol.vid.clone();
    let from_lsn = from_snapshot.head().map_or(LSN::FIRST, |(_, lsn)| lsn);
    let to_lsn = to_snapshot.head().map_or(LSN::FIRST, |(_, lsn)| lsn);

    let result = row_level_diff_checked_out(runtime, &from_vid, &to_vid, from_lsn, to_lsn);
    let _ = runtime.volume_delete(&from_vol.vid);
    let _ = runtime.volume_delete(&to_vol.vid);

    result
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

    // Read master table for both versions
    let from_scanner = TableScanner::new(&from_reader).map_err(|e| {
        tracing::error!("Failed to create from_scanner for {}: {:?}", from_vid, e);
        graft::err::LogicalErr::Other(format!("Failed to parse B-tree for {from_vid}: {e:?}"))
    })?;
    let to_scanner = TableScanner::new(&to_reader).map_err(|e| {
        tracing::error!("Failed to create to_scanner for {}: {:?}", to_vid, e);
        graft::err::LogicalErr::Other(format!("Failed to parse B-tree for {to_vid}: {e:?}"))
    })?;

    let from_master = from_scanner.read_master_table().map_err(|e| {
        tracing::error!("Failed to read from_master_table for {}: {:?}", from_vid, e);
        graft::err::LogicalErr::Other(format!("Failed to read schema for {from_vid}: {e:?}"))
    })?;
    let to_master = to_scanner.read_master_table().map_err(|e| {
        tracing::error!("Failed to read to_master_table for {}: {:?}", to_vid, e);
        graft::err::LogicalErr::Other(format!("Failed to read schema for {to_vid}: {e:?}"))
    })?;

    // Compare schema and tables
    let schema_changes = diff_schema_entries(&from_master, &to_master);
    let mut table_changes = Vec::new();
    let mut limitations = diff_parser_limitations(&from_scanner, &to_scanner);

    // Collect all table names
    let ignored_table_infos = ignored_row_diff_table_infos(&from_master, &to_master);
    limitations.extend(ignored_table_infos.values().map(|table| {
        RowLevelDiffLimitation::new(table.reason.limitation_kind(), Some(table.name.clone()))
    }));
    limitations.extend(generated_column_limitations(&from_master, &to_master));
    dedupe_limitations(&mut limitations);
    let ignored_tables: HashSet<String> = ignored_table_infos.keys().cloned().collect();
    let opaque_changes = diff_opaque_tables(
        &from_reader,
        &to_reader,
        &from_master,
        &to_master,
        &ignored_table_infos,
    );
    let index_btree_changes = diff_index_btrees(&from_reader, &to_reader, &from_master, &to_master);
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
        if is_diffable_table(entry, &ignored_tables) {
            all_tables.insert(entry.name.clone());
        }
    }
    for entry in &to_master {
        if is_diffable_table(entry, &ignored_tables) {
            all_tables.insert(entry.name.clone());
        }
    }

    // Compare each table
    for table_name in all_tables {
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
        let columns: Vec<String> = column_infos.into_iter().map(|c| c.name).collect();

        let changes = match (from_entry, to_entry) {
            (Some(from), Some(to)) => {
                // Table exists in both, diff rows
                diff_table_rows(&from_reader, &to_reader, from, to)?
            }
            (Some(from), None) => {
                // Table deleted, all rows are DELETE
                let rows = read_all_rows(&from_reader, from.root_page)
                    .map_err(|e| table_read_err("from", from, e))?;
                rows.into_iter()
                    .map(|(rowid, row)| RowChange::Delete { rowid, row })
                    .collect()
            }
            (None, Some(to)) => {
                // New table, all rows are INSERT
                let rows = read_all_rows(&to_reader, to.root_page)
                    .map_err(|e| table_read_err("to", to, e))?;
                rows.into_iter()
                    .map(|(rowid, row)| RowChange::Insert { rowid, row })
                    .collect()
            }
            (None, None) => vec![],
        };

        if !changes.is_empty() {
            table_changes.push(TableChanges {
                table_name,
                columns,
                rowid_alias,
                generated_columns,
                semantic_key_columns,
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
    from_reader: &VolumeReader,
    to_reader: &VolumeReader,
    from_entry: &MasterEntry,
    to_entry: &MasterEntry,
) -> Result<Vec<RowChange>, graft::err::LogicalErr> {
    // Read rows from both versions
    let from_rows = read_all_rows(from_reader, from_entry.root_page)
        .map_err(|e| table_read_err("from", from_entry, e))?;
    let to_rows = read_all_rows(to_reader, to_entry.root_page)
        .map_err(|e| table_read_err("to", to_entry, e))?;

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
            if is_without_rowid_table(entry) {
                ignored
                    .entry(entry.name.clone())
                    .or_insert_with(|| IgnoredTable {
                        name: entry.name.clone(),
                        reason: OpaqueChangeReason::WithoutRowidTable,
                        owner: None,
                    });
            }
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
    from_reader: &VolumeReader,
    to_reader: &VolumeReader,
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
    from_reader: &VolumeReader,
    to_reader: &VolumeReader,
    from_master: &[MasterEntry],
    to_master: &[MasterEntry],
) -> Vec<OpaqueChange> {
    let mut changes = Vec::new();
    let mut names = HashSet::new();
    for entry in from_master.iter().chain(to_master.iter()) {
        if is_index_btree(entry) {
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
    from_reader: &VolumeReader,
    to_reader: &VolumeReader,
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

    if matches!(
        reason,
        OpaqueChangeReason::WithoutRowidTable | OpaqueChangeReason::SqliteInternalTable
    ) {
        return opaque_root_page_change_kind(from_reader, to_reader, from, to);
    }

    match diff_table_rows(from_reader, to_reader, from, to) {
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
    from_reader: &VolumeReader,
    to_reader: &VolumeReader,
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
fn quote_identifier(id: &str) -> String {
    if id.chars().all(|c| c.is_alphanumeric() || c == '_')
        && !id.chars().next().unwrap().is_ascii_digit()
    {
        id.to_string()
    } else {
        format!("\"{}\"", id.replace('"', "\"\""))
    }
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
        };

        let sql = diff.to_sql();
        assert!(sql.contains("INSERT INTO users (rowid, name) VALUES (1, 'Alice')"));
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
                changes: vec![RowChange::Delete {
                    rowid: 1,
                    row: make_record(vec![Value::Integer(1), Value::Text("Alice".into())]),
                }],
            }],
            opaque_changes: vec![],
        };

        let sql = diff.to_sql();
        assert!(sql.contains("DELETE FROM users WHERE rowid = 1"));
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
                changes: vec![RowChange::Update {
                    rowid: 1,
                    old_row: make_record(vec![Value::Integer(1), Value::Text("Alice".into())]),
                    new_row: make_record(vec![Value::Integer(1), Value::Text("Alicia".into())]),
                }],
            }],
            opaque_changes: vec![],
        };

        let sql = diff.to_sql();
        assert!(sql.contains("UPDATE users SET"));
        assert!(sql.contains("'Alicia'"));
        assert!(sql.contains("rowid = 1"));
        assert!(!sql.contains("SET id ="));
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
        assert_eq!(sql, "INSERT INTO users (rowid, name) VALUES (7, 'test');");
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
        assert_eq!(sql, "INSERT INTO users (rowid, name) VALUES (7, 'test');");
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
        assert_eq!(sql, "INSERT INTO users (name) VALUES ('test');");
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
        assert_eq!(sql, "INSERT INTO docs (rowid, body) VALUES (1, 'alpha');");
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
        assert_eq!(sql, "UPDATE docs SET body = 'alpha' WHERE rowid = 1;");
    }

    #[test]
    fn test_sql_delete_format() {
        let sql = format_sql_delete("users", 42);
        assert_eq!(sql, "DELETE FROM users WHERE rowid = 42;");
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
        assert!(sql.contains("UPDATE users SET"));
        assert!(sql.contains("name = 'new_name'"));
        assert!(!sql.contains("SET id ="));
    }

    #[test]
    fn test_quote_identifier_simple() {
        assert_eq!(quote_identifier("users"), "users");
        assert_eq!(quote_identifier("my_table"), "my_table");
        assert_eq!(quote_identifier("_col"), "_col");
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
}
