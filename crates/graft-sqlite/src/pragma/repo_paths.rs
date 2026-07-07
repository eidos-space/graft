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
    let path = path.trim();
    if path.is_empty() {
        return Ok(String::new());
    }
    let path_obj = Path::new(path);
    if path_obj.is_absolute() {
        return Ok(normalize_repo_path_filter(&repo.file_key(path_obj)?));
    }
    Ok(normalize_repo_path_filter(path))
}

pub(super) fn normalize_repo_path_filter(path: &str) -> String {
    let path = path.trim().trim_start_matches("./").replace('\\', "/");
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

pub(super) fn repo_input_path(repo: &Repository, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.worktree().join(path)
    }
}
