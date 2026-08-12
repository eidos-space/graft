use super::*;

impl Repository {
    /// Records a physical worktree rename in the index without re-reading or re-importing the
    /// payload. This is the repository equivalent of `git mv`: the source's tracked identity is
    /// preserved at the destination and later content edits remain ordinary unstaged changes.
    pub fn stage_path_move_keys(
        &self,
        previous_path: &str,
        path: &str,
    ) -> Result<Vec<index::IndexEntry>> {
        let previous_path = normalize_repo_path_key(previous_path)?;
        let path = normalize_repo_path_key(path)?;
        if previous_path == path {
            return Err(RepoErr::InvalidPathMove {
                from: previous_path,
                to: path,
                reason: "source and destination are identical",
            });
        }
        let destination_metadata =
            fs::symlink_metadata(self.worktree.join(&path)).map_err(|_| {
                RepoErr::InvalidPathMove {
                    from: previous_path.clone(),
                    to: path.clone(),
                    reason: "destination does not exist",
                }
            })?;
        if !destination_metadata.file_type().is_file() && !destination_metadata.file_type().is_dir()
        {
            return Err(RepoErr::InvalidPathMove {
                from: previous_path,
                to: path,
                reason: "destination is not a regular file or directory",
            });
        }
        if self.worktree.join(&previous_path).exists() {
            return Err(RepoErr::InvalidPathMove {
                from: previous_path,
                to: path,
                reason: "source still exists in the worktree",
            });
        }
        let files = self.index_files()?;
        let artifacts = self.index_artifacts()?;
        let head_files = self.head_files()?;
        let head_artifacts = self.head_artifacts()?;
        let mut moves = Vec::<(String, String, RepoTrackedPathState, bool)>::new();
        if destination_metadata.file_type().is_file() {
            let source_state = files
                .get(&previous_path)
                .cloned()
                .map(RepoTrackedPathState::File)
                .or_else(|| {
                    artifacts
                        .get(&previous_path)
                        .cloned()
                        .map(RepoTrackedPathState::Artifact)
                })
                .ok_or_else(|| RepoErr::PathNotTracked(previous_path.clone()))?;
            moves.push((
                previous_path.clone(),
                path.clone(),
                source_state,
                head_files.contains_key(&previous_path)
                    || head_artifacts.contains_key(&previous_path),
            ));
        } else {
            let prefix = format!("{previous_path}/");
            for (source, state) in
                files
                    .iter()
                    .map(|(source, state)| (source, RepoTrackedPathState::File(state.clone())))
                    .chain(artifacts.iter().map(|(source, state)| {
                        (source, RepoTrackedPathState::Artifact(state.clone()))
                    }))
                    .filter(|(source, _)| source.starts_with(&prefix))
            {
                let suffix = source
                    .strip_prefix(&prefix)
                    .expect("filtered path has the source prefix");
                let destination = format!("{path}/{suffix}");
                moves.push((
                    source.clone(),
                    destination,
                    state,
                    head_files.contains_key(source) || head_artifacts.contains_key(source),
                ));
            }
            if moves.is_empty() {
                return Err(RepoErr::PathNotTracked(previous_path));
            }
        }
        for (_, destination, _, _) in &moves {
            if files.contains_key(destination) || artifacts.contains_key(destination) {
                return Err(RepoErr::InvalidPathMove {
                    from: previous_path,
                    to: path,
                    reason: "destination contains an already tracked path",
                });
            }
            if !self.worktree.join(destination).is_file() {
                return Err(RepoErr::InvalidPathMove {
                    from: previous_path,
                    to: path,
                    reason: "a moved tracked file is missing at the destination",
                });
            }
        }

        let mut index = self.read_index()?;
        if index.has_conflicts() {
            return Err(RepoErr::UnresolvedConflicts);
        }
        let mut entries = Vec::with_capacity(moves.len() * 2);
        let mut cleared = Vec::with_capacity(moves.len() * 2);
        for (source, destination, state, source_was_committed) in moves {
            let destination_entry = match state {
                RepoTrackedPathState::File(file) => self.index_entry_for_state(
                    destination.clone(),
                    index::IndexStage::Normal,
                    file,
                )?,
                RepoTrackedPathState::Artifact(artifact) => self.index_entry_for_artifact_state(
                    destination.clone(),
                    index::IndexStage::Normal,
                    artifact,
                ),
            };
            index.stage(destination_entry.clone());
            entries.push(destination_entry);
            if source_was_committed {
                let removal = index::IndexEntry {
                    path: source.clone(),
                    mode: None,
                    oid: None,
                    stage: index::IndexStage::Normal,
                    file: None,
                    artifact: None,
                };
                index.stage(removal.clone());
                entries.push(removal);
            } else {
                index.remove_path(&source);
            }
            cleared.push(source);
            cleared.push(destination);
        }
        self.write_index(&index)?;
        self.clear_dirty_keys(cleared.iter().map(String::as_str))?;
        Ok(entries)
    }

