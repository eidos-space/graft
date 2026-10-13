use super::*;

enum PushRefExpected {
    Raw(Option<Bytes>),
    #[cfg(not(target_arch = "wasm32"))]
    Observed(crate::remote::RemoteRefObservation),
}

struct PushRemoteHead {
    store: Remote,
    head: Option<String>,
    expected: PushRefExpected,
    expected_source_head: Option<String>,
}

impl PushRefExpected {
    fn bytes(&self) -> Option<&Bytes> {
        match self {
            Self::Raw(bytes) => bytes.as_ref(),
            #[cfg(not(target_arch = "wasm32"))]
            Self::Observed(observed) => observed.bytes(),
        }
    }
}

/// Read-time remote and branch binding carried intact into publication.
/// Its fields cannot be fabricated from a plaintext target or serialized.
#[cfg(not(target_arch = "wasm32"))]
pub struct ObservedRemoteBranchHead {
    remote_name: String,
    branch: String,
    store: Remote,
    observation: crate::remote::RemoteRefObservation,
    head: Option<String>,
    graft_dir: PathBuf,
    config: RemoteConfig,
}

#[cfg(not(target_arch = "wasm32"))]
impl ObservedRemoteBranchHead {
    pub fn head(&self) -> Option<&str> {
        self.head.as_deref()
    }
}

/// Immutable metadata fetched for a branch, before external payload downloads.
/// Dropping this preparation leaves refs, index, and worktree unchanged.
pub struct PreparedFetch<'a> {
    repository: &'a Repository,
    remote: Remote,
    outcome: FetchOutcome,
    payload_bytes: u64,
    boundary: fetch_boundary::FetchBoundary,
}

impl PreparedFetch<'_> {
    pub fn head(&self) -> &str {
        &self.outcome.head
    }

    /// Declare the combined external-file and adapter-owned download budget.
    pub fn plan_download(&self, additional_bytes: u64) -> Result<()> {
        let bytes = self
            .payload_bytes
            .checked_add(additional_bytes)
            .ok_or_else(|| RepoErr::InvalidRemoteObject {
                path: self.outcome.head.clone(),
                message: "download size exceeds u64".into(),
            })?;
        plan_remaining_transfer(TransferDirection::Download, bytes);
        Ok(())
    }

    /// Download and validate external payloads, then publish the tracking ref.
    pub fn finish(self) -> Result<FetchOutcome> {
        cancellation_checkpoint()?;
        self.repository.fetch_commit_chain_with_boundary(
            &self.remote,
            &self.outcome.head,
            &self.boundary,
        )?;
        cancellation_checkpoint()?;
        self.repository.set_remote_tracking_ref(
            &self.outcome.remote,
            &self.outcome.branch,
            &self.outcome.head,
        )?;
        self.boundary.remember(&self.outcome.head)?;
        Ok(self.outcome)
    }
}

