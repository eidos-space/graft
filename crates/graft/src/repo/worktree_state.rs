use super::*;

impl Repository {
    pub fn mark_dirty_path(&self, path: impl AsRef<Path>) -> Result<()> {
        let key = self.file_key(path)?;
        self.mark_dirty_key(key)
    }

    pub fn mark_dirty_key(&self, key: impl Into<String>) -> Result<()> {
        let key = normalize_repo_path_key(&key.into())?;
        let mut state = self.read_worktree_state()?;
        let mut dirty = state.dirty.into_iter().collect::<BTreeSet<_>>();
        dirty.insert(key.clone());
        state.dirty = dirty.into_iter().collect();
        state.deleted.retain(|path| path != &key);
        self.write_worktree_state(&state)
    }

    pub fn mark_deleted_path(&self, path: impl AsRef<Path>) -> Result<()> {
        let key = self.file_key(path)?;
        self.mark_deleted_key(key)
    }

    pub fn mark_deleted_key(&self, key: impl Into<String>) -> Result<()> {
        let key = normalize_repo_path_key(&key.into())?;
        let mut state = self.read_worktree_state()?;
        state.dirty.retain(|path| path != &key);
        let mut deleted = state.deleted.into_iter().collect::<BTreeSet<_>>();
        deleted.insert(key);
        state.deleted = deleted.into_iter().collect();
        self.write_worktree_state(&state)
    }

    pub fn clear_dirty_path(&self, path: impl AsRef<Path>) -> Result<()> {
        let key = self.file_key(path)?;
        self.clear_dirty_key(&key)
    }

    pub fn clear_dirty_key(&self, key: &str) -> Result<()> {
        let key = normalize_repo_path_key(key)?;
        let mut state = self.read_worktree_state()?;
        state.dirty.retain(|path| path != &key);
        state.deleted.retain(|path| path != &key);
        self.write_worktree_state(&state)
    }

