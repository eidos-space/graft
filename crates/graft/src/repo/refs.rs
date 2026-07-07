use super::*;

impl Repository {
    pub fn branches(&self) -> Result<Vec<BranchInfo>> {
        let config = self.config()?;
        let head = self.head()?;
        let current = head.branch_name();
        let mut branches = BTreeMap::<String, Option<String>>::new();

        Self::collect_ref_files(&self.graft_dir.join(DIR_REFS_HEADS), "", &mut branches)?;

        if let Some(current) = current
            && !branches.contains_key(current)
        {
            branches.insert(current.to_string(), None);
        }

        branches
            .into_iter()
            .map(|(name, target)| {
                let upstream = branch_upstream_from_config(&config, &name)?;
                Ok(BranchInfo {
                    current: current == Some(name.as_str()),
                    name,
                    target,
                    upstream,
                })
            })
            .collect::<Result<Vec<_>>>()
    }

    pub fn remote_tracking_branches(&self) -> Result<Vec<RemoteBranchRef>> {
        let mut refs = BTreeMap::<String, Option<String>>::new();
        Self::collect_ref_files(&self.graft_dir.join(DIR_REFS_REMOTES), "", &mut refs)?;

        let mut branches = Vec::new();
        for (name, target) in refs {
            let Some((remote, branch)) = name.split_once('/') else {
                continue;
            };
            validate_remote_name(remote)?;
            validate_ref_name(branch)?;
            let Some(head) = target else {
                continue;
            };
            branches.push(RemoteBranchRef {
                remote: remote.to_string(),
                branch: branch.to_string(),
                head,
            });
        }
        Ok(branches)
    }

    pub fn branch_create(&self, name: &str, start_point: Option<&str>) -> Result<BranchInfo> {
        validate_ref_name(name)?;
        if self.branch_exists(name) {
            return Err(RepoErr::BranchExists(name.to_string()));
        }

        let target = match start_point {
            Some(target) => self.resolve_revision(target)?,
            None => self.head_target()?.ok_or(RepoErr::UnbornHead)?,
        };

        self.write_branch_ref(name, &target, "branch: create")?;
        Ok(BranchInfo {
            name: name.to_string(),
            target: Some(target),
            current: self
                .head()
                .ok()
                .and_then(|head| head.branch_name().map(str::to_string))
                == Some(name.to_string()),
            upstream: self.branch_upstream(name)?,
        })
    }

    pub fn branch_create_unborn(&self, name: &str) -> Result<BranchInfo> {
        validate_ref_name(name)?;
        if self.branch_exists(name) {
            return Err(RepoErr::BranchExists(name.to_string()));
        }
        self.write_ref_update(&format!("refs/heads/{name}"), "", "branch: create unborn")?;
        Ok(BranchInfo {
            name: name.to_string(),
            target: None,
            current: false,
            upstream: self.branch_upstream(name)?,
        })
    }

    pub fn branch_delete(&self, name: &str, force: bool) -> Result<BranchInfo> {
        validate_ref_name(name)?;
        if self.current_branch()?.as_deref() == Some(name) {
            return Err(RepoErr::BranchIsCurrent(name.to_string()));
        }

        if !self.branch_exists(name) {
            return Err(RepoErr::BranchNotFound(name.to_string()));
        }
        let target = self.read_branch_ref(name)?;

        if !force && let Some(target) = &target {
            let merged = if let Some(head) = self.head_target()? {
                self.is_ancestor(target, &head)?
            } else {
                false
            };
            if !merged {
                return Err(RepoErr::BranchNotMerged {
                    branch: name.to_string(),
                    target: target.clone(),
                });
            }
        }

        self.delete_ref(&format!("refs/heads/{name}"))?;
        self.delete_ref_log(&format!("refs/heads/{name}"))?;
        let mut repo_config = self.config()?;
        repo_config.branches.remove(name);
        self.write_config(&repo_config)?;
        Ok(BranchInfo {
            name: name.to_string(),
            target,
            current: false,
            upstream: None,
        })
    }

