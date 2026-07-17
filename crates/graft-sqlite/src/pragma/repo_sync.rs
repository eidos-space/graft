use super::*;

pub(super) fn run_repo_fetch(
    repo: &Repository,
    remote: Option<String>,
    branch: Option<String>,
    refspec: Option<String>,
    all: bool,
) -> Result<String, ErrCtx> {
    let outcome = run_repo_fetch_outcome(repo, remote, branch, refspec, all)?;
    format_fetch_command_outcome(&outcome)
}

pub(super) fn run_repo_fetch_json(
    repo: &Repository,
    remote: Option<String>,
    branch: Option<String>,
    refspec: Option<String>,
    all: bool,
) -> Result<String, ErrCtx> {
    let outcome = run_repo_fetch_outcome(repo, remote, branch, refspec, all)?;
    to_json(&json_fetch_command_outcome(repo, &outcome)?)
}

pub(super) fn run_repo_fetch_outcome(
    repo: &Repository,
    remote: Option<String>,
    branch: Option<String>,
    refspec: Option<String>,
    all: bool,
) -> Result<FetchCommandOutcome, ErrCtx> {
    if let Some(refspec) = refspec {
        let remote = repo_default_remote(repo, remote)?;
        let outcome = repo.fetch_refspec(&remote, &refspec)?;
        Ok(FetchCommandOutcome::Many(outcome))
    } else if all {
        let remote = repo_default_remote(repo, remote)?;
        let outcome = repo.fetch_all(&remote)?;
        Ok(FetchCommandOutcome::Many(outcome))
    } else {
        let upstream = repo_remote_branch(repo, remote, branch)?;
        let outcome = repo.fetch(&upstream.remote, &upstream.branch)?;
        Ok(FetchCommandOutcome::One(outcome))
    }
}

pub(super) fn format_fetch_command_outcome(
    outcome: &FetchCommandOutcome,
) -> Result<String, ErrCtx> {
    match outcome {
        FetchCommandOutcome::One(outcome) => Ok(format!(
            "Fetched {}/{} at {} ({} new commits)",
            outcome.remote,
            outcome.branch,
            &outcome.head[..12],
            outcome.commits
        )),
        FetchCommandOutcome::Many(outcome) => format_fetch_all_outcome(outcome),
    }
}

pub(super) fn json_fetch_command_outcome(
    repo: &Repository,
    outcome: &FetchCommandOutcome,
) -> Result<JsonFetchCommandOutcome, ErrCtx> {
    let (current_head, current_branch) = repo_head_and_branch(repo)?;
    Ok(JsonFetchCommandOutcome {
        operation: "fetch",
        current_head,
        current_branch,
        remote: outcome.remote(),
        branches: outcome.branches(),
        commits: outcome.commits(),
    })
}

pub(super) fn run_repo_pull(
    runtime: &Runtime,
    file: &mut VolFile,
    remote: Option<String>,
    branch: Option<String>,
    refspec: Option<String>,
    all: bool,
) -> Result<RepoPullCommandOutcome, ErrCtx> {
    let repo = repo_for_file(file)?;
    if all {
        return pragma_err!("pull does not support --all; fetch --all first, then pull one branch");
    }
    if !file.is_idle() {
        return pragma_err!("cannot pull while there is an open transaction");
    }
    let _workspace_checkout = begin_workspace_checkout(file)?;
    if repo_has_work_in_progress_for_file(runtime, file, &repo)? {
        return pragma_err!("cannot pull with staged or unstaged changes");
    }
    let local_branch = repo
        .current_branch()?
        .ok_or_else(|| ErrCtx::PragmaErr("cannot pull in detached HEAD".into()))?;
    let (remote, mut plan) = if let Some(refspec) = refspec {
        let remote = repo_default_remote(&repo, remote)?;
        let plan = repo.plan_pull_refspec(&remote, &refspec, &local_branch)?;
        (remote, plan)
    } else {
        let upstream = repo_remote_branch(&repo, remote, branch)?;
        let plan = repo.plan_pull(&upstream.remote, &upstream.branch, &local_branch)?;
        (upstream.remote, plan)
    };
    let checkout_remote = Arc::new(repo.remote_store(&remote)?);
    plan.merge = prepare_repo_merge_plan(runtime, &plan.merge, Some(checkout_remote.clone()))?;
    ensure_checkout_plan_preserves_untracked_paths(runtime, file, &repo, &plan.merge.checkout)?;
    let previous_files = current_repo_files_for_checkout(&repo)?;
    let previous_artifacts = current_repo_artifacts_for_checkout(&repo)?;
    preflight_workspace_checkout(&repo, &plan.merge.checkout, &previous_files)?;
    clear_row_conflict_resolution_state(&repo)?;
    let mut outcome = repo.apply_pull_plan(&plan)?;
    checkout_merge_outcome(
        runtime,
        file,
        &repo,
        &outcome.merge,
        Some(&plan.merge.checkout),
        &previous_files,
        &previous_artifacts,
        Some(checkout_remote.clone()),
    )?;
    if let Ok(Some(row_auto_merge)) = try_row_auto_merge_current_file_conflict(
        runtime,
        file,
        &repo,
        &outcome.merge,
        Some(checkout_remote),
    ) {
        outcome.merge = merge_outcome_with_row_auto_merge(&outcome.merge, &row_auto_merge.key);
    }
    let paths = merge_path_actions(
        &repo,
        &outcome.merge,
        Some(&plan.merge.checkout),
        &previous_files,
        &previous_artifacts,
    )?;
    let status = repo.status()?;
    let current_head = status.head_target;
    let current_branch = repo.current_branch()?;
    Ok(RepoPullCommandOutcome {
        outcome,
        current_head,
        current_branch,
        paths,
    })
}

