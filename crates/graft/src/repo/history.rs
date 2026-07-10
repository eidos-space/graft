use super::*;

impl Repository {
    pub fn resolve_revision(&self, rev: &str) -> Result<String> {
        let rev = rev.trim();
        if rev.is_empty() {
            return Err(RepoErr::InvalidRevision(rev.to_string()));
        }

        let (base, ops) = split_revision_ops(rev)?;
        let mut id = self.resolve_revision_base(base)?;
        for op in ops {
            id = self.apply_revision_op(&id, op, rev)?;
        }
        Ok(id)
    }

    pub fn diff_revisions(&self, from: &str, to: &str, path: Option<&str>) -> Result<RepoDiff> {
        let from_id = self.resolve_revision(from)?;
        let to_id = self.resolve_revision(to)?;
        let from_commit = self.read_commit(&from_id)?;
        let to_commit = self.read_commit(&to_id)?;

        Ok(diff_repo_maps(
            from_id,
            to_id,
            &from_commit.files,
            &to_commit.files,
            &from_commit.artifacts,
            &to_commit.artifacts,
            path,
        ))
    }

    pub fn diff_staged(&self, path: Option<&str>) -> Result<RepoDiff> {
        let from = self.head_target()?.unwrap_or_else(|| "HEAD".to_string());
        let head_files = self.head_files()?;
        let index_files = self.index_files()?;
        let head_artifacts = self.head_artifacts()?;
        let index_artifacts = self.index_artifacts()?;
        Ok(diff_repo_maps(
            from,
            "index",
            &head_files,
            &index_files,
            &head_artifacts,
            &index_artifacts,
            path,
        ))
    }

    pub fn diff_worktree_file(
        &self,
        path: impl AsRef<Path>,
        state: CommitFileState,
        filter: Option<&str>,
    ) -> Result<RepoDiff> {
        let key = self.file_key(path)?;
        let mut worktree_files = self.index_files()?;
        worktree_files.insert(key.clone(), state);
        let mut worktree_artifacts = self.index_artifacts()?;
        worktree_artifacts.remove(&key);
        Ok(diff_repo_maps(
            "index",
            "worktree",
            &self.index_files()?,
            &worktree_files,
            &self.index_artifacts()?,
            &worktree_artifacts,
            filter,
        ))
    }

    pub fn diff_worktree_file_removal(
        &self,
        path: impl AsRef<Path>,
        filter: Option<&str>,
    ) -> Result<RepoDiff> {
        let mut worktree_files = self.index_files()?;
        worktree_files.remove(&self.file_key(path)?);
        Ok(diff_repo_maps(
            "index",
            "worktree",
            &self.index_files()?,
            &worktree_files,
            &self.index_artifacts()?,
            &self.index_artifacts()?,
            filter,
        ))
    }

    pub fn diff_worktree_artifact(
        &self,
        path: impl AsRef<Path>,
        filter: Option<&str>,
    ) -> Result<RepoDiff> {
        let key = self.file_key(path)?;
        let artifact = self.write_artifact_state_from_path(&key, &self.worktree.join(&key))?;
        let index_files = self.index_files()?;
        let mut worktree_files = index_files.clone();
        worktree_files.remove(&key);
        let index_artifacts = self.index_artifacts()?;
        let mut worktree_artifacts = index_artifacts.clone();
        worktree_artifacts.insert(key, artifact);
        Ok(diff_repo_maps(
            "index",
            "worktree",
            &index_files,
            &worktree_files,
            &index_artifacts,
            &worktree_artifacts,
            filter,
        ))
    }

    pub fn diff_worktree_artifact_removal(
        &self,
        path: impl AsRef<Path>,
        filter: Option<&str>,
    ) -> Result<RepoDiff> {
        let key = self.file_key(path)?;
        let index_files = self.index_files()?;
        let index_artifacts = self.index_artifacts()?;
        let mut worktree_artifacts = index_artifacts.clone();
        worktree_artifacts.remove(&key);
        Ok(diff_repo_maps(
            "index",
            "worktree",
            &index_files,
            &index_files,
            &index_artifacts,
            &worktree_artifacts,
            filter,
        ))
    }

    pub fn diff_revision_to_worktree_file(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
        state: CommitFileState,
        filter: Option<&str>,
    ) -> Result<RepoDiff> {
        let from_id = self.resolve_revision(rev)?;
        let from_commit = self.read_commit(&from_id)?;
        let from_files = from_commit.files;
        let from_artifacts = from_commit.artifacts;
        let mut worktree_files = from_files.clone();
        let key = self.file_key(path)?;
        worktree_files.insert(key.clone(), state);
        let mut worktree_artifacts = from_artifacts.clone();
        worktree_artifacts.remove(&key);
        Ok(diff_repo_maps(
            from_id,
            "worktree",
            &from_files,
            &worktree_files,
            &from_artifacts,
            &worktree_artifacts,
            filter,
        ))
    }

