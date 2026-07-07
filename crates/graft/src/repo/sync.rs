use super::*;

impl Repository {
    pub fn fetch(&self, remote: &str, branch: &str) -> Result<FetchOutcome> {
        validate_remote_name(remote)?;
        validate_ref_name(branch)?;
        let remote_store = self.remote_store(remote)?;
        let head_path = format!("refs/heads/{branch}");
        let Some(head) = block_on_remote(remote_store.get_raw(&head_path))? else {
            return Err(RepoErr::RemoteBranchNotFound {
                remote: remote.to_string(),
                branch: branch.to_string(),
            });
        };
        let head = parse_remote_ref(&head_path, head)?;
        let commits = self.fetch_commit_chain(&remote_store, &head)?;
        self.set_remote_tracking_ref(remote, branch, &head)?;
        Ok(FetchOutcome {
            remote: remote.to_string(),
            branch: branch.to_string(),
            head,
            commits,
        })
    }

    pub fn fetch_all(&self, remote: &str) -> Result<FetchAllOutcome> {
        validate_remote_name(remote)?;
        let remote_store = self.remote_store(remote)?;
        let remote_refs = self.remote_branch_refs_from_store(remote, &remote_store)?;
        let mut branches = Vec::with_capacity(remote_refs.len());

        for remote_ref in remote_refs {
            let commits = self.fetch_commit_chain(&remote_store, &remote_ref.head)?;
            self.set_remote_tracking_ref(remote, &remote_ref.branch, &remote_ref.head)?;
            branches.push(FetchOutcome {
                remote: remote.to_string(),
                branch: remote_ref.branch,
                head: remote_ref.head,
                commits,
            });
        }

        Ok(FetchAllOutcome { remote: remote.to_string(), branches })
    }

    pub fn fetch_refspec(&self, remote: &str, refspec: &str) -> Result<FetchAllOutcome> {
        validate_remote_name(remote)?;
        let parsed = parse_fetch_refspec(remote, refspec)?;
        let remote_store = self.remote_store(remote)?;
        let branches = self.fetch_refspec_with_store(remote, &remote_store, refspec, &parsed)?;
        Ok(FetchAllOutcome { remote: remote.to_string(), branches })
    }

    pub(super) fn fetch_refspec_with_store(
        &self,
        remote: &str,
        remote_store: &crate::remote::Remote,
        refspec: &str,
        parsed: &ParsedRefspec,
    ) -> Result<Vec<FetchOutcome>> {
        let source = parsed
            .source
            .as_ref()
            .expect("fetch refspec parser rejects delete refspecs");
        let destination = parsed.destination.as_ref().unwrap_or(source);
        let mut outcomes = Vec::new();

        if let Some(source_branch) = source.exact() {
            let head_path = format!("refs/heads/{source_branch}");
            let Some(head) = block_on_remote(remote_store.get_raw(&head_path))? else {
                return Err(RepoErr::RemoteBranchNotFound {
                    remote: remote.to_string(),
                    branch: source_branch.to_string(),
                });
            };
            let head = parse_remote_ref(&head_path, head)?;
            let destination_branch = destination.expand("")?;
            let commits = self.fetch_commit_chain(remote_store, &head)?;
            self.set_remote_tracking_ref(remote, &destination_branch, &head)?;
            outcomes.push(FetchOutcome {
                remote: remote.to_string(),
                branch: destination_branch,
                head,
                commits,
            });
            return Ok(outcomes);
        }

        let remote_refs = self.remote_branch_refs_from_store(remote, remote_store)?;
        for remote_ref in remote_refs {
            let Some(capture) = source.capture(&remote_ref.branch)? else {
                continue;
            };
            let destination_branch = destination.expand(capture)?;
            let commits = self.fetch_commit_chain(remote_store, &remote_ref.head)?;
            self.set_remote_tracking_ref(remote, &destination_branch, &remote_ref.head)?;
            outcomes.push(FetchOutcome {
                remote: remote.to_string(),
                branch: destination_branch,
                head: remote_ref.head,
                commits,
            });
        }

        if outcomes.is_empty() {
            return Err(RepoErr::InvalidRefspec {
                refspec: refspec.to_string(),
                message: "wildcard matched no remote branches".to_string(),
            });
        }
        Ok(outcomes)
    }

