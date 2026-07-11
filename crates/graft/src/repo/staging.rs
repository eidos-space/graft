use super::*;

impl Repository {
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
        self.stage_file_state(key, file)
    }

    pub(super) fn stage_file_state(
        &self,
        key: String,
        file: CommitFileState,
    ) -> Result<index::IndexEntry> {
        let entry = self.index_entry_for_state(key.clone(), index::IndexStage::Normal, file)?;
        let mut index = self.read_index()?;
        index.stage(entry.clone());
        self.write_index(&index)?;
        self.clear_dirty_key(&key)?;
        Ok(entry)
    }

    pub fn stage_file_state_path(
        &self,
        path: impl AsRef<Path>,
        file: CommitFileState,
    ) -> Result<index::IndexEntry> {
        validate_commit_file_state(&file)?;
        let key = self.file_key(path)?;
        self.stage_file_state(key, file)
    }

    pub fn stage_artifact_path(&self, path: impl AsRef<Path>) -> Result<index::IndexEntry> {
        let key = self.file_key(path)?;
        let physical_path = self.worktree.join(&key);
        let artifact = self.write_artifact_state_from_path(&key, &physical_path)?;
        self.stage_artifact_state(key, artifact)
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
        self.stage_artifact_state(key, artifact)
    }

    pub(super) fn stage_artifact_state(
        &self,
        key: String,
        artifact: CommitArtifactState,
    ) -> Result<index::IndexEntry> {
        let entry =
            self.index_entry_for_artifact_state(key.clone(), index::IndexStage::Normal, artifact);
        let mut index = self.read_index()?;
        index.stage(entry.clone());
        self.write_index(&index)?;
        self.clear_dirty_key(&key)?;
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
        let mut index = self.read_index()?;
        index.stage(entry.clone());
        self.write_index(&index)?;
        self.clear_dirty_key(&entry.path)?;
        Ok(entry)
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
        let object_store = self.object_store();
        let tree = self.write_tree_object(&object_store, &files, &artifacts)?;
        let commit_object = self.canonical_commit_object(
            tree.clone(),
            &parents,
            &message,
            timestamp_ms,
            tables.clone(),
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