    pub fn diff_revision_to_worktree_file_removal(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
        filter: Option<&str>,
    ) -> Result<RepoDiff> {
        let from_id = self.resolve_revision(rev)?;
        let from_commit = self.read_commit(&from_id)?;
        let from_files = from_commit.files;
        let from_artifacts = from_commit.artifacts;
        let mut worktree_files = from_files.clone();
        worktree_files.remove(&self.file_key(path)?);
        Ok(diff_repo_maps(
            from_id,
            "worktree",
            &from_files,
            &worktree_files,
            &from_artifacts,
            &from_artifacts,
            filter,
        ))
    }

    pub fn diff_revision_to_worktree_artifact(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
        filter: Option<&str>,
    ) -> Result<RepoDiff> {
        let from_id = self.resolve_revision(rev)?;
        let from_commit = self.read_commit(&from_id)?;
        let from_files = from_commit.files;
        let from_artifacts = from_commit.artifacts;
        let key = self.file_key(path)?;
        let artifact = self.write_artifact_state_from_path(&key, &self.worktree.join(&key))?;
        let mut worktree_files = from_files.clone();
        worktree_files.remove(&key);
        let mut worktree_artifacts = from_artifacts.clone();
        worktree_artifacts.insert(key, artifact);
        Ok(diff_repo_maps(
            from_id,
            "worktree",
            &from_files,
            &worktree_files,
            &from_artifacts,
            &worktree_artifacts,
            filter,
        ))
    }

    pub fn diff_revision_to_worktree_artifact_removal(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
        filter: Option<&str>,
    ) -> Result<RepoDiff> {
        let from_id = self.resolve_revision(rev)?;
        let from_commit = self.read_commit(&from_id)?;
        let from_files = from_commit.files;
        let from_artifacts = from_commit.artifacts;
        let mut worktree_artifacts = from_artifacts.clone();
        worktree_artifacts.remove(&self.file_key(path)?);
        Ok(diff_repo_maps(
            from_id,
            "worktree",
            &from_files,
            &from_files,
            &from_artifacts,
            &worktree_artifacts,
            filter,
        ))
    }

    pub fn show_revision(&self, rev: &str) -> Result<CommitObject> {
        let id = self.resolve_revision(rev)?;
        self.read_commit(&id)
    }

    pub fn detach(&self, rev: &str) -> Result<String> {
        let plan = self.plan_detach(rev)?;
        self.apply_detach_plan(rev, &plan)
    }

    pub fn plan_detach(&self, rev: &str) -> Result<CheckoutPlan> {
        self.plan_revision_checkout(rev)
    }

    pub fn plan_revision_checkout(&self, rev: &str) -> Result<CheckoutPlan> {
        let id = self.resolve_revision(rev)?;
        self.checkout_plan_for_target(Some(id))
    }

    pub fn apply_detach_plan(&self, rev: &str, plan: &CheckoutPlan) -> Result<String> {
        let id = plan.target.clone().ok_or(RepoErr::UnbornHead)?;
        self.write_head_with_message(
            &Head::Detached { commit: id.clone() },
            &format!("checkout: moving to {rev}"),
        )?;
        Ok(id)
    }

    pub fn checkout_file_from_revision(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
    ) -> Result<CheckoutFileOutcome> {
        let path = self.file_key(path)?;
        self.checkout_file_key_from_revision(rev, path)
    }

    pub fn checkout_file_key_from_revision(
        &self,
        rev: &str,
        path: impl Into<String>,
    ) -> Result<CheckoutFileOutcome> {
        let plan = self.plan_checkout_file_key_from_revision(rev, path)?;
        self.apply_checkout_file_plan(&plan)
    }

    pub fn plan_checkout_file_from_revision(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
    ) -> Result<CheckoutFilePlan> {
        let path = self.file_key(path)?;
        self.plan_checkout_file_key_from_revision(rev, path)
    }

    pub fn plan_checkout_file_key_from_revision(
        &self,
        rev: &str,
        path: impl Into<String>,
    ) -> Result<CheckoutFilePlan> {
        let target = self.resolve_revision(rev)?;
        let path = normalize_repo_path_key(&path.into())?;
        let commit = self.read_commit(&target)?;
        let state =
            commit
                .files
                .get(&path)
                .cloned()
                .ok_or_else(|| RepoErr::PathNotFoundInRevision {
                    path: path.clone(),
                    rev: rev.to_string(),
                })?;
        let entry =
            self.index_entry_for_state(path.clone(), index::IndexStage::Normal, state.clone())?;
        Ok(CheckoutFilePlan { target, path, state, entry })
    }

    pub fn apply_checkout_file_plan(&self, plan: &CheckoutFilePlan) -> Result<CheckoutFileOutcome> {
        let mut index = self.read_index()?;
        index.stage(plan.entry.clone());
        self.write_index(&index)?;
        self.clear_dirty_key(&plan.path)?;
        Ok(CheckoutFileOutcome {
            target: plan.target.clone(),
            path: plan.path.clone(),
            state: plan.state.clone(),
        })
    }