    pub fn clear_dirty(&self) -> Result<()> {
        let path = self.worktree_state_path();
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    pub fn dirty_paths(&self) -> Result<Vec<String>> {
        let state = self.read_worktree_state()?;
        let mut paths = state.dirty.into_iter().collect::<BTreeSet<_>>();
        paths.extend(state.deleted);
        Ok(paths.into_iter().collect())
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty_paths()
            .map(|paths| !paths.is_empty())
            .unwrap_or(false)
    }

    pub fn has_staged_changes(&self) -> Result<bool> {
        Ok(self.read_index()?.has_staged_changes())
    }

    pub fn has_work_in_progress(&self) -> Result<bool> {
        let index = self.read_index()?;
        Ok(!self.dirty_paths()?.is_empty()
            || index.has_staged_changes()
            || index.has_conflicts()
            || self.merge_head()?.is_some())
    }

    pub fn discard_work_in_progress(&self) -> Result<()> {
        self.clear_index()?;
        self.clear_dirty()?;
        self.clear_merge_state()
    }

    pub fn head_file(&self, path: impl AsRef<Path>) -> Result<Option<CommitFileState>> {
        let key = self.file_key(path)?;
        Ok(self
            .head_target()?
            .map(|commit| self.read_commit(&commit))
            .transpose()?
            .and_then(|commit| commit.files.get(&key).cloned()))
    }

    pub fn head_artifact(&self, path: impl AsRef<Path>) -> Result<Option<CommitArtifactState>> {
        let key = self.file_key(path)?;
        Ok(self
            .head_target()?
            .map(|commit| self.read_commit(&commit))
            .transpose()?
            .and_then(|commit| commit.artifacts.get(&key).cloned()))
    }

    pub fn index_file(&self, path: impl AsRef<Path>) -> Result<Option<CommitFileState>> {
        let key = self.file_key(path)?;
        Ok(self.index_files()?.remove(&key))
    }

    pub fn index_artifact(&self, path: impl AsRef<Path>) -> Result<Option<CommitArtifactState>> {
        let key = self.file_key(path)?;
        Ok(self.index_artifacts()?.remove(&key))
    }

    pub fn index_has_entry(&self, path: impl AsRef<Path>) -> Result<bool> {
        let key = self.file_key(path)?;
        self.index_has_key(key)
    }

    pub fn index_has_key(&self, key: impl Into<String>) -> Result<bool> {
        let key = normalize_repo_path_key(&key.into())?;
        Ok(self
            .read_index()?
            .stage0_entries()
            .any(|entry| entry.path == key))
    }

    pub fn restore_index_path_from_head(&self, path: impl AsRef<Path>) -> Result<String> {
        let key = self.file_key(path)?;
        self.restore_index_key_from_head(key)
    }

    pub fn restore_index_key_from_head(&self, key: impl Into<String>) -> Result<String> {
        let key = normalize_repo_path_key(&key.into())?;
        let mut index = self.read_index()?;
        if index.conflicted_paths().iter().any(|path| path == &key) {
            return Err(RepoErr::UnresolvedConflicts);
        }
        let had_index_entry = index.entries.iter().any(|entry| entry.path == key);
        let is_tracked_at_head =
            self.head_files()?.contains_key(&key) || self.head_artifacts()?.contains_key(&key);
        if !had_index_entry && !is_tracked_at_head {
            return Err(RepoErr::PathNotTracked(key));
        }
        index.remove_path(&key);
        self.write_index(&index)?;
        Ok(key)
    }

    pub fn restore_index_path_from_revision(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
    ) -> Result<String> {
        let key = self.file_key(path)?;
        self.restore_index_key_from_revision(rev, key)
    }

    pub fn restore_index_key_from_revision(
        &self,
        rev: &str,
        key: impl Into<String>,
    ) -> Result<String> {
        let key = normalize_repo_path_key(&key.into())?;
        let target = self.resolve_revision(rev)?;
        let source_commit = self.read_commit(&target)?;
        let source_files = source_commit.files;
        let source_artifacts = source_commit.artifacts;
        let source_state = source_files.get(&key).cloned();
        let source_artifact = source_artifacts.get(&key).cloned();
        let head_files = self.head_files()?;
        let head_artifacts = self.head_artifacts()?;
        let head_state = head_files.get(&key);
        let head_artifact = head_artifacts.get(&key);
        let head_has_path = head_state.is_some() || head_artifact.is_some();
        let mut index = self.read_index()?;
        if index.conflicted_paths().iter().any(|path| path == &key) {
            return Err(RepoErr::UnresolvedConflicts);
        }
        let had_index_entry = index.entries.iter().any(|entry| entry.path == key);

        if source_state.is_none() && source_artifact.is_none() && !head_has_path && !had_index_entry
        {
            return Err(RepoErr::PathNotFoundInRevision { path: key, rev: rev.to_string() });
        }

        index.remove_path(&key);
        if source_state.as_ref() == head_state && source_artifact.as_ref() == head_artifact {
            // Resetting the index to HEAD is represented by the absence of an index entry.
        } else if let Some(file) = source_state {
            index.stage(self.index_entry_for_state(
                key.clone(),
                index::IndexStage::Normal,
                file,
            )?);
        } else if let Some(artifact) = source_artifact {
            index.stage(self.index_entry_for_artifact_state(
                key.clone(),
                index::IndexStage::Normal,
                artifact,
            ));
        } else if head_has_path {
            index.stage(index::IndexEntry {
                path: key.clone(),
                mode: None,
                oid: None,
                stage: index::IndexStage::Normal,
                file: None,
                artifact: None,
            });
        }
        self.write_index(&index)?;
        Ok(key)
    }

    pub fn file_from_revision(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
    ) -> Result<Option<CommitFileState>> {
        let target = self.resolve_revision(rev)?;
        let key = self.file_key(path)?;
        Ok(self.read_commit(&target)?.files.get(&key).cloned())
    }

    pub fn artifact_from_revision(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
    ) -> Result<Option<CommitArtifactState>> {
        let target = self.resolve_revision(rev)?;
        let key = self.file_key(path)?;
        Ok(self.read_commit(&target)?.artifacts.get(&key).cloned())
    }

    pub fn materialize_artifact_state(
        &self,
        path: impl AsRef<Path>,
        state: &CommitArtifactState,
    ) -> Result<()> {
        let key = self.file_key(path)?;
        self.materialize_artifact_key(&key, state)
    }

    pub fn materialize_artifact_key(&self, key: &str, state: &CommitArtifactState) -> Result<()> {
        let path = self.worktree.join(key);
        let bytes = self.artifact_bytes(state)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_file_atomic(&path, &bytes)?;
        Ok(())
    }

    pub fn verify_artifact_state(&self, state: &CommitArtifactState) -> Result<()> {
        self.artifact_bytes(state).map(|_| ())
    }

    pub fn materialize_artifact_checkout(
        &self,
        artifacts: &BTreeMap<String, CommitArtifactState>,
        previous_artifacts: &BTreeMap<String, CommitArtifactState>,
        replacement_files: &BTreeMap<String, CommitFileState>,
    ) -> Result<()> {
        for (path, state) in artifacts {
            self.materialize_artifact_key(path, state)?;
        }
        for path in previous_artifacts.keys() {
            if artifacts.contains_key(path) || replacement_files.contains_key(path) {
                continue;
            }
            let physical_path = self.worktree.join(path);
            if physical_path.is_file() {
                fs::remove_file(&physical_path)?;
                remove_empty_parent_dirs(physical_path.parent(), &self.worktree)?;
            }
        }
        Ok(())
    }

    pub fn artifact_path_matches_state(
        &self,
        path: impl AsRef<Path>,
        expected: &CommitArtifactState,
    ) -> Result<Option<bool>> {
        let key = self.file_key(path)?;
        artifact_file_matches(&self.worktree.join(key), expected)
    }

    pub fn file_key(&self, path: impl AsRef<Path>) -> Result<String> {
        let path = path.as_ref();
        let parent = worktree_for_file(path);
        let parent = fs::canonicalize(parent)?;
        let Some(file_name) = path.file_name() else {
            return Err(RepoErr::PathOutsideWorktree {
                path: path.to_path_buf(),
                worktree: self.worktree.clone(),
            });
        };
        let absolute = parent.join(file_name);
        let relative =
            absolute
                .strip_prefix(&self.worktree)
                .map_err(|_| RepoErr::PathOutsideWorktree {
                    path: absolute.clone(),
                    worktree: self.worktree.clone(),
                })?;
        let key = relative
            .to_str()
            .ok_or_else(|| RepoErr::NonUtf8Path(relative.to_path_buf()))?;
        normalize_repo_path_key(key)
    }

    pub fn is_ignored_worktree_path(&self, path: impl AsRef<Path>) -> Result<bool> {
        let path = path.as_ref();
        let key = self.worktree_key_for_path(path)?;
        let is_dir = path.is_dir();
        Ok(self.ignore_rules()?.is_ignored(&key, is_dir))
    }

    pub(super) fn worktree_key_for_path(&self, path: &Path) -> Result<String> {
        let relative =
            path.strip_prefix(&self.worktree)
                .map_err(|_| RepoErr::PathOutsideWorktree {
                    path: path.to_path_buf(),
                    worktree: self.worktree.clone(),
                })?;
        let key = relative
            .to_str()
            .ok_or_else(|| RepoErr::NonUtf8Path(relative.to_path_buf()))?;
        normalize_repo_path_key(key)
    }

    pub(super) fn ignore_rules(&self) -> Result<IgnoreRules> {
        IgnoreRules::load(&self.worktree)
    }

    pub(super) fn create_layout(&self) -> Result<()> {
        for dir in [
            DIR_REFS_HEADS,
            DIR_REFS_REMOTES,
            DIR_REFS_TAGS,
            DIR_OBJECTS,
            DIR_OBJECTS_PACK,
            DIR_STORE_FJALL,
            DIR_STORE_FILES,
            DIR_INDEX,
            DIR_LOCKS,
            DIR_TMP,
            DIR_LOGS_REFS,
            DIR_LOGS_HEAD,
        ] {
            fs::create_dir_all(self.graft_dir.join(dir))?;
        }
        Ok(())
    }

    pub(super) fn ensure_supported_format(&self) -> Result<()> {
        let config = self.config()?;
        let actual = config.core.repository_format_version;
        if actual != REPOSITORY_FORMAT_VERSION {
            return Err(RepoErr::UnsupportedFormat {
                expected: REPOSITORY_FORMAT_VERSION,
                actual,
            });
        }
        let actual = config.extensions.object_format;
        if actual != OBJECT_FORMAT {
            return Err(RepoErr::UnsupportedObjectFormat { expected: OBJECT_FORMAT, actual });
        }
        Ok(())
    }

    pub(super) fn config_path(&self) -> PathBuf {
        self.graft_dir.join(CONFIG_FILE)
    }

    pub(super) fn head_path(&self) -> PathBuf {
        self.graft_dir.join(HEAD_FILE)
    }

    pub(super) fn current_head_for_reflog(&self) -> Result<Option<Head>> {
        if !self.head_path().is_file() {
            return Ok(None);
        }
        self.head().map(Some)
    }

    pub(super) fn head_reflog_target(&self, head: &Head) -> Result<Option<String>> {
        match head {
            Head::Branch { name } => self.read_branch_ref(name),
            Head::Detached { commit } => Ok(Some(commit.clone())),
        }
    }

    pub(super) fn merge_head_path(&self) -> PathBuf {
        self.graft_dir.join(MERGE_HEAD_FILE)
    }

    pub(super) fn orig_head_path(&self) -> PathBuf {
        self.graft_dir.join(ORIG_HEAD_FILE)
    }

    pub(super) fn worktree_state_path(&self) -> PathBuf {
        self.graft_dir.join(DIR_INDEX).join("worktree.toml")
    }

    pub(super) fn index_path(&self) -> PathBuf {
        self.graft_dir.join(DIR_INDEX).join("state.toml")
    }

    pub(super) fn head_target(&self) -> Result<Option<String>> {
        match self.head()? {
            Head::Branch { name } => self.read_branch_ref(&name),
            Head::Detached { commit } => Ok(Some(commit)),
        }
    }

    pub(super) fn move_head_to(&self, id: &str, message: &str) -> Result<()> {
        match self.head()? {
            Head::Branch { name } => self.write_branch_ref(&name, id, message)?,
            Head::Detached { .. } => {
                self.write_head_with_message(&Head::Detached { commit: id.to_string() }, message)?
            }
        }
        Ok(())
    }

    pub(super) fn merge_head(&self) -> Result<Option<String>> {
        let path = self.merge_head_path();
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(path)?;
        let target = raw.trim();
        if target.is_empty() {
            return Ok(None);
        }
        Ok(Some(target.to_string()))
    }

    pub(super) fn orig_head(&self) -> Result<Option<String>> {
        let path = self.orig_head_path();
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(path)?;
        let target = raw.trim();
        if target.is_empty() {
            return Ok(None);
        }
        Ok(Some(target.to_string()))
    }

    pub(super) fn write_merge_state(&self, orig_head: &str, merge_head: &str) -> Result<()> {
        fs::write(self.orig_head_path(), format!("{orig_head}\n"))?;
        fs::write(self.merge_head_path(), format!("{merge_head}\n"))?;
        Ok(())
    }

    pub(super) fn clear_merge_state(&self) -> Result<()> {
        for path in [self.merge_head_path(), self.orig_head_path()] {
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    pub(super) fn commit_parents(&self) -> Result<Vec<String>> {
        let mut parents = Vec::new();
        if let Some(head) = self.head_target()? {
            parents.push(head);
        }
        if let Some(merge_head) = self.merge_head()?
            && !parents.iter().any(|parent| parent == &merge_head)
        {
            parents.push(merge_head);
        }
        Ok(parents)
    }

    pub fn read_commit(&self, id: &str) -> Result<CommitObject> {
        let id = object::ObjectId::from_str(id)?;
        let commit = self
            .read_commit_object(&id)?
            .ok_or_else(|| RepoErr::CommitNotFound(id.to_string()))?;
        self.commit_from_object(&id, commit)
    }

    pub(super) fn read_commit_object(
        &self,
        id: &object::ObjectId,
    ) -> Result<Option<object::CommitObject>> {
        let Some(bytes) = self.object_store().read_raw(id)? else {
            return Ok(None);
        };
        let object = object::Object::decode(&bytes)?;
        let actual = object::ObjectId::for_bytes(&bytes);
        if actual != *id {
            return Err(RepoErr::Object(object::ObjectErr::ObjectIdMismatch {
                expected: id.clone(),
                actual,
            }));
        }
        match object {
            object::Object::Commit(commit) => Ok(Some(commit)),
            object => Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "commit",
                message: format!("object {id} is a {}", object.kind()),
            })),
        }
    }