    pub fn push(&self, remote: &str, branch: &str) -> Result<PushOutcome> {
        self.push_branch(remote, branch, branch)
    }

    pub fn push_all(&self, remote: &str) -> Result<PushAllOutcome> {
        self.push_all_with_force(remote, false)
    }

    pub fn push_all_with_force(&self, remote: &str, force: bool) -> Result<PushAllOutcome> {
        validate_remote_name(remote)?;
        let mut branches = Vec::new();

        for branch in self.branches()? {
            if branch.target.is_none() {
                continue;
            }
            branches.push(self.push_branch_with_force(
                remote,
                &branch.name,
                &branch.name,
                force,
            )?);
        }

        Ok(PushAllOutcome { remote: remote.to_string(), branches })
    }

    pub fn push_refspec_with_force(
        &self,
        remote: &str,
        refspec: &str,
        force: bool,
    ) -> Result<PushAllOutcome> {
        validate_remote_name(remote)?;
        let parsed = parse_push_refspec(refspec)?;
        let force = force || parsed.force;
        if parsed.source.is_none() {
            let Some(destination) = &parsed.destination else {
                return Err(RepoErr::InvalidRefspec {
                    refspec: refspec.to_string(),
                    message: "delete refspecs require a destination".to_string(),
                });
            };
            if destination.is_wildcard() {
                return Err(RepoErr::InvalidRefspec {
                    refspec: refspec.to_string(),
                    message: "wildcard delete refspecs are not supported".to_string(),
                });
            }
            let remote_branch = destination.expand("")?;
            let outcome = self.push_delete_branch_with_force(remote, &remote_branch, force)?;
            return Ok(PushAllOutcome {
                remote: remote.to_string(),
                branches: vec![outcome],
            });
        }

        let source = parsed.source.as_ref().expect("handled delete refspec");
        let destination = parsed.destination.as_ref().unwrap_or(source);
        let mut branches = Vec::new();

        if let Some(local_branch) = source.exact() {
            let remote_branch = destination.expand("")?;
            branches.push(self.push_branch_with_force(
                remote,
                local_branch,
                &remote_branch,
                force,
            )?);
            return Ok(PushAllOutcome { remote: remote.to_string(), branches });
        }

        for branch in self.branches()? {
            if branch.target.is_none() {
                continue;
            }
            let Some(capture) = source.capture(&branch.name)? else {
                continue;
            };
            let remote_branch = destination.expand(capture)?;
            branches.push(self.push_branch_with_force(
                remote,
                &branch.name,
                &remote_branch,
                force,
            )?);
        }

        if branches.is_empty() {
            return Err(RepoErr::InvalidRefspec {
                refspec: refspec.to_string(),
                message: "wildcard matched no local branches".to_string(),
            });
        }
        Ok(PushAllOutcome { remote: remote.to_string(), branches })
    }

    pub fn push_refspec_branches(&self, refspec: &str) -> Result<Vec<PushRefspecBranch>> {
        let parsed = parse_push_refspec(refspec)?;
        let Some(source) = parsed.source.as_ref() else {
            return Ok(Vec::new());
        };
        let destination = parsed.destination.as_ref().unwrap_or(source);

        if let Some(local_branch) = source.exact() {
            let remote_branch = destination.expand("")?;
            return Ok(vec![PushRefspecBranch {
                local_branch: local_branch.to_string(),
                remote_branch,
            }]);
        }

        let mut branches = Vec::new();
        for branch in self.branches()? {
            if branch.target.is_none() {
                continue;
            }
            let Some(capture) = source.capture(&branch.name)? else {
                continue;
            };
            let remote_branch = destination.expand(capture)?;
            branches.push(PushRefspecBranch { local_branch: branch.name, remote_branch });
        }

        if branches.is_empty() {
            return Err(RepoErr::InvalidRefspec {
                refspec: refspec.to_string(),
                message: "wildcard matched no local branches".to_string(),
            });
        }
        Ok(branches)
    }

