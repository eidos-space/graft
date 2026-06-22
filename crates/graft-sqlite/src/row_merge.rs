use std::collections::{BTreeMap, BTreeSet};

use graft::{repo::CommitFileState, rt::runtime::Runtime};

use crate::row_level_diff::{
    InsertRowidMode, RowChange, RowLevelDiff, SchemaChange, SchemaChangeKind,
    row_level_diff_snapshots,
};
use crate::sqlite_parse::{Record, Value};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaMergeConflict {
    pub name: String,
    pub entry_type: String,
    pub ours: Option<SchemaChangeKind>,
    pub theirs: Option<SchemaChangeKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowChangeKind {
    Insert,
    Delete,
    Update,
}

#[derive(Debug)]
pub struct RowMergePlan {
    pub analysis: RowMergeAnalysis,
    theirs_diff: RowLevelDiff,
    identical_touches: BTreeSet<RowKey>,
    omitted_theirs_insert_rowids: BTreeSet<RowKey>,
    schema_additions: Vec<SchemaChange>,
    schema_conflicts: Vec<SchemaMergeConflict>,
    opaque_changes: usize,
}

impl RowMergePlan {
    pub fn has_conflicts(&self) -> bool {
        self.analysis.has_conflicts() || !self.schema_conflicts.is_empty()
    }

    pub fn has_opaque_changes(&self) -> bool {
        self.opaque_changes > 0
    }

    pub fn opaque_changes(&self) -> usize {
        self.opaque_changes
    }

    pub fn schema_conflicts(&self) -> &[SchemaMergeConflict] {
        &self.schema_conflicts
    }

    pub fn apply_change_count(&self) -> usize {
        let row_changes = self
            .theirs_diff
            .table_changes
            .iter()
            .flat_map(|table| {
                table
                    .changes
                    .iter()
                    .map(|change| RowKey::from_change(&table.table_name, change))
            })
            .filter(|row| !self.identical_touches.contains(row))
            .count();
        row_changes + self.schema_additions.len()
    }

    pub fn theirs_apply_sql(&self) -> String {
        let mut sql = String::from("BEGIN TRANSACTION;\n\n");

        for change in &self.schema_additions {
            sql.push_str(&change.sql);
            if !change.sql.trim_end().ends_with(';') {
                sql.push(';');
            }
            sql.push('\n');
        }

        if !self.schema_additions.is_empty() {
            sql.push('\n');
        }

        for table in &self.theirs_diff.table_changes {
            let table_sql = table.to_sql_filtered_with_insert_rowid(
                |change| {
                    !self
                        .identical_touches
                        .contains(&RowKey::from_change(&table.table_name, change))
                },
                |change| {
                    let row = RowKey::from_change(&table.table_name, change);
                    if self.omitted_theirs_insert_rowids.contains(&row) {
                        InsertRowidMode::Omit
                    } else {
                        InsertRowidMode::Preserve
                    }
                },
            );
            if table_sql.is_empty() {
                continue;
            }
            sql.push_str(&format!("-- Table: {}\n", table.table_name));
            sql.push_str(&table_sql);
            sql.push('\n');
        }

        sql.push_str("COMMIT;\n");
        sql
    }
}

pub fn analyze_snapshot_merge(
    runtime: &Runtime,
    base: &CommitFileState,
    ours: &CommitFileState,
    theirs: &CommitFileState,
) -> Result<RowMergeAnalysis, graft::err::GraftErr> {
    Ok(plan_snapshot_merge(runtime, base, ours, theirs)?.analysis)
}

pub fn plan_snapshot_merge(
    runtime: &Runtime,
    base: &CommitFileState,
    ours: &CommitFileState,
    theirs: &CommitFileState,
) -> Result<RowMergePlan, graft::err::GraftErr> {
    let base_snapshot = base.snapshot.to_snapshot();
    let ours_snapshot = ours.snapshot.to_snapshot();
    let theirs_snapshot = theirs.snapshot.to_snapshot();
    let ours_diff = row_level_diff_snapshots(runtime, &base_snapshot, &ours_snapshot)?;
    let theirs_diff = row_level_diff_snapshots(runtime, &base_snapshot, &theirs_snapshot)?;
    let ours_touches = row_touches(&ours_diff.table_changes);
    let theirs_touches = row_touches(&theirs_diff.table_changes);
    let mut conflicts = Vec::new();
    let mut identical_touches = BTreeSet::new();
    let mut omitted_theirs_insert_rowids = BTreeSet::new();

    for (row, ours_change) in &ours_touches {
        let Some(theirs_change) = theirs_touches.get(row) else {
            continue;
        };
        if ours_change.change == theirs_change.change {
            identical_touches.insert(row.clone());
            continue;
        }
        if should_remap_theirs_insert_rowid(ours_change, theirs_change) {
            omitted_theirs_insert_rowids.insert(row.clone());
            continue;
        }
        conflicts.push(RowMergeConflict {
            table: row.table.clone(),
            rowid: row.rowid,
            ours: ours_change.kind,
            theirs: theirs_change.kind,
        });
    }

    let analysis = RowMergeAnalysis {
        ours_changes: ours_touches.len(),
        theirs_changes: theirs_touches.len(),
        conflicts,
    };
    let opaque_changes = ours_diff.opaque_changes.len() + theirs_diff.opaque_changes.len();
    let (schema_additions, schema_conflicts) =
        plan_schema_additions(&ours_diff.schema_changes, &theirs_diff.schema_changes);

    Ok(RowMergePlan {
        analysis,
        theirs_diff,
        identical_touches,
        omitted_theirs_insert_rowids,
        schema_additions,
        schema_conflicts,
        opaque_changes,
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
                    semantic_key: semantic_insert_key(
                        &table.columns,
                        &table.semantic_key_columns,
                        change,
                    ),
                    can_omit_insert_rowid: table.rowid_alias.is_none(),
                },
            );
        }
    }
    touches
}