    pub(super) fn commit_tree_state(
        &self,
        id: &str,
    ) -> Result<
        Option<(
            BTreeMap<String, CommitFileState>,
            BTreeMap<String, CommitArtifactState>,
        )>,
    > {
        let id = object::ObjectId::from_str(id)?;
        let Some(commit) = self.read_commit_object(&id)? else {
            return Ok(None);
        };
        self.tree_state_from_object(&commit.tree).map(Some)
    }

    pub(super) fn commit_from_object(
        &self,
        id: &object::ObjectId,
        commit: object::CommitObject,
    ) -> Result<CommitObject> {
        let parents = commit
            .parents
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let tables = commit.tables;
        let changed_tables = tables.len();
        let (files, artifacts) = self.tree_state_from_object(&commit.tree)?;
        let changes =
            self.commit_changes(parents.first().map(String::as_str), &files, &artifacts)?;
        Ok(CommitObject {
            id: id.to_string(),
            parent: parents.first().cloned(),
            parents,
            tree: Some(commit.tree.to_string()),
            message: commit.message,
            timestamp_ms: commit.committer.timestamp_ms,
            files,
            artifacts,
            changes,
            tables,
            changed_tables,
        })
    }

    pub(super) fn tree_state_from_object(
        &self,
        id: &object::ObjectId,
    ) -> Result<(
        BTreeMap<String, CommitFileState>,
        BTreeMap<String, CommitArtifactState>,
    )> {
        let object = self.object_store().read(id)?;
        let object::Object::Tree(tree) = object else {
            return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "tree",
                message: format!("object {id} is not a tree"),
            }));
        };

        let mut files = BTreeMap::new();
        let mut artifacts = BTreeMap::new();
        for entry in tree.entries {
            match entry.mode {
                object::TreeEntryMode::SqliteDatabase => {
                    let object = self.object_store().read(&entry.oid)?;
                    let object::Object::Blob(object::BlobObject::SqliteSnapshot(blob)) = object
                    else {
                        return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                            kind: "blob",
                            message: format!(
                                "tree entry `{}` is not a sqlite snapshot",
                                entry.path
                            ),
                        }));
                    };
                    files.insert(entry.path, file_state_from_sqlite_snapshot_blob(blob));
                }
                object::TreeEntryMode::Regular => {
                    let object = self.object_store().read(&entry.oid)?;
                    let object::Object::Blob(blob) = object else {
                        return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                            kind: "blob",
                            message: format!("tree entry `{}` is not a blob", entry.path),
                        }));
                    };
                    artifacts.insert(entry.path, artifact_state_from_blob(entry.oid, blob)?);
                }
            }
        }
        Ok((files, artifacts))
    }

    pub fn read_index(&self) -> Result<index::Index> {
        let path = self.index_path();
        if !path.exists() {
            return Ok(index::Index::default());
        }
        let raw = fs::read_to_string(path)?;
        Ok(toml::from_str(&raw)?)
    }

    pub fn index_files(&self) -> Result<BTreeMap<String, CommitFileState>> {
        let index = self.read_index()?;
        if index.has_conflicts() {
            return Err(RepoErr::UnresolvedConflicts);
        }

        let mut files = self.head_files()?;
        for entry in index.stage0_entries() {
            if let Some(file) = &entry.file {
                files.insert(entry.path.clone(), file.clone());
            } else if entry.artifact.is_some() {
                files.remove(&entry.path);
            } else {
                files.remove(&entry.path);
            }
        }
        Ok(files)
    }

    pub fn index_artifacts(&self) -> Result<BTreeMap<String, CommitArtifactState>> {
        let index = self.read_index()?;
        if index.has_conflicts() {
            return Err(RepoErr::UnresolvedConflicts);
        }

        let mut artifacts = self.head_artifacts()?;
        for entry in index.stage0_entries() {
            if let Some(artifact) = &entry.artifact {
                artifacts.insert(entry.path.clone(), artifact.clone());
            } else if entry.file.is_some() {
                artifacts.remove(&entry.path);
            } else {
                artifacts.remove(&entry.path);
            }
        }
        Ok(artifacts)
    }

    pub(super) fn files_for_worktree_status(
        &self,
        index: &index::Index,
    ) -> Result<BTreeMap<String, CommitFileState>> {
        let mut files = self.head_files()?;
        for entry in index.stage0_entries() {
            if let Some(file) = &entry.file {
                files.insert(entry.path.clone(), file.clone());
            } else if entry.artifact.is_some() {
                files.remove(&entry.path);
            } else {
                files.remove(&entry.path);
            }
        }
        Ok(files)
    }

    pub(super) fn artifacts_for_worktree_status(
        &self,
        index: &index::Index,
    ) -> Result<BTreeMap<String, CommitArtifactState>> {
        let mut artifacts = self.head_artifacts()?;
        for entry in index.stage0_entries() {
            if let Some(artifact) = &entry.artifact {
                artifacts.insert(entry.path.clone(), artifact.clone());
            } else if entry.file.is_some() {
                artifacts.remove(&entry.path);
            } else {
                artifacts.remove(&entry.path);
            }
        }
        Ok(artifacts)
    }

    pub(super) fn staged_changes_for_index(
        &self,
        index: &index::Index,
    ) -> Result<Vec<RepoStagedChange>> {
        let head_files = self.head_files()?;
        let head_artifacts = self.head_artifacts()?;
        let mut changes = Vec::new();

        for entry in index.stage0_entries() {
            let was_tracked =
                head_files.contains_key(&entry.path) || head_artifacts.contains_key(&entry.path);
            let (change, kind, storage) = if entry.file.is_some() {
                (
                    if was_tracked {
                        RepoFileChange::Modified
                    } else {
                        RepoFileChange::Added
                    },
                    RepoTrackedPathKind::SqliteDatabase,
                    RepoPathStorage::SqliteSnapshot,
                )
            } else if let Some(artifact) = &entry.artifact {
                (
                    if was_tracked {
                        RepoFileChange::Modified
                    } else {
                        RepoFileChange::Added
                    },
                    artifact_tracked_path_kind(artifact),
                    artifact_tracked_path_storage(artifact),
                )
            } else {
                let (kind, storage) = if head_files.contains_key(&entry.path) {
                    (
                        RepoTrackedPathKind::SqliteDatabase,
                        RepoPathStorage::SqliteSnapshot,
                    )
                } else if let Some(artifact) = head_artifacts.get(&entry.path) {
                    (
                        artifact_tracked_path_kind(artifact),
                        artifact_tracked_path_storage(artifact),
                    )
                } else {
                    (RepoTrackedPathKind::BinaryFile, RepoPathStorage::Inline)
                };
                (RepoFileChange::Deleted, kind, storage)
            };

            changes.push(RepoStagedChange {
                path: entry.path.clone(),
                change,
                kind,
                storage,
            });
        }

        Ok(changes)
    }

    pub(super) fn conflicted_changes_for_index(
        &self,
        index: &index::Index,
    ) -> Vec<RepoConflictChange> {
        fn kind_priority(kind: RepoTrackedPathKind) -> u8 {
            match kind {
                RepoTrackedPathKind::TextFile => 1,
                RepoTrackedPathKind::BinaryFile => 1,
                RepoTrackedPathKind::SqliteDatabase => 3,
            }
        }

        let mut by_path = BTreeMap::<String, (RepoTrackedPathKind, RepoPathStorage)>::new();

        for entry in index
            .entries
            .iter()
            .filter(|entry| entry.stage != index::IndexStage::Normal)
        {
            let (kind, storage) = if entry.file.is_some() {
                (
                    RepoTrackedPathKind::SqliteDatabase,
                    RepoPathStorage::SqliteSnapshot,
                )
            } else if let Some(artifact) = &entry.artifact {
                (
                    artifact_tracked_path_kind(artifact),
                    artifact_tracked_path_storage(artifact),
                )
            } else {
                (RepoTrackedPathKind::BinaryFile, RepoPathStorage::Inline)
            };
            by_path
                .entry(entry.path.clone())
                .and_modify(|existing| {
                    if kind_priority(kind) > kind_priority(existing.0) {
                        *existing = (kind, storage);
                    }
                })
                .or_insert((kind, storage));
        }

        by_path
            .into_iter()
            .map(|(path, (kind, storage))| RepoConflictChange { path, kind, storage })
            .collect()
    }

    pub(super) fn unstaged_changes_for_index(
        &self,
        index: &index::Index,
    ) -> Result<Vec<RepoWorktreeChange>> {
        let tracked = self.files_for_worktree_status(index)?;
        let tracked_artifacts = self.artifacts_for_worktree_status(index)?;
        let track_roots = self.track_roots()?;
        let state = self.read_worktree_state()?;
        let mut changes = BTreeMap::<
            String,
            (RepoWorktreeChangeKind, RepoTrackedPathKind, RepoPathStorage),
        >::new();
        for path in state.dirty {
            let (change, kind, storage) = if tracked.contains_key(&path) {
                (
                    RepoWorktreeChangeKind::Modified,
                    RepoTrackedPathKind::SqliteDatabase,
                    RepoPathStorage::SqliteSnapshot,
                )
            } else if let Some(artifact) = tracked_artifacts.get(&path) {
                (
                    RepoWorktreeChangeKind::Modified,
                    artifact_tracked_path_kind(artifact),
                    artifact_tracked_path_storage(artifact),
                )
            } else {
                if !track_roots.is_empty() && !config_path_patterns_match(&track_roots, &path) {
                    continue;
                }
                let (kind, storage) = self.worktree_path_descriptor(&path)?;
                (RepoWorktreeChangeKind::Untracked, kind, storage)
            };
            changes.insert(path, (change, kind, storage));
        }
        for path in state.deleted {
            if tracked.contains_key(&path) {
                changes.insert(
                    path,
                    (
                        RepoWorktreeChangeKind::Deleted,
                        RepoTrackedPathKind::SqliteDatabase,
                        RepoPathStorage::SqliteSnapshot,
                    ),
                );
            } else if let Some(artifact) = tracked_artifacts.get(&path) {
                changes.insert(
                    path,
                    (
                        RepoWorktreeChangeKind::Deleted,
                        artifact_tracked_path_kind(artifact),
                        artifact_tracked_path_storage(artifact),
                    ),
                );
            }
        }
        for (path, expected) in &tracked_artifacts {
            if changes.contains_key(path) {
                continue;
            }
            let physical_path = self.worktree.join(&path);
            if fs::symlink_metadata(&physical_path)
                .is_ok_and(|metadata| !metadata.file_type().is_file())
            {
                changes.insert(
                    path.clone(),
                    (
                        RepoWorktreeChangeKind::Deleted,
                        artifact_tracked_path_kind(expected),
                        artifact_tracked_path_storage(expected),
                    ),
                );
                continue;
            }
            match artifact_file_matches(&physical_path, expected)? {
                Some(true) => {}
                Some(false) => {
                    changes.insert(
                        path.clone(),
                        (
                            RepoWorktreeChangeKind::Modified,
                            artifact_tracked_path_kind(expected),
                            artifact_tracked_path_storage(expected),
                        ),
                    );
                }
                None => {
                    changes.insert(
                        path.clone(),
                        (
                            RepoWorktreeChangeKind::Deleted,
                            artifact_tracked_path_kind(expected),
                            artifact_tracked_path_storage(expected),
                        ),
                    );
                }
            }
        }
        for path in self.configured_untracked_paths_for_index(index)? {
            changes.entry(path.path).or_insert((
                RepoWorktreeChangeKind::Untracked,
                path.kind,
                path.storage,
            ));
        }
        Ok(changes
            .into_iter()
            .map(|(path, (change, kind, storage))| RepoWorktreeChange {
                path,
                change,
                kind,
                storage,
            })
            .collect())
    }

    pub(super) fn configured_untracked_paths_for_index(
        &self,
        index: &index::Index,
    ) -> Result<Vec<RepoTrackedPath>> {
        let paths = self.untracked_paths_for_index(index)?;
        let roots = self.track_roots()?;
        if roots.is_empty() {
            return Ok(paths);
        }
        Ok(paths
            .into_iter()
            .filter(|path| config_path_patterns_match(&roots, &path.path))
            .collect())
    }

    pub(super) fn untracked_paths_for_index(
        &self,
        index: &index::Index,
    ) -> Result<Vec<RepoTrackedPath>> {
        let tracked = self.files_for_worktree_status(index)?;
        let tracked_artifacts = self.artifacts_for_worktree_status(index)?;
        let mut paths = BTreeMap::<String, RepoTrackedPath>::new();

        for path in self.scan_untracked_sqlite_files()? {
            if !tracked.contains_key(&path) && !tracked_artifacts.contains_key(&path) {
                let size = self.worktree_path_size(&path)?;
                paths.insert(
                    path.clone(),
                    RepoTrackedPath {
                        path,
                        kind: RepoTrackedPathKind::SqliteDatabase,
                        storage: RepoPathStorage::SqliteSnapshot,
                        size,
                        page_count: None,
                    },
                );
            }
        }

        for path in self.scan_untracked_artifact_files()? {
            if !tracked.contains_key(&path) && !tracked_artifacts.contains_key(&path) {
                let (kind, storage) = self.worktree_path_descriptor(&path)?;
                let size = self.worktree_path_size(&path)?;
                paths.entry(path.clone()).or_insert(RepoTrackedPath {
                    path,
                    kind,
                    storage,
                    size,
                    page_count: None,
                });
            }
        }

        Ok(paths.into_values().collect())
    }

    pub(super) fn worktree_path_size(&self, key: &str) -> Result<Option<u64>> {
        match fs::metadata(self.worktree.join(key)) {
            Ok(metadata) => Ok(Some(metadata.len())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub(super) fn worktree_path_descriptor(
        &self,
        key: &str,
    ) -> Result<(RepoTrackedPathKind, RepoPathStorage)> {
        let path = self.worktree.join(key);
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok((RepoTrackedPathKind::BinaryFile, RepoPathStorage::External));
            }
            Err(err) => return Err(err.into()),
        };
        if is_sqlite_database_file(&path)? {
            return Ok((
                RepoTrackedPathKind::SqliteDatabase,
                RepoPathStorage::SqliteSnapshot,
            ));
        }
        let kind = classify_artifact_path(&path)?;
        let storage = artifact_storage_for_path(key, kind, metadata.len(), &self.file_config()?);
        Ok((kind, storage))
    }

    pub(super) fn scan_untracked_sqlite_files(&self) -> Result<Vec<String>> {
        let mut paths = BTreeSet::new();
        let ignore = self.ignore_rules()?;
        self.collect_sqlite_worktree_files(&self.worktree, &ignore, &mut paths)?;
        Ok(paths.into_iter().collect())
    }

    pub(super) fn scan_untracked_artifact_files(&self) -> Result<Vec<String>> {
        let mut paths = BTreeSet::new();
        let ignore = self.ignore_rules()?;
        self.collect_artifact_worktree_files(&self.worktree, &ignore, &mut paths)?;
        Ok(paths.into_iter().collect())
    }

    pub(super) fn collect_sqlite_worktree_files(
        &self,
        dir: &Path,
        ignore: &IgnoreRules,
        out: &mut BTreeSet<String>,
    ) -> Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                if entry.file_name() == GRAFT_DIR {
                    continue;
                }
                let key = self.worktree_key_for_path(&path)?;
                if ignore.is_ignored(&key, true) {
                    continue;
                }
                self.collect_sqlite_worktree_files(&path, ignore, out)?;
            } else if file_type.is_file() {
                let key = self.worktree_key_for_path(&path)?;
                if ignore.is_ignored(&key, false) {
                    continue;
                }
                if is_sqlite_database_file(&path)? {
                    out.insert(key);
                }
            }
        }
        Ok(())
    }

    pub(super) fn collect_artifact_worktree_files(
        &self,
        dir: &Path,
        ignore: &IgnoreRules,
        out: &mut BTreeSet<String>,
    ) -> Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                if entry.file_name() == GRAFT_DIR {
                    continue;
                }
                let key = self.worktree_key_for_path(&path)?;
                if ignore.is_ignored(&key, true) {
                    continue;
                }
                self.collect_artifact_worktree_files(&path, ignore, out)?;
            } else if file_type.is_file()
                && !is_sqlite_sidecar_file(&path)
                && !is_sqlite_database_file(&path)?
            {
                let key = self.worktree_key_for_path(&path)?;
                if ignore.is_ignored(&key, false) {
                    continue;
                }
                out.insert(key);
            }
        }
        Ok(())
    }

    pub(super) fn files_for_commit(
        &self,
        id: Option<&str>,
    ) -> Result<BTreeMap<String, CommitFileState>> {
        id.map(|id| self.read_commit(id).map(|commit| commit.files))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    pub(super) fn artifacts_for_commit(
        &self,
        id: Option<&str>,
    ) -> Result<BTreeMap<String, CommitArtifactState>> {
        id.map(|id| self.read_commit(id).map(|commit| commit.artifacts))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    pub(super) fn checkout_plan_for_target(&self, target: Option<String>) -> Result<CheckoutPlan> {
        let files = self.files_for_commit(target.as_deref())?;
        let artifacts = self.artifacts_for_commit(target.as_deref())?;
        Ok(CheckoutPlan { target, files, artifacts })
    }

    pub(super) fn stage_merge_conflict(
        &self,
        key: &str,
        base: Option<&CommitFileState>,
        ours: Option<&CommitFileState>,
        theirs: Option<&CommitFileState>,
        index: &mut index::Index,
    ) -> Result<()> {
        index.remove_path(key);
        for (stage, state) in [
            (index::IndexStage::Base, base),
            (index::IndexStage::Ours, ours),
            (index::IndexStage::Theirs, theirs),
        ] {
            if let Some(state) = state {
                index.stage(self.index_entry_for_state(key.to_string(), stage, state.clone())?);
            }
        }
        Ok(())
    }

    pub(super) fn stage_merge_artifact_conflict(
        &self,
        key: &str,
        base: Option<&CommitArtifactState>,
        ours: Option<&CommitArtifactState>,
        theirs: Option<&CommitArtifactState>,
        index: &mut index::Index,
    ) {
        index.remove_path(key);
        for (stage, state) in [
            (index::IndexStage::Base, base),
            (index::IndexStage::Ours, ours),
            (index::IndexStage::Theirs, theirs),
        ] {
            if let Some(state) = state {
                index.stage(self.index_entry_for_artifact_state(
                    key.to_string(),
                    stage,
                    state.clone(),
                ));
            }
        }
    }

    pub(super) fn write_index(&self, index: &index::Index) -> Result<()> {
        let path = self.index_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, toml::to_string_pretty(index)?)?;
        Ok(())
    }

    pub(super) fn clear_index(&self) -> Result<()> {
        let path = self.index_path();
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    pub(super) fn read_worktree_state(&self) -> Result<WorktreeState> {
        let path = self.worktree_state_path();
        if !path.exists() {
            return Ok(WorktreeState::default());
        }
        let raw = fs::read_to_string(path)?;
        let mut state: WorktreeState = toml::from_str(&raw)?;
        let dirty = state.dirty.into_iter().collect::<BTreeSet<_>>();
        state.dirty = dirty.into_iter().collect();
        let deleted = state.deleted.into_iter().collect::<BTreeSet<_>>();
        state.deleted = deleted.into_iter().collect();
        Ok(state)
    }

    pub(super) fn write_worktree_state(&self, state: &WorktreeState) -> Result<()> {
        let path = self.worktree_state_path();
        if state.dirty.is_empty() && state.deleted.is_empty() {
            if path.exists() {
                fs::remove_file(path)?;
            }
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_file_atomic(&path, toml::to_string_pretty(state)?.as_bytes())
    }
}
