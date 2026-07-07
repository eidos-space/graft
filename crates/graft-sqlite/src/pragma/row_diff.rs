use super::*;

pub(super) fn row_diff_impl(
    runtime: &Runtime,
    file: &VolFile,
    from: LSN,
    to: LSN,
) -> Result<Option<String>, ErrCtx> {
    let mut output = String::new();

    // Call row-level diff
    let diff = crate::row_level_diff::row_level_diff(runtime, &file.vid, from, to)
        .map_err(|e| ErrCtx::PragmaErr(format!("Row diff error: {e:?}").into()))?;

    writeln!(&mut output, "Row-level diff from LSN {from} to LSN {to}")?;
    writeln!(&mut output)?;

    if diff.table_changes.is_empty() && diff.opaque_changes.is_empty() {
        writeln!(&mut output, "No table changes detected.")?;
        return Ok(Some(output));
    }

    writeln!(&mut output, "Changed tables: {}", diff.table_changes.len())?;
    writeln!(&mut output)?;

    // Show changes for each table
    for table in &diff.table_changes {
        writeln!(&mut output, "Table: {}", table.table_name)?;
        writeln!(&mut output, "  Changes: {}", table.changes.len())?;

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

        if inserts > 0 {
            writeln!(&mut output, "    +{inserts} inserts")?;
        }
        if deletes > 0 {
            writeln!(&mut output, "    -{deletes} deletes")?;
        }
        if updates > 0 {
            writeln!(&mut output, "    ~{updates} updates")?;
        }

        // Show details for first few changes
        for (i, change) in table.changes.iter().take(5).enumerate() {
            match change {
                crate::row_level_diff::RowChange::Insert { rowid, row } => {
                    writeln!(&mut output, "    [{}] INSERT rowid={}", i + 1, rowid)?;
                    writeln!(
                        &mut output,
                        "      values: {:?}",
                        row.values
                            .iter()
                            .map(|v| format!("{v:?}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )?;
                }
                crate::row_level_diff::RowChange::Delete { rowid, .. } => {
                    writeln!(&mut output, "    [{}] DELETE rowid={}", i + 1, rowid)?;
                }
                crate::row_level_diff::RowChange::Update { rowid, old_row, new_row } => {
                    writeln!(&mut output, "    [{}] UPDATE rowid={}", i + 1, rowid)?;
                    writeln!(
                        &mut output,
                        "      old: {:?}",
                        old_row
                            .values
                            .iter()
                            .map(|v| format!("{v:?}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )?;
                    writeln!(
                        &mut output,
                        "      new: {:?}",
                        new_row
                            .values
                            .iter()
                            .map(|v| format!("{v:?}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )?;
                }
            }
        }

        if table.changes.len() > 5 {
            writeln!(
                &mut output,
                "    ... and {} more changes",
                table.changes.len() - 5
            )?;
        }

        writeln!(&mut output)?;
    }

    if !diff.opaque_changes.is_empty() {
        writeln!(&mut output, "Opaque changes:")?;
        for change in &diff.opaque_changes {
            let owner = change
                .owner
                .as_ref()
                .map(|owner| format!(" owned by {owner}"))
                .unwrap_or_default();
            writeln!(
                &mut output,
                "  {} {} ({}{})",
                change.change.as_str(),
                change.name,
                change.reason.as_str(),
                owner
            )?;
        }
        writeln!(&mut output)?;
    }

    // Generate SQL script
    writeln!(&mut output, "-- SQL Script --")?;
    for table in &diff.table_changes {
        writeln!(&mut output, "{}", table.to_sql())?;
    }

    Ok(Some(output))
}

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
) -> Result<crate::json::JsonRepoRowDiffResult, ErrCtx> {
    let paths = diff
        .paths
        .iter()
        .map(|path| crate::json::JsonRepoPathDiff {
            path: path.path.clone(),
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
            match repo_file_row_diff(runtime, repo, file) {
                Ok(Some(row_diff)) => Ok(crate::json::JsonRepoRowDiffFile {
                    path: file.path.clone(),
                    change,
                    kind,
                    storage,
                    row_diff_available: true,
                    logical_status: row_diff.logical_status().as_str().to_string(),
                    capabilities: json_diff_capabilities(&row_diff),
                    limitations: json_diff_limitations(&row_diff),
                    message: None,
                    tables: json_table_changes(&row_diff.table_changes),
                    opaque_changes: json_opaque_changes(&row_diff.opaque_changes),
                }),
                Ok(None) => Ok(crate::json::JsonRepoRowDiffFile {
                    path: file.path.clone(),
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
                    tables: Vec::new(),
                    opaque_changes: Vec::new(),
                }),
                Err(err) => Ok(crate::json::JsonRepoRowDiffFile {
                    path: file.path.clone(),
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
                    tables: Vec::new(),
                    opaque_changes: Vec::new(),
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

pub(super) fn json_table_changes(
    changes: &[crate::row_level_diff::TableChanges],
) -> Vec<crate::json::JsonTableChanges> {
    changes
        .iter()
        .map(|table| crate::json::JsonTableChanges {
            name: table.table_name.clone(),
            columns: table.columns.clone(),
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
            rowid: *rowid,
            values: row
                .values
                .iter()
                .map(crate::json::JsonRowChange::value_to_json)
                .collect(),
            old_values: None,
        },
        crate::row_level_diff::RowChange::Delete { rowid, row } => crate::json::JsonRowChange {
            op: "delete".into(),
            rowid: *rowid,
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
                rowid: *rowid,
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

/// Count changes for JSON summary
pub(super) fn count_changes_json(
    changes: &[crate::row_level_diff::RowChange],
) -> (usize, usize, usize) {
    let mut inserts = 0;
    let mut deletes = 0;
    let mut updates = 0;
    for change in changes {
        match change {
            crate::row_level_diff::RowChange::Insert { .. } => inserts += 1,
            crate::row_level_diff::RowChange::Delete { .. } => deletes += 1,
            crate::row_level_diff::RowChange::Update { .. } => updates += 1,
        }
    }
    (inserts, deletes, updates)
}

/// Generate JSON volume info
pub(super) fn json_volume_info(
    runtime: &Runtime,
    file: &VolFile,
) -> Result<crate::json::JsonVolumeInfo, ErrCtx> {
    let state = runtime.volume_get(&file.vid)?;
    let page_count = file.page_count()?;
    let snapshot_size_bytes =
        (graft::core::page::PAGESIZE.as_usize() as u64) * (page_count.to_usize() as u64);

    Ok(crate::json::JsonVolumeInfo {
        vid: state.vid.to_string(),
        local: state.local.to_string(),
        remote: state.remote.to_string(),
        page_count: page_count.to_u32(),
        snapshot_size_bytes,
        snapshot_pages: page_count.to_u32(),
    })
}

/// Local struct for table log entries (text and JSON output)
pub(super) struct TableLogEntry {
    pub(super) lsn: u64,
    pub(super) timestamp_ms: Option<u64>,
    pub(super) when: String,
    pub(super) summary: String,
    pub(super) detail: String,
}

/// Find all commits that modified a specific table by diffing adjacent LSN pairs.
pub(super) fn table_log_entries(
    runtime: &Runtime,
    vid: &VolumeId,
    table: &str,
) -> Result<Vec<TableLogEntry>, ErrCtx> {
    let commits = runtime.volume_log(vid)?;
    if commits.len() < 2 {
        return Ok(vec![]);
    }

    // Parse schema once to find the table's pages.
    // We do one checkout to read the schema, then reuse for page-level checks.
    let table_pages = get_table_page_set(runtime, vid, table)?;

    let volume = runtime
        .volume_get(vid)
        .map_err(|e| ErrCtx::PragmaErr(format!("Volume error: {e:?}").into()))?;
    let log_id = volume.local;

    // Commits come newest-first; iterate adjacent pairs ascending.
    let mut results: Vec<(usize, &graft::CommitInfo)> = Vec::new();
    for i in (1..commits.len()).rev() {
        let from = &commits[i];
        let to = &commits[i - 1];

        // Fast page-level check: did *any* of the table's pages change?
        let diff = runtime
            .diff_commits(&log_id, from.lsn, to.lsn)
            .map_err(|e| ErrCtx::PragmaErr(format!("Diff error: {e:?}").into()))?;

        let changed = table_pages.iter().any(|&page_num| {
            graft::core::PageIdx::try_new(page_num)
                .is_some_and(|pi| diff.added_or_modified_pages.contains(pi))
        });
        if changed {
            results.push((i, to));
        }
    }

    // If nothing changed, return early — no expensive diff needed.
    if results.is_empty() {
        return Ok(vec![]);
    }

    // Now do row-level diffs ONLY for the detected commit pairs.
    let mut entries = Vec::new();
    for (i, to) in results {
        let from = &commits[i];
        let diff = crate::row_level_diff::row_level_diff(runtime, vid, from.lsn, to.lsn)
            .map_err(|e| ErrCtx::PragmaErr(format!("Diff error: {e:?}").into()))?;

        if let Some(tc) = diff.table_changes.iter().find(|t| t.table_name == table)
            && !tc.is_empty()
        {
            let (inserts, deletes, updates) = count_changes_json(&tc.changes);
            let mut parts = Vec::new();
            if inserts > 0 {
                parts.push(format!("+{inserts}"));
            }
            if deletes > 0 {
                parts.push(format!("-{deletes}"));
            }
            if updates > 0 {
                parts.push(format!("~{updates}"));
            }
            let detail = format!("{inserts} inserts, {deletes} deletes, {updates} updates");
            entries.push(TableLogEntry {
                lsn: to.lsn.to_u64(),
                timestamp_ms: to.timestamp,
                when: to
                    .timestamp
                    .map_or_else(|| "-".to_string(), format_unix_millis),
                summary: parts.join(" "),
                detail,
            });
        }
    }

    Ok(entries)
}

/// Get the set of page indices that belong to a table's B-tree.
pub(super) fn get_table_page_set(
    runtime: &Runtime,
    vid: &VolumeId,
    table: &str,
) -> Result<Vec<u32>, ErrCtx> {
    // Get latest LSN from commit history
    let commits = runtime
        .volume_log(vid)
        .map_err(|e| ErrCtx::PragmaErr(format!("Volume error: {e:?}").into()))?;
    let latest_lsn = commits
        .first()
        .map(|c| c.lsn)
        .ok_or_else(|| ErrCtx::PragmaErr("No commits".into()))?;

    let co = runtime
        .volume_checkout(vid, latest_lsn)
        .map_err(|e| ErrCtx::PragmaErr(format!("Checkout error: {e:?}").into()))?;
    let co_vid = co.vid;
    let reader = runtime
        .volume_reader(co_vid.clone())
        .map_err(|e| ErrCtx::PragmaErr(format!("Reader error: {e:?}").into()))?;

    let scanner = crate::sqlite_parse::TableScanner::new(&reader)
        .map_err(|e| ErrCtx::PragmaErr(format!("Parse error: {e:?}").into()))?;
    let master = scanner
        .read_master_table()
        .map_err(|e| ErrCtx::PragmaErr(format!("Schema error: {e:?}").into()))?;

    let root_page = master
        .iter()
        .find(|e| e.name == table)
        .map_or(0, |e| e.root_page);

    let mut pages = Vec::new();
    if root_page > 0 {
        collect_btree_pages(&reader, root_page, &mut pages);
    }

    let _ = runtime.volume_delete(&co_vid);
    Ok(pages)
}

/// Recursively collect all page numbers in a table B-tree.
pub(super) fn collect_btree_pages(
    reader: &graft::volume_reader::VolumeReader,
    page_num: u32,
    pages: &mut Vec<u32>,
) {
    if page_num == 0 || pages.contains(&page_num) {
        return;
    }
    pages.push(page_num);

    let page_idx = match graft::core::PageIdx::try_new(page_num) {
        Some(p) => p,
        None => return,
    };
    let Ok(page) = reader.read_page(page_idx) else {
        return;
    };
    let data = page.as_ref();

    if data.len() < 12 {
        return;
    }
    let page_type = data[0];
    let num_cells = u16::from_be_bytes([data[3], data[4]]) as usize;

    if page_type == 5 {
        // Interior table page: recurse into children
        let right_child = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
        collect_btree_pages(reader, right_child, pages);
        for i in 0..num_cells {
            let ptr = 12 + i * 2;
            if ptr + 2 > data.len() {
                break;
            }
            let cell_off = u16::from_be_bytes([data[ptr], data[ptr + 1]]) as usize;
            if cell_off + 4 <= data.len() {
                let left = u32::from_be_bytes([
                    data[cell_off],
                    data[cell_off + 1],
                    data[cell_off + 2],
                    data[cell_off + 3],
                ]);
                collect_btree_pages(reader, left, pages);
            }
        }
    }
    // Leaf pages (13) have no children — nothing to recurse.
}
