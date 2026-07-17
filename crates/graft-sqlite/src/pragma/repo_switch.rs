use super::*;

pub(super) fn run_repo_switch_branch(
    runtime: &Runtime,
    file: &mut VolFile,
    name: String,
    force: bool,
) -> Result<RepoSwitchOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot switch branches while there is an open transaction");
    }
    let _workspace_checkout = begin_workspace_checkout(file)?;
    let repo = repo_for_file(file)?;
    let plan = repo.plan_switch_branch(&name)?;
    prepare_repo_switch_checkout(runtime, file, &repo, &plan, force)?;
    let previous_files = current_repo_files_for_checkout(&repo)?;
    let previous_artifacts = current_repo_artifacts_for_checkout(&repo)?;
    let paths = checkout_plan_path_actions(&plan, &previous_files, &previous_artifacts);
    preflight_workspace_checkout(&repo, &plan, &previous_files)?;
    repo.apply_switch_branch_plan(&name, &plan)?;
    checkout_repo_plan(
        runtime,
        file,
        &repo,
        &plan,
        &previous_files,
        &previous_artifacts,
        None,
    )?;
    Ok(RepoSwitchOutcome { branch: name, target: plan.target, paths })
}

pub(super) fn run_repo_switch_create(
    runtime: &Runtime,
    file: &mut VolFile,
    name: String,
    start_point: Option<String>,
    force: bool,
) -> Result<RepoSwitchCreateOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot switch branches while there is an open transaction");
    }
    let _workspace_checkout = begin_workspace_checkout(file)?;
    let repo = repo_for_file(file)?;
    let plan = repo.plan_switch_new_branch(&name, start_point.as_deref())?;
    prepare_repo_switch_checkout(runtime, file, &repo, &plan.checkout, force)?;
    let previous_files = current_repo_files_for_checkout(&repo)?;
    let previous_artifacts = current_repo_artifacts_for_checkout(&repo)?;
    let paths = checkout_plan_path_actions(&plan.checkout, &previous_files, &previous_artifacts);
    preflight_workspace_checkout(&repo, &plan.checkout, &previous_files)?;
    let branch = repo.apply_switch_new_branch_plan(&plan)?;
    checkout_repo_plan(
        runtime,
        file,
        &repo,
        &plan.checkout,
        &previous_files,
        &previous_artifacts,
        None,
    )?;
    Ok(RepoSwitchCreateOutcome { branch, paths })
}

pub(super) fn prepare_repo_switch_checkout(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    plan: &CheckoutPlan,
    force: bool,
) -> Result<(), ErrCtx> {
    if repo_has_work_in_progress_for_file(runtime, file, repo)? {
        if force {
            repo.discard_work_in_progress()?;
        } else {
            return pragma_err!("cannot switch branches with staged or unstaged changes");
        }
    }
    if !force {
        ensure_checkout_plan_preserves_untracked_paths(runtime, file, repo, plan)?;
    }
    verify_repo_checkout_plan(runtime, plan, None)
}
