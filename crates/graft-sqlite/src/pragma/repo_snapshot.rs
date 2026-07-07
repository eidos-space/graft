use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SnapshotHashPolicy {
    Strict,
    AllowHydratedMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RepoSnapshotPurpose {
    Checkout,
    Diff,
    Export,
    Merge,
    Push,
    Reset,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RepoSnapshotRemoteMode {
    LocalOnly,
    Remote,
    LocalThenRemote,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RepoSnapshotResolveSource {
    Local,
    Remote,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RepoSnapshotResolvePolicy {
    pub(super) purpose: RepoSnapshotPurpose,
    pub(super) remote_mode: RepoSnapshotRemoteMode,
    pub(super) hash_policy: SnapshotHashPolicy,
    pub(super) normalize: bool,
}

#[derive(Debug)]
pub(super) struct ResolvedRepoSnapshot {
    pub(super) snapshot: RepoSnapshot,
    pub(super) runtime_snapshot: graft::snapshot::Snapshot,
    pub(super) source: RepoSnapshotResolveSource,
    pub(super) hash_mismatches: usize,
}

pub(super) struct RepoSnapshotResolver<'a> {
    pub(super) runtime: &'a Runtime,
    pub(super) remote: Option<Arc<Remote>>,
    pub(super) policy: RepoSnapshotResolvePolicy,
}

pub(super) fn hydrate_repo_file_state(
    runtime: &Runtime,
    state: &CommitFileState,
    remote: Option<Arc<Remote>>,
) -> Result<(), ErrCtx> {
    hydrate_repo_file_state_for(runtime, state, remote, RepoSnapshotPurpose::Checkout)
}

pub(super) fn hydrate_repo_file_state_for(
    runtime: &Runtime,
    state: &CommitFileState,
    remote: Option<Arc<Remote>>,
    purpose: RepoSnapshotPurpose,
) -> Result<(), ErrCtx> {
    RepoSnapshotResolver::strict(runtime, remote, purpose)
        .resolve_file_state(state)
        .map(|_| ())
}

pub(super) fn prepare_repo_snapshot_for_push(
    runtime: &Runtime,
    snapshot: &RepoSnapshot,
) -> Result<(), ErrCtx> {
    RepoSnapshotResolver::normalizing(
        runtime,
        None,
        RepoSnapshotPurpose::Push,
        SnapshotHashPolicy::AllowHydratedMismatch,
    )
    .resolve_snapshot(snapshot)
    .map(|_| ())
}

pub(super) fn verify_repo_checkout_plan(
    runtime: &Runtime,
    plan: &CheckoutPlan,
    remote: Option<Arc<Remote>>,
) -> Result<(), ErrCtx> {
    RepoSnapshotResolver::strict(runtime, remote, RepoSnapshotPurpose::Checkout)
        .resolve_checkout_plan(plan)
        .map(|_| ())
}

pub(super) fn prepare_repo_checkout_plan(
    runtime: &Runtime,
    plan: &CheckoutPlan,
    remote: Option<Arc<Remote>>,
) -> Result<CheckoutPlan, ErrCtx> {
    let hash_policy = if remote.is_some() {
        SnapshotHashPolicy::AllowHydratedMismatch
    } else {
        SnapshotHashPolicy::Strict
    };
    RepoSnapshotResolver::normalizing(runtime, remote, RepoSnapshotPurpose::Checkout, hash_policy)
        .resolve_checkout_plan(plan)
}

pub(super) fn prepare_repo_checkout_plan_with_hash_policy(
    runtime: &Runtime,
    plan: &CheckoutPlan,
    remote: Option<Arc<Remote>>,
    hash_policy: SnapshotHashPolicy,
) -> Result<CheckoutPlan, ErrCtx> {
    RepoSnapshotResolver::normalizing(runtime, remote, RepoSnapshotPurpose::Reset, hash_policy)
        .resolve_checkout_plan(plan)
}

pub(super) fn prepare_repo_merge_plan(
    runtime: &Runtime,
    plan: &MergePlan,
    remote: Option<Arc<Remote>>,
) -> Result<MergePlan, ErrCtx> {
    let hash_policy = if remote.is_some() {
        SnapshotHashPolicy::AllowHydratedMismatch
    } else {
        SnapshotHashPolicy::Strict
    };
    RepoSnapshotResolver::normalizing(runtime, remote, RepoSnapshotPurpose::Merge, hash_policy)
        .resolve_merge_plan(plan)
}

impl<'a> RepoSnapshotResolver<'a> {
    pub(super) fn strict(
        runtime: &'a Runtime,
        remote: Option<Arc<Remote>>,
        purpose: RepoSnapshotPurpose,
    ) -> Self {
        let remote_mode = if remote.is_some() {
            RepoSnapshotRemoteMode::Remote
        } else {
            RepoSnapshotRemoteMode::LocalOnly
        };
        Self {
            runtime,
            remote,
            policy: RepoSnapshotResolvePolicy {
                purpose,
                remote_mode,
                hash_policy: SnapshotHashPolicy::Strict,
                normalize: false,
            },
        }
    }

    pub(super) fn normalizing(
        runtime: &'a Runtime,
        remote: Option<Arc<Remote>>,
        purpose: RepoSnapshotPurpose,
        hash_policy: SnapshotHashPolicy,
    ) -> Self {
        let remote_mode = if remote.is_some() {
            RepoSnapshotRemoteMode::Remote
        } else {
            RepoSnapshotRemoteMode::LocalOnly
        };
        Self {
            runtime,
            remote,
            policy: RepoSnapshotResolvePolicy {
                purpose,
                remote_mode,
                hash_policy,
                normalize: hash_policy != SnapshotHashPolicy::Strict,
            },
        }
    }

    pub(super) fn local_then_remote(
        runtime: &'a Runtime,
        remote: Option<Arc<Remote>>,
        purpose: RepoSnapshotPurpose,
        hash_policy: SnapshotHashPolicy,
    ) -> Self {
        Self {
            runtime,
            remote,
            policy: RepoSnapshotResolvePolicy {
                purpose,
                remote_mode: RepoSnapshotRemoteMode::LocalThenRemote,
                hash_policy,
                normalize: false,
            },
        }
    }

    pub(super) fn resolve_file_state(
        &self,
        state: &CommitFileState,
    ) -> Result<CommitFileState, ErrCtx> {
        let resolved = self.resolve_snapshot(&state.snapshot)?;
        Ok(CommitFileState {
            volume: state.volume.clone(),
            snapshot: resolved.snapshot,
        })
    }

    pub(super) fn resolve_checkout_plan(
        &self,
        plan: &CheckoutPlan,
    ) -> Result<CheckoutPlan, ErrCtx> {
        let mut plan = plan.clone();
        for state in plan.files.values_mut() {
            *state = self.resolve_file_state(state)?;
        }
        Ok(plan)
    }

    pub(super) fn resolve_merge_plan(&self, plan: &MergePlan) -> Result<MergePlan, ErrCtx> {
        let mut plan = plan.clone();
        if matches!(plan.outcome, MergeOutcome::FastForward { .. }) {
            plan.checkout = self.resolve_checkout_plan(&plan.checkout)?;
        }
        if let Some(index) = &mut plan.index {
            for entry in &mut index.entries {
                if let Some(state) = entry.file.clone() {
                    entry.file = Some(self.resolve_file_state(&state)?);
                }
            }
        }
        Ok(plan)
    }

    pub(super) fn resolve_snapshot(
        &self,
        snapshot: &RepoSnapshot,
    ) -> Result<ResolvedRepoSnapshot, ErrCtx> {
        if self.policy.remote_mode == RepoSnapshotRemoteMode::Remote {
            return self.resolve_snapshot_once(
                snapshot,
                RepoSnapshotResolveSource::Remote,
                self.remote.clone(),
            );
        }

        match self.resolve_snapshot_once(snapshot, RepoSnapshotResolveSource::Local, None) {
            Ok(resolved) => Ok(resolved),
            Err(local_err)
                if self.policy.remote_mode == RepoSnapshotRemoteMode::LocalThenRemote =>
            {
                let Some(remote) = self.remote.clone() else {
                    return Err(local_err);
                };
                self.resolve_snapshot_once(snapshot, RepoSnapshotResolveSource::Remote, Some(remote))
                    .map_err(|remote_err| {
                        ErrCtx::PragmaErr(
                            format!(
                                "local snapshot hydrate failed: {local_err}; remote snapshot hydrate failed: {remote_err}"
                            )
                            .into(),
                        )
                    })
            }
            Err(err) => Err(err),
        }
    }

    pub(super) fn resolve_snapshot_once(
        &self,
        snapshot: &RepoSnapshot,
        source: RepoSnapshotResolveSource,
        remote: Option<Arc<Remote>>,
    ) -> Result<ResolvedRepoSnapshot, ErrCtx> {
        let runtime_snapshot = snapshot.to_snapshot();
        if !runtime_snapshot.is_empty() {
            match source {
                RepoSnapshotResolveSource::Local => {
                    for range in &snapshot.ranges {
                        self.runtime.fetch_log(range.log.clone(), Some(range.end))?;
                    }
                    self.runtime.snapshot_hydrate(runtime_snapshot.clone())?;
                }
                RepoSnapshotResolveSource::Remote => {
                    let Some(remote) = remote else {
                        return Err(ErrCtx::PragmaErr(
                            "snapshot resolver remote source requires a remote".into(),
                        ));
                    };
                    self.runtime
                        .snapshot_hydrate_from(runtime_snapshot.clone(), remote)?;
                }
            }
        }

        let hash_mismatches =
            verify_repo_snapshot_commit_hashes(self.runtime, snapshot, self.policy.hash_policy)?;
        let resolved_snapshot =
            if self.policy.normalize && self.policy.hash_policy != SnapshotHashPolicy::Strict {
                repo_snapshot_with_commit_hashes(self.runtime, &runtime_snapshot)?
            } else {
                snapshot.clone()
            };
        let resolved = ResolvedRepoSnapshot {
            snapshot: resolved_snapshot,
            runtime_snapshot,
            source,
            hash_mismatches,
        };
        resolved.trace_if_needed(self.policy.purpose);
        Ok(resolved)
    }
}

impl ResolvedRepoSnapshot {
    pub(super) fn trace_if_needed(&self, purpose: RepoSnapshotPurpose) {
        if self.hash_mismatches > 0 {
            tracing::warn!(
                mismatches = self.hash_mismatches,
                source = ?self.source,
                purpose = ?purpose,
                "snapshot storage commit hashes mismatched; using hydrated storage commit hashes"
            );
        } else if matches!(self.source, RepoSnapshotResolveSource::Remote) {
            tracing::debug!(
                source = ?self.source,
                purpose = ?purpose,
                ranges = self.snapshot.ranges.len(),
                runtime_ranges = self.runtime_snapshot.iter().count(),
                pages = self.snapshot.page_count.to_u32(),
                "resolved repository snapshot from remote"
            );
        }
    }
}

pub(super) fn verify_repo_snapshot_commit_hashes(
    runtime: &Runtime,
    snapshot: &RepoSnapshot,
    hash_policy: SnapshotHashPolicy,
) -> Result<usize, ErrCtx> {
    let mut mismatches = 0_usize;
    for range in &snapshot.ranges {
        let mut expected_commits = range.commits.iter();
        for lsn in (range.start..=range.end).iter() {
            let Some(expected) = expected_commits.next() else {
                return Err(ErrCtx::PragmaErr(
                    format!(
                        "snapshot references missing storage commit hash for {:?}/{}",
                        range.log, lsn
                    )
                    .into(),
                ));
            };
            if expected.lsn != lsn {
                if expected.lsn > lsn {
                    return Err(ErrCtx::PragmaErr(
                        format!(
                            "snapshot references missing storage commit hash for {:?}/{}",
                            range.log, lsn
                        )
                        .into(),
                    ));
                }
                return Err(ErrCtx::PragmaErr(
                    format!(
                        "snapshot storage commit hash out of order for {:?}: expected LSN {}, got {}",
                        range.log, lsn, expected.lsn
                    )
                    .into(),
                ));
            }
            let Some(actual) = repo_storage_commit_hash(runtime, &range.log, lsn)? else {
                return Err(ErrCtx::PragmaErr(
                    format!(
                        "snapshot references missing storage commit {:?}/{}",
                        range.log, lsn
                    )
                    .into(),
                ));
            };
            if actual != expected.commit_hash {
                match hash_policy {
                    SnapshotHashPolicy::Strict => {
                        return Err(ErrCtx::PragmaErr(
                            format!(
                                "snapshot storage commit hash mismatch for {:?}/{}: expected {}, got {}",
                                range.log, lsn, expected.commit_hash, actual
                            )
                            .into(),
                        ));
                    }
                    SnapshotHashPolicy::AllowHydratedMismatch => {
                        mismatches += 1;
                    }
                }
            }
        }
        if let Some(extra) = expected_commits.next() {
            return Err(ErrCtx::PragmaErr(
                format!(
                    "snapshot references extra storage commit hash for {:?}/{} outside {}..={}",
                    range.log, extra.lsn, range.start, range.end
                )
                .into(),
            ));
        }
    }
    Ok(mismatches)
}

pub(super) fn repo_storage_commit_hash(
    runtime: &Runtime,
    log: &LogId,
    lsn: LSN,
) -> Result<Option<graft::core::commit_hash::CommitHash>, ErrCtx> {
    let Some(commit) = runtime.get_commit(log, lsn)? else {
        return Ok(None);
    };
    if let Some(commit_hash) = commit.commit_hash().cloned() {
        return Ok(Some(commit_hash));
    }
    runtime.commit_hash(log, lsn).map_err(ErrCtx::from)
}

pub(super) fn publish_repo_branch_snapshots(
    runtime: &Runtime,
    repo: &Repository,
    remote: &str,
    branch: &str,
    stop_at: Option<&str>,
) -> Result<(), ErrCtx> {
    let remote_store = Arc::new(repo.remote_store(remote)?);
    let mut stop_commits = BTreeSet::<String>::new();
    if let Some(stop_at) = stop_at {
        stop_commits.insert(stop_at.to_string());
    } else {
        stop_commits.extend(repo_remote_reachable_commits_known_locally(repo, remote)?);
    }
    let mut stack = vec![
        repo.branch_target(branch)?
            .ok_or(ErrCtx::Repo(graft::repo::RepoErr::UnbornHead))?,
    ];
    let mut seen = std::collections::BTreeSet::<String>::new();
    let mut snapshots = Vec::new();

    while let Some(next) = stack.pop() {
        if !seen.insert(next.clone()) {
            continue;
        }
        if stop_commits.contains(&next) {
            continue;
        }
        let commit = repo.read_commit(&next)?;
        let parent_files = repo_commit_parent_file_states(repo, &commit)?;
        for (path, state) in &commit.files {
            let snapshot = repo_file_delta_snapshot(
                state,
                parent_files.get(path).map(Vec::as_slice).unwrap_or(&[]),
            );
            let runtime_snapshot = snapshot.to_snapshot();
            if runtime_snapshot.is_empty() {
                continue;
            }
            prepare_repo_snapshot_for_push(runtime, &snapshot)?;
            snapshots.push(runtime_snapshot);
        }

        if commit.parents.is_empty() {
            if let Some(parent) = commit.parent {
                stack.push(parent);
            }
        } else {
            stack.extend(commit.parents);
        }
    }

    runtime.snapshots_push_to(snapshots, remote_store)?;

    Ok(())
}

pub(super) fn repo_remote_reachable_commits_known_locally(
    repo: &Repository,
    remote: &str,
) -> Result<BTreeSet<String>, ErrCtx> {
    let roots = repo
        .remote_branch_refs(remote)?
        .into_iter()
        .map(|branch| branch.head)
        .collect::<Vec<_>>();
    Ok(repo_reachable_commits_known_locally(repo, roots))
}

pub(super) fn repo_reachable_commits_known_locally(
    repo: &Repository,
    roots: impl IntoIterator<Item = String>,
) -> BTreeSet<String> {
    let mut reachable = BTreeSet::new();
    let mut stack = roots.into_iter().collect::<Vec<_>>();
    while let Some(next) = stack.pop() {
        if !reachable.insert(next.clone()) {
            continue;
        }
        let Ok(commit) = repo.read_commit(&next) else {
            continue;
        };
        stack.extend(repo_commit_parent_ids(&commit));
    }
    reachable
}

pub(super) fn repo_commit_parent_file_states(
    repo: &Repository,
    commit: &CommitObject,
) -> Result<BTreeMap<String, Vec<CommitFileState>>, ErrCtx> {
    let mut files = BTreeMap::<String, Vec<CommitFileState>>::new();
    for parent in repo_commit_parent_ids(commit) {
        for (path, state) in repo.read_commit(&parent)?.files {
            files.entry(path).or_default().push(state);
        }
    }
    Ok(files)
}

pub(super) fn repo_commit_parent_ids(commit: &CommitObject) -> Vec<String> {
    if commit.parents.is_empty() {
        commit.parent.iter().cloned().collect()
    } else {
        commit.parents.clone()
    }
}

pub(super) fn repo_file_delta_snapshot(
    state: &CommitFileState,
    parent_states: &[CommitFileState],
) -> RepoSnapshot {
    let coverage = repo_file_parent_coverage(state, parent_states);
    let mut ranges = Vec::new();
    for range in &state.snapshot.ranges {
        let intervals = coverage.get(&range.log).map(Vec::as_slice).unwrap_or(&[]);
        append_uncovered_repo_log_ranges(&mut ranges, range, intervals);
    }

    RepoSnapshot {
        page_count: state.snapshot.page_count,
        ranges,
    }
}

pub(super) fn repo_file_parent_coverage(
    state: &CommitFileState,
    parent_states: &[CommitFileState],
) -> BTreeMap<LogId, Vec<(LSN, LSN)>> {
    let mut coverage = BTreeMap::<LogId, Vec<(LSN, LSN)>>::new();
    for parent_state in parent_states {
        if parent_state.volume != state.volume {
            continue;
        }
        for range in &parent_state.snapshot.ranges {
            coverage
                .entry(range.log.clone())
                .or_default()
                .push((range.start, range.end));
        }
    }

    for intervals in coverage.values_mut() {
        intervals.sort_by_key(|(start, _)| *start);
        let mut merged = Vec::<(LSN, LSN)>::new();
        for (start, end) in intervals.drain(..) {
            if let Some((_, current_end)) = merged.last_mut() {
                if current_end.checked_next().is_none_or(|next| start <= next) {
                    if end > *current_end {
                        *current_end = end;
                    }
                    continue;
                }
            }
            merged.push((start, end));
        }
        *intervals = merged;
    }

    coverage
}

pub(super) fn append_uncovered_repo_log_ranges(
    ranges: &mut Vec<RepoLogRange>,
    range: &RepoLogRange,
    covered_intervals: &[(LSN, LSN)],
) {
    let mut cursor = Some(range.start);

    for (covered_start, covered_end) in covered_intervals {
        let Some(start) = cursor else {
            break;
        };
        if *covered_end < start {
            continue;
        }
        if *covered_start > range.end {
            break;
        }
        if *covered_start > start {
            let end = covered_start
                .checked_prev()
                .unwrap_or(range.end)
                .min(range.end);
            if start <= end {
                push_repo_log_range(ranges, range, start, end);
            }
        }
        if *covered_end >= range.end {
            cursor = None;
            break;
        }
        cursor = covered_end.checked_next();
    }

    if let Some(start) = cursor {
        if start <= range.end {
            push_repo_log_range(ranges, range, start, range.end);
        }
    }
}

pub(super) fn push_repo_log_range(
    ranges: &mut Vec<RepoLogRange>,
    source: &RepoLogRange,
    start: LSN,
    end: LSN,
) {
    ranges.push(RepoLogRange {
        log: source.log.clone(),
        start,
        end,
        commits: source
            .commits
            .iter()
            .filter(|commit| commit.lsn >= start && commit.lsn <= end)
            .cloned()
            .collect(),
    });
}

pub(super) fn publish_repo_all_branch_snapshots(
    runtime: &Runtime,
    repo: &Repository,
    remote: &str,
) -> Result<(), ErrCtx> {
    for branch in repo.branches()? {
        if branch.target.is_some() {
            let stop_at = repo.remote_branch_head(remote, &branch.name)?;
            publish_repo_branch_snapshots(runtime, repo, remote, &branch.name, stop_at.as_deref())?;
        }
    }
    Ok(())
}

pub(super) fn publish_repo_refspec_snapshots(
    runtime: &Runtime,
    repo: &Repository,
    remote: &str,
    refspec: &str,
) -> Result<(), ErrCtx> {
    for branch in repo.push_refspec_branches(refspec)? {
        let stop_at = repo.remote_branch_head(remote, &branch.remote_branch)?;
        publish_repo_branch_snapshots(
            runtime,
            repo,
            remote,
            &branch.local_branch,
            stop_at.as_deref(),
        )?;
    }
    Ok(())
}
