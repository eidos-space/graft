use super::*;

pub(super) fn run_repo_init(
    file: &mut VolFile,
    spec: RepoInitSpec,
) -> Result<RepoInitOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot initialize a repository while there is an open transaction");
    }
    let repo = match spec.worktree {
        Some(worktree) => Repository::init(resolve_repo_worktree_arg(file, &worktree)?)?,
        None => Repository::init_for_file(&file.tag)?,
    };
    let preserved_contents = file.attach_repo_preserving_contents(repo.clone())?;
    if preserved_contents {
        repo.mark_dirty_path(&file.tag)?;
    }
    let path = repo.file_key(&file.tag)?;
    let (current_head, current_branch) = repo_head_and_branch(&repo)?;
    Ok(RepoInitOutcome {
        graft_dir: repo.graft_dir().to_path_buf(),
        worktree: repo.worktree().to_path_buf(),
        path,
        preserved_contents,
        current_head,
        current_branch,
    })
}

pub(super) fn resolve_repo_worktree_arg(
    file: &VolFile,
    worktree: &Path,
) -> Result<PathBuf, ErrCtx> {
    if worktree.is_absolute() {
        return Ok(worktree.to_path_buf());
    }
    let parent = Path::new(&file.tag)
        .parent()
        .unwrap_or_else(|| Path::new("."));
    Ok(parent.join(worktree))
}

pub(super) fn format_repo_init_outcome(outcome: &RepoInitOutcome) -> String {
    format!(
        "Initialized empty Graft repository in {}",
        outcome.graft_dir.display()
    )
}

pub(super) fn repo_for_file(file: &mut VolFile) -> Result<Repository, ErrCtx> {
    if let Some(repo) = &file.repo {
        return Ok(repo.clone());
    }

    if !should_discover_repo(&file.tag) {
        return Err(ErrCtx::Repo(graft::repo::RepoErr::NotFound(PathBuf::from(
            &file.tag,
        ))));
    }
    let repo = Repository::discover_for_file(&file.tag)?;
    file.repo = Some(repo.clone());
    Ok(repo)
}

pub(super) fn run_repo_clone(
    file: &mut VolFile,
    spec: RepoCloneSpec,
) -> Result<RepoCloneOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot clone while there is an open transaction");
    }
    if file.repo.is_some() || Repository::discover_for_file(&file.tag).is_ok() {
        return pragma_err!("cannot clone into an existing .graft repository");
    }

    let repo = match spec.worktree.as_ref() {
        Some(worktree) => Repository::init(resolve_repo_worktree_arg(file, worktree)?)?,
        None => Repository::init_for_file(&file.tag)?,
    };
    let graft_dir = repo.graft_dir().to_path_buf();
    let mut attached = false;
    let result = (|| {
        let remote_info = repo.remote_add("origin", spec.config)?;
        let branch = match spec.branch {
            Some(branch) => branch,
            None => repo
                .remote_default_branch("origin")?
                .unwrap_or(repo.default_branch()?),
        };
        let fetch = repo.fetch("origin", &branch)?;
        repo.branch_create(&branch, Some(&format!("refs/remotes/origin/{branch}")))?;
        repo.set_branch_upstream(&branch, "origin", &branch)?;
        let plan = repo.plan_switch_branch(&branch)?;
        file.attach_repo(repo.clone())?;
        attached = true;
        let runtime = file.runtime().clone();
        let remote = Arc::new(repo.remote_store("origin")?);
        let plan = prepare_repo_checkout_plan(&runtime, &plan, Some(remote.clone()))?;
        let previous_files = BTreeMap::new();
        let previous_artifacts = BTreeMap::new();
        let paths = checkout_plan_path_actions(&plan, &previous_files, &previous_artifacts);
        repo.apply_switch_branch_plan(&branch, &plan)?;
        checkout_repo_plan(
            &runtime,
            file,
            &repo,
            &plan,
            &previous_files,
            &previous_artifacts,
            Some(remote),
        )?;
        let (current_head, current_branch) = repo_head_and_branch(&repo)?;
        Ok(RepoCloneOutcome {
            remote: remote_info,
            current_head,
            current_branch,
            branch: fetch.branch,
            head: fetch.head,
            commits: fetch.commits,
            graft_dir: graft_dir.clone(),
            paths,
        })
    })();
    if result.is_err() && !attached {
        let _ = std::fs::remove_dir_all(graft_dir);
    }
    result
}