pub(super) fn run_repo_push(
    runtime: &Runtime,
    repo: &Repository,
    remote: Option<String>,
    branch: Option<String>,
    refspec: Option<String>,
    all: bool,
    force: bool,
) -> Result<PushCommandOutcome, ErrCtx> {
    if let Some(refspec) = refspec {
        let remote = repo_default_remote(repo, remote)?;
        publish_repo_refspec_snapshots(runtime, repo, &remote, &refspec)?;
        let outcome = repo.push_refspec_with_force(&remote, &refspec, force)?;
        Ok(PushCommandOutcome::Many(outcome))
    } else if all {
        let remote = repo_default_remote(repo, remote)?;
        publish_repo_all_branch_snapshots(runtime, repo, &remote)?;
        let outcome = repo.push_all_with_force(&remote, force)?;
        Ok(PushCommandOutcome::Many(outcome))
    } else {
        let (remote, local_branch, remote_branch) = repo_push_branches(repo, remote, branch)?;
        let remote_head = repo.remote_branch_head_state(&remote, &remote_branch)?;
        let tracking_head = if !force {
            repo.remote_tracking_ref(&remote, &remote_branch)?
        } else {
            None
        };
        publish_repo_branch_snapshots(
            runtime,
            repo,
            &remote,
            &local_branch,
            tracking_head.as_deref().or(remote_head.head.as_deref()),
        )?;
        let outcome = repo.push_branch_with_force_and_remote_head(
            &remote,
            &local_branch,
            &remote_branch,
            force,
            remote_head,
        )?;
        Ok(PushCommandOutcome::One(outcome))
    }
}

pub(super) fn format_push_command_outcome(outcome: &PushCommandOutcome) -> Result<String, ErrCtx> {
    match outcome {
        PushCommandOutcome::One(outcome) => Ok(format!(
            "{} {}/{} to {} ({} commits)",
            if outcome.forced {
                "Force-pushed"
            } else {
                "Pushed"
            },
            outcome.remote,
            outcome.remote_branch,
            &outcome.head[..12],
            outcome.commits
        )),
        PushCommandOutcome::Many(outcome) => format_push_all_outcome(outcome),
    }
}

pub(super) fn json_push_command_outcome(
    repo: &Repository,
    outcome: &PushCommandOutcome,
) -> Result<JsonPushCommandOutcome, ErrCtx> {
    let (current_head, current_branch) = repo_head_and_branch(repo)?;
    Ok(JsonPushCommandOutcome {
        operation: "push",
        current_head,
        current_branch,
        remote: outcome.remote(),
        branches: outcome.branches(),
        commits: outcome.commits(),
        forced: outcome.forced(),
    })
}

pub(super) fn reset_mode_label(mode: ResetMode) -> &'static str {
    match mode {
        ResetMode::Soft => "soft",
        ResetMode::Mixed => "mixed",
        ResetMode::Hard => "hard",
    }
}

pub(super) fn repo_remote_branch(
    repo: &Repository,
    remote: Option<String>,
    branch: Option<String>,
) -> Result<BranchUpstream, ErrCtx> {
    Ok(repo.default_remote_branch(remote.as_deref(), branch.as_deref())?)
}

pub(super) fn repo_default_remote(
    repo: &Repository,
    remote: Option<String>,
) -> Result<String, ErrCtx> {
    Ok(repo.default_remote_branch(remote.as_deref(), None)?.remote)
}

pub(super) fn repo_push_branches(
    repo: &Repository,
    remote: Option<String>,
    branch: Option<String>,
) -> Result<(String, String, String), ErrCtx> {
    let current_branch = repo
        .current_branch()?
        .ok_or_else(|| ErrCtx::PragmaErr("cannot push in detached HEAD".into()))?;
    let upstream = repo.default_remote_branch(remote.as_deref(), branch.as_deref())?;
    let local_branch = branch.unwrap_or(current_branch);
    Ok((upstream.remote, local_branch, upstream.branch))
}