    pub fn branch_rename(&self, old: &str, new: &str, force: bool) -> Result<BranchInfo> {
        validate_ref_name(old)?;
        validate_ref_name(new)?;

        if old == new {
            return self.branch_info(old);
        }

        let current = self.current_branch()?;
        let old_is_current = current.as_deref() == Some(old);
        let new_is_current = current.as_deref() == Some(new);
        let old_exists = self.branch_exists(old);
        if !old_exists && !old_is_current {
            return Err(RepoErr::BranchNotFound(old.to_string()));
        }
        if new_is_current {
            return Err(RepoErr::BranchIsCurrent(new.to_string()));
        }

        let new_exists = self.branch_exists(new);
        if new_exists && !force {
            return Err(RepoErr::BranchExists(new.to_string()));
        }

        let old_ref = format!("refs/heads/{old}");
        let new_ref = format!("refs/heads/{new}");
        let target = if old_exists {
            self.read_branch_ref(old)?
        } else {
            None
        };
        let target_raw = target.as_deref().unwrap_or("");
        let message = format!("branch: renamed {old} to {new}");

        let mut repo_config = self.config()?;
        let old_branch_config = repo_config.branches.remove(old);
        if force {
            repo_config.branches.remove(new);
        }
        if let Some(old_branch_config) = old_branch_config {
            repo_config
                .branches
                .insert(new.to_string(), old_branch_config);
        }

        Self::ensure_path_namespace_available_for_rename(&self.graft_dir, &old_ref, &new_ref)?;
        let reflog_root = self.graft_dir.join(DIR_LOGS_REFS);
        if reflog_root.join(&old_ref).is_file() {
            Self::ensure_path_namespace_available_for_rename(&reflog_root, &old_ref, &new_ref)?;
        }

        if new_exists {
            self.delete_ref(&new_ref)?;
            self.delete_ref_log(&new_ref)?;
        }
        if old_exists {
            self.delete_ref(&old_ref)?;
        }

        self.ensure_ref_namespace_available(&new_ref)?;
        self.move_ref_log_for_rename(&old_ref, &new_ref)?;
        self.write_ref(&new_ref, target_raw)?;
        self.append_ref_reflog(&new_ref, target.as_deref(), target.as_deref(), &message)?;

        if old_is_current {
            write_file_atomic(&self.head_path(), Head::branch(new).serialize().as_bytes())?;
            self.append_head_reflog(target.as_deref(), target.as_deref(), &message)?;
        }

        self.write_config(&repo_config)?;
        self.branch_info(new)
    }

    pub fn switch_branch(&self, name: &str) -> Result<()> {
        let plan = self.plan_switch_branch(name)?;
        self.apply_switch_branch_plan(name, &plan)
    }

    pub fn plan_switch_branch(&self, name: &str) -> Result<CheckoutPlan> {
        validate_ref_name(name)?;

        let default_branch = self.config()?.core.default_branch;
        let target = self.read_branch_ref(name)?;
        if target.is_none() && name != default_branch && !self.branch_exists(name) {
            return Err(RepoErr::BranchNotFound(name.to_string()));
        }

        self.checkout_plan_for_target(target)
    }

    pub fn apply_switch_branch_plan(&self, name: &str, _plan: &CheckoutPlan) -> Result<()> {
        validate_ref_name(name)?;
        self.write_head_with_message(&Head::branch(name), &format!("checkout: moving to {name}"))
    }

    pub fn switch_new_branch(&self, name: &str, start_point: Option<&str>) -> Result<BranchInfo> {
        let plan = self.plan_switch_new_branch(name, start_point)?;
        self.apply_switch_new_branch_plan(&plan)
    }

    pub fn plan_switch_new_branch(
        &self,
        name: &str,
        start_point: Option<&str>,
    ) -> Result<SwitchNewBranchPlan> {
        validate_ref_name(name)?;
        if self.branch_exists(name) {
            return Err(RepoErr::BranchExists(name.to_string()));
        }
        self.ensure_ref_namespace_available(&format!("refs/heads/{name}"))?;

        let target = match start_point {
            Some(target) => Some(self.resolve_revision(target)?),
            None => self.head_target()?,
        };
        let checkout = self.checkout_plan_for_target(target.clone())?;
        let branch = BranchInfo {
            name: name.to_string(),
            target,
            current: true,
            upstream: self.branch_upstream(name)?,
        };
        Ok(SwitchNewBranchPlan { branch, checkout })
    }

    pub fn apply_switch_new_branch_plan(&self, plan: &SwitchNewBranchPlan) -> Result<BranchInfo> {
        if let Some(target) = &plan.branch.target {
            self.write_branch_ref(&plan.branch.name, target, "branch: create")?;
        } else {
            self.write_ref_update(
                &format!("refs/heads/{}", plan.branch.name),
                "",
                "branch: create unborn",
            )?;
        }
        self.write_head_with_message(
            &Head::branch(plan.branch.name.clone()),
            &format!("checkout: moving to {}", plan.branch.name),
        )?;
        Ok(plan.branch.clone())
    }

    pub fn tags(&self) -> Result<Vec<TagInfo>> {
        let mut tags = BTreeMap::<String, Option<String>>::new();
        Self::collect_ref_files(&self.graft_dir.join(DIR_REFS_TAGS), "", &mut tags)?;
        tags.into_iter()
            .filter_map(|(name, target)| target.map(|target| self.tag_info_from_ref(name, target)))
            .collect()
    }

    pub fn tag_create(&self, name: &str, target: Option<&str>) -> Result<TagInfo> {
        validate_ref_name(name)?;
        if self.tag_exists(name) {
            return Err(RepoErr::TagExists(name.to_string()));
        }

        let target = match target {
            Some(target) => self.resolve_revision(target)?,
            None => self.head_target()?.ok_or(RepoErr::UnbornHead)?,
        };

        self.write_tag_ref(name, &target, "tag: create")?;
        Ok(TagInfo {
            name: name.to_string(),
            object: target.clone(),
            target,
            annotated: false,
            message: None,
        })
    }