    pub fn push_branch(
        &self,
        remote: &str,
        local_branch: &str,
        remote_branch: &str,
    ) -> Result<PushOutcome> {
        self.push_branch_with_force(remote, local_branch, remote_branch, false)
    }

    pub fn push_branch_with_force(
        &self,
        remote: &str,
        local_branch: &str,
        remote_branch: &str,
        force: bool,
    ) -> Result<PushOutcome> {
        let remote_head = self.remote_branch_head_state(remote, remote_branch)?;
        self.push_branch_with_force_and_remote_head(
            remote,
            local_branch,
            remote_branch,
            force,
            remote_head,
        )
    }

    pub fn push_branch_with_force_and_remote_head(
        &self,
        remote: &str,
        local_branch: &str,
        remote_branch: &str,
        force: bool,
        remote_head: RemoteBranchHead,
    ) -> Result<PushOutcome> {
        validate_remote_name(remote)?;
        validate_ref_name(local_branch)?;
        validate_ref_name(remote_branch)?;
        let Some(head) = self.branch_target(local_branch)? else {
            return Err(RepoErr::UnbornHead);
        };

        let remote_store = self.remote_store(remote)?;
        let RemoteBranchHead { raw: remote_head_raw, head: remote_head } = remote_head;
        let remote_branch_existed = remote_head_raw.is_some();

        if remote_head.as_deref() == Some(head.as_str()) {
            self.set_remote_tracking_ref(remote, remote_branch, &head)?;
            return Ok(PushOutcome {
                remote: remote.to_string(),
                local_branch: local_branch.to_string(),
                remote_branch: remote_branch.to_string(),
                head,
                commits: 0,
                forced: force,
                deleted: false,
            });
        }

        if let Some(remote_head) = &remote_head
            && !force
            && !self.is_ancestor(remote_head, &head)?
        {
            return Err(RepoErr::NonFastForward {
                remote: remote.to_string(),
                local_branch: local_branch.to_string(),
                remote_branch: remote_branch.to_string(),
            });
        }

        let commits = self.push_commit_chain(&remote_store, &head, remote_head.as_deref())?;
        let head_path = format!("refs/heads/{remote_branch}");
        match block_on_remote(remote_store.compare_and_swap_raw(
            &head_path,
            remote_head_raw.as_deref(),
            format!("{head}\n"),
        )) {
            Ok(()) => {}
            Err(RepoErr::Remote(RemoteErr::CompareAndSwap { .. } | RemoteErr::LockBusy { .. })) => {
                return Err(RepoErr::RemoteRefChanged {
                    remote: remote.to_string(),
                    branch: remote_branch.to_string(),
                });
            }
            Err(err) => return Err(err),
        }
        if !remote_branch_existed {
            self.set_remote_head_if_absent(&remote_store, remote_branch)?;
        }
        self.set_remote_tracking_ref(remote, remote_branch, &head)?;

        Ok(PushOutcome {
            remote: remote.to_string(),
            local_branch: local_branch.to_string(),
            remote_branch: remote_branch.to_string(),
            head,
            commits,
            forced: force,
            deleted: false,
        })
    }

