use std::{
    fmt::Write,
    path::{Path, PathBuf},
};

use graft::{core::lsn::LSN, volume_reader::VolumeRead};

use crate::{
    error::ErrCtx,
    json::{
        JsonRowDiffTelemetry, JsonSqliteFileDiffResult, JsonSqliteFileDiffSide,
        JsonSqliteFileRowDiff,
    },
    pragma::{
        row_diff::{
            json_diff_capabilities, json_diff_limitations, json_opaque_changes,
            json_schema_changes, json_table_changes,
        },
        sqlite_worktree::PhysicalSqliteReader,
    },
    row_level_diff::{RowLevelDiff, row_level_diff_readers},
};

/// Options for comparing two ordinary physical `SQLite` database files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SqliteFileDiffOptions {
    /// Expand a changed file pair into schema, table, and row changes.
    pub rows: bool,
}

/// Read-only comparison of two ordinary physical `SQLite` database files.
#[derive(Debug)]
pub struct SqliteFileDiff {
    pub from: PathBuf,
    pub to: PathBuf,
    pub from_page_count: u32,
    pub to_page_count: u32,
    pub changed: bool,
    pub rows: bool,
    pub row_diff: Option<RowLevelDiff>,
}

impl SqliteFileDiff {
    /// Format the same human-oriented shape used by the CLI.
    pub fn to_report(&self) -> Result<String, ErrCtx> {
        let mut report = String::new();
        writeln!(
            &mut report,
            "{} --no-index {}..{}",
            if self.rows { "Row Diff" } else { "Diff" },
            self.from.display(),
            self.to.display()
        )?;
        if !self.changed {
            writeln!(&mut report, "No changes.")?;
            return Ok(report);
        }

        writeln!(
            &mut report,
            "modified: {} -> {}",
            self.from.display(),
            self.to.display()
        )?;
        writeln!(
            &mut report,
            "  from: {} page(s), physical file",
            self.from_page_count
        )?;
        writeln!(
            &mut report,
            "  to:   {} page(s), physical file",
            self.to_page_count
        )?;
        if let Some(row_diff) = &self.row_diff {
            for line in row_diff.to_report_body().lines() {
                writeln!(&mut report, "  {line}")?;
            }
        }
        Ok(report)
    }

    /// Convert the comparison into the stable JSON CLI contract.
    pub fn to_json(&self) -> JsonSqliteFileDiffResult {
        let row_diff = self.row_diff.as_ref().map(|diff| JsonSqliteFileRowDiff {
            logical_status: diff.logical_status().as_str().to_string(),
            capabilities: json_diff_capabilities(diff),
            limitations: json_diff_limitations(diff),
            schema_changes: json_schema_changes(&diff.schema_changes),
            tables: json_table_changes(&diff.table_changes),
            opaque_changes: json_opaque_changes(&diff.opaque_changes),
            telemetry: JsonRowDiffTelemetry {
                requested_table: diff.telemetry.requested_table.clone(),
                tables_considered: diff.telemetry.tables_considered,
                tables_scanned: diff.telemetry.tables_scanned,
            },
        });
        JsonSqliteFileDiffResult {
            from: JsonSqliteFileDiffSide {
                path: self.from.to_string_lossy().into_owned(),
                page_count: self.from_page_count,
            },
            to: JsonSqliteFileDiffSide {
                path: self.to.to_string_lossy().into_owned(),
                page_count: self.to_page_count,
            },
            changed: self.changed,
            kind: "sqlite_database".to_string(),
            rows: self.rows,
            row_diff,
        }
    }
}

/// Compare two physical `SQLite` files without discovering or modifying a Graft repository.
///
/// Each side is captured through `SQLite`'s online backup API, so committed WAL frames are included
/// and the diff observes an internally consistent image even while an application has the database
/// open. File-level equality follows Graft's `SQLite` snapshot semantics and ignores volatile
/// page-1 cache-invalidation counters plus the last-writer library version.
pub fn diff_sqlite_files(
    from: &Path,
    to: &Path,
    options: SqliteFileDiffOptions,
) -> Result<SqliteFileDiff, ErrCtx> {
    let from_reader = PhysicalSqliteReader::open(from)?;
    let to_reader = PhysicalSqliteReader::open(to)?;
    let changed = !from_reader.matches_reader(&to_reader)?;
    let row_diff = if options.rows && changed {
        Some(row_level_diff_readers(
            &from_reader,
            &to_reader,
            LSN::FIRST,
            LSN::FIRST.saturating_next(),
        )?)
    } else {
        None
    };

    Ok(SqliteFileDiff {
        from: from.to_path_buf(),
        to: to.to_path_buf(),
        from_page_count: from_reader.page_count().to_u32(),
        to_page_count: to_reader.page_count().to_u32(),
        changed,
        rows: options.rows,
        row_diff,
    })
}