    pub fn commit(&self, message: impl Into<String>) -> Result<CommitObject> {
        let commit = self.commit_with_files(message, BTreeMap::new(), Vec::new())?;
        self.clear_dirty()?;
        Ok(commit)
    }

    #[cfg(test)]
    pub(super) fn stage_file(
        &self,
        path: impl AsRef<Path>,
        volume: VolumeId,
        snapshot: &Snapshot,
    ) -> Result<index::IndexEntry> {
        let key = self.file_key(path)?;
        let file = CommitFileState {
            volume,
            snapshot: repo_snapshot_with_test_hashes(snapshot),
        };
        let entry = self.index_entry_for_state(key, index::IndexStage::Normal, file)?;
        self.stage_index_entries(std::slice::from_ref(&entry))?;
        Ok(entry)
    }

    pub fn prepare_file_state_path(
        &self,
        path: impl AsRef<Path>,
        file: CommitFileState,
    ) -> Result<index::IndexEntry> {
        validate_commit_file_state(&file)?;
        let key = self.file_key(path)?;
        self.index_entry_for_state(key, index::IndexStage::Normal, file)
    }

    pub fn stage_file_state_path(
        &self,
        path: impl AsRef<Path>,
        file: CommitFileState,
    ) -> Result<index::IndexEntry> {
        let entry = self.prepare_file_state_path(path, file)?;
        self.stage_index_entries(std::slice::from_ref(&entry))?;
        Ok(entry)
    }

    pub fn prepare_artifact_path(&self, path: impl AsRef<Path>) -> Result<index::IndexEntry> {
        let key = self.file_key(path)?;
        let physical_path = self.worktree.join(&key);
        let artifact = self.write_artifact_state_from_path(&key, &physical_path)?;
        Ok(self.index_entry_for_artifact_state(key, index::IndexStage::Normal, artifact))
    }

    pub fn stage_artifact_path(&self, path: impl AsRef<Path>) -> Result<index::IndexEntry> {
        let entry = self.prepare_artifact_path(path)?;
        self.stage_index_entries(std::slice::from_ref(&entry))?;
        Ok(entry)
    }

    #[cfg(test)]
    pub(super) fn stage_artifact_path_with_inline_text_threshold(
        &self,
        path: impl AsRef<Path>,
        inline_text_threshold: u64,
    ) -> Result<index::IndexEntry> {
        let key = self.file_key(path)?;
        let physical_path = self.worktree.join(&key);
        let config = FileConfig {
            inline_text_threshold: ByteUnit::new(inline_text_threshold),
            external_paths: Vec::new(),
        };
        let artifact =
            self.write_artifact_state_from_path_with_file_config(&key, &physical_path, &config)?;
        let entry = self.index_entry_for_artifact_state(key, index::IndexStage::Normal, artifact);
        self.stage_index_entries(std::slice::from_ref(&entry))?;
        Ok(entry)
    }

    pub fn stage_file_removal(&self, path: impl AsRef<Path>) -> Result<index::IndexEntry> {
        let key = self.file_key(path)?;
        self.stage_file_removal_key(key)
    }

    pub fn stage_file_removal_key(&self, key: impl Into<String>) -> Result<index::IndexEntry> {
        let key = normalize_repo_path_key(&key.into())?;
        if !self.head_files()?.contains_key(&key) && !self.head_artifacts()?.contains_key(&key) {
            return Err(RepoErr::PathNotTracked(key));
        }
        let entry = index::IndexEntry {
            path: key,
            mode: None,
            oid: None,
            stage: index::IndexStage::Normal,
            file: None,
            artifact: None,
        };
        self.stage_index_entries(std::slice::from_ref(&entry))?;
        Ok(entry)
    }