    pub fn push_delete_branch_with_force(
        &self,
        remote: &str,
        remote_branch: &str,
        force: bool,
    ) -> Result<PushOutcome> {
        validate_remote_name(remote)?;
        validate_ref_name(remote_branch)?;

        let remote_store = self.remote_store(remote)?;
        let head_path = format!("refs/heads/{remote_branch}");
        let remote_head_raw =
            block_on_remote(remote_store.get_raw(&head_path))?.ok_or_else(|| {
                RepoErr::RemoteBranchNotFound {
                    remote: remote.to_string(),
                    branch: remote_branch.to_string(),
                }
            })?;
        let remote_head = parse_remote_ref(&head_path, remote_head_raw.clone())?;

        if !force
            && let Some(local_tracking) = self.remote_tracking_ref(remote, remote_branch)?
            && local_tracking != remote_head
        {
            return Err(RepoErr::RemoteRefChanged {
                remote: remote.to_string(),
                branch: remote_branch.to_string(),
            });
        }

        match block_on_remote(
            remote_store.compare_and_delete_raw(&head_path, Some(remote_head_raw.as_ref())),
        ) {
            Ok(()) => {}
            Err(RepoErr::Remote(RemoteErr::CompareAndSwap { .. } | RemoteErr::LockBusy { .. })) => {
                return Err(RepoErr::RemoteRefChanged {
                    remote: remote.to_string(),
                    branch: remote_branch.to_string(),
                });
            }
            Err(err) => return Err(err),
        }

        self.delete_ref_if_exists(&format!("refs/remotes/{remote}/{remote_branch}"))?;
        self.delete_ref_log(&format!("refs/remotes/{remote}/{remote_branch}"))?;

        Ok(PushOutcome {
            remote: remote.to_string(),
            local_branch: String::new(),
            remote_branch: remote_branch.to_string(),
            head: remote_head,
            commits: 0,
            forced: force,
            deleted: true,
        })
    }

    pub fn pull(
        &self,
        remote: &str,
        remote_branch: &str,
        local_branch: &str,
    ) -> Result<PullOutcome> {
        let plan = self.plan_pull(remote, remote_branch, local_branch)?;
        self.apply_pull_plan(&plan)
    }

    pub fn plan_pull(
        &self,
        remote: &str,
        remote_branch: &str,
        local_branch: &str,
    ) -> Result<PullPlan> {
        validate_remote_name(remote)?;
        validate_ref_name(remote_branch)?;
        validate_ref_name(local_branch)?;
        if self.current_branch()?.as_deref() != Some(local_branch) {
            return Err(RepoErr::NotCurrentBranch(local_branch.to_string()));
        }
        if self.merge_head()?.is_some() {
            return Err(RepoErr::MergeInProgress);
        }

        let fetch = self.fetch(remote, remote_branch)?;
        let merge = self.plan_merge_revision(&format!("refs/remotes/{remote}/{remote_branch}"))?;
        Ok(PullPlan {
            remote: remote.to_string(),
            remote_branch: remote_branch.to_string(),
            local_branch: local_branch.to_string(),
            fetch,
            merge,
        })
    }

    pub fn plan_pull_refspec(
        &self,
        remote: &str,
        refspec: &str,
        local_branch: &str,
    ) -> Result<PullPlan> {
        validate_remote_name(remote)?;
        validate_ref_name(local_branch)?;
        if self.current_branch()?.as_deref() != Some(local_branch) {
            return Err(RepoErr::NotCurrentBranch(local_branch.to_string()));
        }
        if self.merge_head()?.is_some() {
            return Err(RepoErr::MergeInProgress);
        }

        let mut fetch = self.fetch_refspec(remote, refspec)?.branches;
        if fetch.len() != 1 {
            return Err(RepoErr::InvalidRefspec {
                refspec: refspec.to_string(),
                message: "pull refspec must update exactly one remote-tracking branch".to_string(),
            });
        }
        let fetch = fetch.pop().expect("length checked");
        let merge = self.plan_merge_revision(&format!("refs/remotes/{remote}/{}", fetch.branch))?;
        Ok(PullPlan {
            remote: remote.to_string(),
            remote_branch: fetch.branch.clone(),
            local_branch: local_branch.to_string(),
            fetch,
            merge,
        })
    }

    pub fn apply_pull_plan(&self, plan: &PullPlan) -> Result<PullOutcome> {
        let merge = self.apply_merge_plan(&plan.merge)?;
        Ok(PullOutcome {
            remote: plan.remote.clone(),
            remote_branch: plan.remote_branch.clone(),
            local_branch: plan.local_branch.clone(),
            head: plan.fetch.head.clone(),
            commits: plan.fetch.commits,
            merge,
        })
    }
}
