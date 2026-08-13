use std::collections::{BTreeMap, BTreeSet};

use graft::{repo::CommitFileState, rt::runtime::Runtime};

use crate::row_level_diff::{
    InsertRowidMode, OpaqueChange, OpaqueChangeReason, RowChange, RowIdentity, RowLevelDiff,
    RowLevelDiffLimitation, SchemaChange, SchemaChangeKind, TableChanges, row_level_diff_snapshots,
};
use crate::sqlite_parse::{
    ColumnDefinition, GeneratedColumnKind, Record, Value, parse_create_table_column_definitions,
    parse_create_table_items,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowMergeSide {
    Ours,
    Theirs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowMergePolicy {
    pub same_row_merge: bool,
    pub internal_resolvers: BTreeMap<String, RowMergeInternalResolver>,
    pub schema_resolvers: BTreeMap<String, RowMergeSchemaResolver>,
    pub default_semantic_keys: Vec<String>,
    pub semantic_keys: BTreeMap<String, Vec<String>>,
    pub semantic_key_collations:
        BTreeMap<String, BTreeMap<String, graft::repo::SemanticKeyCollation>>,
    pub generated_columns: BTreeMap<String, Vec<String>>,
    pub column_resolvers: BTreeMap<String, BTreeMap<String, graft::repo::ManagedColumnResolver>>,
}

impl Default for RowMergePolicy {
    fn default() -> Self {
        let mut internal_resolvers = BTreeMap::new();
        let mut schema_resolvers = BTreeMap::new();
        internal_resolvers.insert(
            "sqlite_sequence".to_string(),
            RowMergeInternalResolver::SequenceMax,
        );
        for table in [
            "sqlite_stat1",
            "sqlite_stat2",
            "sqlite_stat3",
            "sqlite_stat4",
        ] {
            internal_resolvers.insert(table.to_string(), RowMergeInternalResolver::Rebuild);
        }
        internal_resolvers.insert("index_btree".to_string(), RowMergeInternalResolver::Reindex);
        schema_resolvers.insert(
            "add_column".to_string(),
            RowMergeSchemaResolver::AlterTableAddColumn,
        );
        Self {
            same_row_merge: false,
            internal_resolvers,
            schema_resolvers,
            default_semantic_keys: Vec::new(),
            semantic_keys: BTreeMap::new(),
            semantic_key_collations: BTreeMap::new(),
            generated_columns: BTreeMap::new(),
            column_resolvers: BTreeMap::new(),
        }
    }
}

impl RowMergePolicy {
    fn resolver_for_opaque_change(
        &self,
        change: &OpaqueChange,
    ) -> Option<RowMergeInternalResolver> {
        if change.reason == OpaqueChangeReason::IndexBtree {
            return self.internal_resolvers.get("index_btree").copied();
        }
        if change.reason != OpaqueChangeReason::SqliteInternalTable {
            return None;
        }
        self.internal_resolvers.get(&change.name).copied()
    }

    fn resolver_for_schema_operation(&self, operation: &str) -> Option<RowMergeSchemaResolver> {
        self.schema_resolvers.get(operation).copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowMergeInternalResolver {
    SequenceMax,
    Rebuild,
    Reindex,
}

impl RowMergeInternalResolver {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SequenceMax => "sequence_max",
            Self::Rebuild => "rebuild",
            Self::Reindex => "reindex",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "sequence_max" => Some(Self::SequenceMax),
            "rebuild" => Some(Self::Rebuild),
            "reindex" => Some(Self::Reindex),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowMergeSchemaResolver {
    AlterTableAddColumn,
}

impl RowMergeSchemaResolver {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AlterTableAddColumn => "alter_table_add_column",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "alter_table_add_column" => Some(Self::AlterTableAddColumn),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowMergeResolvedOpaqueChange {
    pub name: String,
    pub reason: OpaqueChangeReason,
    pub resolver: RowMergeInternalResolver,
}

#[derive(Debug, Clone, PartialEq)]
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

#[derive(Debug, Clone, PartialEq)]
pub struct RowMergeConflict {
    pub reason: RowMergeConflictReason,
    pub table: String,
    pub columns: Vec<String>,
    pub identity: RowIdentity,
    pub ours_identity: RowIdentity,
    pub theirs_identity: RowIdentity,
    pub semantic_key: Option<Vec<String>>,
    pub semantic_key_collations: Option<Vec<graft::repo::SemanticKeyCollation>>,
    pub cell_conflicts: Vec<RowMergeCellConflict>,
    pub ours: RowChangeKind,
    pub theirs: RowChangeKind,
    pub base_row: Option<Record>,
    pub ours_row: Option<Record>,
    pub theirs_row: Option<Record>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RowMergeCellConflict {
    pub column: String,
    pub base: Value,
    pub ours: Value,
    pub theirs: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowMergePendingRecomputation {
    pub table: String,
    pub identity: RowIdentity,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowMergeConflictReason {
    RowIdentity,
    SemanticKey,
    Cell,
}

impl RowMergeConflictReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RowIdentity => "row_conflict",
            Self::SemanticKey => "semantic_key_conflict",
            Self::Cell => "cell_conflict",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaMergeConflict {
    pub reason: SchemaMergeConflictReason,
    pub name: String,
    pub entry_type: String,
    pub ours: Option<SchemaChangeKind>,
    pub theirs: Option<SchemaChangeKind>,
    pub column_changes: Vec<SchemaMergeColumnChange>,
    pub message: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaMergeColumnChange {
    pub side: SchemaMergeConflictSide,
    pub operation: SchemaMergeColumnOperation,
    pub from: Option<String>,
    pub to: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaMergeConflictSide {
    Ours,
    Theirs,
}

impl SchemaMergeConflictSide {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ours => "ours",
            Self::Theirs => "theirs",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaMergeColumnOperation {
    AddColumn,
    DropColumn,
    RenameColumn,
    ModifyColumn,
}

impl SchemaMergeColumnOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AddColumn => "add_column",
            Self::DropColumn => "drop_column",
            Self::RenameColumn => "rename_column",
            Self::ModifyColumn => "modify_column",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaMergeConflictReason {
    SchemaDelete,
    SchemaModify,
    SameNameDifferentDefinition,
    UnsupportedSchemaChange,
}

impl SchemaMergeConflictReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SchemaDelete => "schema_delete_conflict",
            Self::SchemaModify => "schema_modify_conflict",
            Self::SameNameDifferentDefinition => "schema_same_name_conflict",
            Self::UnsupportedSchemaChange => "schema_conflict",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::SchemaDelete => "schema entry was deleted or removed on one side",
            Self::SchemaModify => {
                "schema entry was modified and does not match a compatible schema resolver"
            }
            Self::SameNameDifferentDefinition => {
                "same schema entry has different definitions on both sides"
            }
            Self::UnsupportedSchemaChange => {
                "schema change does not match an automatic merge resolver"
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SchemaApplyChange {
    sql: String,
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
    ours_diff: RowLevelDiff,
    theirs_diff: RowLevelDiff,
    identical_touches: BTreeSet<RowKey>,
    omitted_theirs_insert_rowids: BTreeSet<RowKey>,
    merged_rows: BTreeMap<RowKey, Record>,
    cell_merge_rows: BTreeMap<RowKey, Record>,
    pending_recomputations: Vec<RowMergePendingRecomputation>,
    ours_schema_additions: Vec<SchemaApplyChange>,
    schema_additions: Vec<SchemaApplyChange>,
    schema_conflicts: Vec<SchemaMergeConflict>,
    unresolved_opaque_changes: Vec<OpaqueChange>,
    resolved_opaque_changes: Vec<RowMergeResolvedOpaqueChange>,
    policy: RowMergePolicy,
}

impl RowMergePlan {
    pub fn has_conflicts(&self) -> bool {
        self.analysis.has_conflicts() || !self.schema_conflicts.is_empty()
    }

    pub fn has_opaque_changes(&self) -> bool {
        !self.unresolved_opaque_changes.is_empty()
    }

    pub fn opaque_changes(&self) -> usize {
        self.unresolved_opaque_changes.len()
    }

    pub fn unresolved_opaque_changes(&self) -> &[OpaqueChange] {
        &self.unresolved_opaque_changes
    }

    pub fn resolved_opaque_changes(&self) -> &[RowMergeResolvedOpaqueChange] {
        &self.resolved_opaque_changes
    }

    pub fn limitations(&self) -> Vec<RowLevelDiffLimitation> {
        let mut seen = BTreeSet::new();
        let mut limitations = Vec::new();
        for limitation in self
            .ours_diff
            .analysis
            .limitations
            .iter()
            .chain(self.theirs_diff.analysis.limitations.iter())
        {
            if seen.insert((limitation.kind.as_str(), limitation.subject.clone())) {
                limitations.push(limitation.clone());
            }
        }
        limitations
    }

    pub fn policy(&self) -> &RowMergePolicy {
        &self.policy
    }

    pub fn schema_conflicts(&self) -> &[SchemaMergeConflict] {
        &self.schema_conflicts
    }

    pub fn pending_recomputations(&self) -> &[RowMergePendingRecomputation] {
        &self.pending_recomputations
    }

    pub fn requires_validation(&self) -> bool {
        !self.pending_recomputations.is_empty()
    }

    pub fn apply_change_count(&self) -> usize {
        self.source_apply_change_count(&self.theirs_diff, &self.schema_additions)
    }

    /// Whether the merge result already represented by `ours` is complete without applying SQL.
    ///
    /// This covers physical-only snapshot changes and identical logical changes made on both
    /// sides. Unsupported surfaces and incomplete analysis remain conservative conflicts.
    pub fn can_resolve_to_ours_without_apply(&self) -> bool {
        self.apply_change_count() == 0
            && !self.has_conflicts()
            && !self.has_opaque_changes()
            && self.limitations().is_empty()
            && !self.requires_validation()
    }

    pub fn theirs_logical_status(&self) -> crate::row_level_diff::LogicalDiffStatus {
        self.theirs_diff.logical_status()
    }

    pub fn theirs_apply_sql(&self) -> String {
        self.source_apply_sql(&self.theirs_diff, &self.schema_additions)
    }

    /// SQL for the safe Theirs projection while leaving provider-owned tables at Ours.
    pub fn theirs_apply_sql_excluding(&self, excluded_tables: &BTreeSet<String>) -> String {
        self.source_apply_sql_excluding(&self.theirs_diff, &self.schema_additions, excluded_tables)
    }

    pub fn conflict_count_outside(&self, excluded_tables: &BTreeSet<String>) -> usize {
        self.analysis
            .conflicts
            .iter()
            .filter(|conflict| !excluded_tables.contains(&conflict.table))
            .count()
    }

    pub fn conflict_count_inside(&self, included_tables: &BTreeSet<String>) -> usize {
        self.analysis
            .conflicts
            .iter()
            .filter(|conflict| included_tables.contains(&conflict.table))
            .count()
    }

    pub fn has_schema_additions(&self) -> bool {
        !self.schema_additions.is_empty()
    }

    pub fn ours_apply_sql(&self) -> String {
        self.source_apply_sql(&self.ours_diff, &self.ours_schema_additions)
    }

    pub fn conflict_apply_sql(
        &self,
        side: RowMergeSide,
        table_name: &str,
        identity: &RowIdentity,
    ) -> Option<String> {
        let diff = match side {
            RowMergeSide::Ours => &self.ours_diff,
            RowMergeSide::Theirs => &self.theirs_diff,
        };
        self.source_row_apply_sql(diff, table_name, identity)
    }

    pub fn cell_resolution_apply_sql(
        &self,
        table_name: &str,
        identity: &RowIdentity,
        selections: &BTreeMap<String, RowMergeSide>,
    ) -> Option<String> {
        let key = RowKey {
            table: table_name.to_string(),
            identity: identity.clone(),
        };
        let conflict = self.analysis.conflicts.iter().find(|conflict| {
            conflict.reason == RowMergeConflictReason::Cell
                && conflict.table == table_name
                && conflict.identity == *identity
        })?;
        if conflict
            .cell_conflicts
            .iter()
            .any(|cell| !selections.contains_key(&cell.column))
        {
            return None;
        }
        let mut row = self.cell_merge_rows.get(&key)?.clone();
        let table = self
            .ours_diff
            .table_changes
            .iter()
            .find(|table| table.table_name == table_name)?;
        for cell in &conflict.cell_conflicts {
            let side = selections.get(&cell.column)?;
            let index = table
                .columns
                .iter()
                .position(|column| column.eq_ignore_ascii_case(&cell.column))?;
            row.values[index] = match side {
                RowMergeSide::Ours => cell.ours.clone(),
                RowMergeSide::Theirs => cell.theirs.clone(),
            };
        }
        let sql = table.format_record_update(&self.apply_generated_columns(table), identity, &row);
        if sql.is_empty() {
            return None;
        }
        Some(format!(
            "BEGIN TRANSACTION;\n\n-- Table: {}\n{}\nCOMMIT;\n",
            table.table_name, sql
        ))
    }

    fn source_apply_change_count(
        &self,
        diff: &RowLevelDiff,
        schema_additions: &[SchemaApplyChange],
    ) -> usize {
        let conflict_rows = self.conflict_rows();
        let row_changes = diff
            .table_changes
            .iter()
            .flat_map(|table| {
                table
                    .changes
                    .iter()
                    .map(|change| RowKey::from_change(&table.table_name, change))
            })
            .filter(|row| {
                !self.identical_touches.contains(row)
                    && !conflict_rows.contains(row)
                    && !self.merged_rows.contains_key(row)
            })
            .count();
        // A combined row must always be written. Even when it equals the selected source
        // side's result, that source row was filtered from the normal apply stream and the
        // candidate starts from the opposite side.
        let merged_rows = self.merged_rows.len();
        row_changes
            + merged_rows
            + schema_additions.len()
            + usize::from(self.rebuilds_sqlite_stats())
            + self.reindexed_sqlite_indexes().count()
    }

    fn source_apply_sql(
        &self,
        diff: &RowLevelDiff,
        schema_additions: &[SchemaApplyChange],
    ) -> String {
        self.source_apply_sql_excluding(diff, schema_additions, &BTreeSet::new())
    }

    fn source_apply_sql_excluding(
        &self,
        diff: &RowLevelDiff,
        schema_additions: &[SchemaApplyChange],
        excluded_tables: &BTreeSet<String>,
    ) -> String {
        let mut sql = String::from("BEGIN TRANSACTION;\n\n");
        let mut has_changes = false;

        for change in schema_additions {
            has_changes = true;
            sql.push_str(&change.sql);
            if !change.sql.trim_end().ends_with(';') {
                sql.push(';');
            }
            sql.push('\n');
        }

        if !schema_additions.is_empty() {
            sql.push('\n');
        }

        let conflict_rows = self.conflict_rows();
        for table in &diff.table_changes {
            if excluded_tables.contains(&table.table_name) {
                continue;
            }
            let generated_columns = self.apply_generated_columns(table);
            let mut table_sql = table.to_sql_filtered_with_insert_rowid_and_generated(
                &generated_columns,
                |change| {
                    let row = RowKey::from_change(&table.table_name, change);
                    !self.identical_touches.contains(&row)
                        && !conflict_rows.contains(&row)
                        && !self.merged_rows.contains_key(&row)
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
            for (row, merged) in self
                .merged_rows
                .iter()
                .filter(|(row, _)| row.table == table.table_name)
            {
                let update = table.format_record_update(&generated_columns, &row.identity, merged);
                if !update.is_empty() {
                    table_sql.push_str(&update);
                    table_sql.push('\n');
                }
            }
            if table_sql.is_empty() {
                continue;
            }
            has_changes = true;
            sql.push_str(&format!("-- Table: {}\n", table.table_name));
            sql.push_str(&table_sql);
            sql.push('\n');
        }

        if self.rebuilds_sqlite_stats() {
            has_changes = true;
            sql.push_str("-- Internal resolver: rebuild SQLite statistics\n");
            sql.push_str("ANALYZE;\n\n");
        }

        let reindexed: Vec<_> = self.reindexed_sqlite_indexes().collect();
        if !reindexed.is_empty() {
            has_changes = true;
            sql.push_str("-- Internal resolver: rebuild SQLite indexes\n");
            for index_name in reindexed {
                sql.push_str(&format!("REINDEX {};\n", quote_identifier(index_name)));
            }
            sql.push('\n');
        }

        if !has_changes {
            return String::new();
        }
        sql.push_str("COMMIT;\n");
        sql
    }

    fn source_row_apply_sql(
        &self,
        diff: &RowLevelDiff,
        table_name: &str,
        identity: &RowIdentity,
    ) -> Option<String> {
        let table = diff
            .table_changes
            .iter()
            .find(|table| table.table_name == table_name)?;
        let row = RowKey {
            table: table_name.to_string(),
            identity: identity.clone(),
        };
        if !self.conflict_rows().contains(&row) {
            return None;
        }
        let generated_columns = self.apply_generated_columns(table);
        let table_sql = table.to_sql_filtered_with_insert_rowid_and_generated(
            &generated_columns,
            |change| RowKey::from_change(&table.table_name, change) == row,
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
            return None;
        }

        let mut sql = String::from("BEGIN TRANSACTION;\n\n");
        sql.push_str(&format!("-- Table: {}\n", table.table_name));
        sql.push_str(&table_sql);
        sql.push('\n');
        sql.push_str("COMMIT;\n");
        Some(sql)
    }

    fn apply_generated_columns(
        &self,
        table: &TableChanges,
    ) -> BTreeMap<String, GeneratedColumnKind> {
        let mut generated_columns = table.generated_columns.clone();
        if let Some(policy_columns) = self.policy.generated_columns.get(&table.table_name) {
            for column in policy_columns {
                generated_columns
                    .entry(column.clone())
                    .or_insert(GeneratedColumnKind::Stored);
            }
        }
        if let Some(resolvers) = self.policy.column_resolvers.get(&table.table_name) {
            for (column, resolver) in resolvers {
                if *resolver == graft::repo::ManagedColumnResolver::Recompute {
                    generated_columns
                        .entry(column.clone())
                        .or_insert(GeneratedColumnKind::Stored);
                }
            }
        }
        generated_columns
    }

    fn conflict_rows(&self) -> BTreeSet<RowKey> {
        let mut rows = BTreeSet::new();
        for conflict in &self.analysis.conflicts {
            rows.insert(RowKey {
                table: conflict.table.clone(),
                identity: conflict.ours_identity.clone(),
            });
            rows.insert(RowKey {
                table: conflict.table.clone(),
                identity: conflict.theirs_identity.clone(),
            });
        }
        rows
    }

    fn rebuilds_sqlite_stats(&self) -> bool {
        self.resolved_opaque_changes.iter().any(|change| {
            change.resolver == RowMergeInternalResolver::Rebuild
                && change.name.starts_with("sqlite_stat")
        })
    }

    fn reindexed_sqlite_indexes(&self) -> impl Iterator<Item = &str> {
        let mut seen = BTreeSet::new();
        self.resolved_opaque_changes
            .iter()
            .filter(|change| change.resolver == RowMergeInternalResolver::Reindex)
            .filter_map(move |change| {
                if seen.insert(change.name.as_str()) {
                    Some(change.name.as_str())
                } else {
                    None
                }
            })
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
    plan_snapshot_merge_with_policy(runtime, base, ours, theirs, &RowMergePolicy::default())
}

pub fn plan_snapshot_merge_with_policy(
    runtime: &Runtime,
    base: &CommitFileState,
    ours: &CommitFileState,
    theirs: &CommitFileState,
    policy: &RowMergePolicy,
) -> Result<RowMergePlan, graft::err::GraftErr> {
    let base_snapshot = base.snapshot.to_snapshot();
    let ours_snapshot = ours.snapshot.to_snapshot();
    let theirs_snapshot = theirs.snapshot.to_snapshot();
    let ours_diff = row_level_diff_snapshots(runtime, &base_snapshot, &ours_snapshot)?;
    let theirs_diff = row_level_diff_snapshots(runtime, &base_snapshot, &theirs_snapshot)?;
    let ours_touches = row_touches(&ours_diff.table_changes, policy);
    let theirs_touches = row_touches(&theirs_diff.table_changes, policy);
    let mut conflicts = Vec::new();
    let mut identical_touches = BTreeSet::new();
    let mut omitted_theirs_insert_rowids = BTreeSet::new();
    let mut merged_rows = BTreeMap::new();
    let mut cell_merge_rows = BTreeMap::new();
    let mut pending_recomputations = Vec::new();

    for (row, ours_change) in &ours_touches {
        let Some(theirs_change) = theirs_touches.get(row) else {
            continue;
        };
        if policy.same_row_merge
            && let Some(merged) = merge_same_row_updates(
                policy,
                &row.table,
                &ours_change.columns,
                &ours_change.change,
                &theirs_change.change,
            )
        {
            if !merged.pending_recomputations.is_empty() {
                pending_recomputations.push(RowMergePendingRecomputation {
                    table: row.table.clone(),
                    identity: row.identity.clone(),
                    columns: merged.pending_recomputations.clone(),
                });
            }
            if merged.cell_conflicts.is_empty() {
                merged_rows.insert(row.clone(), merged.row);
                continue;
            }
            conflicts.push(RowMergeConflict {
                reason: RowMergeConflictReason::Cell,
                table: row.table.clone(),
                columns: merged
                    .cell_conflicts
                    .iter()
                    .map(|conflict| conflict.column.clone())
                    .collect(),
                identity: row.identity.clone(),
                ours_identity: row.identity.clone(),
                theirs_identity: row.identity.clone(),
                semantic_key: ours_change
                    .semantic_key_display
                    .clone()
                    .or_else(|| theirs_change.semantic_key_display.clone()),
                semantic_key_collations: Some(ours_change.semantic_key_collations.clone())
                    .filter(|collations| !collations.is_empty()),
                cell_conflicts: merged.cell_conflicts,
                ours: ours_change.kind,
                theirs: theirs_change.kind,
                base_row: change_base_row(&ours_change.change)
                    .or_else(|| change_base_row(&theirs_change.change)),
                ours_row: change_result_row(&ours_change.change),
                theirs_row: change_result_row(&theirs_change.change),
            });
            cell_merge_rows.insert(row.clone(), merged.row);
            continue;
        }
        if ours_change.change == theirs_change.change {
            identical_touches.insert(row.clone());
            continue;
        }
        if should_remap_theirs_insert_rowid(ours_change, theirs_change) {
            omitted_theirs_insert_rowids.insert(row.clone());
            continue;
        }
        conflicts.push(RowMergeConflict {
            reason: RowMergeConflictReason::RowIdentity,
            table: row.table.clone(),
            columns: if ours_change.columns.is_empty() {
                theirs_change.columns.clone()
            } else {
                ours_change.columns.clone()
            },
            identity: row.identity.clone(),
            ours_identity: row.identity.clone(),
            theirs_identity: row.identity.clone(),
            semantic_key: ours_change
                .semantic_key_display
                .clone()
                .or_else(|| theirs_change.semantic_key_display.clone()),
            semantic_key_collations: Some(ours_change.semantic_key_collations.clone())
                .filter(|collations| !collations.is_empty()),
            cell_conflicts: Vec::new(),
            ours: ours_change.kind,
            theirs: theirs_change.kind,
            base_row: change_base_row(&ours_change.change)
                .or_else(|| change_base_row(&theirs_change.change)),
            ours_row: change_result_row(&ours_change.change),
            theirs_row: change_result_row(&theirs_change.change),
        });
    }
    add_semantic_key_conflicts(&ours_touches, &theirs_touches, &mut conflicts);

    let analysis = RowMergeAnalysis {
        ours_changes: ours_touches.len(),
        theirs_changes: theirs_touches.len(),
        conflicts,
    };
    let (unresolved_opaque_changes, resolved_opaque_changes) =
        classify_opaque_changes(policy, &ours_diff, &theirs_diff);
    let (schema_additions, ours_schema_additions, schema_conflicts) = plan_schema_changes(
        policy,
        &ours_diff.schema_changes,
        &theirs_diff.schema_changes,
    );

    Ok(RowMergePlan {
        analysis,
        ours_diff,
        theirs_diff,
        identical_touches,
        omitted_theirs_insert_rowids,
        merged_rows,
        cell_merge_rows,
        pending_recomputations,
        ours_schema_additions,
        schema_additions,
        schema_conflicts,
        unresolved_opaque_changes,
        resolved_opaque_changes,
        policy: policy.clone(),
    })
}

fn classify_opaque_changes(
    policy: &RowMergePolicy,
    ours_diff: &RowLevelDiff,
    theirs_diff: &RowLevelDiff,
) -> (Vec<OpaqueChange>, Vec<RowMergeResolvedOpaqueChange>) {
    let mut unresolved = Vec::new();
    let mut resolved = Vec::new();

    for change in ours_diff
        .opaque_changes
        .iter()
        .chain(theirs_diff.opaque_changes.iter())
    {
        if let Some(resolver) = policy.resolver_for_opaque_change(change) {
            resolved.push(RowMergeResolvedOpaqueChange {
                name: change.name.clone(),
                reason: change.reason,
                resolver,
            });
        } else {
            unresolved.push(change.clone());
        }
    }

    (unresolved, resolved)
}

#[derive(Debug)]
struct SameRowMerge {
    row: Record,
    cell_conflicts: Vec<RowMergeCellConflict>,
    pending_recomputations: Vec<String>,
}

fn merge_same_row_updates(
    policy: &RowMergePolicy,
    table: &str,
    columns: &[String],
    ours: &RowChange,
    theirs: &RowChange,
) -> Option<SameRowMerge> {
    let (ours_base, ours_row) = match ours {
        RowChange::Update { old_row, new_row, .. }
        | RowChange::PrimaryKeyUpdate { old_row, new_row, .. } => (old_row, new_row),
        _ => return None,
    };
    let (theirs_base, theirs_row) = match theirs {
        RowChange::Update { old_row, new_row, .. }
        | RowChange::PrimaryKeyUpdate { old_row, new_row, .. } => (old_row, new_row),
        _ => return None,
    };
    if ours_base != theirs_base
        || columns.len() != ours_base.values.len()
        || columns.len() != ours_row.values.len()
        || columns.len() != theirs_row.values.len()
    {
        return None;
    }

    let mut values = Vec::with_capacity(columns.len());
    let mut cell_conflicts = Vec::new();
    let mut pending_recomputations = Vec::new();
    for (index, column) in columns.iter().enumerate() {
        let base = &ours_base.values[index];
        let ours = &ours_row.values[index];
        let theirs = &theirs_row.values[index];
        let ours_changed = ours != base;
        let theirs_changed = theirs != base;
        let resolver = managed_column_resolver(policy, table, column);

        if resolver == Some(graft::repo::ManagedColumnResolver::Recompute) {
            values.push(ours.clone());
            pending_recomputations.push(column.clone());
            continue;
        }
        if !ours_changed {
            values.push(theirs.clone());
            continue;
        }
        if !theirs_changed || ours == theirs {
            values.push(ours.clone());
            continue;
        }

        let resolved = match resolver {
            Some(graft::repo::ManagedColumnResolver::IgnoreForConflict) => Some(ours.clone()),
            Some(graft::repo::ManagedColumnResolver::Max) => {
                resolve_ordered_values(ours, theirs, true, false)
            }
            Some(graft::repo::ManagedColumnResolver::Min) => {
                resolve_ordered_values(ours, theirs, false, false)
            }
            Some(graft::repo::ManagedColumnResolver::MaxTimestamp) => {
                resolve_ordered_values(ours, theirs, true, true)
            }
            Some(graft::repo::ManagedColumnResolver::Recompute) => unreachable!(),
            None => None,
        };
        if let Some(value) = resolved {
            values.push(value);
        } else {
            values.push(ours.clone());
            cell_conflicts.push(RowMergeCellConflict {
                column: column.clone(),
                base: base.clone(),
                ours: ours.clone(),
                theirs: theirs.clone(),
            });
        }
    }

    Some(SameRowMerge {
        row: Record { values },
        cell_conflicts,
        pending_recomputations,
    })
}

fn managed_column_resolver(
    policy: &RowMergePolicy,
    table: &str,
    column: &str,
) -> Option<graft::repo::ManagedColumnResolver> {
    policy
        .column_resolvers
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(table))
        .and_then(|(_, resolvers)| {
            resolvers
                .iter()
                .find(|(candidate, _)| candidate.eq_ignore_ascii_case(column))
                .map(|(_, resolver)| *resolver)
        })
}

fn resolve_ordered_values(
    ours: &Value,
    theirs: &Value,
    use_max: bool,
    timestamp_only: bool,
) -> Option<Value> {
    use std::cmp::Ordering;

    let ordering = match (ours, theirs) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Integer(left), Value::Integer(right)) => left.cmp(right),
        (Value::Integer(left), Value::Real(right)) if !timestamp_only => {
            (*left as f64).partial_cmp(right)?
        }
        (Value::Real(left), Value::Integer(right)) if !timestamp_only => {
            left.partial_cmp(&(*right as f64))?
        }
        (Value::Real(left), Value::Real(right)) if !timestamp_only => left.partial_cmp(right)?,
        (Value::Text(left), Value::Text(right)) => left.cmp(right),
        (Value::Blob(left), Value::Blob(right)) if !timestamp_only => left.cmp(right),
        _ => return None,
    };
    let choose_ours = if use_max {
        ordering != Ordering::Less
    } else {
        ordering != Ordering::Greater
    };
    Some(if choose_ours {
        ours.clone()
    } else {
        theirs.clone()
    })
}

fn row_touches(
    changes: &[crate::row_level_diff::TableChanges],
    policy: &RowMergePolicy,
) -> BTreeMap<RowKey, RowTouch> {
    let mut touches = BTreeMap::new();
    for table in changes {
        let configured_semantic_key_columns = policy
            .semantic_keys
            .get(&table.table_name)
            .cloned()
            .or_else(|| default_semantic_key_columns(table, policy));
        let semantic_key_columns = configured_semantic_key_columns
            .as_deref()
            .unwrap_or(&table.semantic_key_columns);
        let semantic_key_collations = semantic_key_columns
            .iter()
            .map(|column| semantic_key_collation(policy, &table.table_name, column))
            .collect::<Vec<_>>();
        for change in &table.changes {
            let semantic_key = semantic_change_key(
                &table.columns,
                semantic_key_columns,
                &semantic_key_collations,
                change,
            );
            touches.insert(
                RowKey {
                    table: table.table_name.clone(),
                    identity: change.identity(),
                },
                RowTouch {
                    kind: change.kind(),
                    change: change.clone(),
                    columns: table.columns.clone(),
                    semantic_key: semantic_key.as_ref().map(|key| key.normalized.clone()),
                    semantic_key_display: semantic_key.map(|key| key.display),
                    semantic_key_collations: semantic_key_collations.clone(),
                    can_omit_insert_rowid: change.rowid().is_some() && table.rowid_alias.is_none(),
                },
            );
        }
    }
    touches
}

fn default_semantic_key_columns(
    table: &crate::row_level_diff::TableChanges,
    policy: &RowMergePolicy,
) -> Option<Vec<String>> {
    if policy.default_semantic_keys.is_empty() {
        return None;
    }
    let mut resolved = Vec::with_capacity(policy.default_semantic_keys.len());
    for key_column in &policy.default_semantic_keys {
        let column = table
            .columns
            .iter()
            .find(|column| column.eq_ignore_ascii_case(key_column))?;
        resolved.push(column.clone());
    }
    Some(resolved)
}

fn add_semantic_key_conflicts(
    ours_touches: &BTreeMap<RowKey, RowTouch>,
    theirs_touches: &BTreeMap<RowKey, RowTouch>,
    conflicts: &mut Vec<RowMergeConflict>,
) {
    let mut existing_conflict_rows: BTreeSet<RowKey> = conflicts
        .iter()
        .flat_map(|conflict| {
            [
                RowKey {
                    table: conflict.table.clone(),
                    identity: conflict.ours_identity.clone(),
                },
                RowKey {
                    table: conflict.table.clone(),
                    identity: conflict.theirs_identity.clone(),
                },
            ]
        })
        .collect();
    let theirs_by_semantic_key = semantic_result_touches(theirs_touches);

    for (ours_row, ours_touch) in ours_touches {
        if existing_conflict_rows.contains(ours_row) || ours_touch.kind == RowChangeKind::Delete {
            continue;
        }
        let Some(semantic_key) = ours_touch.semantic_key.as_ref() else {
            continue;
        };
        let semantic_row = SemanticRowKey {
            table: ours_row.table.clone(),
            key: semantic_key.clone(),
        };
        let Some((theirs_row, theirs_touch)) = theirs_by_semantic_key.get(&semantic_row) else {
            continue;
        };
        if *ours_row == *theirs_row || existing_conflict_rows.contains(theirs_row) {
            continue;
        }
        conflicts.push(RowMergeConflict {
            reason: RowMergeConflictReason::SemanticKey,
            table: ours_row.table.clone(),
            columns: if ours_touch.columns.is_empty() {
                theirs_touch.columns.clone()
            } else {
                ours_touch.columns.clone()
            },
            identity: ours_row.identity.clone(),
            ours_identity: ours_row.identity.clone(),
            theirs_identity: theirs_row.identity.clone(),
            semantic_key: ours_touch
                .semantic_key_display
                .clone()
                .or_else(|| theirs_touch.semantic_key_display.clone()),
            semantic_key_collations: Some(ours_touch.semantic_key_collations.clone()),
            cell_conflicts: Vec::new(),
            ours: ours_touch.kind,
            theirs: theirs_touch.kind,
            base_row: None,
            ours_row: change_result_row(&ours_touch.change),
            theirs_row: change_result_row(&theirs_touch.change),
        });
        existing_conflict_rows.insert(ours_row.clone());
        existing_conflict_rows.insert(theirs_row.clone());
    }
}

fn semantic_result_touches(
    touches: &BTreeMap<RowKey, RowTouch>,
) -> BTreeMap<SemanticRowKey, (RowKey, RowTouch)> {
    touches
        .iter()
        .filter_map(|(row, touch)| {
            if touch.kind == RowChangeKind::Delete {
                return None;
            }
            let semantic_key = touch.semantic_key.clone()?;
            Some((
                SemanticRowKey {
                    table: row.table.clone(),
                    key: semantic_key,
                },
                (row.clone(), touch.clone()),
            ))
        })
        .collect()
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

fn change_base_row(change: &RowChange) -> Option<Record> {
    match change {
        RowChange::Insert { .. } | RowChange::PrimaryKeyInsert { .. } => None,
        RowChange::Delete { row, .. } | RowChange::PrimaryKeyDelete { row, .. } => {
            Some(row.clone())
        }
        RowChange::Update { old_row, .. } | RowChange::PrimaryKeyUpdate { old_row, .. } => {
            Some(old_row.clone())
        }
    }
}

fn change_result_row(change: &RowChange) -> Option<Record> {
    match change {
        RowChange::Insert { row, .. } | RowChange::PrimaryKeyInsert { row, .. } => {
            Some(row.clone())
        }
        RowChange::Delete { .. } | RowChange::PrimaryKeyDelete { .. } => None,
        RowChange::Update { new_row, .. } | RowChange::PrimaryKeyUpdate { new_row, .. } => {
            Some(new_row.clone())
        }
    }
}

fn plan_schema_changes(
    policy: &RowMergePolicy,
    ours_changes: &[SchemaChange],
    theirs_changes: &[SchemaChange],
) -> (
    Vec<SchemaApplyChange>,
    Vec<SchemaApplyChange>,
    Vec<SchemaMergeConflict>,
) {
    let ours_by_name: BTreeMap<&str, &SchemaChange> = ours_changes
        .iter()
        .map(|change| (change.name.as_str(), change))
        .collect();
    let theirs_by_name: BTreeMap<&str, &SchemaChange> = theirs_changes
        .iter()
        .map(|change| (change.name.as_str(), change))
        .collect();
    let mut names = BTreeSet::new();
    names.extend(ours_by_name.keys().copied());
    names.extend(theirs_by_name.keys().copied());

    let mut theirs_apply = Vec::new();
    let mut ours_apply = Vec::new();
    let mut conflicts = Vec::new();

    for name in names {
        let ours = ours_by_name.get(name).copied();
        let theirs = theirs_by_name.get(name).copied();
        match (ours, theirs) {
            (None, Some(theirs)) => {
                if let Some(apply) = schema_apply_changes(policy, theirs, None) {
                    theirs_apply.extend(apply);
                } else {
                    conflicts.push(schema_merge_conflict(name, None, Some(theirs)));
                }
            }
            (Some(ours), None) => {
                if let Some(apply) = schema_apply_changes(policy, ours, None) {
                    ours_apply.extend(apply);
                } else if !is_compatible_local_schema_change(policy, ours) {
                    conflicts.push(schema_merge_conflict(name, Some(ours), None));
                }
            }
            (Some(ours), Some(theirs)) => {
                if ours.entry_type == theirs.entry_type
                    && ours.kind == theirs.kind
                    && ours.sql == theirs.sql
                {
                    continue;
                }
                if ours.kind == SchemaChangeKind::Modified
                    && theirs.kind == SchemaChangeKind::Modified
                    && ours.entry_type == "table"
                    && theirs.entry_type == "table"
                {
                    if let Some((theirs_delta, ours_delta)) =
                        compatible_bidirectional_add_columns(policy, ours, theirs)
                    {
                        theirs_apply.extend(theirs_delta);
                        ours_apply.extend(ours_delta);
                        continue;
                    }
                }
                conflicts.push(schema_merge_conflict(name, Some(ours), Some(theirs)));
            }
            (None, None) => {}
        }
    }

    (theirs_apply, ours_apply, conflicts)
}

fn schema_merge_conflict(
    name: &str,
    ours: Option<&SchemaChange>,
    theirs: Option<&SchemaChange>,
) -> SchemaMergeConflict {
    let entry_type = theirs
        .or(ours)
        .map(|change| change.entry_type.clone())
        .unwrap_or_default();
    let reason = schema_merge_conflict_reason(ours, theirs);
    SchemaMergeConflict {
        reason,
        name: name.to_string(),
        entry_type,
        ours: ours.map(|change| change.kind),
        theirs: theirs.map(|change| change.kind),
        column_changes: schema_merge_column_changes(ours, theirs),
        message: reason.message(),
    }
}

fn schema_merge_column_changes(
    ours: Option<&SchemaChange>,
    theirs: Option<&SchemaChange>,
) -> Vec<SchemaMergeColumnChange> {
    let mut changes = Vec::new();
    if let Some(ours) = ours {
        changes.extend(schema_change_column_changes(
            SchemaMergeConflictSide::Ours,
            ours,
        ));
    }
    if let Some(theirs) = theirs {
        changes.extend(schema_change_column_changes(
            SchemaMergeConflictSide::Theirs,
            theirs,
        ));
    }
    changes
}

fn schema_change_column_changes(
    side: SchemaMergeConflictSide,
    change: &SchemaChange,
) -> Vec<SchemaMergeColumnChange> {
    if change.kind != SchemaChangeKind::Modified || change.entry_type != "table" {
        return Vec::new();
    }
    let Some(old_sql) = change.old_sql.as_ref() else {
        return Vec::new();
    };
    let old_columns = parse_create_table_column_definitions(old_sql);
    let new_columns = parse_create_table_column_definitions(&change.sql);
    if old_columns.is_empty() || new_columns.is_empty() {
        return Vec::new();
    }

    let mut deleted: Vec<_> = old_columns
        .iter()
        .filter(|old| {
            !new_columns
                .iter()
                .any(|new| column_names_equal(&old.name, &new.name))
        })
        .collect();
    let mut added: Vec<_> = new_columns
        .iter()
        .filter(|new| {
            !old_columns
                .iter()
                .any(|old| column_names_equal(&old.name, &new.name))
        })
        .collect();
    let mut changes = Vec::new();

    if deleted.len() == 1
        && added.len() == 1
        && column_definitions_equal_except_name(deleted[0], added[0])
    {
        changes.push(SchemaMergeColumnChange {
            side,
            operation: SchemaMergeColumnOperation::RenameColumn,
            from: Some(deleted[0].name.clone()),
            to: Some(added[0].name.clone()),
        });
        deleted.clear();
        added.clear();
    }

    for column in deleted {
        changes.push(SchemaMergeColumnChange {
            side,
            operation: SchemaMergeColumnOperation::DropColumn,
            from: Some(column.name.clone()),
            to: None,
        });
    }
    for column in added {
        changes.push(SchemaMergeColumnChange {
            side,
            operation: SchemaMergeColumnOperation::AddColumn,
            from: None,
            to: Some(column.name.clone()),
        });
    }
    for old in &old_columns {
        let Some(new) = new_columns
            .iter()
            .find(|new| column_names_equal(&old.name, &new.name))
        else {
            continue;
        };
        if normalize_schema_item(&old.sql) != normalize_schema_item(&new.sql) {
            changes.push(SchemaMergeColumnChange {
                side,
                operation: SchemaMergeColumnOperation::ModifyColumn,
                from: Some(old.name.clone()),
                to: Some(new.name.clone()),
            });
        }
    }

    changes
}

fn schema_merge_conflict_reason(
    ours: Option<&SchemaChange>,
    theirs: Option<&SchemaChange>,
) -> SchemaMergeConflictReason {
    if [ours, theirs]
        .into_iter()
        .flatten()
        .any(|change| change.kind == SchemaChangeKind::Deleted)
    {
        return SchemaMergeConflictReason::SchemaDelete;
    }
    if [ours, theirs]
        .into_iter()
        .flatten()
        .any(|change| change.kind == SchemaChangeKind::Modified)
    {
        return SchemaMergeConflictReason::SchemaModify;
    }
    if ours.is_some() && theirs.is_some() {
        return SchemaMergeConflictReason::SameNameDifferentDefinition;
    }
    SchemaMergeConflictReason::UnsupportedSchemaChange
}

fn schema_apply_changes(
    policy: &RowMergePolicy,
    change: &SchemaChange,
    already_present: Option<&[ColumnDefinition]>,
) -> Option<Vec<SchemaApplyChange>> {
    match change.kind {
        SchemaChangeKind::Added => Some(vec![SchemaApplyChange { sql: change.sql.clone() }]),
        SchemaChangeKind::Modified => {
            compatible_add_column_changes(policy, change, already_present)
        }
        SchemaChangeKind::Deleted => None,
    }
}

fn is_compatible_local_schema_change(policy: &RowMergePolicy, change: &SchemaChange) -> bool {
    matches!(change.kind, SchemaChangeKind::Added)
        || compatible_add_column_changes(policy, change, None).is_some()
}

fn compatible_bidirectional_add_columns(
    policy: &RowMergePolicy,
    ours: &SchemaChange,
    theirs: &SchemaChange,
) -> Option<(Vec<SchemaApplyChange>, Vec<SchemaApplyChange>)> {
    let ours_columns = compatible_added_columns(ours)?;
    let theirs_columns = compatible_added_columns(theirs)?;
    if columns_overlap_with_different_defs(&ours_columns, &theirs_columns) {
        return None;
    }

    let theirs_apply = schema_apply_changes(policy, theirs, Some(&ours_columns))?;
    let ours_apply = schema_apply_changes(policy, ours, Some(&theirs_columns))?;
    Some((theirs_apply, ours_apply))
}

fn compatible_add_column_changes(
    policy: &RowMergePolicy,
    change: &SchemaChange,
    already_present: Option<&[ColumnDefinition]>,
) -> Option<Vec<SchemaApplyChange>> {
    if policy.resolver_for_schema_operation("add_column")
        != Some(RowMergeSchemaResolver::AlterTableAddColumn)
    {
        return None;
    }
    compatible_added_columns(change).map(|columns| {
        columns
            .into_iter()
            .filter(|column| {
                already_present.is_none_or(|present| {
                    !present
                        .iter()
                        .any(|candidate| column_names_equal(&candidate.name, &column.name))
                })
            })
            .map(|column| SchemaApplyChange {
                sql: format!(
                    "ALTER TABLE {} ADD COLUMN {};",
                    quote_identifier(&change.name),
                    column.sql
                ),
            })
            .collect()
    })
}

fn compatible_added_columns(change: &SchemaChange) -> Option<Vec<ColumnDefinition>> {
    if change.kind != SchemaChangeKind::Modified || change.entry_type != "table" {
        return None;
    }
    let old_sql = change.old_sql.as_ref()?;
    let old_items = parse_create_table_items(old_sql);
    let new_items = parse_create_table_items(&change.sql);
    if old_items.is_empty() || new_items.len() <= old_items.len() {
        return None;
    }
    if !new_items
        .iter()
        .zip(old_items.iter())
        .all(|(new, old)| normalize_schema_item(new) == normalize_schema_item(old))
    {
        return None;
    }

    let old_columns = parse_create_table_column_definitions(old_sql);
    let new_columns = parse_create_table_column_definitions(&change.sql);
    let added_columns: Vec<_> = new_columns
        .into_iter()
        .filter(|column| {
            !old_columns
                .iter()
                .any(|old| column_names_equal(&old.name, &column.name))
        })
        .collect();
    let appended_item_count = new_items.len() - old_items.len();
    if added_columns.is_empty() || added_columns.len() != appended_item_count {
        return None;
    }
    Some(added_columns)
}

fn columns_overlap_with_different_defs(
    ours: &[ColumnDefinition],
    theirs: &[ColumnDefinition],
) -> bool {
    ours.iter().any(|ours| {
        theirs.iter().any(|theirs| {
            column_names_equal(&ours.name, &theirs.name)
                && normalize_schema_item(&ours.sql) != normalize_schema_item(&theirs.sql)
        })
    })
}

fn column_definitions_equal_except_name(left: &ColumnDefinition, right: &ColumnDefinition) -> bool {
    normalize_schema_item(strip_leading_identifier(&left.sql))
        == normalize_schema_item(strip_leading_identifier(&right.sql))
}

fn column_names_equal(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn normalize_schema_item(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_leading_identifier(sql: &str) -> &str {
    let sql = sql.trim_start();
    let mut chars = sql.char_indices();
    let Some((_, first)) = chars.next() else {
        return sql;
    };
    let closing = match first {
        '"' | '\'' | '`' => Some(first),
        '[' => Some(']'),
        _ => None,
    };
    if let Some(closing) = closing {
        let mut escaped = false;
        for (idx, ch) in chars {
            if ch == closing && !escaped {
                return sql[idx + ch.len_utf8()..].trim_start();
            }
            escaped = ch == closing && !escaped;
        }
        return "";
    }

    sql.find(char::is_whitespace)
        .map(|idx| sql[idx..].trim_start())
        .unwrap_or("")
}

fn quote_identifier(id: &str) -> String {
    crate::row_level_diff::quote_identifier(id)
}

#[derive(Debug)]
struct SemanticKeyValue {
    normalized: Vec<String>,
    display: Vec<String>,
}

fn semantic_change_key(
    columns: &[String],
    key_columns: &[String],
    collations: &[graft::repo::SemanticKeyCollation],
    change: &RowChange,
) -> Option<SemanticKeyValue> {
    let result = match change {
        RowChange::Insert { row, .. } | RowChange::PrimaryKeyInsert { row, .. } => row,
        RowChange::Update { new_row, .. } | RowChange::PrimaryKeyUpdate { new_row, .. } => new_row,
        RowChange::Delete { .. } | RowChange::PrimaryKeyDelete { .. } => return None,
    };
    if key_columns.is_empty() || collations.len() != key_columns.len() {
        return None;
    }
    semantic_record_key(columns, key_columns, collations, result)
}

fn semantic_record_key(
    columns: &[String],
    key_columns: &[String],
    collations: &[graft::repo::SemanticKeyCollation],
    row: &Record,
) -> Option<SemanticKeyValue> {
    let mut normalized = Vec::with_capacity(key_columns.len());
    let mut display = Vec::with_capacity(key_columns.len());
    for (column, collation) in key_columns.iter().zip(collations) {
        let index = columns
            .iter()
            .position(|candidate| candidate.eq_ignore_ascii_case(column))?;
        let value = semantic_value_key(row, index)?;
        display.push(value.clone());
        normalized.push(match (collation, value.strip_prefix("t:")) {
            (graft::repo::SemanticKeyCollation::NoCase, Some(text)) => {
                format!("t:{}", text.to_ascii_lowercase())
            }
            _ => value,
        });
    }
    Some(SemanticKeyValue { normalized, display })
}

fn semantic_key_collation(
    policy: &RowMergePolicy,
    table: &str,
    column: &str,
) -> graft::repo::SemanticKeyCollation {
    policy
        .semantic_key_collations
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(table))
        .and_then(|(_, columns)| {
            columns
                .iter()
                .find(|(candidate, _)| candidate.eq_ignore_ascii_case(column))
                .map(|(_, collation)| *collation)
        })
        .unwrap_or_default()
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
    identity: RowIdentity,
}

impl RowKey {
    fn from_change(table: &str, change: &RowChange) -> Self {
        Self {
            table: table.to_string(),
            identity: change.identity(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SemanticRowKey {
    table: String,
    key: Vec<String>,
}

#[derive(Debug, Clone)]
struct RowTouch {
    kind: RowChangeKind,
    change: RowChange,
    columns: Vec<String>,
    semantic_key: Option<Vec<String>>,
    semantic_key_display: Option<Vec<String>>,
    semantic_key_collations: Vec<graft::repo::SemanticKeyCollation>,
    can_omit_insert_rowid: bool,
}

trait RowChangeExt {
    fn kind(&self) -> RowChangeKind;
}

impl RowChangeExt for RowChange {
    fn kind(&self) -> RowChangeKind {
        match self {
            RowChange::Insert { .. } | RowChange::PrimaryKeyInsert { .. } => RowChangeKind::Insert,
            RowChange::Delete { .. } | RowChange::PrimaryKeyDelete { .. } => RowChangeKind::Delete,
            RowChange::Update { .. } | RowChange::PrimaryKeyUpdate { .. } => RowChangeKind::Update,
        }
    }
}
