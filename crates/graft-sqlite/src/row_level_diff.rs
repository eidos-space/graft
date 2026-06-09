//! Row-level Diff - Built-in Implementation
//!
//! Parses `SQLite` B-tree directly to compare row data between versions

use crate::sqlite_parse::{MasterEntry, ParseError, Record, TableScanner, read_all_rows};
use graft::core::{VolumeId, lsn::LSN};
use graft::rt::runtime::Runtime;
use graft::volume_reader::VolumeReader;

/// Type of row change
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
pub struct TableChanges {
    pub table_name: String,
    pub columns: Vec<String>,
    pub changes: Vec<RowChange>,
}

impl TableChanges {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Generate SQL statements using actual column names
    pub fn to_sql(&self) -> String {
        let mut sql = String::new();

        for change in &self.changes {
            match change {
                RowChange::Insert { rowid, row } => {
                    sql.push_str(&format_sql_insert(
                        &self.table_name,
                        &self.columns,
                        *rowid,
                        row,
                    ));
                    sql.push('\n');
                }
                RowChange::Delete { rowid, .. } => {
                    sql.push_str(&format_sql_delete(&self.table_name, *rowid));
                    sql.push('\n');
                }
                RowChange::Update { rowid, new_row, .. } => {
                    sql.push_str(&format_sql_update(
                        &self.table_name,
                        &self.columns,
                        *rowid,
                        new_row,
                    ));
                    sql.push('\n');
                }
            }
        }

        sql
    }
}

/// Row-level diff result
#[derive(Debug)]
pub struct RowLevelDiff {
    pub from_lsn: LSN,
    pub to_lsn: LSN,
    pub table_changes: Vec<TableChanges>,
}

impl RowLevelDiff {
    /// Generate complete SQL diff
    pub fn to_sql(&self) -> String {
        let mut sql = format!(
            "-- Row-level Diff: LSN {} -> {}\n",
            self.from_lsn, self.to_lsn
        );
        sql.push_str("BEGIN TRANSACTION;\n\n");

        for table in &self.table_changes {
            if !table.is_empty() {
                sql.push_str(&format!("-- Table: {}\n", table.table_name));
                sql.push_str(&table.to_sql());
                sql.push('\n');
            }
        }

        sql.push_str("COMMIT;\n");
        sql
    }

    /// Generate human-readable report
    pub fn to_report(&self) -> String {
        let mut report = format!("Diff LSN {} -> {}\n", self.from_lsn, self.to_lsn);
        report.push_str("============================\n\n");

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

    // Compare tables
    let mut table_changes = Vec::new();

    // Collect all table names
    let mut all_tables: std::collections::HashSet<String> = std::collections::HashSet::new();
    for entry in &from_master {
        if entry.entry_type == "table" && !entry.name.starts_with("sqlite_") {
            all_tables.insert(entry.name.clone());
        }
    }
    for entry in &to_master {
        if entry.entry_type == "table" && !entry.name.starts_with("sqlite_") {
            all_tables.insert(entry.name.clone());
        }
    }

    // Compare each table
    for table_name in all_tables {
        let from_entry = from_master.iter().find(|e| e.name == table_name);
        let to_entry = to_master.iter().find(|e| e.name == table_name);

        // Get columns from schema (prefer to-entry, fallback to from-entry)
        let columns: Vec<String> = to_entry
            .or(from_entry)
            .map(|e| e.parse_columns().into_iter().map(|c| c.name).collect())
            .unwrap_or_default();

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
            table_changes.push(TableChanges { table_name, columns, changes });
        }
    }