    pub fn tag_create_annotated(
        &self,
        name: &str,
        target: Option<&str>,
        message: impl Into<String>,
    ) -> Result<TagInfo> {
        validate_ref_name(name)?;
        if self.tag_exists(name) {
            return Err(RepoErr::TagExists(name.to_string()));
        }

        let target = match target {
            Some(target) => self.resolve_revision(target)?,
            None => self.head_target()?.ok_or(RepoErr::UnbornHead)?,
        };
        let target_id = object::ObjectId::from_str(&target)?;
        let message = message.into();
        let tag_object = object::TagObject {
            object: target_id,
            object_type: object::ObjectKind::Commit,
            name: name.to_string(),
            tagger: object::Signature::new("Graft", "graft@example.invalid", now_ms(), "+0000"),
            message: message.clone(),
        };
        let object = self
            .object_store()
            .write(&object::Object::Tag(tag_object))?;
        let object = object.to_string();

        self.write_tag_ref(name, &object, "tag: create annotated")?;
        Ok(TagInfo {
            name: name.to_string(),
            object,
            target,
            annotated: true,
            message: Some(message),
        })
    }

    pub fn tag_delete(&self, name: &str) -> Result<TagInfo> {
        validate_ref_name(name)?;
        let object = self
            .read_tag_ref(name)?
            .ok_or_else(|| RepoErr::TagNotFound(name.to_string()))?;
        let tag = self.tag_info_from_ref(name.to_string(), object)?;
        self.delete_tag_ref(name)?;
        self.delete_ref_log(&format!("refs/tags/{name}"))?;
        Ok(tag)
    }

    pub fn remote_add(&self, name: &str, config: RemoteConfig) -> Result<RemoteInfo> {
        validate_remote_name(name)?;
        let mut repo_config = self.config()?;
        if repo_config.remotes.contains_key(name) {
            return Err(RepoErr::RemoteExists(name.to_string()));
        }
        repo_config.remotes.insert(name.to_string(), config.clone());
        self.write_config(&repo_config)?;
        fs::create_dir_all(self.graft_dir.join(DIR_REFS_REMOTES).join(name))?;
        Ok(RemoteInfo { name: name.to_string(), config })
    }

    pub fn remote_remove(&self, name: &str) -> Result<RemoteInfo> {
        validate_remote_name(name)?;
        let mut repo_config = self.config()?;
        let Some(config) = repo_config.remotes.remove(name) else {
            return Err(RepoErr::RemoteNotFound(name.to_string()));
        };
        repo_config
            .branches
            .retain(|_, branch| branch.remote.as_deref() != Some(name));
        self.write_config(&repo_config)?;

        remove_path_if_exists(self.graft_dir.join(DIR_REFS_REMOTES).join(name))?;
        remove_path_if_exists(
            self.graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs")
                .join("remotes")
                .join(name),
        )?;
        Ok(RemoteInfo { name: name.to_string(), config })
    }

    pub fn remote_rename(&self, old: &str, new: &str) -> Result<RemoteInfo> {
        validate_remote_name(old)?;
        validate_remote_name(new)?;
        if old == new {
            let config = self
                .config()?
                .remotes
                .remove(old)
                .ok_or_else(|| RepoErr::RemoteNotFound(old.to_string()))?;
            return Ok(RemoteInfo { name: new.to_string(), config });
        }

        let mut repo_config = self.config()?;
        let Some(config) = repo_config.remotes.remove(old) else {
            return Err(RepoErr::RemoteNotFound(old.to_string()));
        };
        if repo_config.remotes.contains_key(new) {
            return Err(RepoErr::RemoteExists(new.to_string()));
        }
        if self.graft_dir.join(DIR_REFS_REMOTES).join(new).exists()
            || self
                .graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs")
                .join("remotes")
                .join(new)
                .exists()
        {
            return Err(RepoErr::RemoteExists(new.to_string()));
        }

        for branch in repo_config.branches.values_mut() {
            if branch.remote.as_deref() == Some(old) {
                branch.remote = Some(new.to_string());
            }
        }
        repo_config.remotes.insert(new.to_string(), config.clone());
        self.write_config(&repo_config)?;

        move_path_if_exists(
            self.graft_dir.join(DIR_REFS_REMOTES).join(old),
            self.graft_dir.join(DIR_REFS_REMOTES).join(new),
        )?;
        move_path_if_exists(
            self.graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs")
                .join("remotes")
                .join(old),
            self.graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs")
                .join("remotes")
                .join(new),
        )?;

        Ok(RemoteInfo { name: new.to_string(), config })
    }

    pub fn remote_get_url(&self, name: &str) -> Result<RemoteInfo> {
        validate_remote_name(name)?;
        let config = self
            .config()?
            .remotes
            .get(name)
            .cloned()
            .ok_or_else(|| RepoErr::RemoteNotFound(name.to_string()))?;
        Ok(RemoteInfo { name: name.to_string(), config })
    }

