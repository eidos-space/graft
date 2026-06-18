use std::collections::BTreeMap;

use graft::{repo::CommitFileState, rt::runtime::Runtime};

use crate::row_level_diff::{RowChange, row_level_diff_snapshots};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowMergeAnalysis {
    pub ours_changes: usize,
    pub theirs_changes: usize,
    pub conflicts: Vec<RowMergeConflict>,
}

impl RowMergeAnalysis {
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowMergeConflict {
    pub table: String,
    pub rowid: i64,
    pub ours: RowChangeKind,
    pub theirs: RowChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowChangeKind {
    Insert,
    Delete,
    Update,
}

pub fn analyze_snapshot_merge(
    runtime: &Runtime,
    base: &CommitFileState,
    ours: &CommitFileState,
    theirs: &CommitFileState,
) -> Result<RowMergeAnalysis, graft::err::GraftErr> {
    let base_snapshot = base.snapshot.to_snapshot();
    let ours_snapshot = ours.snapshot.to_snapshot();
    let theirs_snapshot = theirs.snapshot.to_snapshot();
    let ours_diff = row_level_diff_snapshots(runtime, &base_snapshot, &ours_snapshot)?;
    let theirs_diff = row_level_diff_snapshots(runtime, &base_snapshot, &theirs_snapshot)?;
    let ours_touches = row_touches(&ours_diff.table_changes);
    let theirs_touches = row_touches(&theirs_diff.table_changes);
    let mut conflicts = Vec::new();

    for (row, ours_change) in &ours_touches {
        let Some(theirs_change) = theirs_touches.get(row) else {
            continue;
        };
        if ours_change.change == theirs_change.change {
            continue;
        }
        conflicts.push(RowMergeConflict {
            table: row.table.clone(),
            rowid: row.rowid,
            ours: ours_change.kind,
            theirs: theirs_change.kind,
        });
    }

    Ok(RowMergeAnalysis {
        ours_changes: ours_touches.len(),
        theirs_changes: theirs_touches.len(),
        conflicts,
    })
}

fn row_touches(changes: &[crate::row_level_diff::TableChanges]) -> BTreeMap<RowKey, RowTouch> {
    let mut touches = BTreeMap::new();
    for table in changes {
        for change in &table.changes {
            let rowid = change.rowid();
            touches.insert(
                RowKey { table: table.table_name.clone(), rowid },
                RowTouch {
                    kind: change.kind(),
                    change: change.clone(),
                },
            );
        }
    }
    touches
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RowKey {
    table: String,
    rowid: i64,
}

#[derive(Debug, Clone)]
struct RowTouch {
    kind: RowChangeKind,
    change: RowChange,
}

trait RowChangeExt {
    fn rowid(&self) -> i64;
    fn kind(&self) -> RowChangeKind;
}

impl RowChangeExt for RowChange {
    fn rowid(&self) -> i64 {
        match self {
            RowChange::Insert { rowid, .. }
            | RowChange::Delete { rowid, .. }
            | RowChange::Update { rowid, .. } => *rowid,
        }
    }

    fn kind(&self) -> RowChangeKind {
        match self {
            RowChange::Insert { .. } => RowChangeKind::Insert,
            RowChange::Delete { .. } => RowChangeKind::Delete,
            RowChange::Update { .. } => RowChangeKind::Update,
        }
    }
}
