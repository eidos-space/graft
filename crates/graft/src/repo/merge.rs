use super::*;

impl Repository {
    pub fn merge_revision(&self, rev: &str) -> Result<MergeOutcome> {
        let plan = self.plan_merge_revision(rev)?;
        self.apply_merge_plan(&plan)
    }

    pub fn plan_merge_revision(&self, rev: &str) -> Result<MergePlan> {
        if self.merge_head()?.is_some() {
            return Err(RepoErr::MergeInProgress);
        }
        let target = self.resolve_revision(rev)?;
        let checkout = self.checkout_plan_for_target(Some(target.clone()))?;
        let head = self.head_target()?;

        let Some(head) = head else {
            let outcome = MergeOutcome::FastForward { from: None, to: target.clone() };
            return Ok(MergePlan {
                rev: rev.to_string(),
                target,
                checkout,
                outcome,
                index: None,
            });
        };

        if self.is_ancestor(&target, &head)? {
            let outcome = MergeOutcome::AlreadyUpToDate { head };
            return Ok(MergePlan {
                rev: rev.to_string(),
                target,
                checkout,
                outcome,
                index: None,
            });
        }

        if self.is_ancestor(&head, &target)? {
            let outcome = MergeOutcome::FastForward { from: Some(head), to: target.clone() };
            return Ok(MergePlan {
                rev: rev.to_string(),
                target,
                checkout,
                outcome,
                index: None,
            });
        }

        let merge_base = self.merge_base(&head, &target)?;
        let base_files = self.files_for_commit(merge_base.as_deref())?;
        let ours_files = self.files_for_commit(Some(&head))?;
        let theirs_files = self.files_for_commit(Some(&target))?;
        let base_artifacts = self.artifacts_for_commit(merge_base.as_deref())?;
        let ours_artifacts = self.artifacts_for_commit(Some(&head))?;
        let theirs_artifacts = self.artifacts_for_commit(Some(&target))?;
        let mut index = self.read_index()?;
        let mut staged = Vec::new();
        let mut conflicted = Vec::new();

        let mut keys = BTreeMap::<String, ()>::new();
        for key in base_files
            .keys()
            .chain(ours_files.keys())
            .chain(theirs_files.keys())
        {
            keys.insert(key.clone(), ());
        }

        for key in keys.keys() {
            let base = base_files.get(key);
            let ours = ours_files.get(key);
            let theirs = theirs_files.get(key);

            if ours == theirs || base == theirs {
                continue;
            }

            if base == ours {
                index.remove_path(key);
                if let Some(theirs) = theirs {
                    index.stage(self.index_entry_for_state(
                        key.clone(),
                        index::IndexStage::Normal,
                        theirs.clone(),
                    )?);
                } else {
                    index.stage(index::IndexEntry {
                        path: key.clone(),
                        mode: None,
                        oid: None,
                        stage: index::IndexStage::Normal,
                        file: None,
                        artifact: None,
                    });
                }
                staged.push(key.clone());
                continue;
            }

            self.stage_merge_conflict(key, base, ours, theirs, &mut index)?;
            conflicted.push(key.clone());
        }

        let mut artifact_keys = BTreeMap::<String, ()>::new();
        for key in base_artifacts
            .keys()
            .chain(ours_artifacts.keys())
            .chain(theirs_artifacts.keys())
        {
            artifact_keys.insert(key.clone(), ());
        }

        for key in artifact_keys.keys() {
            let base = base_artifacts.get(key);
            let ours = ours_artifacts.get(key);
            let theirs = theirs_artifacts.get(key);

            if ours == theirs || base == theirs {
                continue;
            }

            if base == ours {
                index.remove_path(key);
                if let Some(theirs) = theirs {
                    index.stage(self.index_entry_for_artifact_state(
                        key.clone(),
                        index::IndexStage::Normal,
                        theirs.clone(),
                    ));
                } else {
                    index.stage(index::IndexEntry {
                        path: key.clone(),
                        mode: None,
                        oid: None,
                        stage: index::IndexStage::Normal,
                        file: None,
                        artifact: None,
                    });
                }
                staged.push(key.clone());
                continue;
            }

            self.stage_merge_artifact_conflict(key, base, ours, theirs, &mut index);
            conflicted.push(key.clone());
        }

        let outcome = MergeOutcome::Merged {
            head,
            target: target.clone(),
            merge_base,
            staged,
            conflicted,
        };
        Ok(MergePlan {
            rev: rev.to_string(),
            target,
            checkout,
            outcome,
            index: Some(index),
        })
    }

    pub fn apply_merge_plan(&self, plan: &MergePlan) -> Result<MergeOutcome> {
        if self.merge_head()?.is_some() {
            return Err(RepoErr::MergeInProgress);
        }

        match &plan.outcome {
            MergeOutcome::FastForward { to, .. } => {
                self.move_head_to(to, &format!("merge {}: fast-forward", plan.rev))?;
            }
            MergeOutcome::AlreadyUpToDate { .. } => {}
            MergeOutcome::Merged { head, target, .. } => {
                let index = plan.index.as_ref().ok_or(RepoErr::UnresolvedConflicts)?;
                self.write_index(index)?;
                self.write_merge_state(head, target)?;
            }
        }

        Ok(plan.outcome.clone())
    }

    pub fn merge_abort(&self) -> Result<String> {
        let plan = self.plan_merge_abort()?;
        self.apply_merge_abort_plan(&plan)
    }

    pub fn plan_merge_abort(&self) -> Result<MergeAbortPlan> {
        let target = self.orig_head()?.ok_or(RepoErr::NoMergeInProgress)?;
        let checkout = self.checkout_plan_for_target(Some(target.clone()))?;
        Ok(MergeAbortPlan { target, checkout })
    }

    pub fn apply_merge_abort_plan(&self, plan: &MergeAbortPlan) -> Result<String> {
        if self.orig_head()?.is_none() && self.merge_head()?.is_none() {
            return Err(RepoErr::NoMergeInProgress);
        }
        self.move_head_to(&plan.target, "merge: abort")?;
        self.clear_index()?;
        self.clear_dirty()?;
        self.clear_merge_state()?;
        Ok(plan.target.clone())
    }
}