    pub fn remote_set_url(&self, name: &str, config: RemoteConfig) -> Result<RemoteInfo> {
        validate_remote_name(name)?;
        let mut repo_config = self.config()?;
        let Some(remote_config) = repo_config.remotes.get_mut(name) else {
            return Err(RepoErr::RemoteNotFound(name.to_string()));
        };
        *remote_config = config.clone();
        self.write_config(&repo_config)?;
        Ok(RemoteInfo { name: name.to_string(), config })
    }

    pub fn remotes(&self) -> Result<Vec<RemoteInfo>> {
        Ok(self
            .config()?
            .remotes
            .into_iter()
            .map(|(name, config)| RemoteInfo { name, config })
            .collect())
    }

    pub fn set_remote_tracking_ref(&self, remote: &str, branch: &str, target: &str) -> Result<()> {
        validate_remote_name(remote)?;
        validate_ref_name(branch)?;
        self.write_ref_update(
            &format!("refs/remotes/{remote}/{branch}"),
            target,
            &format!("fetch {remote}/{branch}"),
        )
    }

    pub fn remote_tracking_ref(&self, remote: &str, branch: &str) -> Result<Option<String>> {
        validate_remote_name(remote)?;
        validate_ref_name(branch)?;
        self.read_ref(&format!("refs/remotes/{remote}/{branch}"))
    }

    pub fn remote_default_branch(&self, remote: &str) -> Result<Option<String>> {
        validate_remote_name(remote)?;
        let remote_store = self.remote_store(remote)?;
        let Some(head) = block_on_remote(remote_store.get_raw(HEAD_FILE))? else {
            return Ok(None);
        };
        parse_remote_head_branch(HEAD_FILE, head)
    }

    pub fn remote_branch_refs(&self, remote: &str) -> Result<Vec<RemoteBranchRef>> {
        validate_remote_name(remote)?;
        let remote_store = self.remote_store(remote)?;
        self.remote_branch_refs_from_store(remote, &remote_store)
    }

    pub fn remote_branch_head(&self, remote: &str, branch: &str) -> Result<Option<String>> {
        Ok(self.remote_branch_head_state(remote, branch)?.head)
    }

    pub fn remote_branch_head_state(&self, remote: &str, branch: &str) -> Result<RemoteBranchHead> {
        validate_remote_name(remote)?;
        validate_ref_name(branch)?;
        let remote_store = self.remote_store(remote)?;
        Self::remote_branch_head_from_store(&remote_store, branch)
    }

    pub(super) fn remote_branch_head_from_store(
        remote_store: &crate::remote::Remote,
        branch: &str,
    ) -> Result<RemoteBranchHead> {
        let head_path = format!("refs/heads/{branch}");
        let raw = block_on_remote(remote_store.get_raw(&head_path))?;
        let head = raw
            .as_ref()
            .map(|bytes| parse_remote_ref(&head_path, bytes.clone()))
            .transpose()?;
        Ok(RemoteBranchHead { raw, head })
    }

    pub fn remote_prune(&self, remote: &str) -> Result<RemotePruneOutcome> {
        validate_remote_name(remote)?;
        let remote_store = self.remote_store(remote)?;
        let remote_branches = self
            .remote_branch_refs_from_store(remote, &remote_store)?
            .into_iter()
            .map(|reference| reference.branch)
            .collect::<BTreeSet<_>>();
        let mut local_tracking = BTreeMap::<String, Option<String>>::new();
        Self::collect_ref_files(
            &self.graft_dir.join(DIR_REFS_REMOTES).join(remote),
            "",
            &mut local_tracking,
        )?;

        let mut branches = Vec::new();
        for branch in local_tracking.keys() {
            validate_ref_name(branch)?;
            if remote_branches.contains(branch) {
                continue;
            }
            let reference = format!("refs/remotes/{remote}/{branch}");
            self.delete_ref_if_exists(&reference)?;
            self.delete_ref_log(&reference)?;
            branches.push(branch.clone());
        }

        Ok(RemotePruneOutcome { remote: remote.to_string(), branches })
    }

    pub fn current_branch(&self) -> Result<Option<String>> {
        Ok(self.head()?.branch_name().map(ToString::to_string))
    }

    pub fn default_branch(&self) -> Result<String> {
        Ok(self.config()?.core.default_branch)
    }

    pub fn branch_target(&self, branch: &str) -> Result<Option<String>> {
        validate_ref_name(branch)?;
        self.read_branch_ref(branch)
    }

    pub fn branch_upstream(&self, branch: &str) -> Result<Option<BranchUpstream>> {
        validate_ref_name(branch)?;
        branch_upstream_from_config(&self.config()?, branch)
    }

    pub fn set_branch_upstream(
        &self,
        branch: &str,
        remote: &str,
        remote_branch: &str,
    ) -> Result<BranchInfo> {
        self.ensure_local_branch_for_config(branch)?;
        validate_remote_name(remote)?;
        validate_ref_name(remote_branch)?;

        let mut repo_config = self.config()?;
        if !repo_config.remotes.contains_key(remote) {
            return Err(RepoErr::RemoteNotFound(remote.to_string()));
        }

        repo_config.branches.insert(
            branch.to_string(),
            BranchConfig {
                remote: Some(remote.to_string()),
                merge: Some(branch_merge_ref(remote_branch)),
            },
        );
        self.write_config(&repo_config)?;
        self.branch_info(branch)
    }