    pub fn stage_index_entries(&self, entries: &[index::IndexEntry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut index = self.read_index()?;
        index.stage_all(entries.iter().cloned());
        self.write_index(&index)?;
        self.clear_dirty_keys(entries.iter().map(|entry| entry.path.as_str()))
    }

    /// Restores the original non-normal index stages for one path in an active merge.
    ///
    /// Embedders use this as the repository-level equivalent of Git's resolve-undo. The caller
    /// supplies stages captured from the same durable merge session; this method validates that
    /// they address exactly one normalized path before replacing the current stage-0 result.
    pub fn restore_merge_conflict_stages(
        &self,
        path: impl AsRef<Path>,
        entries: &[index::IndexEntry],
    ) -> Result<()> {
        let key = self.file_key(path)?;
        if self.merge_head()?.is_none() {
            return Err(RepoErr::NoMergeInProgress);
        }
        if entries.is_empty()
            || entries
                .iter()
                .any(|entry| entry.path != key || entry.stage == index::IndexStage::Normal)
        {
            return Err(RepoErr::PathNotConflicted(key));
        }
        let mut stages = BTreeSet::new();
        if entries
            .iter()
            .any(|entry| !stages.insert(u8::from(entry.stage)))
        {
            return Err(RepoErr::PathNotConflicted(key));
        }

        let mut index = self.read_index()?;
        index.remove_path(&key);
        index.stage_all(entries.iter().cloned());
        self.write_index(&index)?;
        self.clear_dirty_key(&key)
    }

    pub fn resolve_file_conflict(
        &self,
        path: impl AsRef<Path>,
        file: Option<CommitFileState>,
    ) -> Result<index::IndexEntry> {
        let key = self.file_key(path)?;
        let mut index = self.read_index()?;
        if !index.conflicted_paths().iter().any(|path| path == &key) {
            return Err(RepoErr::PathNotConflicted(key));
        }

        let entry = if let Some(file) = file {
            self.index_entry_for_state(key.clone(), index::IndexStage::Normal, file)?
        } else {
            index::IndexEntry {
                path: key.clone(),
                mode: None,
                oid: None,
                stage: index::IndexStage::Normal,
                file: None,
                artifact: None,
            }
        };
        index.stage(entry.clone());
        self.write_index(&index)?;
        self.clear_dirty_key(&key)?;
        Ok(entry)
    }

    pub fn resolve_artifact_conflict(
        &self,
        path: impl AsRef<Path>,
        artifact: Option<CommitArtifactState>,
    ) -> Result<index::IndexEntry> {
        let key = self.file_key(path)?;
        self.resolve_artifact_conflict_key(key, artifact)
    }

    pub fn resolve_artifact_conflict_from_path(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<index::IndexEntry> {
        let key = self.file_key(path)?;
        let physical_path = self.worktree.join(&key);
        let artifact = self.write_artifact_state_from_path(&key, &physical_path)?;
        self.resolve_artifact_conflict_key(key, Some(artifact))
    }

    pub(super) fn resolve_artifact_conflict_key(
        &self,
        key: String,
        artifact: Option<CommitArtifactState>,
    ) -> Result<index::IndexEntry> {
        let mut index = self.read_index()?;
        if !index.conflicted_paths().iter().any(|path| path == &key) {
            return Err(RepoErr::PathNotConflicted(key));
        }

        let entry = if let Some(artifact) = artifact {
            self.index_entry_for_artifact_state(key.clone(), index::IndexStage::Normal, artifact)
        } else {
            index::IndexEntry {
                path: key.clone(),
                mode: None,
                oid: None,
                stage: index::IndexStage::Normal,
                file: None,
                artifact: None,
            }
        };
        index.stage(entry.clone());
        self.write_index(&index)?;
        self.clear_dirty_key(&key)?;
        Ok(entry)
    }

    pub(super) fn index_entry_for_state(
        &self,
        key: String,
        stage: index::IndexStage,
        file: CommitFileState,
    ) -> Result<index::IndexEntry> {
        let blob = object::Object::Blob(object::BlobObject::SqliteSnapshot(sqlite_snapshot_blob(
            &file,
        )));
        let oid = self.object_store().write(&blob)?;
        Ok(index::IndexEntry {
            path: key,
            mode: Some(object::TreeEntryMode::SqliteDatabase),
            oid: Some(oid),
            stage,
            file: Some(file),
            artifact: None,
        })
    }

    pub(super) fn index_entry_for_artifact_state(
        &self,
        key: String,
        stage: index::IndexStage,
        artifact: CommitArtifactState,
    ) -> index::IndexEntry {
        index::IndexEntry {
            path: key,
            mode: Some(object::TreeEntryMode::Regular),
            oid: Some(artifact.oid().clone()),
            stage,
            file: None,
            artifact: Some(artifact),
        }
    }

    pub fn commit_staged(&self, message: impl Into<String>) -> Result<CommitObject> {
        self.commit_staged_with_table_summary(message, Vec::new())
    }

    pub fn commit_staged_with_table_summary(
        &self,
        message: impl Into<String>,
        tables: Vec<CommitTableSummary>,
    ) -> Result<CommitObject> {
        let index = self.read_index()?;
        if index.has_conflicts() {
            return Err(RepoErr::UnresolvedConflicts);
        }
        if !index.has_staged_changes() && self.merge_head()?.is_none() {
            return Err(RepoErr::NoStagedChanges);
        }

        let mut files = self.head_files()?;
        let mut artifacts = self.head_artifacts()?;
        for entry in index.stage0_entries() {
            if let Some(file) = &entry.file {
                files.insert(entry.path.clone(), file.clone());
                artifacts.remove(&entry.path);
            } else if let Some(artifact) = &entry.artifact {
                artifacts.insert(entry.path.clone(), artifact.clone());
                files.remove(&entry.path);
            } else {
                files.remove(&entry.path);
                artifacts.remove(&entry.path);
            }
        }
        let commit = self.commit_with_files_and_artifacts(message, files, artifacts, tables)?;
        self.clear_index()?;
        Ok(commit)
    }

    #[cfg(test)]
    pub(super) fn commit_file(
        &self,
        path: impl AsRef<Path>,
        message: impl Into<String>,
        volume: VolumeId,
        snapshot: &Snapshot,
    ) -> Result<CommitObject> {
        self.stage_file(path, volume, snapshot)?;
        self.commit_staged(message)
    }

    pub(super) fn commit_with_files(
        &self,
        message: impl Into<String>,
        files: BTreeMap<String, CommitFileState>,
        tables: Vec<CommitTableSummary>,
    ) -> Result<CommitObject> {
        self.commit_with_files_and_artifacts(message, files, BTreeMap::new(), tables)
    }

    pub(super) fn commit_with_files_and_artifacts(
        &self,
        message: impl Into<String>,
        files: BTreeMap<String, CommitFileState>,
        artifacts: BTreeMap<String, CommitArtifactState>,
        tables: Vec<CommitTableSummary>,
    ) -> Result<CommitObject> {
        let head = self.head()?;
        let parents = self.commit_parents()?;
        let parent = parents.first().cloned();
        let timestamp_ms = now_ms();
        let message = message.into();
        let tables = normalize_commit_table_summary(tables);
        let changed_tables = tables.len();
        let changes =
            self.commit_changes(parents.first().map(String::as_str), &files, &artifacts)?;
        let path_changes = Some(commit_path_change_counts(&changes));
        let object_store = self.object_store();
        let tree = self.write_tree_object(&object_store, &files, &artifacts)?;
        let commit_object = self.canonical_commit_object(
            tree.clone(),
            &parents,
            &message,
            timestamp_ms,
            tables.clone(),
            path_changes,
        )?;
        let id = object_store.write(&object::Object::Commit(commit_object))?;
        let commit = CommitObject {
            id: id.to_string(),
            parent,
            parents,
            tree: Some(tree.to_string()),
            message,
            timestamp_ms,
            files,
            artifacts,
            changes,
            tables,
            changed_tables,
        };

        match head {
            Head::Branch { name } => {
                self.write_branch_ref(&name, &commit.id, &format!("commit: {}", commit.message))?
            }
            Head::Detached { .. } => self.write_head_with_message(
                &Head::Detached { commit: commit.id.clone() },
                &format!("commit: {}", commit.message),
            )?,
        }

        self.clear_merge_state()?;
        Ok(commit)
    }

    pub(super) fn commit_changes(
        &self,
        parent: Option<&str>,
        files: &BTreeMap<String, CommitFileState>,
        artifacts: &BTreeMap<String, CommitArtifactState>,
    ) -> Result<Vec<CommitPathChange>> {
        let Some(parent) = parent else {
            return Ok(commit_path_changes(
                &BTreeMap::new(),
                files,
                &BTreeMap::new(),
                artifacts,
            ));
        };
        let Some((parent_files, parent_artifacts)) = self.commit_tree_state(parent)? else {
            return Ok(Vec::new());
        };
        Ok(commit_path_changes(
            &parent_files,
            files,
            &parent_artifacts,
            artifacts,
        ))
    }

    pub fn log(&self) -> Result<Vec<CommitObject>> {
        self.log_page(usize::MAX, None).map(|(commits, _)| commits)
    }

    /// Returns a bounded history page without reading any commit tree or blob object.
    pub fn history_summary_page(
        &self,
        limit: usize,
        after: Option<&str>,
    ) -> Result<RepoHistorySummaryPage> {
        if limit == 0 {
            return Ok(RepoHistorySummaryPage {
                commits: Vec::new(),
                has_more: self.head_target()?.is_some(),
                next_cursor: None,
            });
        }
        let mut commits = Vec::with_capacity(limit.min(256));
        let mut frontier = self.head_target()?.into_iter().collect::<Vec<_>>();
        let mut seen = BTreeSet::<String>::new();
        let mut cache = BTreeMap::<String, RepoCommitSummary>::new();
        let mut after_seen = after.is_none();

        while let Some((idx, id)) =
            self.next_summary_frontier_commit(&frontier, &seen, &mut cache)?
        {
            cancellation_checkpoint()?;
            frontier.remove(idx);
            if !seen.insert(id.clone()) {
                continue;
            }
            let commit = cache
                .remove(&id)
                .unwrap_or_else(|| unreachable!("commit summary was cached"));
            for parent in &commit.parents {
                if !seen.contains(parent) {
                    frontier.push(parent.clone());
                }
            }
            if !after_seen {
                if after == Some(commit.id.as_str()) {
                    after_seen = true;
                }
                continue;
            }
            commits.push(commit);
            if commits.len() > limit {
                commits.truncate(limit);
                let next_cursor = commits.last().map(|commit| commit.id.clone());
                return Ok(RepoHistorySummaryPage { commits, has_more: true, next_cursor });
            }
        }

        if !after_seen {
            return Err(RepoErr::InvalidRevision(
                after.unwrap_or_default().to_string(),
            ));
        }
        let next_cursor = commits.last().map(|commit| commit.id.clone());
        Ok(RepoHistorySummaryPage { commits, has_more: false, next_cursor })
    }

    /// Lazily hydrates one commit and returns a bounded page of paths changed from its first
    /// parent. A root commit is compared with an empty tree.
    pub fn commit_changed_paths_page(
        &self,
        revision: &str,
        limit: usize,
        after: Option<&str>,
    ) -> Result<RepoCommitChangedPathsPage> {
        cancellation_checkpoint()?;
        let revision = self.resolve_revision(revision)?;
        let commit = self.read_commit(&revision)?;
        cancellation_checkpoint()?;

        let parent = commit.parents.first().cloned();
        let mut paths = commit.changes;
        paths.sort_by(|left, right| left.path.cmp(&right.path));
        let total_changed_paths = paths.len();
        if let Some(after) = after {
            paths.retain(|change| change.path.as_str() > after);
        }
        let has_more = paths.len() > limit;
        paths.truncate(limit);
        let next_cursor = paths.last().map(|change| change.path.clone());
        cancellation_checkpoint()?;

        Ok(RepoCommitChangedPathsPage {
            revision,
            parent,
            paths,
            total_changed_paths,
            has_more,
            next_cursor,
        })
    }

    /// Reads only the canonical commit object. Use [`Self::read_commit`] for lazy details.
    pub fn read_commit_summary(&self, id: &str) -> Result<RepoCommitSummary> {
        let id = object::ObjectId::from_str(id)?;
        let commit = self
            .read_commit_object(&id)?
            .ok_or_else(|| RepoErr::CommitNotFound(id.to_string()))?;
        let path_counts_complete = commit.path_changes.is_some();
        let changed_tables = commit.tables.len();
        Ok(RepoCommitSummary {
            id: id.to_string(),
            parents: commit.parents.iter().map(ToString::to_string).collect(),
            message: commit.message,
            timestamp_ms: commit.committer.timestamp_ms,
            path_changes: commit.path_changes,
            path_counts_complete,
            tables: commit.tables,
            changed_tables,
        })
    }

    fn next_summary_frontier_commit(
        &self,
        frontier: &[String],
        seen: &BTreeSet<String>,
        cache: &mut BTreeMap<String, RepoCommitSummary>,
    ) -> Result<Option<(usize, String)>> {
        let mut selected = None;
        let mut selected_timestamp = 0;
        for (idx, id) in frontier.iter().enumerate() {
            cancellation_checkpoint()?;
            if seen.contains(id) {
                continue;
            }
            if !cache.contains_key(id) {
                cache.insert(id.clone(), self.read_commit_summary(id)?);
            }
            let timestamp = cache
                .get(id)
                .map(|commit| commit.timestamp_ms)
                .unwrap_or_default();
            if selected.is_none() || timestamp > selected_timestamp {
                selected = Some((idx, id.clone()));
                selected_timestamp = timestamp;
            }
        }
        Ok(selected)
    }

    /// Walk repository history in display order and return one bounded page.
    ///
    /// `after` is an exact commit object id from a previous page. The walk
    /// stops as soon as it can determine whether another page exists, so the
    /// caller does not have to load and serialize the full repository log.
    pub fn log_page(&self, limit: usize, after: Option<&str>) -> Result<(Vec<CommitObject>, bool)> {
        if limit == 0 {
            return Ok((Vec::new(), self.head_target()?.is_some()));
        }
        let mut commits = vec![];
        let mut frontier = self.head_target()?.into_iter().collect::<Vec<_>>();
        let mut seen = BTreeSet::<String>::new();
        let mut cache = BTreeMap::<String, CommitObject>::new();
        let mut after_seen = after.is_none();

        while let Some((idx, id)) = self.next_log_frontier_commit(&frontier, &seen, &mut cache)? {
            cancellation_checkpoint()?;
            frontier.remove(idx);
            if !seen.insert(id.clone()) {
                continue;
            }
            let commit = cache
                .remove(&id)
                .unwrap_or_else(|| unreachable!("commit was cached"));
            for parent in commit_parent_ids(&commit) {
                if !seen.contains(&parent) {
                    frontier.push(parent);
                }
            }
            if !after_seen {
                if after == Some(commit.id.as_str()) {
                    after_seen = true;
                }
                continue;
            }
            commits.push(commit);
            if commits.len() > limit {
                commits.truncate(limit);
                return Ok((commits, true));
            }
        }

        if !after_seen {
            return Err(RepoErr::InvalidRevision(
                after.unwrap_or_default().to_string(),
            ));
        }
        Ok((commits, false))
    }

    pub(super) fn next_log_frontier_commit(
        &self,
        frontier: &[String],
        seen: &BTreeSet<String>,
        cache: &mut BTreeMap<String, CommitObject>,
    ) -> Result<Option<(usize, String)>> {
        let mut selected = None;
        let mut selected_timestamp = 0;

        for (idx, id) in frontier.iter().enumerate() {
            cancellation_checkpoint()?;
            if seen.contains(id) {
                continue;
            }
            if !cache.contains_key(id) {
                cache.insert(id.clone(), self.read_commit(id)?);
            }
            let timestamp = cache
                .get(id)
                .map(|commit| commit.timestamp_ms)
                .unwrap_or_default();
            if selected.is_none() || timestamp > selected_timestamp {
                selected = Some((idx, id.clone()));
                selected_timestamp = timestamp;
            }
        }

        Ok(selected)
    }
}