fn should_remap_theirs_insert_rowid(ours: &RowTouch, theirs: &RowTouch) -> bool {
    matches!(ours.kind, RowChangeKind::Insert)
        && matches!(theirs.kind, RowChangeKind::Insert)
        && ours.can_omit_insert_rowid
        && theirs.can_omit_insert_rowid
        && ours.semantic_key.is_some()
        && theirs.semantic_key.is_some()
        && ours.semantic_key != theirs.semantic_key
}

fn plan_schema_additions(
    ours_changes: &[SchemaChange],
    theirs_changes: &[SchemaChange],
) -> (Vec<SchemaChange>, Vec<SchemaMergeConflict>) {
    let ours_by_name: BTreeMap<&str, &SchemaChange> = ours_changes
        .iter()
        .map(|change| (change.name.as_str(), change))
        .collect();
    let mut additions = Vec::new();
    let mut conflicts = Vec::new();

    for change in theirs_changes {
        match change.kind {
            SchemaChangeKind::Added => {
                if let Some(ours) = ours_by_name.get(change.name.as_str()) {
                    if ours.kind == SchemaChangeKind::Added
                        && ours.entry_type == change.entry_type
                        && ours.sql == change.sql
                    {
                        continue;
                    }
                    conflicts.push(SchemaMergeConflict {
                        name: change.name.clone(),
                        entry_type: change.entry_type.clone(),
                        ours: Some(ours.kind),
                        theirs: Some(change.kind),
                    });
                    continue;
                }
                additions.push(change.clone());
            }
            SchemaChangeKind::Deleted | SchemaChangeKind::Modified => {
                conflicts.push(SchemaMergeConflict {
                    name: change.name.clone(),
                    entry_type: change.entry_type.clone(),
                    ours: None,
                    theirs: Some(change.kind),
                });
            }
        }
    }

    for change in ours_changes.iter().filter(|change| {
        matches!(
            change.kind,
            SchemaChangeKind::Deleted | SchemaChangeKind::Modified
        )
    }) {
        conflicts.push(SchemaMergeConflict {
            name: change.name.clone(),
            entry_type: change.entry_type.clone(),
            ours: Some(change.kind),
            theirs: None,
        });
    }

    (additions, conflicts)
}

fn semantic_insert_key(
    columns: &[String],
    key_columns: &[String],
    change: &RowChange,
) -> Option<Vec<String>> {
    let RowChange::Insert { row, .. } = change else {
        return None;
    };
    if key_columns.is_empty() {
        return None;
    }
    let mut key = Vec::with_capacity(key_columns.len());
    for column in key_columns {
        let index = columns.iter().position(|candidate| candidate == column)?;
        key.push(semantic_value_key(row, index)?);
    }
    Some(key)
}

fn semantic_value_key(row: &Record, index: usize) -> Option<String> {
    let value = row.values.get(index)?;
    match value {
        Value::Null => None,
        Value::Integer(value) => Some(format!("i:{value}")),
        Value::Real(value) => Some(format!("r:{value:.15}")),
        Value::Text(value) => Some(format!("t:{value}")),
        Value::Blob(value) => {
            let hex: String = value.iter().map(|byte| format!("{byte:02x}")).collect();
            Some(format!("b:{hex}"))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RowKey {
    table: String,
    rowid: i64,
}

impl RowKey {
    fn from_change(table: &str, change: &RowChange) -> Self {
        Self {
            table: table.to_string(),
            rowid: change.rowid(),
        }
    }
}

#[derive(Debug, Clone)]
struct RowTouch {
    kind: RowChangeKind,
    change: RowChange,
    semantic_key: Option<Vec<String>>,
    can_omit_insert_rowid: bool,
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