    pub fn unset_branch_upstream(&self, branch: &str) -> Result<BranchInfo> {
        self.ensure_local_branch_for_config(branch)?;
        let mut repo_config = self.config()?;
        repo_config.branches.remove(branch);
        self.write_config(&repo_config)?;
        self.branch_info(branch)
    }

    pub fn default_remote_branch(
        &self,
        remote: Option<&str>,
        branch: Option<&str>,
    ) -> Result<BranchUpstream> {
        if let Some(remote) = remote {
            validate_remote_name(remote)?;
        }
        if let Some(branch) = branch {
            validate_ref_name(branch)?;
        }

        let current_branch = self.current_branch()?;
        let current_upstream = current_branch
            .as_deref()
            .map(|branch| self.branch_upstream(branch))
            .transpose()?
            .flatten();

        let resolved_remote = remote
            .map(ToString::to_string)
            .or_else(|| {
                current_upstream
                    .as_ref()
                    .map(|upstream| upstream.remote.clone())
            })
            .unwrap_or_else(|| "origin".to_string());
        let resolved_branch = branch
            .map(ToString::to_string)
            .or_else(|| {
                if remote.is_none() {
                    current_upstream
                        .as_ref()
                        .map(|upstream| upstream.branch.clone())
                } else {
                    None
                }
            })
            .or(current_branch)
            .unwrap_or_else(|| self.default_branch().unwrap_or_else(|_| "main".to_string()));

        Ok(BranchUpstream {
            remote: resolved_remote,
            branch: resolved_branch,
        })
    }
}

impl Repository {
    pub fn head(&self) -> Result<Head> {
        let raw = fs::read_to_string(self.head_path())?;
        Head::parse(&raw)
    }

    pub fn write_head(&self, head: &Head) -> Result<()> {
        self.write_head_with_message(head, "HEAD update")
    }

    pub(super) fn write_head_with_message(&self, head: &Head, message: &str) -> Result<()> {
        if let Head::Branch { name } = head {
            validate_ref_name(name)?;
        }
        let old = self.current_head_for_reflog()?;
        let old_target = old
            .as_ref()
            .map(|head| self.head_reflog_target(head))
            .transpose()?
            .flatten();
        let new_target = self.head_reflog_target(head)?;
        write_file_atomic(&self.head_path(), head.serialize().as_bytes())?;
        self.append_head_reflog(old_target.as_deref(), new_target.as_deref(), message)?;
        Ok(())
    }
}

