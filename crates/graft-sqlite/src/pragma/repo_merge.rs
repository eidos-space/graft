use super::*;

pub(super) fn run_repo_merge_abort(
    runtime: &Runtime,
    file: &mut VolFile,
) -> Result<RepoMergeAbortCommandOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot abort merge while there is an open transaction");
    }
    let _workspace_checkout = begin_workspace_checkout(file)?;
    let repo = repo_for_file(file)?;
    let plan = repo.plan_merge_abort()?;
    let previous_files = current_repo_files_for_checkout(&repo)?;
    let previous_artifacts = current_repo_artifacts_for_checkout(&repo)?;
    let paths = checkout_plan_path_actions(&plan.checkout, &previous_files, &previous_artifacts);
    preflight_workspace_checkout(&repo, &plan.checkout, &previous_files)?;
    let target = repo.apply_merge_abort_plan(&plan)?;
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
    let branch = repo.current_branch()?;
    Ok(RepoMergeAbortCommandOutcome { target, branch, paths })
}

pub(super) fn run_repo_merge_continue(
    runtime: &Runtime,
    file: &mut VolFile,
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
    let tables = staged_commit_table_summary(runtime, &repo)?;
    let commit = repo.commit_staged_with_table_summary(message, tables)?;
    let materialized = materialize_commit_sqlite_files(runtime, &repo, &commit)?;
    clear_row_conflict_resolution_state(&repo)?;
    let branch = repo.current_branch()?;
    Ok(RepoCommitOutcome { commit, branch, materialized })
}

pub(super) fn run_repo_merge(
    runtime: &Runtime,
    file: &mut VolFile,
    rev: &str,
) -> Result<RepoMergeCommandOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot merge while there is an open transaction");
    }
    let _workspace_checkout = begin_workspace_checkout(file)?;
    let repo = repo_for_file(file)?;
    if repo_has_work_in_progress_for_file(runtime, file, &repo)? {
        return pragma_err!("cannot merge with staged or unstaged changes");
    }
    clear_row_conflict_resolution_state(&repo)?;
    let plan = repo.plan_merge_revision(rev)?;
    let plan = prepare_repo_merge_plan(runtime, &plan, None)?;
    ensure_checkout_plan_preserves_untracked_paths(runtime, file, &repo, &plan.checkout)?;
    let previous_files = current_repo_files_for_checkout(&repo)?;
    let previous_artifacts = current_repo_artifacts_for_checkout(&repo)?;
    preflight_workspace_checkout(&repo, &plan.checkout, &previous_files)?;
    let mut outcome = repo.apply_merge_plan(&plan)?;
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
    let row_auto_merge =
        match try_row_auto_merge_current_file_conflict(runtime, file, &repo, &outcome, None) {
            Ok(row_auto_merge) => row_auto_merge,
            Err(err) => {
                tracing::warn!("row-level auto-merge unavailable: {err}");
                None
            }
        };
    if let Some(row_auto_merge) = &row_auto_merge {
        outcome = merge_outcome_with_row_auto_merge(&outcome, &row_auto_merge.key);
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

pub(super) fn merge_fast_forward_head(outcome: &MergeOutcome) -> Option<String> {
    match outcome {
        MergeOutcome::FastForward { to, .. } => Some(to.clone()),
        MergeOutcome::AlreadyUpToDate { .. } | MergeOutcome::Merged { .. } => None,
    }
}
