//! SQL Diff Implementation - Built-in table-level analysis

use std::fmt::Write;

use graft::core::lsn::LSN;
use graft::rt::runtime::Runtime;

use crate::file::vol_file::VolFile;
use crate::vfs::ErrCtx;

/// Generate summary diff report using built-in row-level diff
pub fn generate_diff_report(
    runtime: &Runtime,
    file: &VolFile,
    from_lsn: LSN,
    to_lsn: LSN,
) -> Result<String, ErrCtx> {
    // Use built-in row-level diff to get actual table changes
    let row_diff = crate::row_level_diff::row_level_diff(runtime, &file.vid, from_lsn, to_lsn)
        .map_err(|e| ErrCtx::PragmaErr(format!("Diff error: {e:?}").into()))?;

    let mut output = String::new();

    writeln!(&mut output, "Diff Summary (LSN {from_lsn} -> {to_lsn}):").unwrap();
    writeln!(&mut output).unwrap();

    if row_diff.table_changes.is_empty() {
        writeln!(&mut output, "No changes detected.").unwrap();
        return Ok(output);
    }

    writeln!(
        &mut output,
        "Changed tables: {}",
        row_diff.table_changes.len()
    )
    .unwrap();
    writeln!(&mut output).unwrap();

    for table in &row_diff.table_changes {
        // Count change types
        let mut inserts = 0;
        let mut deletes = 0;
        let mut updates = 0;

        for change in &table.changes {
            match change {
                crate::row_level_diff::RowChange::Insert { .. } => inserts += 1,
                crate::row_level_diff::RowChange::Delete { .. } => deletes += 1,
                crate::row_level_diff::RowChange::Update { .. } => updates += 1,
            }
        }

        write!(
            &mut output,
            "  {}: {} changes",
            table.table_name,
            table.changes.len()
        )
        .unwrap();

        let mut parts = Vec::new();
        if inserts > 0 {
            parts.push(format!("+{inserts} inserts"));
        }
        if deletes > 0 {
            parts.push(format!("-{deletes} deletes"));
        }
        if updates > 0 {
            parts.push(format!("~{updates} updates"));
        }

        if !parts.is_empty() {
            write!(&mut output, " ({})", parts.join(", ")).unwrap();
        }
        writeln!(&mut output).unwrap();
    }

    writeln!(&mut output).unwrap();
    writeln!(
        &mut output,
        "Use 'PRAGMA graft_debug_volume_diff = \"{from_lsn},{to_lsn},rows\"' for detailed row-level diff."
    )
    .unwrap();

    Ok(output)
}
