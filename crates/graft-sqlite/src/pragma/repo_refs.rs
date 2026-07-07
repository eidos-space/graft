use super::*;

pub(super) fn run_repo_branch_create(
    file: &mut VolFile,
    name: String,
    start_point: Option<String>,
) -> Result<BranchInfo, ErrCtx> {
    let repo = repo_for_file(file)?;
    if start_point.is_some() || repo.status()?.head_target.is_some() {
        repo.branch_create(&name, start_point.as_deref())
            .map_err(Into::into)
    } else {
        repo.branch_create_unborn(&name).map_err(Into::into)
    }
}

pub(super) fn run_repo_branch_delete(
    file: &mut VolFile,
    name: String,
    force: bool,
) -> Result<BranchInfo, ErrCtx> {
    let repo = repo_for_file(file)?;
    repo.branch_delete(&name, force).map_err(Into::into)
}

pub(super) fn run_repo_branch_rename(
    file: &mut VolFile,
    old: Option<String>,
    new: String,
    force: bool,
) -> Result<(String, BranchInfo), ErrCtx> {
    let repo = repo_for_file(file)?;
    let old = match old {
        Some(old) => old,
        None => repo.current_branch()?.ok_or_else(|| {
            ErrCtx::PragmaErr("cannot rename current branch in detached HEAD".into())
        })?,
    };
    let branch = repo.branch_rename(&old, &new, force)?;
    Ok((old, branch))
}

pub(super) fn run_repo_branch_upstream(
    file: &mut VolFile,
    branch: Option<String>,
    remote: String,
    remote_branch: String,
) -> Result<BranchInfo, ErrCtx> {
    let repo = repo_for_file(file)?;
    let branch = current_or_named_branch(&repo, branch, "set upstream")?;
    repo.set_branch_upstream(&branch, &remote, &remote_branch)
        .map_err(Into::into)
}

pub(super) fn run_repo_branch_unset_upstream(
    file: &mut VolFile,
    branch: Option<String>,
) -> Result<BranchInfo, ErrCtx> {
    let repo = repo_for_file(file)?;
    let branch = current_or_named_branch(&repo, branch, "unset upstream")?;
    repo.unset_branch_upstream(&branch).map_err(Into::into)
}

pub(super) fn current_or_named_branch(
    repo: &Repository,
    branch: Option<String>,
    action: &'static str,
) -> Result<String, ErrCtx> {
    match branch {
        Some(branch) => Ok(branch),
        None => repo
            .current_branch()?
            .ok_or_else(|| ErrCtx::PragmaErr(format!("cannot {action} in detached HEAD").into())),
    }
}

pub(super) fn json_branch_mutation_outcome(
    file: &mut VolFile,
    operation: &'static str,
    branch: BranchInfo,
    old_branch: Option<String>,
) -> Result<JsonBranchMutationOutcome, ErrCtx> {
    let repo = repo_for_file(file)?;
    let (current_head, current_branch) = repo_head_and_branch(&repo)?;
    Ok(JsonBranchMutationOutcome {
        operation,
        current_head,
        current_branch,
        branch,
        old_branch,
    })
}

pub(super) fn json_tag_mutation_outcome(
    file: &mut VolFile,
    operation: &'static str,
    tag: TagInfo,
) -> Result<JsonTagMutationOutcome, ErrCtx> {
    let repo = repo_for_file(file)?;
    let (current_head, current_branch) = repo_head_and_branch(&repo)?;
    Ok(JsonTagMutationOutcome {
        operation,
        current_head,
        current_branch,
        tag,
    })
}

pub(super) fn json_remote_mutation_outcome(
    file: &mut VolFile,
    operation: &'static str,
    remote: RemoteInfo,
    old_name: Option<String>,
) -> Result<JsonRemoteMutationOutcome, ErrCtx> {
    let repo = repo_for_file(file)?;
    let (current_head, current_branch) = repo_head_and_branch(&repo)?;
    Ok(JsonRemoteMutationOutcome {
        operation,
        current_head,
        current_branch,
        remote: json_remote_info(remote),
        old_name,
    })
}

pub(super) fn run_repo_tag_create(
    file: &mut VolFile,
    name: String,
    target: Option<String>,
    message: Option<String>,
) -> Result<TagInfo, ErrCtx> {
    let repo = repo_for_file(file)?;
    match message {
        Some(message) => repo
            .tag_create_annotated(&name, target.as_deref(), message)
            .map_err(Into::into),
        None => repo
            .tag_create(&name, target.as_deref())
            .map_err(Into::into),
    }
}

pub(super) fn run_repo_tag_delete(file: &mut VolFile, name: String) -> Result<TagInfo, ErrCtx> {
    let repo = repo_for_file(file)?;
    repo.tag_delete(&name).map_err(Into::into)
}
