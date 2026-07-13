use super::*;

pub(super) fn repo_diff_path(
    repo: &Repository,
    path: Option<&str>,
) -> Result<Option<String>, ErrCtx> {
    let Some(path) = path else {
        return Ok(None);
    };
    Ok(Some(repo_path_arg(repo, path)?))
}

pub(super) fn repo_path_arg(repo: &Repository, path: &str) -> Result<String, ErrCtx> {
    if path.is_empty() {
        return Ok(String::new());
    }
    let path_obj = Path::new(path);
    if path_obj.is_absolute() {
        return Ok(normalize_repo_path_filter(&repo.file_key(path_obj)?));
    }
    graft::repo::validate_repo_path_identity(path)?;
    Ok(normalize_repo_path_filter(path))
}

pub(super) fn normalize_repo_path_filter(path: &str) -> String {
    let path = path.trim_start_matches("./");
    #[cfg(windows)]
    let path = path.replace('\\', "/");
    let path = path.trim_end_matches('/');
    if path == "." {
        String::new()
    } else {
        path.to_string()
    }
}

pub(super) fn repo_physical_path_arg(
    repo: &Repository,
    path: &Path,
) -> Result<(String, PathBuf), ErrCtx> {
    let physical_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.worktree().join(path)
    };
    let key = repo.file_key(&physical_path)?;
    Ok((key.clone(), repo.worktree().join(key)))
}

pub(super) fn repo_restore_path_arg(
    repo: &Repository,
    path: &Path,
) -> Result<(String, PathBuf), ErrCtx> {
    let physical_path = repo_input_path(repo, path);
    let key = match repo.file_key(&physical_path) {
        Ok(key) => key,
        Err(graft::repo::RepoErr::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            repo_key_with_missing_parent(repo, &physical_path)?
        }
        Err(err) => return Err(err.into()),
    };
    Ok((key.clone(), repo.worktree().join(key)))
}

fn repo_key_with_missing_parent(repo: &Repository, path: &Path) -> Result<String, ErrCtx> {
    let file_name = path
        .file_name()
        .ok_or_else(|| path_outside_worktree(repo, path))?;
    let mut parent = path
        .parent()
        .ok_or_else(|| path_outside_worktree(repo, path))?;
    let mut missing = Vec::new();

    let existing_parent = loop {
        match std::fs::symlink_metadata(parent) {
            Ok(_) => break std::fs::canonicalize(parent)?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let component = parent
                    .file_name()
                    .ok_or_else(|| path_outside_worktree(repo, path))?;
                missing.push(component.to_os_string());
                parent = parent
                    .parent()
                    .ok_or_else(|| path_outside_worktree(repo, path))?;
            }
            Err(err) => return Err(err.into()),
        }
    };
    if !existing_parent.starts_with(repo.worktree()) {
        return Err(path_outside_worktree(repo, path));
    }

    let mut canonical_parent = existing_parent;
    for component in missing.into_iter().rev() {
        canonical_parent.push(component);
    }
    let absolute = canonical_parent.join(file_name);
    let relative = absolute
        .strip_prefix(repo.worktree())
        .map_err(|_| path_outside_worktree(repo, path))?;
    if relative
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(path_outside_worktree(repo, path));
    }
    relative
        .to_str()
        .map(|path| path.replace('\\', "/"))
        .ok_or_else(|| graft::repo::RepoErr::NonUtf8Path(relative.to_path_buf()).into())
}

fn path_outside_worktree(repo: &Repository, path: &Path) -> ErrCtx {
    graft::repo::RepoErr::PathOutsideWorktree {
        path: path.to_path_buf(),
        worktree: repo.worktree().to_path_buf(),
    }
    .into()
}

pub(super) fn repo_input_path(repo: &Repository, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.worktree().join(path)
    }
}
