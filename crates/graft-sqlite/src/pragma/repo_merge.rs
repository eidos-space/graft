use super::*;

pub(super) fn run_repo_merge_abort(
    runtime: &Runtime,
    file: &mut RepositorySessionContext,
) -> Result<RepoMergeAbortCommandOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot abort merge while there is an open transaction");
    }
    let repo = repo_for_file(file)?;
    let plan = repo.plan_merge_abort()?;
    let previous_files = current_repo_files_for_checkout(&repo)?;
    let previous_artifacts = current_repo_artifacts_for_checkout(&repo)?;
    let paths = checkout_plan_path_actions(&plan.checkout, &previous_files, &previous_artifacts);
    let mut _sqlite_replacement_guards =
        preflight_workspace_checkout(&repo, &plan.checkout, &previous_files)?;
    let target = repo.apply_merge_abort_plan(&plan)?;
    release_sqlite_guards_for_filesystem_change(&mut _sqlite_replacement_guards);
    checkout_repo_plan(
        runtime,
        file,
        &repo,
        &plan.checkout,
        &previous_files,
        &previous_artifacts,
        None,
    )?;
    clear_row_conflict_resolution_state(&repo)?;
    file.clear_row_merge_table_summaries();
    let branch = repo.current_branch()?;
    Ok(RepoMergeAbortCommandOutcome { target, branch, paths })
}

pub(super) fn run_repo_merge_continue(
    runtime: &Runtime,
    file: &mut RepositorySessionContext,
    message: String,
) -> Result<RepoCommitOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot continue merge while there is an open transaction");
    }
    let repo = repo_for_file(file)?;
    if repo.status()?.merge_head.is_none() {
        return pragma_err!("no merge in progress");
    }
    try_row_auto_merge_current_file_status_conflict(runtime, file, &repo, None)?;
    let conflicted = repo.status()?.conflicted;
    try_row_auto_merge_paths(runtime, file, &repo, &conflicted, None, false)?;
    let tables = staged_commit_table_summary_for_file(runtime, file, &repo)?;
    let commit = repo.commit_staged_with_table_summary(message, tables)?;
    let materialized = materialize_commit_sqlite_files(runtime, &repo, &commit)?;
    clear_row_conflict_resolution_state(&repo)?;
    file.clear_row_merge_table_summaries();
    let branch = repo.current_branch()?;
    Ok(RepoCommitOutcome { commit, branch, materialized })
}

pub(super) fn run_repo_merge(
    runtime: &Runtime,
    file: &mut RepositorySessionContext,
    rev: &str,
) -> Result<RepoMergeCommandOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot merge while there is an open transaction");
    }
    let repo = repo_for_file(file)?;
    if repo_has_work_in_progress_for_file(runtime, file, &repo)? {
        return pragma_err!("cannot merge with staged or unstaged changes");
    }
    clear_row_conflict_resolution_state(&repo)?;
    file.clear_row_merge_table_summaries();
    let plan = repo.plan_merge_revision(rev)?;
    // A merge may target a fetched remote-tracking revision whose SQLite storage commits are
    // not hydrated yet. Prefer local storage for base/ours and fall back to the configured remote
    // for missing target commits, keeping ordinary local-branch merges offline-capable.
    let remote = repo_merge_remote_store(&repo, rev, &plan.target)?;
    let plan = prepare_repo_merge_plan(runtime, &plan, remote)?;
    ensure_checkout_plan_preserves_untracked_paths(runtime, file, &repo, &plan.checkout)?;
    let previous_files = current_repo_files_for_checkout(&repo)?;
    let previous_artifacts = current_repo_artifacts_for_checkout(&repo)?;
    let mut _sqlite_replacement_guards =
        preflight_workspace_checkout(&repo, &plan.checkout, &previous_files)?;
    let mut outcome = repo.apply_merge_plan(&plan)?;
    release_sqlite_guards_for_filesystem_change(&mut _sqlite_replacement_guards);
    checkout_merge_outcome(
        runtime,
        file,
        &repo,
        &outcome,
        Some(&plan.checkout),
        &previous_files,
        &previous_artifacts,
        None,
    )?;
    initialize_merge_resolution_state(&repo)?;
    let mut row_auto_merge = match try_row_auto_merge_current_file_conflict(
        runtime, file, &repo, &outcome, None, true,
    ) {
        Ok(row_auto_merge) => row_auto_merge,
        Err(err) => {
            tracing::warn!("row-level auto-merge unavailable: {err}");
            None
        }
    };
    if let Some(row_auto_merge) = &row_auto_merge
        && row_auto_merge.resolved
    {
        outcome = merge_outcome_with_row_auto_merge(&outcome, &row_auto_merge.key);
    }
    match try_row_auto_merge_conflicts(runtime, file, &repo, &outcome, None, true) {
        Ok(results) => {
            for result in results {
                if result.resolved {
                    outcome = merge_outcome_with_row_auto_merge(&outcome, &result.key);
                }
                if row_auto_merge.is_none() {
                    row_auto_merge = Some(result);
                }
            }
        }
        Err(err) => tracing::warn!("SQLite auto-merge unavailable: {err}"),
    }
    let paths = merge_path_actions(
        &repo,
        &outcome,
        Some(&plan.checkout),
        &previous_files,
        &previous_artifacts,
    )?;
    let branch = repo.current_branch()?;
    Ok(RepoMergeCommandOutcome { outcome, branch, paths, row_auto_merge })
}

fn repo_merge_remote_store(
    repo: &Repository,
    rev: &str,
    target: &str,
) -> Result<Option<Arc<Remote>>, ErrCtx> {
    let remote_branches = repo.remote_tracking_branches()?;
    let remote_ref = rev.strip_prefix("refs/remotes/").unwrap_or(rev);

    if let Some(branch) = remote_branches
        .iter()
        .find(|branch| format!("{}/{}", branch.remote, branch.branch) == remote_ref)
    {
        return Ok(Some(Arc::new(repo.remote_store(&branch.remote)?)));
    }

    let mut target_remotes = remote_branches
        .iter()
        .filter(|branch| branch.head == target)
        .map(|branch| branch.remote.as_str());
    if let Some(remote) = target_remotes.next()
        && target_remotes.all(|candidate| candidate == remote)
    {
        return Ok(Some(Arc::new(repo.remote_store(remote)?)));
    }

    Ok(repo_default_remote_store(repo))
}

pub(super) fn merge_fast_forward_head(outcome: &MergeOutcome) -> Option<String> {
    match outcome {
        MergeOutcome::FastForward { to, .. } => Some(to.clone()),
        MergeOutcome::AlreadyUpToDate { .. } | MergeOutcome::Merged { .. } => None,
    }
}