impl Repository {
    pub(super) fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let mut stack = vec![descendant.to_string()];
        let mut seen = BTreeMap::<String, ()>::new();
        while let Some(id) = stack.pop() {
            if seen.insert(id.clone(), ()).is_some() {
                continue;
            }
            if id == ancestor {
                return Ok(true);
            }
            for parent in commit_parent_ids(&self.read_commit(&id)?) {
                stack.push(parent);
            }
        }
        Ok(false)
    }

    pub(super) fn merge_base(&self, left: &str, right: &str) -> Result<Option<String>> {
        let mut left_ancestors = BTreeMap::<String, ()>::new();
        let mut stack = vec![left.to_string()];
        while let Some(id) = stack.pop() {
            if left_ancestors.insert(id.clone(), ()).is_some() {
                continue;
            }
            for parent in commit_parent_ids(&self.read_commit(&id)?) {
                stack.push(parent);
            }
        }

        let mut stack = vec![right.to_string()];
        let mut seen = BTreeMap::<String, ()>::new();
        while let Some(id) = stack.pop() {
            if seen.insert(id.clone(), ()).is_some() {
                continue;
            }
            if left_ancestors.contains_key(&id) {
                return Ok(Some(id));
            }
            for parent in commit_parent_ids(&self.read_commit(&id)?) {
                stack.push(parent);
            }
        }

        Ok(None)
    }

    pub(super) fn head_files(&self) -> Result<BTreeMap<String, CommitFileState>> {
        Ok(self
            .head_target()?
            .map(|commit| self.read_commit(&commit))
            .transpose()?
            .map(|commit| commit.files)
            .unwrap_or_default())
    }

    pub(super) fn head_artifacts(&self) -> Result<BTreeMap<String, CommitArtifactState>> {
        Ok(self
            .head_target()?
            .map(|commit| self.read_commit(&commit))
            .transpose()?
            .map(|commit| commit.artifacts)
            .unwrap_or_default())
    }

    pub(super) fn read_branch_ref(&self, name: &str) -> Result<Option<String>> {
        self.read_ref(&format!("refs/heads/{name}"))
    }

    pub(super) fn branch_info(&self, name: &str) -> Result<BranchInfo> {
        self.ensure_local_branch_for_config(name)?;
        let current = self.current_branch()?.as_deref() == Some(name);
        Ok(BranchInfo {
            name: name.to_string(),
            target: self.read_branch_ref(name)?,
            current,
            upstream: self.branch_upstream(name)?,
        })
    }

    pub(super) fn resolve_revision_base(&self, rev: &str) -> Result<String> {
        match rev {
            "HEAD" | "@" => return self.head_target()?.ok_or(RepoErr::UnbornHead),
            _ => {}
        }

        if let Some(target) = self.resolve_refish(rev)? {
            return Ok(target);
        }

        self.resolve_commit_prefix(rev)
    }

    pub(super) fn apply_revision_op(&self, id: &str, op: RevisionOp, rev: &str) -> Result<String> {
        match op {
            RevisionOp::FirstParent(ancestors) => {
                let mut id = id.to_string();
                for _ in 0..ancestors {
                    let parents = commit_parent_ids(&self.read_commit(&id)?);
                    id = parents
                        .into_iter()
                        .next()
                        .ok_or_else(|| RepoErr::UnknownRevision(rev.to_string()))?;
                }
                Ok(id)
            }
            RevisionOp::Parent(parent) => {
                if parent == 0 {
                    return Ok(id.to_string());
                }
                let parents = commit_parent_ids(&self.read_commit(id)?);
                parents
                    .get(parent - 1)
                    .cloned()
                    .ok_or_else(|| RepoErr::UnknownRevision(rev.to_string()))
            }
        }
    }

    pub(super) fn resolve_refish(&self, rev: &str) -> Result<Option<String>> {
        if rev.starts_with("refs/") {
            return self
                .read_ref(rev)?
                .map(|target| {
                    if rev.starts_with("refs/tags/") {
                        self.peel_object_to_commit(&target, rev)
                    } else {
                        Ok(target)
                    }
                })
                .transpose();
        }

        if let Some(target) = self.read_ref(&format!("refs/heads/{rev}"))? {
            return Ok(Some(target));
        }

        if let Some(target) = self.read_ref(&format!("refs/tags/{rev}"))? {
            return Ok(Some(self.peel_object_to_commit(&target, rev)?));
        }

        if let Some((remote, branch)) = rev.split_once('/')
            && validate_remote_name(remote).is_ok()
            && validate_ref_name(branch).is_ok()
            && let Some(target) = self.read_ref(&format!("refs/remotes/{remote}/{branch}"))?
        {
            return Ok(Some(target));
        }

        Ok(None)
    }

    pub(super) fn resolve_commit_prefix(&self, rev: &str) -> Result<String> {
        if rev.len() < 4 || rev.len() > 64 || !rev.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(RepoErr::UnknownRevision(rev.to_string()));
        }

        if rev.len() == 64 {
            let id = object::ObjectId::from_str(rev)?;
            return self.peel_object_id_to_commit(&id, rev);
        }

        let mut matches = self.commitish_object_ids_with_prefix(rev)?;

        match matches.len() {
            0 => Err(RepoErr::UnknownRevision(rev.to_string())),
            1 => {
                let id = object::ObjectId::from_str(&matches.pop().expect("one match"))?;
                self.peel_object_id_to_commit(&id, rev)
            }
            _ => Err(RepoErr::AmbiguousRevision(rev.to_string())),
        }
    }

    pub(super) fn commitish_object_ids_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let mut matches = Vec::new();
        let root = self.object_store().root().to_path_buf();
        if !root.exists() {
            return Ok(matches);
        }

        for dir in fs::read_dir(root)? {
            let dir = dir?;
            if !dir.file_type()?.is_dir() {
                continue;
            }
            let fanout = dir.file_name().to_string_lossy().into_owned();
            if fanout.len() != 2 || !fanout.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }

            for file in fs::read_dir(dir.path())? {
                let file = file?;
                if !file.file_type()?.is_file() {
                    continue;
                }
                let suffix = file.file_name().to_string_lossy().into_owned();
                if suffix.len() != 62 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    continue;
                }

                let id = format!("{fanout}{suffix}");
                if !id.starts_with(prefix) {
                    continue;
                }

                let object_id = object::ObjectId::from_str(&id)?;
                let Some(bytes) = self.object_store().read_raw(&object_id)? else {
                    continue;
                };
                let object = object::Object::decode(&bytes)?;
                let actual = object.id();
                if actual != object_id {
                    return Err(RepoErr::Object(object::ObjectErr::ObjectIdMismatch {
                        expected: object_id,
                        actual,
                    }));
                }
                if matches!(object, object::Object::Commit(_) | object::Object::Tag(_)) {
                    matches.push(id);
                }
            }
        }

        matches.sort();
        Ok(matches)
    }

    pub(super) fn peel_object_to_commit(&self, id: &str, rev: &str) -> Result<String> {
        let id = object::ObjectId::from_str(id)?;
        self.peel_object_id_to_commit(&id, rev)
    }

    pub(super) fn peel_object_id_to_commit(
        &self,
        id: &object::ObjectId,
        rev: &str,
    ) -> Result<String> {
        let mut current = id.clone();
        let mut seen = BTreeMap::<String, ()>::new();

        loop {
            if seen.insert(current.to_string(), ()).is_some() {
                return Err(RepoErr::UnknownRevision(rev.to_string()));
            }

            let Some(bytes) = self.object_store().read_raw(&current)? else {
                return Err(RepoErr::UnknownRevision(rev.to_string()));
            };
            let object = object::Object::decode(&bytes)?;
            let actual = object.id();
            if actual != current {
                return Err(RepoErr::Object(object::ObjectErr::ObjectIdMismatch {
                    expected: current,
                    actual,
                }));
            }

            match object {
                object::Object::Commit(_) => return Ok(current.to_string()),
                object::Object::Tag(tag) => {
                    if !matches!(
                        tag.object_type,
                        object::ObjectKind::Commit | object::ObjectKind::Tag
                    ) {
                        return Err(RepoErr::UnknownRevision(rev.to_string()));
                    }
                    current = tag.object;
                }
                _ => return Err(RepoErr::UnknownRevision(rev.to_string())),
            }
        }
    }

    pub(super) fn branch_exists(&self, name: &str) -> bool {
        self.graft_dir.join(DIR_REFS_HEADS).join(name).is_file()
    }

    pub(super) fn ensure_local_branch_for_config(&self, name: &str) -> Result<()> {
        validate_ref_name(name)?;
        if self.branch_exists(name) || self.current_branch()?.as_deref() == Some(name) {
            Ok(())
        } else {
            Err(RepoErr::BranchNotFound(name.to_string()))
        }
    }

    pub(super) fn tag_exists(&self, name: &str) -> bool {
        self.graft_dir.join(DIR_REFS_TAGS).join(name).is_file()
    }

    pub(super) fn write_branch_ref(&self, name: &str, target: &str, message: &str) -> Result<()> {
        self.write_ref_update(&format!("refs/heads/{name}"), target, message)
    }

    pub(super) fn read_tag_ref(&self, name: &str) -> Result<Option<String>> {
        validate_ref_name(name)?;
        self.read_ref(&format!("refs/tags/{name}"))
    }

    pub(super) fn tag_info_from_ref(&self, name: String, object: String) -> Result<TagInfo> {
        let object_id = object::ObjectId::from_str(&object)?;
        match self.object_store().read(&object_id)? {
            object::Object::Commit(_) => Ok(TagInfo {
                name,
                object: object.clone(),
                target: object,
                annotated: false,
                message: None,
            }),
            object::Object::Tag(tag) => {
                let target = self.peel_object_id_to_commit(&tag.object, &name)?;
                Ok(TagInfo {
                    name,
                    object,
                    target,
                    annotated: true,
                    message: Some(tag.message),
                })
            }
            object => Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "tag",
                message: format!("tag ref `{name}` points at a {}", object.kind()),
            })),
        }
    }

    pub(super) fn write_tag_ref(&self, name: &str, target: &str, message: &str) -> Result<()> {
        self.write_ref_update(&format!("refs/tags/{name}"), target, message)
    }

    pub(super) fn delete_tag_ref(&self, name: &str) -> Result<()> {
        validate_ref_name(name)?;
        let path = self.graft_dir.join(DIR_REFS_TAGS).join(name);
        if !path.is_file() {
            return Err(RepoErr::TagNotFound(name.to_string()));
        }
        fs::remove_file(&path)?;
        remove_empty_parent_dirs(path.parent(), &self.graft_dir.join(DIR_REFS_TAGS))?;
        Ok(())
    }

    pub(super) fn read_ref(&self, reference: &str) -> Result<Option<String>> {
        validate_full_ref(reference)?;
        let path = self.graft_dir.join(reference);
        if !path.exists() {
            return Ok(None);
        }
        if !path.is_file() {
            return Err(RepoErr::BranchNotFound(reference.to_string()));
        }

        let raw = fs::read_to_string(path)?;
        let target = raw.trim();
        if target.is_empty() {
            Ok(None)
        } else {
            Ok(Some(target.to_string()))
        }
    }

    pub(super) fn write_ref_update(
        &self,
        reference: &str,
        target: &str,
        message: &str,
    ) -> Result<()> {
        validate_full_ref(reference)?;
        self.ensure_ref_namespace_available(reference)?;
        let old = self.read_ref(reference)?;
        self.write_ref(reference, target)?;
        self.append_ref_reflog(reference, old.as_deref(), Some(target), message)?;
        Ok(())
    }

    pub(super) fn write_ref(&self, reference: &str, target: &str) -> Result<()> {
        validate_full_ref(reference)?;
        self.ensure_ref_namespace_available(reference)?;
        let path = self.graft_dir.join(reference);
        write_file_atomic(&path, format!("{target}\n").as_bytes())?;
        Ok(())
    }

    pub(super) fn ensure_ref_namespace_available(&self, reference: &str) -> Result<()> {
        validate_full_ref(reference)?;
        let path = self.graft_dir.join(reference);
        if path.is_dir() {
            return Err(RepoErr::RefNameConflict {
                reference: reference.to_string(),
                existing: reference.to_string(),
            });
        }

        let mut current = path.parent();
        while let Some(parent) = current {
            if parent == self.graft_dir {
                break;
            }
            if parent.is_file() {
                let existing = parent.strip_prefix(&self.graft_dir).map_or_else(
                    |_| parent.display().to_string(),
                    |path| path.to_string_lossy().replace('\\', "/"),
                );
                return Err(RepoErr::RefNameConflict {
                    reference: reference.to_string(),
                    existing,
                });
            }
            current = parent.parent();
        }

        Ok(())
    }

    pub(super) fn ensure_path_namespace_available_for_rename(
        root: &Path,
        old_reference: &str,
        new_reference: &str,
    ) -> Result<()> {
        validate_full_ref(old_reference)?;
        validate_full_ref(new_reference)?;

        let old_path = root.join(old_reference);
        let new_path = root.join(new_reference);
        if new_path.is_dir() && !path_tree_contains_only_file(&new_path, &old_path)? {
            return Err(RepoErr::RefNameConflict {
                reference: new_reference.to_string(),
                existing: new_reference.to_string(),
            });
        }

        let mut current = new_path.parent();
        while let Some(parent) = current {
            if parent == root {
                break;
            }
            if parent.is_file() && parent != old_path {
                let existing = parent.strip_prefix(root).map_or_else(
                    |_| parent.display().to_string(),
                    |path| path.to_string_lossy().replace('\\', "/"),
                );
                return Err(RepoErr::RefNameConflict {
                    reference: new_reference.to_string(),
                    existing,
                });
            }
            current = parent.parent();
        }

        Ok(())
    }

    pub(super) fn delete_ref(&self, reference: &str) -> Result<()> {
        validate_full_ref(reference)?;
        let path = self.graft_dir.join(reference);
        if !path.is_file() {
            return Err(RepoErr::BranchNotFound(reference.to_string()));
        }
        fs::remove_file(&path)?;
        remove_empty_parent_dirs(path.parent(), &self.graft_dir.join(DIR_REFS_HEADS))?;
        Ok(())
    }

    pub(super) fn delete_ref_if_exists(&self, reference: &str) -> Result<()> {
        validate_full_ref(reference)?;
        let path = self.graft_dir.join(reference);
        if path.is_file() {
            fs::remove_file(&path)?;
            remove_empty_parent_dirs(path.parent(), &self.graft_dir.join("refs"))?;
        }
        Ok(())
    }

    pub(super) fn collect_ref_files(
        dir: &Path,
        prefix: &str,
        out: &mut BTreeMap<String, Option<String>>,
    ) -> Result<()> {
        if !dir.exists() {
            return Ok(());
        }

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let name = if prefix.is_empty() {
                file_name
            } else {
                format!("{prefix}/{file_name}")
            };

            if entry.file_type()?.is_dir() {
                Self::collect_ref_files(&entry.path(), &name, out)?;
            } else {
                let raw = fs::read_to_string(entry.path())?;
                let target = raw.trim();
                out.insert(
                    name,
                    if target.is_empty() {
                        None
                    } else {
                        Some(target.to_string())
                    },
                );
            }
        }

        Ok(())
    }

    pub(super) fn delete_ref_log(&self, reference: &str) -> Result<()> {
        validate_full_ref(reference)?;
        let path = self.graft_dir.join(DIR_LOGS_REFS).join(reference);
        if path.is_file() {
            fs::remove_file(&path)?;
            remove_empty_parent_dirs(path.parent(), &self.graft_dir.join(DIR_LOGS_REFS))?;
        }
        Ok(())
    }

    pub(super) fn move_ref_log_for_rename(
        &self,
        old_reference: &str,
        new_reference: &str,
    ) -> Result<()> {
        validate_full_ref(old_reference)?;
        validate_full_ref(new_reference)?;

        let root = self.graft_dir.join(DIR_LOGS_REFS);
        let old_path = root.join(old_reference);
        if !old_path.is_file() {
            return Ok(());
        }

        let bytes = fs::read(&old_path)?;
        fs::remove_file(&old_path)?;
        remove_empty_parent_dirs(old_path.parent(), &root)?;

        let new_path = root.join(new_reference);
        write_file_atomic(&new_path, &bytes)?;
        Ok(())
    }

    pub(super) fn append_head_reflog(
        &self,
        old: Option<&str>,
        new: Option<&str>,
        message: &str,
    ) -> Result<()> {
        fs::create_dir_all(self.graft_dir.join(DIR_LOGS_HEAD))?;
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.graft_dir.join(DIR_LOGS_HEAD).join("HEAD"))?
            .write_all(reflog_line(old, new, message).as_bytes())?;
        Ok(())
    }

    pub(super) fn append_ref_reflog(
        &self,
        reference: &str,
        old: Option<&str>,
        new: Option<&str>,
        message: &str,
    ) -> Result<()> {
        validate_full_ref(reference)?;
        let path = self.graft_dir.join(DIR_LOGS_REFS).join(reference);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(reflog_line(old, new, message).as_bytes())?;
        Ok(())
    }
}
