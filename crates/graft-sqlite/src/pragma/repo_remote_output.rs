use super::*;

pub(super) fn json_remote_info(remote: RemoteInfo) -> JsonRemoteInfo {
    let url = remote_config_uri(&remote.config);
    JsonRemoteInfo {
        name: remote.name,
        config: remote.config,
        url,
    }
}

pub(super) fn format_remote(remote: &RemoteInfo) -> String {
    format!(
        "Added remote '{}': {}",
        remote.name,
        remote_config_uri(&remote.config)
    )
}

pub(super) fn format_remotes(remotes: &[RemoteInfo]) -> Result<String, ErrCtx> {
    if remotes.is_empty() {
        return Ok("No remotes configured.".to_string());
    }

    let mut f = String::new();
    for remote in remotes {
        writeln!(
            &mut f,
            "{}\t{}",
            remote.name,
            remote_config_uri(&remote.config)
        )?;
    }
    Ok(f)
}

pub(super) fn format_remote_prune_outcome(outcome: &RemotePruneOutcome) -> Result<String, ErrCtx> {
    if outcome.branches.is_empty() {
        return Ok(format!(
            "Pruned {} (no stale remote-tracking branches)",
            outcome.remote
        ));
    }

    let mut f = String::new();
    writeln!(
        &mut f,
        "Pruned {} ({} {})",
        outcome.remote,
        outcome.branches.len(),
        pluralize!(outcome.branches.len(), "branch")
    )?;
    for branch in &outcome.branches {
        writeln!(&mut f, "  {}/{}", outcome.remote, branch)?;
    }
    Ok(f)
}

pub(super) fn format_ls_remote(
    remote: &str,
    default_branch: Option<&str>,
    refs: &[RemoteBranchRef],
) -> Result<String, ErrCtx> {
    if refs.is_empty() {
        return Ok(format!("No refs found for {remote}."));
    }

    let mut f = String::new();
    if let Some(default_branch) = default_branch
        && let Some(reference) = refs
            .iter()
            .find(|reference| reference.branch == default_branch)
    {
        writeln!(&mut f, "{}\tHEAD", reference.head)?;
    }
    for reference in refs {
        writeln!(
            &mut f,
            "{}\trefs/heads/{}",
            reference.head, reference.branch
        )?;
    }
    Ok(f)
}

pub(super) fn remote_config_uri(config: &RemoteConfig) -> String {
    match config {
        RemoteConfig::Memory => "memory".to_string(),
        RemoteConfig::Fs { root } => format!("fs://{root}"),
        RemoteConfig::S3Compatible { bucket, prefix, endpoint } => {
            let mut uri = prefix.as_ref().map_or_else(
                || format!("s3://{bucket}"),
                |prefix| format!("s3://{bucket}/{prefix}"),
            );
            if let Some(endpoint) = endpoint {
                uri.push_str("?endpoint=");
                uri.push_str(endpoint);
            }
            uri
        }
        RemoteConfig::Http { url, token_env } => {
            let mut uri = if let Some(rest) = url.strip_prefix("https://") {
                format!("graft+https://{rest}")
            } else if let Some(rest) = url.strip_prefix("http://") {
                format!("graft+http://{rest}")
            } else {
                url.clone()
            };
            if let Some(token_env) = token_env {
                uri.push_str("?token_env=");
                uri.push_str(token_env);
            }
            uri
        }
    }
}

pub(super) fn format_repo_log(repo: &Repository) -> Result<String, ErrCtx> {
    let commits = repo.log()?;
    if commits.is_empty() {
        return Ok("No commits yet.".to_string());
    }

    let mut f = String::new();
    for commit in commits {
        writeln!(&mut f, "commit {}", commit.id)?;
        if let Some(parent) = commit.parent {
            writeln!(&mut f, "parent {parent}")?;
        }
        writeln!(&mut f, "date {}", format_unix_millis(commit.timestamp_ms))?;
        writeln!(&mut f)?;
        writeln!(&mut f, "    {}", commit.message)?;
        writeln!(&mut f)?;
    }
    Ok(f)
}