    pub fn checkout_artifact_from_revision(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
    ) -> Result<CheckoutArtifactOutcome> {
        let path = self.file_key(path)?;
        self.checkout_artifact_key_from_revision(rev, path)
    }

    pub fn checkout_artifact_key_from_revision(
        &self,
        rev: &str,
        path: impl Into<String>,
    ) -> Result<CheckoutArtifactOutcome> {
        let plan = self.plan_checkout_artifact_key_from_revision(rev, path)?;
        self.apply_checkout_artifact_plan(&plan)
    }

    pub fn plan_checkout_artifact_from_revision(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
    ) -> Result<CheckoutArtifactPlan> {
        let path = self.file_key(path)?;
        self.plan_checkout_artifact_key_from_revision(rev, path)
    }

    pub fn plan_checkout_artifact_key_from_revision(
        &self,
        rev: &str,
        path: impl Into<String>,
    ) -> Result<CheckoutArtifactPlan> {
        let target = self.resolve_revision(rev)?;
        let path = normalize_repo_path_key(&path.into())?;
        let commit = self.read_commit(&target)?;
        let state = commit.artifacts.get(&path).cloned().ok_or_else(|| {
            RepoErr::PathNotFoundInRevision { path: path.clone(), rev: rev.to_string() }
        })?;
        let entry = self.index_entry_for_artifact_state(
            path.clone(),
            index::IndexStage::Normal,
            state.clone(),
        );
        Ok(CheckoutArtifactPlan { target, path, state, entry })
    }

    pub fn apply_checkout_artifact_plan(
        &self,
        plan: &CheckoutArtifactPlan,
    ) -> Result<CheckoutArtifactOutcome> {
        let mut index = self.read_index()?;
        index.stage(plan.entry.clone());
        self.write_index(&index)?;
        self.clear_dirty_key(&plan.path)?;
        Ok(CheckoutArtifactOutcome {
            target: plan.target.clone(),
            path: plan.path.clone(),
            state: plan.state.clone(),
        })
    }

    pub fn reset(&self, rev: &str, mode: ResetMode) -> Result<ResetOutcome> {
        let plan = self.plan_reset(rev, mode)?;
        self.apply_reset_plan(&plan)
    }

    pub fn plan_reset(&self, rev: &str, mode: ResetMode) -> Result<ResetPlan> {
        let target = self.resolve_revision(rev)?;
        let checkout = self.checkout_plan_for_target(Some(target.clone()))?;
        Ok(ResetPlan {
            rev: rev.to_string(),
            target,
            mode,
            checkout,
        })
    }

    pub fn apply_reset_plan(&self, plan: &ResetPlan) -> Result<ResetOutcome> {
        self.move_head_to(&plan.target, &format!("reset: moving to {}", plan.rev))?;
        match plan.mode {
            ResetMode::Soft => {}
            ResetMode::Mixed => self.clear_index()?,
            ResetMode::Hard => {
                self.clear_index()?;
                self.clear_dirty()?;
            }
        }
        self.clear_merge_state()?;
        Ok(ResetOutcome {
            target: plan.target.clone(),
            mode: plan.mode,
        })
    }
}

impl Repository {
    pub(super) fn write_tree_object(
        &self,
        object_store: &object::LooseObjectStore,
        files: &BTreeMap<String, CommitFileState>,
        artifacts: &BTreeMap<String, CommitArtifactState>,
    ) -> Result<object::ObjectId> {
        let mut entries = Vec::with_capacity(files.len() + artifacts.len());
        for (path, state) in files {
            let blob = object::Object::Blob(object::BlobObject::SqliteSnapshot(
                sqlite_snapshot_blob(state),
            ));
            let oid = object_store.write(&blob)?;
            entries.push(object::TreeEntry {
                mode: object::TreeEntryMode::SqliteDatabase,
                oid,
                path: path.clone(),
            });
        }
        for (path, state) in artifacts {
            entries.push(object::TreeEntry {
                mode: object::TreeEntryMode::Regular,
                oid: state.oid().clone(),
                path: path.clone(),
            });
        }
        let tree = object::TreeObject::new(entries)?;
        Ok(object_store.write(&object::Object::Tree(tree))?)
    }

    pub(super) fn canonical_commit_object(
        &self,
        tree: object::ObjectId,
        parents: &[String],
        message: &str,
        timestamp_ms: u64,
        tables: Vec<CommitTableSummary>,
    ) -> Result<object::CommitObject> {
        let parents = parents
            .iter()
            .map(|parent| object::ObjectId::from_str(parent))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let signature =
            object::Signature::new("Graft", "graft@example.invalid", timestamp_ms, "+0000");
        Ok(object::CommitObject {
            tree,
            parents,
            author: signature.clone(),
            committer: signature,
            repo_format_version: REPOSITORY_FORMAT_VERSION,
            tables,
            message: message.to_string(),
        })
    }
}