    Ok(RowLevelDiff { from_lsn, to_lsn, table_changes })
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

/// Format SQL INSERT (values only, `SQLite` auto-assigns rowid)
fn format_sql_insert(table: &str, _columns: &[String], _rowid: i64, row: &Record) -> String {
    let values: Vec<_> = row.values.iter().map(|v| v.to_sql()).collect();
    format!(
        "INSERT INTO {} VALUES ({});",
        quote_identifier(table),
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
fn format_sql_update(table: &str, columns: &[String], rowid: i64, row: &Record) -> String {
    let set_clause: Vec<_> = if columns.len() == row.values.len() {
        columns
            .iter()
            .zip(row.values.iter())
            .map(|(col, val)| format!("{} = {}", quote_identifier(col), val.to_sql()))
            .collect()
    } else {
        // Fallback: use positional names when column count doesn't match
        row.values
            .iter()
            .enumerate()
            .map(|(i, val)| format!("col{} = {}", i, val.to_sql()))
            .collect()
    };

    format!(
        "UPDATE {} SET {} WHERE rowid = {};",
        quote_identifier(table),
        set_clause.join(", "),
        rowid
    )
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
    use crate::sqlite_parse::Value;

    fn make_record(vals: Vec<Value>) -> Record {
        Record { values: vals }
    }

    #[test]
    fn test_row_level_diff_insert_only() {
        let diff = RowLevelDiff {
            from_lsn: graft::core::lsn::LSN::new(1),
            to_lsn: graft::core::lsn::LSN::new(2),
            table_changes: vec![TableChanges {
                table_name: "users".into(),
                columns: vec!["id".into(), "name".into()],
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
        };

        let sql = diff.to_sql();
        assert!(sql.contains("INSERT INTO users VALUES"));
        assert!(sql.contains("'Alice'"));
        assert!(sql.contains("'Bob'"));
        assert!(sql.contains("COMMIT"));
    }

    #[test]
    fn test_row_level_diff_delete_only() {
        let diff = RowLevelDiff {
            from_lsn: graft::core::lsn::LSN::new(1),
            to_lsn: graft::core::lsn::LSN::new(2),
            table_changes: vec![TableChanges {
                table_name: "users".into(),
                columns: vec!["id".into(), "name".into()],
                changes: vec![RowChange::Delete {
                    rowid: 1,
                    row: make_record(vec![Value::Integer(1), Value::Text("Alice".into())]),
                }],
            }],
        };

        let sql = diff.to_sql();
        assert!(sql.contains("DELETE FROM users WHERE rowid = 1"));
    }

    #[test]
    fn test_row_level_diff_update_only() {
        let diff = RowLevelDiff {
            from_lsn: graft::core::lsn::LSN::new(1),
            to_lsn: graft::core::lsn::LSN::new(2),
            table_changes: vec![TableChanges {
                table_name: "users".into(),
                columns: vec!["id".into(), "name".into()],
                changes: vec![RowChange::Update {
                    rowid: 1,
                    old_row: make_record(vec![Value::Integer(1), Value::Text("Alice".into())]),
                    new_row: make_record(vec![Value::Integer(1), Value::Text("Alicia".into())]),
                }],
            }],
        };

        let sql = diff.to_sql();
        assert!(sql.contains("UPDATE users SET"));
        assert!(sql.contains("'Alicia'"));
        assert!(sql.contains("rowid = 1"));
    }

    #[test]
    fn test_row_level_diff_empty() {
        let diff = RowLevelDiff {
            from_lsn: graft::core::lsn::LSN::new(1),
            to_lsn: graft::core::lsn::LSN::new(2),
            table_changes: vec![],
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
        let row = make_record(vec![Value::Integer(1), Value::Text("test".into())]);
        let sql = format_sql_insert("users", &["id".into(), "name".into()], 1, &row);
        assert_eq!(sql, "INSERT INTO users VALUES (1, 'test');");
    }

    #[test]
    fn test_sql_delete_format() {
        let sql = format_sql_delete("users", 42);
        assert_eq!(sql, "DELETE FROM users WHERE rowid = 42;");
    }

    #[test]
    fn test_sql_update_format() {
        let row = make_record(vec![Value::Integer(1), Value::Text("new_name".into())]);
        let sql = format_sql_update("users", &["id".into(), "name".into()], 1, &row);
        assert!(sql.contains("UPDATE users SET"));
        assert!(sql.contains("id = 1"));
        assert!(sql.contains("name = 'new_name'"));
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