impl Repository {
    /// Fetch the immutable graph without downloading external file contents.
    /// Adapters can add their snapshot budget before completing this fetch.
    pub fn prepare_fetch(&self, remote: &str, branch: &str) -> Result<PreparedFetch<'_>> {
        cancellation_checkpoint()?;
        validate_remote_name(remote)?;
        validate_ref_name(branch)?;
        ACTIVE_TRANSFER_PROGRESS.with(|active| {
            if let Some(reporter) = active.borrow().as_ref() {
                reporter.start_planning(TransferDirection::Download);
            }
        });
        let remote_store = self.remote_store(remote)?;
        let head_path = format!("refs/heads/{branch}");
        let have = self.verified_fetch_have(remote, branch)?;
        let boundary = self.fetch_boundary(&remote_store, remote, branch, have.as_deref())?;
        let bundle = tempfile::tempdir()?;
        let downloaded = block_on_remote(remote_store.download_fetch_bundle(
            &head_path,
            have.as_deref(),
            bundle.path(),
        ))?;
        let (head, imported) = match downloaded {
            FetchBundleOutcome::Downloaded => {
                let store = RemoteConfig::Fs {
                    root: bundle.path().to_string_lossy().into_owned(),
                }
                .build()?;
                let head = block_on_remote(store.get_raw(&head_path))?.ok_or_else(|| {
                    RepoErr::InvalidRemoteObject {
                        path: head_path.clone(),
                        message: "fetch-bundle omitted its reference".into(),
                    }
                })?;
                (
                    parse_remote_ref(&head_path, head)?,
                    self.import_remote_object_pack_bundle(bundle.path())?,
                )
            }
            FetchBundleOutcome::Unsupported => {
                let head = block_on_remote(remote_store.get_raw(&head_path))?.ok_or_else(|| {
                    RepoErr::RemoteBranchNotFound {
                        remote: remote.into(),
                        branch: branch.into(),
                    }
                })?;
                (parse_remote_ref(&head_path, head)?, BTreeSet::new())
            }
        };
        let (fetched, payload_bytes) =
            self.fetch_graph_metadata(&remote_store, &head, &boundary)?;
        let reachable = self.fetched_commit_ancestors(&head, &boundary)?;
        let commits = fetched + imported.intersection(&reachable).count();
        Ok(PreparedFetch {
            repository: self,
            remote: remote_store,
            outcome: FetchOutcome {
                remote: remote.into(),
                branch: branch.into(),
                head,
                commits,
            },
            payload_bytes,
            boundary,
        })
    }

    pub fn fetch(&self, remote: &str, branch: &str) -> Result<FetchOutcome> {
        validate_remote_name(remote)?;
        validate_ref_name(branch)?;
        let remote_store = self.remote_store(remote)?;
        self.fetch_with_bundle_or_store(remote, branch, &remote_store)
    }

    pub fn fetch_for_clone(&self, remote: &str, branch: &str) -> Result<CloneFetch> {
        validate_remote_name(remote)?;
        validate_ref_name(branch)?;
        let network_remote = self.remote_store(remote)?;
        if network_remote.buffered_read_limit().is_some() {
            // Encrypted aggregate fetches select packs only. Complete omitted
            // canonical dependencies against the encrypted remote before clone.
            let fetch = self.fetch_with_bundle_or_store(remote, branch, &network_remote)?;
            self.remote_credentials.reset_http_clients();
            return Ok(CloneFetch {
                fetch,
                remote: Arc::new(self.remote_store(remote)?),
                _bundle: None,
            });
        }
        let head_path = format!("refs/heads/{branch}");
        let bundle = tempfile::tempdir()?;
        let outcome =
            block_on_remote(network_remote.download_upload_bundle(&head_path, bundle.path()))?;
        match outcome {
            UploadBundleOutcome::Downloaded => {
                let root = bundle.path().to_string_lossy().into_owned();
                let remote_store = RemoteConfig::Fs { root }.build()?;
                let fetch = self.fetch_with_store(remote, branch, &remote_store)?;
                Ok(CloneFetch {
                    fetch,
                    remote: Arc::new(remote_store),
                    _bundle: Some(bundle),
                })
            }
            UploadBundleOutcome::Unsupported => {
                let fetch = self.fetch_with_store(remote, branch, &network_remote)?;
                // Do not carry a pooled HTTP/1 connection from large pack downloads into
                // checkout. Some proxies leave that connection readable but unusable for
                // the first storage-commit request. A new pool preserves the legacy fallback.
                self.remote_credentials.reset_http_clients();
                let checkout_remote = self.remote_store(remote)?;
                Ok(CloneFetch {
                    fetch,
                    remote: Arc::new(checkout_remote),
                    _bundle: None,
                })
            }
        }
    }

    fn fetch_with_store(
        &self,
        remote: &str,
        branch: &str,
        remote_store: &Remote,
    ) -> Result<FetchOutcome> {
        let head_path = format!("refs/heads/{branch}");
        let Some(head) = block_on_remote(remote_store.get_raw(&head_path))? else {
            return Err(RepoErr::RemoteBranchNotFound {
                remote: remote.to_string(),
                branch: branch.to_string(),
            });
        };
        let head = parse_remote_ref(&head_path, head)?;
        let commits = self.fetch_commit_chain(remote_store, &head)?;
        self.set_remote_tracking_ref(remote, branch, &head)?;
        Ok(FetchOutcome {
            remote: remote.to_string(),
            branch: branch.to_string(),
            head,
            commits,
        })
    }

    fn verified_fetch_have(&self, remote: &str, branch: &str) -> Result<Option<String>> {
        Ok(self.remote_tracking_ref(remote, branch)?.filter(|head| {
            object::ObjectId::from_str(head).ok().is_some_and(|id| {
                self.object_store()
                    .read_raw(&id)
                    .ok()
                    .flatten()
                    .is_some_and(|bytes| {
                        object::ObjectId::for_bytes(&bytes) == id
                            && matches!(
                                object::Object::decode(&bytes),
                                Ok(object::Object::Commit(_))
                            )
                    })
            })
        }))
    }

    fn fetch_with_bundle_or_store(
        &self,
        remote: &str,
        branch: &str,
        remote_store: &Remote,
    ) -> Result<FetchOutcome> {
        let trace = crate::trace::PushTraceSpan::new("remote_fetch_bundle");
        let head_path = format!("refs/heads/{branch}");
        let have = self.verified_fetch_have(remote, branch)?;
        let boundary = self.fetch_boundary(remote_store, remote, branch, have.as_deref())?;
        let bundle = tempfile::tempdir()?;
        let outcome = block_on_remote(remote_store.download_fetch_bundle(
            &head_path,
            have.as_deref(),
            bundle.path(),
        ))?;
        match outcome {
            FetchBundleOutcome::Downloaded => {
                let root = bundle.path().to_string_lossy().into_owned();
                let bundle_store = RemoteConfig::Fs { root }.build()?;
                let Some(head) = block_on_remote(bundle_store.get_raw(&head_path))? else {
                    return Err(RepoErr::InvalidRemoteObject {
                        path: head_path,
                        message: "fetch-bundle omitted its reference".to_string(),
                    });
                };
                let head = parse_remote_ref(&format!("refs/heads/{branch}"), head)?;
                let imported = self.import_remote_object_pack_bundle(bundle.path())?;
                let (fetched, reachable) =
                    self.fetch_commit_graph_with_boundary(remote_store, &head, &boundary)?;
                let imported = imported.intersection(&reachable).count();
                self.set_remote_tracking_ref(remote, branch, &head)?;
                boundary.remember(&head)?;
                trace.finish(&[
                    ("supported", 1),
                    ("imported_commits", imported as u64),
                    ("fallback_commits", fetched as u64),
                ]);
                Ok(FetchOutcome {
                    remote: remote.to_string(),
                    branch: branch.to_string(),
                    head,
                    commits: imported + fetched,
                })
            }
            FetchBundleOutcome::Unsupported => {
                trace.finish(&[("supported", 0)]);
                let head = block_on_remote(remote_store.get_raw(&head_path))?.ok_or_else(|| {
                    RepoErr::RemoteBranchNotFound {
                        remote: remote.into(),
                        branch: branch.into(),
                    }
                })?;
                let head = parse_remote_ref(&head_path, head)?;
                let commits =
                    self.fetch_commit_chain_with_boundary(remote_store, &head, &boundary)?;
                self.set_remote_tracking_ref(remote, branch, &head)?;
                boundary.remember(&head)?;
                Ok(FetchOutcome {
                    remote: remote.into(),
                    branch: branch.into(),
                    head,
                    commits,
                })
            }
        }
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
        let total_trace = crate::trace::PushTraceSpan::new("push_branch_total");
        #[cfg(not(target_arch = "wasm32"))]
        let outcome = {
            let observation_trace = crate::trace::PushTraceSpan::new("push_remote_observation");
            let observed = self.observe_remote_branch_head(remote, remote_branch)?;
            observation_trace.finish(&[]);
            self.push_branch_with_observed_head_and_snapshots(
                remote,
                local_branch,
                remote_branch,
                force,
                observed,
                crate::PreparedSnapshotPush::default(),
            )
        };
        #[cfg(target_arch = "wasm32")]
        let outcome = {
            let observation_trace = crate::trace::PushTraceSpan::new("push_remote_observation");
            let remote_head = self.remote_branch_head_state(remote, remote_branch)?;
            observation_trace.finish(&[]);
            self.push_branch_with_force_and_remote_head(
                remote,
                local_branch,
                remote_branch,
                force,
                remote_head,
            )
        };
        if outcome.is_ok() {
            total_trace.finish(&[]);
        }
        outcome
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn observe_remote_branch_head(
        &self,
        remote: &str,
        branch: &str,
    ) -> Result<ObservedRemoteBranchHead> {
        let local_trace = crate::trace::PushTraceSpan::new("push_observation_local_prepare");
        validate_remote_name(remote)?;
        validate_ref_name(branch)?;
        let config = self
            .config()?
            .remotes
            .get(remote)
            .cloned()
            .ok_or_else(|| RepoErr::RemoteNotFound(remote.to_owned()))?;
        let store = config
            .clone()
            .build_with_credentials(remote, &self.remote_credentials)?;
        let path = format!("refs/heads/{branch}");
        local_trace.finish(&[]);
        let remote_trace = crate::trace::PushTraceSpan::new("push_observation_remote_await");
        let observation = block_on_remote(store.observe_ref(&path))?;
        remote_trace.finish(&[]);
        let head = observation
            .bytes()
            .map(|bytes| parse_remote_ref(&path, bytes.clone()))
            .transpose()?;
        Ok(ObservedRemoteBranchHead {
            remote_name: remote.to_owned(),
            branch: branch.to_owned(),
            store,
            observation,
            head,
            graft_dir: self.graft_dir.clone(),
            config,
        })
    }

    /// Publish the exact source retained by a trusted initialization journal.
    /// A changed source fails before discovery and is rechecked after discovery.
    #[cfg(all(feature = "e2ee", not(target_arch = "wasm32")))]
    pub fn push_initialization_intent(
        &self,
        remote: &str,
        intent: &crate::e2ee::InitializationIntent,
    ) -> Result<PushOutcome> {
        validate_remote_name(remote)?;
        if self.branch_target(intent.branch())?.as_deref() != Some(intent.commit()) {
            return Err(crate::remote::RemoteErr::from(
                crate::e2ee::EncryptionError::StaleObservation,
            )
            .into());
        }
        let observed = self.observe_remote_branch_head(remote, intent.branch())?;
        self.push_initialization_intent_with_observed_head_and_snapshots(
            remote,
            intent,
            observed,
            crate::PreparedSnapshotPush::default(),
        )
    }

    #[cfg(all(feature = "e2ee", not(target_arch = "wasm32")))]
    pub fn push_initialization_intent_with_observed_head_and_snapshots(
        &self,
        remote: &str,
        intent: &crate::e2ee::InitializationIntent,
        observed: ObservedRemoteBranchHead,
        snapshots: crate::PreparedSnapshotPush,
    ) -> Result<PushOutcome> {
        if observed.remote_name != remote
            || observed.branch != intent.branch()
            || observed.graft_dir != self.graft_dir
            || self.config()?.remotes.get(remote) != Some(&observed.config)
            || observed
                .head
                .as_deref()
                .is_some_and(|head| head != intent.commit())
        {
            return Err(crate::remote::RemoteErr::from(
                crate::e2ee::EncryptionError::StaleObservation,
            )
            .into());
        }
        self.push_branch_with_expected_ref_and_snapshots(
            remote,
            intent.branch(),
            intent.branch(),
            false,
            PushRemoteHead {
                store: observed.store,
                head: observed.head,
                expected: PushRefExpected::Observed(observed.observation),
                expected_source_head: Some(intent.commit().to_owned()),
            },
            snapshots,
        )
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn push_branch_with_observed_head_and_snapshots(
        &self,
        remote: &str,
        local_branch: &str,
        remote_branch: &str,
        force: bool,
        observed: ObservedRemoteBranchHead,
        snapshots: crate::PreparedSnapshotPush,
    ) -> Result<PushOutcome> {
        if observed.remote_name != remote
            || observed.branch != remote_branch
            || observed.graft_dir != self.graft_dir
            || self.config()?.remotes.get(remote) != Some(&observed.config)
        {
            return Err(RepoErr::RemoteRefChanged {
                remote: remote.to_owned(),
                branch: remote_branch.to_owned(),
            });
        }
        self.push_branch_with_expected_ref_and_snapshots(
            remote,
            local_branch,
            remote_branch,
            force,
            PushRemoteHead {
                store: observed.store,
                head: observed.head,
                expected: PushRefExpected::Observed(observed.observation),
                expected_source_head: None,
            },
            snapshots,
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
        self.push_branch_with_force_remote_head_and_snapshots(
            remote,
            local_branch,
            remote_branch,
            force,
            remote_head,
            crate::PreparedSnapshotPush::default(),
        )
    }

    pub fn push_branch_with_force_remote_head_and_snapshots(
        &self,
        remote: &str,
        local_branch: &str,
        remote_branch: &str,
        force: bool,
        remote_head: RemoteBranchHead,
        snapshots: crate::PreparedSnapshotPush,
    ) -> Result<PushOutcome> {
        validate_remote_name(remote)?;
        let store = self.remote_store(remote)?;
        self.push_branch_with_expected_ref_and_snapshots(
            remote,
            local_branch,
            remote_branch,
            force,
            PushRemoteHead {
                store,
                head: remote_head.head,
                expected: PushRefExpected::Raw(remote_head.raw),
                expected_source_head: None,
            },
            snapshots,
        )
    }

    fn push_branch_with_expected_ref_and_snapshots(
        &self,
        remote: &str,
        local_branch: &str,
        remote_branch: &str,
        force: bool,
        state: PushRemoteHead,
        snapshots: crate::PreparedSnapshotPush,
    ) -> Result<PushOutcome> {
        let branch_trace = crate::trace::PushTraceSpan::new("push_branch_publication_path");
        let preflight_trace = crate::trace::PushTraceSpan::new("push_branch_preflight");
        let PushRemoteHead {
            store: remote_store,
            head: remote_head,
            expected,
            expected_source_head,
        } = state;
        validate_remote_name(remote)?;
        validate_ref_name(local_branch)?;
        validate_ref_name(remote_branch)?;
        #[cfg(not(target_arch = "wasm32"))]
        if let PushRefExpected::Observed(observed) = &expected {
            remote_store
                .validate_ref_observation(&format!("refs/heads/{remote_branch}"), observed)?;
        }
        let local_head = self.branch_target(local_branch)?;
        #[cfg(all(feature = "e2ee", not(target_arch = "wasm32")))]
        if expected_source_head
            .as_deref()
            .is_some_and(|expected| local_head.as_deref() != Some(expected))
        {
            return Err(crate::remote::RemoteErr::from(
                crate::e2ee::EncryptionError::StaleObservation,
            )
            .into());
        }
        #[cfg(not(all(feature = "e2ee", not(target_arch = "wasm32"))))]
        debug_assert!(expected_source_head.is_none());
        let Some(head) = local_head else {
            return Err(RepoErr::UnbornHead);
        };

        let remote_branch_existed = expected.bytes().is_some();

        if remote_head.as_deref() == Some(head.as_str()) {
            #[cfg(all(feature = "e2ee", not(target_arch = "wasm32")))]
            if let PushRefExpected::Observed(observed) = &expected {
                block_on_remote(remote_store.reconcile_observed_ref_journal(
                    &format!("refs/heads/{remote_branch}"),
                    observed,
                ))?;
            }
            preflight_trace.finish(&[]);
            // A previous push may have confirmed the branch but failed before
            // creating its default HEAD. Retry that independent create without
            // republishing dependencies or taking the branch mutation lock.
            let head_trace = crate::trace::PushTraceSpan::new("push_default_head");
            self.set_remote_head_if_absent(&remote_store, remote_branch)?;
            head_trace.finish(&[]);
            let tracking_trace = crate::trace::PushTraceSpan::new("push_local_tracking");
            self.set_remote_tracking_ref(remote, remote_branch, &head)?;
            tracking_trace.finish(&[]);
            branch_trace.finish(&[]);
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

        preflight_trace.finish(&[]);
        let preparation_trace = crate::trace::PushTraceSpan::new("push_commit_chain_prepare");
        let mut prepared = self.push_commit_chain(&remote_store, &head, remote_head.as_deref())?;
        let mut bundle_objects = snapshots.into_bundle_objects()?;
        bundle_objects.append(&mut prepared.bundle_objects);
        preparation_trace.finish(&[]);
        let head_path = format!("refs/heads/{remote_branch}");
        let publication_trace = crate::trace::PushTraceSpan::new("remote_publication");
        let replacement = format!("{head}\n");
        let publication = match expected {
            PushRefExpected::Raw(raw) => {
                block_on_remote(remote_store.publish_object_bundle_and_ref(
                    bundle_objects,
                    prepared.pack,
                    &head_path,
                    raw.as_deref(),
                    replacement,
                ))
            }
            #[cfg(not(target_arch = "wasm32"))]
            PushRefExpected::Observed(observed) => {
                block_on_remote(remote_store.publish_observed_object_bundle_and_ref(
                    bundle_objects,
                    prepared.pack,
                    &head_path,
                    &observed,
                    replacement,
                ))
            }
        };
        match publication {
            Ok(()) => publication_trace.finish(&[]),
            Err(RepoErr::Remote(RemoteErr::CompareAndSwap { .. } | RemoteErr::LockBusy { .. })) => {
                return Err(RepoErr::RemoteRefChanged {
                    remote: remote.to_string(),
                    branch: remote_branch.to_string(),
                });
            }
            Err(err) => return Err(err),
        }
        if !remote_branch_existed {
            let head_trace = crate::trace::PushTraceSpan::new("push_default_head");
            self.set_remote_head_if_absent(&remote_store, remote_branch)?;
            head_trace.finish(&[]);
        }
        let tracking_trace = crate::trace::PushTraceSpan::new("push_local_tracking");
        self.set_remote_tracking_ref(remote, remote_branch, &head)?;
        tracking_trace.finish(&[]);
        branch_trace.finish(&[]);

        Ok(PushOutcome {
            remote: remote.to_string(),
            local_branch: local_branch.to_string(),
            remote_branch: remote_branch.to_string(),
            head,
            commits: prepared.commits,
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
        #[cfg(not(target_arch = "wasm32"))]
        let observed = block_on_remote(remote_store.observe_ref(&head_path))?;
        #[cfg(not(target_arch = "wasm32"))]
        let remote_head_raw = observed.bytes().cloned();
        #[cfg(target_arch = "wasm32")]
        let remote_head_raw = block_on_remote(remote_store.get_raw(&head_path))?;
        let remote_head_raw = remote_head_raw.ok_or_else(|| RepoErr::RemoteBranchNotFound {
            remote: remote.to_string(),
            branch: remote_branch.to_string(),
        })?;
        #[cfg(not(target_arch = "wasm32"))]
        let remote_head = parse_remote_ref(&head_path, remote_head_raw)?;
        #[cfg(target_arch = "wasm32")]
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

        #[cfg(not(target_arch = "wasm32"))]
        let deletion =
            block_on_remote(remote_store.compare_and_delete_observed_ref(&head_path, &observed));
        #[cfg(target_arch = "wasm32")]
        let deletion = block_on_remote(
            remote_store.compare_and_delete_raw(&head_path, Some(remote_head_raw.as_ref())),
        );
        match deletion {
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
        // Snapshot hydration runs on the retained runtime after repository objects are fetched on
        // the repository runtime. Do not carry a pooled HTTP/1 connection across that boundary:
        // some proxies leave it readable but unusable for the first storage-commit request.
        self.remote_credentials.reset_http_clients();
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
        // Keep the same object-fetch to snapshot-hydration boundary as branch-based pulls.
        self.remote_credentials.reset_http_clients();
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
