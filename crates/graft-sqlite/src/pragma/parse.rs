use std::path::PathBuf;

use graft::{
    core::{byte_unit::ByteUnit, logref::LogRef, lsn::LSN},
    remote::RemoteConfig,
    repo::{DEFAULT_TEXT_DIFF_CONTENT_LIMIT, RepoTrackedPathKind, ResetMode},
};
use sqlite_plugin::vfs::PragmaErr;

use super::{
    BranchListMode, DiffMode, JsonConfigListMode, JsonFetchAsyncMode, JsonLogMode, JsonLogSpec,
    JsonTagsMode, LargeFileFetchSpec, LargeFilePruneSpec, LargeFileStatusSpec, LsFilesSpec,
    RepoAddSpec, RepoAuditSpec, RepoCheckoutSpec, RepoCloneSpec, RepoDiffSpec, RepoDiffTarget,
    RepoExportSpec, RepoInitSpec, RepoRemoveSpec, RepoResolveRowSpec, RepoResolveSpec,
    RepoRestoreSpec, RepoTextContentSpec, ResolveSide, StatusSpec, parse_or_fail, pragma_fail,
};

pub(super) fn parse_remote_add(arg: &str) -> Result<(String, RemoteConfig), PragmaErr> {
    let (name, uri) = arg
        .split_once(char::is_whitespace)
        .ok_or_else(|| pragma_fail("argument must be in the form: `name remote-uri`"))?;
    Ok((
        name.trim().to_string(),
        parse_remote_config_uri(uri.trim())?,
    ))
}

pub(super) fn parse_remote_rename(arg: &str) -> Result<(String, String), PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [old, new] => Ok(((*old).to_string(), (*new).to_string())),
        _ => Err(pragma_fail("argument must be in the form: `old new`")),
    }
}

pub(super) fn parse_repo_clone_arg(arg: &str) -> Result<RepoCloneSpec, PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    let (worktree, uri, branch) = match parts.as_slice() {
        [uri] => (None, *uri, None),
        [uri, branch] => (None, *uri, Some(*branch)),
        ["--branch" | "-b", branch, uri] => (None, *uri, Some(*branch)),
        ["--worktree", worktree, uri] => (Some(PathBuf::from(worktree)), *uri, None),
        ["--worktree", worktree, uri, branch] => {
            (Some(PathBuf::from(worktree)), *uri, Some(*branch))
        }
        ["--worktree", worktree, "--branch" | "-b", branch, uri] => {
            (Some(PathBuf::from(worktree)), *uri, Some(*branch))
        }
        ["--branch" | "-b", branch, "--worktree", worktree, uri] => {
            (Some(PathBuf::from(worktree)), *uri, Some(*branch))
        }
        _ => {
            return Err(pragma_fail(
                "argument must be in the form: `[--worktree path] remote-uri [branch]` or `[--worktree path] --branch branch remote-uri`",
            ));
        }
    };
    if branch.is_some_and(str::is_empty) {
        return Err(pragma_fail("branch name must not be empty"));
    }
    Ok(RepoCloneSpec {
        config: parse_remote_config_uri(uri)?,
        branch: branch.map(str::to_string),
        worktree,
    })
}

pub(super) fn parse_repo_init_arg(arg: Option<&str>) -> Result<RepoInitSpec, PragmaErr> {
    let Some(arg) = arg.map(str::trim).filter(|arg| !arg.is_empty()) else {
        return Ok(RepoInitSpec { worktree: None });
    };
    let parts = split_pragma_words(arg)?;
    if parts.len() == 1 {
        Ok(RepoInitSpec { worktree: Some(PathBuf::from(&parts[0])) })
    } else if parts.len() == 2 && parts[0] == "--worktree" {
        Ok(RepoInitSpec { worktree: Some(PathBuf::from(&parts[1])) })
    } else {
        Err(pragma_fail(
            "argument must be empty or in the form: `[--worktree] path`",
        ))
    }
}

pub(super) fn parse_remote_config_uri(uri: &str) -> Result<RemoteConfig, PragmaErr> {
    if uri.is_empty() {
        return Err(pragma_fail("remote URI must not be empty"));
    }

    Ok(if uri == "memory" {
        RemoteConfig::Memory
    } else if let Some(root) = uri.strip_prefix("fs://") {
        RemoteConfig::Fs { root: root.to_string() }
    } else if let Some(rest) = uri
        .strip_prefix("s3://")
        .or_else(|| uri.strip_prefix("s3_compatible://"))
    {
        let (path, endpoint) = parse_s3_remote_uri_query(rest)?;
        let (bucket, prefix) = path
            .split_once('/')
            .map_or((path, None), |(bucket, prefix)| (bucket, Some(prefix)));
        if bucket.is_empty() {
            return Err(pragma_fail("S3 remote URI must include a bucket"));
        }
        RemoteConfig::S3Compatible {
            bucket: bucket.to_string(),
            prefix: prefix
                .filter(|prefix| !prefix.is_empty())
                .map(ToString::to_string),
            endpoint,
        }
    } else if let Some(rest) = uri.strip_prefix("graft+https://") {
        let (path, token_env) = parse_http_remote_uri_query(rest)?;
        RemoteConfig::Http {
            url: format!("https://{path}"),
            token_env,
        }
    } else if let Some(rest) = uri.strip_prefix("graft+http://") {
        let (path, token_env) = parse_http_remote_uri_query(rest)?;
        RemoteConfig::Http { url: format!("http://{path}"), token_env }
    } else {
        return Err(pragma_fail(
            "remote URI must start with memory, fs://, s3://, s3_compatible://, graft+https://, or graft+http://",
        ));
    })
}

pub(super) fn parse_s3_remote_uri_query(uri: &str) -> Result<(&str, Option<String>), PragmaErr> {
    let (path, query) = uri
        .split_once('?')
        .map_or((uri, ""), |(path, query)| (path, query));
    if query.is_empty() {
        return Ok((path, None));
    }

    let mut endpoint = None;
    for part in query.split('&').filter(|part| !part.is_empty()) {
        let (key, value) = part
            .split_once('=')
            .map_or((part, ""), |(key, value)| (key, value));
        match key {
            "endpoint" => {
                if value.is_empty() {
                    return Err(pragma_fail("S3 remote endpoint must not be empty"));
                }
                if endpoint.replace(value.to_string()).is_some() {
                    return Err(pragma_fail("S3 remote endpoint specified more than once"));
                }
            }
            _ => {
                return Err(pragma_fail(format!(
                    "unsupported S3 remote URI query parameter `{key}`"
                )));
            }
        }
    }

    Ok((path, endpoint))
}

pub(super) fn parse_http_remote_uri_query(uri: &str) -> Result<(&str, Option<String>), PragmaErr> {
    let (path, query) = uri
        .split_once('?')
        .map_or((uri, ""), |(path, query)| (path, query));
    if path.is_empty() {
        return Err(pragma_fail(
            "Graft HTTP remote URI must include a host and path",
        ));
    }
    if query.is_empty() {
        return Ok((path, None));
    }

    let mut token_env = None;
    for part in query.split('&').filter(|part| !part.is_empty()) {
        let (key, value) = part
            .split_once('=')
            .map_or((part, ""), |(key, value)| (key, value));
        match key {
            "token_env" => {
                if value.is_empty() {
                    return Err(pragma_fail("Graft HTTP remote token_env must not be empty"));
                }
                if token_env.replace(value.to_string()).is_some() {
                    return Err(pragma_fail(
                        "Graft HTTP remote token_env specified more than once",
                    ));
                }
            }
            _ => {
                return Err(pragma_fail(format!(
                    "unsupported Graft HTTP remote URI query parameter `{key}`"
                )));
            }
        }
    }

    Ok((path, token_env))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RemoteBranchArg {
    pub(super) remote: Option<String>,
    pub(super) branch: Option<String>,
    pub(super) refspec: Option<String>,
    pub(super) all: bool,
    pub(super) force: bool,
}

pub(super) fn parse_remote_branch_arg(arg: Option<&str>) -> Result<RemoteBranchArg, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(RemoteBranchArg {
            remote: None,
            branch: None,
            refspec: None,
            all: false,
            force: false,
        });
    };
    let mut all = false;
    let mut force = false;
    let mut positional = Vec::new();
    for part in arg.split_whitespace() {
        match part {
            "--all" => all = true,
            "--force" | "-f" => force = true,
            part => positional.push(part),
        }
    }

    if all {
        return match positional.as_slice() {
            [] => Ok(RemoteBranchArg {
                remote: None,
                branch: None,
                refspec: None,
                all,
                force,
            }),
            [remote] => Ok(RemoteBranchArg {
                remote: Some((*remote).to_string()),
                branch: None,
                refspec: None,
                all,
                force,
            }),
            _ => Err(pragma_fail(
                "argument must be in the form: `[--force] [remote] [branch]` or `[--force] --all [remote]`",
            )),
        };
    }

    match positional.as_slice() {
        [] => Ok(RemoteBranchArg {
            remote: None,
            branch: None,
            refspec: None,
            all,
            force,
        }),
        [remote_or_refspec] if looks_like_refspec(remote_or_refspec) => Ok(RemoteBranchArg {
            remote: None,
            branch: None,
            refspec: Some((*remote_or_refspec).to_string()),
            all,
            force,
        }),
        [remote] => Ok(RemoteBranchArg {
            remote: Some((*remote).to_string()),
            branch: None,
            refspec: None,
            all,
            force,
        }),
        [remote, branch_or_refspec] if looks_like_refspec(branch_or_refspec) => {
            Ok(RemoteBranchArg {
                remote: Some((*remote).to_string()),
                branch: None,
                refspec: Some((*branch_or_refspec).to_string()),
                all,
                force,
            })
        }
        [remote, branch] => Ok(RemoteBranchArg {
            remote: Some((*remote).to_string()),
            branch: Some((*branch).to_string()),
            refspec: None,
            all,
            force,
        }),
        _ => Err(pragma_fail(
            "argument must be in the form: `[--force] [remote] [branch]` or `[--force] --all [remote]`",
        )),
    }
}

pub(super) fn looks_like_refspec(value: &str) -> bool {
    let value = value.strip_prefix('+').unwrap_or(value);
    value.contains(':') || value.contains('*') || value.starts_with("refs/")
}

pub(super) fn parse_repo_diff_arg(arg: Option<&str>) -> Result<RepoDiffSpec, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(RepoDiffSpec {
            mode: DiffMode::Default,
            kind: None,
            target: RepoDiffTarget::Worktree { path: None },
            content: None,
        });
    };
    reject_ambiguous_posix_path_escape(arg)?;
    let raw_parts = split_pragma_words(arg)?;
    let mut mode = DiffMode::Default;
    let mut kind = None;
    let mut include_content = false;
    let mut max_content_bytes = None;
    let mut root = None;
    let mut parts = Vec::new();
    let mut in_path = false;
    let mut index = 0;
    while index < raw_parts.len() {
        let part = &raw_parts[index];
        if !in_path && part == "--" {
            in_path = true;
            parts.push(part.as_str());
            index += 1;
        } else if !in_path && part == "--rows" {
            if mode == DiffMode::Rows {
                return Err(pragma_fail("`--rows` may only be specified once"));
            }
            mode = DiffMode::Rows;
            index += 1;
        } else if !in_path && part == "--kind" {
            if kind.is_some() {
                return Err(pragma_fail("diff accepts --kind only once"));
            }
            let Some(value) = raw_parts.get(index + 1) else {
                return Err(pragma_fail("diff --kind requires a value"));
            };
            kind = Some(parse_repo_tracked_path_kind_arg(value)?);
            index += 2;
        } else if !in_path && part == "--content" {
            if include_content {
                return Err(pragma_fail("diff accepts --content only once"));
            }
            include_content = true;
            index += 1;
        } else if !in_path && part == "--max-content-bytes" {
            if max_content_bytes.is_some() {
                return Err(pragma_fail("diff accepts --max-content-bytes only once"));
            }
            let Some(value) = raw_parts.get(index + 1) else {
                return Err(pragma_fail("diff --max-content-bytes requires a value"));
            };
            let value = value
                .parse::<u64>()
                .map_err(|_| pragma_fail("diff --max-content-bytes must be a positive integer"))?;
            if value == 0 {
                return Err(pragma_fail(
                    "diff --max-content-bytes must be a positive integer",
                ));
            }
            max_content_bytes = Some(value);
            index += 2;
        } else if !in_path && part == "--root" {
            if root.is_some() {
                return Err(pragma_fail("diff accepts --root only once"));
            }
            let Some(value) = raw_parts.get(index + 1) else {
                return Err(pragma_fail("diff --root requires a target revision"));
            };
            root = Some(value.clone());
            index += 2;
        } else {
            parts.push(part.as_str());
            index += 1;
        }
    }
    let target = if let Some(to) = root {
        match parts.as_slice() {
            [] => RepoDiffTarget::Root { to, path: None },
            ["--", path @ ..] if !path.is_empty() => {
                RepoDiffTarget::Root { to, path: Some(path.join(" ")) }
            }
            _ => {
                return Err(pragma_fail(
                    "diff --root accepts one target revision and an optional `-- path`",
                ));
            }
        }
    } else {
        match parts.as_slice() {
            [] => RepoDiffTarget::Worktree { path: None },
            ["--", path @ ..] if !path.is_empty() => {
                RepoDiffTarget::Worktree { path: Some(path.join(" ")) }
            }
            ["--staged"] | ["--cached"] => RepoDiffTarget::Staged { path: None },
            ["--staged", "--", path @ ..] | ["--cached", "--", path @ ..] if !path.is_empty() => {
                RepoDiffTarget::Staged { path: Some(path.join(" ")) }
            }
            [rev] => RepoDiffTarget::RevisionToWorktree { rev: (*rev).to_string(), path: None },
            [rev, "--", path @ ..] if !path.is_empty() => RepoDiffTarget::RevisionToWorktree {
                rev: (*rev).to_string(),
                path: Some(path.join(" ")),
            },
            [from, to] => RepoDiffTarget::Revisions {
                from: (*from).to_string(),
                to: (*to).to_string(),
                path: None,
            },
            [from, to, "--", path @ ..] if !path.is_empty() => RepoDiffTarget::Revisions {
                from: (*from).to_string(),
                to: (*to).to_string(),
                path: Some(path.join(" ")),
            },
            _ => {
                return Err(pragma_fail(
                    "argument must be in the form: `[--rows] [--staged] [rev] [rev] [-- path]` or `--root rev [-- path]`",
                ));
            }
        }
    };
    if max_content_bytes.is_some() && !include_content {
        return Err(pragma_fail("diff --max-content-bytes requires --content"));
    }
    let content = if include_content {
        if mode != DiffMode::Default {
            return Err(pragma_fail("diff --content cannot be combined with --rows"));
        }
        if kind.is_some_and(|kind| kind != RepoTrackedPathKind::TextFile) {
            return Err(pragma_fail("diff --content only supports text_file paths"));
        }
        if !matches!(
            &target,
            RepoDiffTarget::RevisionToWorktree { path: Some(path), .. }
                | RepoDiffTarget::Revisions { path: Some(path), .. }
                | RepoDiffTarget::Root { path: Some(path), .. }
                if !path.is_empty()
        ) {
            return Err(pragma_fail(
                "diff --content requires a source revision, an optional target revision, and one path",
            ));
        }
        Some(RepoTextContentSpec {
            max_bytes: ByteUnit::new(
                max_content_bytes.unwrap_or(DEFAULT_TEXT_DIFF_CONTENT_LIMIT.as_u64()),
            ),
        })
    } else {
        None
    };
    Ok(RepoDiffSpec { mode, kind, target, content })
}

pub(super) fn parse_volume_diff_arg(arg: &str) -> Result<(LSN, LSN, DiffMode), PragmaErr> {
    let parts: Vec<&str> = arg.split(',').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return Err(pragma_fail(
            "argument must be in the form: `from_lsn,to_lsn[,mode]`",
        ));
    }
    let mode = if parts.len() == 3 {
        match parts[2] {
            "rows" => DiffMode::Rows,
            _ => return Err(pragma_fail("mode must be 'rows' or omitted")),
        }
    } else {
        DiffMode::Default
    };
    Ok((parse_or_fail(parts[0])?, parse_or_fail(parts[1])?, mode))
}

pub(super) fn parse_debug_diff_lsn_arg(arg: &str) -> Result<(LogRef, LogRef), PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [from, to] => Ok((parse_or_fail(from)?, parse_or_fail(to)?)),
        _ => Err(pragma_fail(
            "argument must be in the form: `from_log:from_lsn to_log:to_lsn`",
        )),
    }
}

pub(super) fn parse_repo_add_arg(arg: Option<&str>) -> Result<RepoAddSpec, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(RepoAddSpec {
            path: None,
            force: false,
            all: false,
            kind: None,
        });
    };
    let arg = arg.trim();
    if arg.is_empty() {
        return Ok(RepoAddSpec {
            path: None,
            force: false,
            all: false,
            kind: None,
        });
    }

    if arg.split_whitespace().any(|part| part == "--kind") {
        let parts = split_pragma_words(arg)?;
        let mut all = false;
        let mut kind = None;
        let mut index = 0;
        while index < parts.len() {
            match parts[index].as_str() {
                "--all" | "-A" => {
                    if all {
                        return Err(pragma_fail("add accepts --all only once"));
                    }
                    all = true;
                    index += 1;
                }
                "--kind" => {
                    if kind.is_some() {
                        return Err(pragma_fail("add accepts --kind only once"));
                    }
                    let Some(value) = parts.get(index + 1) else {
                        return Err(pragma_fail("add --kind requires a value"));
                    };
                    kind = Some(parse_repo_tracked_path_kind_arg(value)?);
                    index += 2;
                }
                value => {
                    return Err(pragma_fail(format!(
                        "unknown add argument `{value}`; `--kind` may only be used with `--all`"
                    )));
                }
            }
        }
        if !all {
            return Err(pragma_fail("add --kind requires --all"));
        }
        return Ok(RepoAddSpec {
            path: None,
            force: false,
            all: true,
            kind,
        });
    }

    if arg == "--all" || arg == "-A" {
        return Ok(RepoAddSpec {
            path: None,
            force: false,
            all: true,
            kind: None,
        });
    }

    for flag in ["--force", "-f"] {
        if arg == flag {
            return Ok(RepoAddSpec {
                path: None,
                force: true,
                all: false,
                kind: None,
            });
        }
        if let Some(path) = arg.strip_prefix(&format!("{flag} -- ")) {
            let path = parse_delimited_repo_path(path, "add")?;
            return Ok(RepoAddSpec {
                path: Some(path),
                force: true,
                all: false,
                kind: None,
            });
        }
        if let Some(path) = arg.strip_prefix(&format!("{flag} ")) {
            if path == "--all" || path == "-A" {
                return Err(pragma_fail(
                    "argument must be in the form: `[--all|-A]` or `[--force] [path]`",
                ));
            }
            return Ok(RepoAddSpec {
                path: Some(PathBuf::from(path)),
                force: true,
                all: false,
                kind: None,
            });
        }
    }

    if let Some(path) = arg.strip_prefix("-- ") {
        return Ok(RepoAddSpec {
            path: Some(parse_delimited_repo_path(path, "add")?),
            force: false,
            all: false,
            kind: None,
        });
    }

    if arg.starts_with('-') {
        return Err(pragma_fail(
            "argument must be in the form: `[--all|-A]` or `[--force] [path]`",
        ));
    }

    Ok(RepoAddSpec {
        path: Some(PathBuf::from(arg)),
        force: false,
        all: false,
        kind: None,
    })
}

fn parse_delimited_repo_path(value: &str, operation: &str) -> Result<PathBuf, PragmaErr> {
    let parts = split_pragma_words(value)?;
    match parts.as_slice() {
        [path] if !path.is_empty() => Ok(PathBuf::from(path)),
        _ => Err(pragma_fail(format!(
            "{operation} accepts exactly one path after `--`"
        ))),
    }
}

pub(super) fn parse_repo_remove_arg(arg: Option<&str>) -> Result<RepoRemoveSpec, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(RepoRemoveSpec { path: None, cached: false });
    };
    reject_ambiguous_posix_path_escape(arg)?;
    let parts = split_pragma_words(arg.trim())?;
    if parts.is_empty() {
        return Ok(RepoRemoveSpec { path: None, cached: false });
    }

    let mut cached = false;
    let mut path = Vec::new();
    let mut index = 0;
    let mut in_path = false;
    while index < parts.len() {
        if in_path {
            path.push(parts[index].clone());
            index += 1;
            continue;
        }
        match parts[index].as_str() {
            "--" => {
                in_path = true;
                index += 1;
            }
            "--cached" => {
                if cached {
                    return Err(pragma_fail("rm accepts --cached only once"));
                }
                cached = true;
                index += 1;
            }
            value if value.starts_with('-') && path.is_empty() => {
                return Err(pragma_fail(format!("unknown rm argument `{value}`")));
            }
            _ => {
                path.extend(parts[index..].iter().cloned());
                break;
            }
        }
    }

    Ok(RepoRemoveSpec {
        path: (!path.is_empty()).then(|| PathBuf::from(path.join(" "))),
        cached,
    })
}

pub(super) fn parse_repo_audit_arg(arg: Option<&str>) -> Result<RepoAuditSpec, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(RepoAuditSpec { repair: false, remote: None });
    };
    let parts = split_pragma_words(arg.trim())?;
    if parts.is_empty() {
        return Ok(RepoAuditSpec { repair: false, remote: None });
    }

    let mut repair = false;
    let mut remote = None;
    let mut index = 0;
    while index < parts.len() {
        match parts[index].as_str() {
            "--repair" => {
                if repair {
                    return Err(pragma_fail("audit accepts --repair only once"));
                }
                repair = true;
                index += 1;
            }
            value if value.starts_with('-') => {
                return Err(pragma_fail(format!("unknown audit argument `{value}`")));
            }
            value => {
                if remote.is_some() {
                    return Err(pragma_fail("audit --repair accepts at most one remote"));
                }
                remote = Some(value.to_string());
                index += 1;
            }
        }
    }

    if remote.is_some() && !repair {
        return Err(pragma_fail("audit remote requires --repair"));
    }

    Ok(RepoAuditSpec { repair, remote })
}

pub(super) fn parse_lfs_fetch_arg(arg: Option<&str>) -> Result<LargeFileFetchSpec, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(LargeFileFetchSpec { remote: None, rev: None });
    };
    let parts = split_pragma_words(arg.trim())?;
    if parts.is_empty() {
        return Ok(LargeFileFetchSpec { remote: None, rev: None });
    }

    let mut remote = None;
    let mut rev = None;
    let mut index = 0;
    while index < parts.len() {
        match parts[index].as_str() {
            "--remote" => {
                if remote.is_some() {
                    return Err(pragma_fail("payload fetch accepts --remote only once"));
                }
                let Some(value) = parts.get(index + 1) else {
                    return Err(pragma_fail("payload fetch --remote requires a remote name"));
                };
                if value.starts_with('-') {
                    return Err(pragma_fail("payload fetch --remote requires a remote name"));
                }
                remote = Some(value.clone());
                index += 2;
            }
            value if value.starts_with('-') => {
                return Err(pragma_fail(format!(
                    "unknown payload fetch argument `{value}`"
                )));
            }
            value => {
                if rev.is_some() {
                    return Err(pragma_fail("payload fetch accepts at most one revision"));
                }
                rev = Some(value.to_string());
                index += 1;
            }
        }
    }

    Ok(LargeFileFetchSpec { remote, rev })
}

pub(super) fn parse_lfs_status_arg(arg: Option<&str>) -> Result<LargeFileStatusSpec, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(LargeFileStatusSpec { rev: None });
    };
    let parts = split_pragma_words(arg.trim())?;
    if parts.is_empty() {
        return Ok(LargeFileStatusSpec { rev: None });
    }
    if parts.len() > 1 {
        return Err(pragma_fail("payload status accepts at most one revision"));
    }
    let rev = &parts[0];
    if rev.starts_with('-') {
        return Err(pragma_fail(format!(
            "unknown payload status argument `{rev}`"
        )));
    }
    Ok(LargeFileStatusSpec { rev: Some(rev.to_string()) })
}

pub(super) fn parse_lfs_prune_arg(arg: Option<&str>) -> Result<LargeFilePruneSpec, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(LargeFilePruneSpec { dry_run: true });
    };
    let parts = split_pragma_words(arg.trim())?;
    if parts.is_empty() {
        return Ok(LargeFilePruneSpec { dry_run: true });
    }

    let mut dry_run = None;
    for part in parts {
        match part.as_str() {
            "--dry-run" => {
                if dry_run.replace(true).is_some() {
                    return Err(pragma_fail("payload prune accepts only one mode flag"));
                }
            }
            "--force" => {
                if dry_run.replace(false).is_some() {
                    return Err(pragma_fail("payload prune accepts only one mode flag"));
                }
            }
            value => {
                return Err(pragma_fail(format!(
                    "unknown payload prune argument `{value}`; expected `--dry-run` or `--force`"
                )));
            }
        }
    }

    Ok(LargeFilePruneSpec { dry_run: dry_run.unwrap_or(true) })
}

pub(super) fn parse_status_arg(arg: Option<&str>) -> Result<StatusSpec, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(StatusSpec { kind: None });
    };
    let parts = split_pragma_words(arg)?;
    let mut kind = None;
    let mut index = 0;
    while index < parts.len() {
        match parts[index].as_str() {
            "--kind" => {
                if kind.is_some() {
                    return Err(pragma_fail("status accepts --kind only once"));
                }
                let Some(value) = parts.get(index + 1) else {
                    return Err(pragma_fail("status --kind requires a value"));
                };
                kind = Some(parse_repo_tracked_path_kind_arg(value)?);
                index += 2;
            }
            value => {
                return Err(pragma_fail(format!(
                    "unknown status argument `{value}`; expected `--kind <kind>`"
                )));
            }
        }
    }
    Ok(StatusSpec { kind })
}

pub(super) fn parse_ls_files_arg(arg: Option<&str>) -> Result<LsFilesSpec, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(LsFilesSpec {
            stage: false,
            details: false,
            others: false,
            kind: None,
        });
    };
    let parts = split_pragma_words(arg)?;
    let mut stage = false;
    let mut details = false;
    let mut others = false;
    let mut kind = None;
    let mut index = 0;
    while index < parts.len() {
        match parts[index].as_str() {
            "--stage" | "-s" => {
                if stage {
                    return Err(pragma_fail("ls-files accepts --stage only once"));
                }
                stage = true;
                index += 1;
            }
            "--details" => {
                if details {
                    return Err(pragma_fail("ls-files accepts --details only once"));
                }
                details = true;
                index += 1;
            }
            "--others" => {
                if others {
                    return Err(pragma_fail("ls-files accepts --others only once"));
                }
                others = true;
                index += 1;
            }
            "--kind" => {
                if kind.is_some() {
                    return Err(pragma_fail("ls-files accepts --kind only once"));
                }
                let Some(value) = parts.get(index + 1) else {
                    return Err(pragma_fail("ls-files --kind requires a value"));
                };
                kind = Some(parse_repo_tracked_path_kind_arg(value)?);
                index += 2;
            }
            value => {
                return Err(pragma_fail(format!(
                    "unknown ls-files argument `{value}`; expected `--stage`, `--details`, `--others`, or `--kind <kind>`"
                )));
            }
        }
    }
    if stage && details {
        return Err(pragma_fail(
            "ls-files --details cannot be used with --stage",
        ));
    }
    if others && stage {
        return Err(pragma_fail("ls-files --others cannot be used with --stage"));
    }
    if others && details {
        return Err(pragma_fail(
            "ls-files --others cannot be used with --details",
        ));
    }
    Ok(LsFilesSpec { stage, details, others, kind })
}

pub(super) fn parse_repo_tracked_path_kind_arg(
    value: &str,
) -> Result<RepoTrackedPathKind, PragmaErr> {
    match value {
        "sqlite" | "sqlite_database" | "sqlite-database" | "database" | "db" => {
            Ok(RepoTrackedPathKind::SqliteDatabase)
        }
        "text" | "text_file" | "text-file" => Ok(RepoTrackedPathKind::TextFile),
        "binary" | "binary_file" | "binary-file" => Ok(RepoTrackedPathKind::BinaryFile),
        _ => Err(pragma_fail(
            "--kind must be one of sqlite_database, text_file, or binary_file",
        )),
    }
}

pub(super) fn parse_repo_config_set_arg(arg: &str) -> Result<(String, String), PragmaErr> {
    let arg = arg.trim();
    if let Some((key, value)) = arg.split_once(" --") {
        return config_set_parts(key, value);
    }

    let mut parts = arg.splitn(2, char::is_whitespace);
    let key = parts.next().unwrap_or_default();
    let value = parts.next().unwrap_or_default();
    config_set_parts(key, value)
}

pub(super) fn config_set_parts(key: &str, value: &str) -> Result<(String, String), PragmaErr> {
    let key = key.trim();
    let value = value.trim();
    if key.is_empty() || value.is_empty() {
        return Err(pragma_fail("argument must be in the form: `key -- value`"));
    }
    Ok((key.to_string(), value.to_string()))
}

pub(super) fn parse_repo_checkout_arg(arg: &str) -> Result<RepoCheckoutSpec, PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [rev] => Ok(RepoCheckoutSpec::Detach { rev: (*rev).to_string(), force: false }),
        ["--force" | "-f", rev] => {
            Ok(RepoCheckoutSpec::Detach { rev: (*rev).to_string(), force: true })
        }
        [rev, "--", path @ ..] if !path.is_empty() => Ok(RepoCheckoutSpec::Path {
            rev: (*rev).to_string(),
            path: path.join(" "),
        }),
        _ => Err(pragma_fail(
            "argument must be in the form: `[--force] rev [-- path]`",
        )),
    }
}

pub(super) fn parse_repo_restore_arg(arg: &str) -> Result<RepoRestoreSpec, PragmaErr> {
    reject_ambiguous_posix_path_escape(arg)?;
    let parts = split_pragma_words(arg)?;
    let mut source = None;
    let mut expected_head = None;
    let mut require_clean = false;
    let mut staged = false;
    let mut all = false;
    let mut kind = None;
    let mut path = Vec::new();
    let mut index = 0;
    let mut in_path = false;

    while index < parts.len() {
        if in_path {
            path.push(parts[index].clone());
            index += 1;
            continue;
        }

        match parts[index].as_str() {
            "--" => {
                in_path = true;
                index += 1;
            }
            "--staged" | "--cached" => {
                if staged {
                    return Err(pragma_fail("restore accepts --staged only once"));
                }
                staged = true;
                index += 1;
            }
            "--source" | "-s" => {
                if source.is_some() {
                    return Err(pragma_fail("restore accepts --source only once"));
                }
                let Some(value) = parts.get(index + 1) else {
                    return Err(pragma_fail("restore --source requires a revision"));
                };
                source = Some(value.clone());
                index += 2;
            }
            "--expected-head" => {
                if expected_head.is_some() {
                    return Err(pragma_fail("restore accepts --expected-head only once"));
                }
                let Some(value) = parts.get(index + 1) else {
                    return Err(pragma_fail("restore --expected-head requires an object id"));
                };
                if value.starts_with('-') {
                    return Err(pragma_fail("restore --expected-head requires an object id"));
                }
                expected_head = Some(value.clone());
                index += 2;
            }
            "--require-clean" => {
                if require_clean {
                    return Err(pragma_fail("restore accepts --require-clean only once"));
                }
                require_clean = true;
                index += 1;
            }
            "--all" | "-A" => {
                if all {
                    return Err(pragma_fail("restore accepts --all only once"));
                }
                all = true;
                index += 1;
            }
            "--kind" => {
                if kind.is_some() {
                    return Err(pragma_fail("restore accepts --kind only once"));
                }
                let Some(value) = parts.get(index + 1) else {
                    return Err(pragma_fail("restore --kind requires a value"));
                };
                kind = Some(parse_repo_tracked_path_kind_arg(value)?);
                index += 2;
            }
            value if value.starts_with('-') && path.is_empty() => {
                return Err(pragma_fail(format!("unknown restore argument `{value}`")));
            }
            _ => {
                path.extend(parts[index..].iter().cloned());
                break;
            }
        }
    }

    if all {
        if !staged {
            return Err(pragma_fail("restore --all requires --staged"));
        }
        if !path.is_empty() {
            return Err(pragma_fail("restore --all does not accept a path"));
        }
    } else if kind.is_some() {
        return Err(pragma_fail("restore --kind requires --all"));
    } else if path.is_empty() {
        return Err(pragma_fail(
            "argument must be in the form: `[--staged] [--source rev] path` or `--staged --all [--kind kind]`",
        ));
    }

    Ok(RepoRestoreSpec {
        source,
        expected_head,
        require_clean,
        staged,
        all,
        kind,
        path: (!path.is_empty()).then(|| PathBuf::from(path.join(" "))),
    })
}

pub(super) fn split_pragma_words(arg: &str) -> Result<Vec<String>, PragmaErr> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut in_word = false;

    for ch in arg.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            in_word = true;
            continue;
        }

        if ch == '\\' {
            if cfg!(windows) {
                current.push(ch);
            } else {
                escaped = true;
            }
            in_word = true;
            continue;
        }

        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                current.push(ch);
            }
            in_word = true;
            continue;
        }

        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                in_word = true;
            }
            ch if ch.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            ch => {
                current.push(ch);
                in_word = true;
            }
        }
    }

    if escaped {
        current.push('\\');
    }
    if quote.is_some() {
        return Err(pragma_fail("unterminated quoted argument"));
    }
    if in_word {
        words.push(current);
    }
    Ok(words)
}

fn reject_ambiguous_posix_path_escape(arg: &str) -> Result<(), PragmaErr> {
    #[cfg(not(windows))]
    {
        let mut chars = arg.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '\\' {
                continue;
            }
            let escaped = chars.peek().copied();
            if escaped.is_some_and(|ch| ch.is_whitespace() || matches!(ch, '\'' | '"')) {
                continue;
            }
            return Err(pragma_fail(
                "backslashes are not supported in POSIX repository paths",
            ));
        }
    }
    Ok(())
}

pub(super) fn parse_json_log_arg(arg: Option<&str>) -> Result<JsonLogSpec, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(JsonLogSpec {
            mode: JsonLogMode::LegacyArray,
            limit: None,
            after: None,
        });
    };
    let words = split_pragma_words(arg)?;
    let mut mode = JsonLogMode::LegacyArray;
    let mut limit = None;
    let mut after = None;
    let mut index = 0;
    while index < words.len() {
        match words[index].as_str() {
            "--with-status" if mode == JsonLogMode::LegacyArray => {
                mode = JsonLogMode::WithStatus;
                index += 1;
            }
            "--limit" if limit.is_none() => {
                let value = words
                    .get(index + 1)
                    .ok_or_else(|| pragma_fail("json_log --limit requires a positive integer"))?;
                let parsed = value
                    .parse::<usize>()
                    .map_err(|_| pragma_fail("json_log --limit requires a positive integer"))?;
                if parsed == 0 {
                    return Err(pragma_fail("json_log --limit requires a positive integer"));
                }
                limit = Some(parsed);
                index += 2;
            }
            "--after" if after.is_none() => {
                after = Some(
                    words
                        .get(index + 1)
                        .filter(|value| !value.starts_with("--"))
                        .ok_or_else(|| pragma_fail("json_log --after requires a commit id"))?
                        .clone(),
                );
                index += 2;
            }
            _ => {
                return Err(pragma_fail(
                    "argument must use: `[--with-status] [--limit n] [--after oid]`",
                ));
            }
        }
    }
    if after.is_some() && limit.is_none() {
        return Err(pragma_fail("json_log --after requires --limit"));
    }
    Ok(JsonLogSpec { mode, limit, after })
}

pub(super) fn parse_json_config_list_arg(
    arg: Option<&str>,
) -> Result<JsonConfigListMode, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(JsonConfigListMode::LegacyArray);
    };
    let words = split_pragma_words(arg)?;
    match words.as_slice() {
        [] => Ok(JsonConfigListMode::LegacyArray),
        [flag] if flag == "--with-status" => Ok(JsonConfigListMode::WithStatus),
        _ => Err(pragma_fail(
            "argument must be empty or in the form: `--with-status`",
        )),
    }
}

pub(super) fn parse_json_tags_arg(arg: Option<&str>) -> Result<JsonTagsMode, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(JsonTagsMode::LegacyArray);
    };
    let words = split_pragma_words(arg)?;
    match words.as_slice() {
        [] => Ok(JsonTagsMode::LegacyArray),
        [flag] if flag == "--with-status" => Ok(JsonTagsMode::WithStatus),
        _ => Err(pragma_fail(
            "argument must be empty or in the form: `--with-status`",
        )),
    }
}

pub(super) fn parse_json_fetch_async_arg(
    arg: Option<&str>,
) -> Result<(RemoteBranchArg, JsonFetchAsyncMode), PragmaErr> {
    let Some(arg) = arg else {
        return Ok((parse_remote_branch_arg(None)?, JsonFetchAsyncMode::LegacyId));
    };

    let mut mode = JsonFetchAsyncMode::LegacyId;
    let mut remote_words = Vec::new();
    for word in split_pragma_words(arg)? {
        if word == "--with-status" {
            if mode == JsonFetchAsyncMode::WithStatus {
                return Err(pragma_fail("argument contains duplicate `--with-status`"));
            }
            mode = JsonFetchAsyncMode::WithStatus;
        } else {
            remote_words.push(word);
        }
    }

    let remote_arg = if remote_words.is_empty() {
        None
    } else {
        Some(remote_words.join(" "))
    };
    Ok((parse_remote_branch_arg(remote_arg.as_deref())?, mode))
}

pub(super) fn parse_repo_export_arg(arg: &str) -> Result<RepoExportSpec, PragmaErr> {
    reject_ambiguous_posix_path_escape(arg)?;
    let mut source = None;
    let mut output = None;
    let mut path = Vec::new();
    let mut after_path_separator = false;
    let mut parts = split_pragma_words(arg)?.into_iter().peekable();

    while let Some(part) = parts.next() {
        match part.as_str() {
            "--source" | "-s" if !after_path_separator => {
                if source.is_some() {
                    return Err(pragma_fail("export accepts only one source revision"));
                }
                let Some(value) = parts.next() else {
                    return Err(pragma_fail("export --source requires a revision"));
                };
                source = Some(value);
            }
            "--output" | "-o" if !after_path_separator => {
                if output.is_some() {
                    return Err(pragma_fail("export accepts only one output path"));
                }
                let Some(value) = parts.next() else {
                    return Err(pragma_fail("export --output requires a path"));
                };
                output = Some(PathBuf::from(value));
            }
            "--" if !after_path_separator => {
                after_path_separator = true;
            }
            value if value.starts_with('-') && !after_path_separator => {
                return Err(pragma_fail(format!("unknown export option `{value}`")));
            }
            _ => {
                path.push(part);
                if after_path_separator {
                    path.extend(parts);
                    break;
                }
            }
        }
    }

    let Some(output) = output else {
        return Err(pragma_fail(
            "argument must be in the form: `[--source rev] --output output.db [-- path]`",
        ));
    };

    Ok(RepoExportSpec {
        source,
        path: (!path.is_empty()).then(|| PathBuf::from(path.join(" "))),
        output,
    })
}

pub(super) fn parse_repo_resolve_arg(arg: &str) -> Result<RepoResolveSpec, PragmaErr> {
    let mut side = None;
    let mut row = None;
    let mut path = Vec::new();
    let parts: Vec<&str> = arg.split_whitespace().collect();
    let mut index = 0;

    while index < parts.len() {
        match parts[index] {
            "--ours" => {
                if side.replace(ResolveSide::Ours).is_some() {
                    return Err(pragma_fail("resolve accepts only one side"));
                }
                index += 1;
            }
            "--theirs" => {
                if side.replace(ResolveSide::Theirs).is_some() {
                    return Err(pragma_fail("resolve accepts only one side"));
                }
                index += 1;
            }
            "--manual" => {
                if side.replace(ResolveSide::Manual).is_some() {
                    return Err(pragma_fail("resolve accepts only one side"));
                }
                index += 1;
            }
            "--row" => {
                if row.is_some() {
                    return Err(pragma_fail("resolve accepts only one row selector"));
                }
                let Some(table) = parts.get(index + 1) else {
                    return Err(pragma_fail("resolve --row requires a table name"));
                };
                let Some(rowid) = parts.get(index + 2) else {
                    return Err(pragma_fail("resolve --row requires a rowid"));
                };
                let rowid = rowid
                    .parse::<i64>()
                    .map_err(|_| pragma_fail("resolve --row rowid must be an integer"))?;
                row = Some(RepoResolveRowSpec { table: (*table).to_string(), rowid });
                index += 3;
            }
            "--path" => {
                let Some(value) = parts.get(index + 1) else {
                    return Err(pragma_fail("resolve --path requires a path"));
                };
                path.push(*value);
                index += 2;
            }
            value => {
                path.push(value);
                index += 1;
            }
        }
    }

    let Some(side) = side else {
        return Err(pragma_fail(
            "argument must include `--ours`, `--theirs`, or `--manual`",
        ));
    };

    Ok(RepoResolveSpec {
        side,
        path: (!path.is_empty()).then(|| PathBuf::from(path.join(" "))),
        row,
    })
}

pub(super) fn parse_branch_delete_arg(arg: &str) -> Result<(String, bool), PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [name] => Ok(((*name).to_string(), false)),
        ["--force", name] | ["-D", name] | ["-f", name] => Ok(((*name).to_string(), true)),
        _ => Err(pragma_fail(
            "argument must be in the form: `[--force] name`",
        )),
    }
}

pub(super) fn parse_branch_list_mode(arg: Option<&str>) -> Result<BranchListMode, PragmaErr> {
    match arg.map(str::trim).filter(|arg| !arg.is_empty()) {
        None => Ok(BranchListMode::Local),
        Some("-r" | "--remote" | "--remotes") => Ok(BranchListMode::Remote),
        Some("-a" | "--all") => Ok(BranchListMode::All),
        Some(_) => Err(pragma_fail(
            "argument must be one of: `--remote`, `-r`, `--all`, `-a`",
        )),
    }
}

pub(super) fn parse_branch_rename_arg(
    arg: &str,
) -> Result<(Option<String>, String, bool), PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [new] => Ok((None, (*new).to_string(), false)),
        ["--force" | "-M" | "-f", new] => Ok((None, (*new).to_string(), true)),
        ["--force" | "-M" | "-f", old, new] => {
            Ok((Some((*old).to_string()), (*new).to_string(), true))
        }
        [old, new] => Ok((Some((*old).to_string()), (*new).to_string(), false)),
        _ => Err(pragma_fail(
            "argument must be in the form: `[--force] [old] new`",
        )),
    }
}

pub(super) fn parse_branch_upstream_arg(
    arg: &str,
) -> Result<(Option<String>, String, String), PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [upstream] => {
            let (remote, branch) = parse_remote_branch_ref(upstream)?;
            Ok((None, remote, branch))
        }
        [branch, upstream] => {
            let (remote, remote_branch) = parse_remote_branch_ref(upstream)?;
            Ok((Some((*branch).to_string()), remote, remote_branch))
        }
        _ => Err(pragma_fail(
            "argument must be in the form: `[branch] remote/branch`",
        )),
    }
}

pub(super) fn parse_remote_branch_ref(value: &str) -> Result<(String, String), PragmaErr> {
    let Some((remote, branch)) = value.split_once('/') else {
        return Err(pragma_fail("upstream must be in the form: `remote/branch`"));
    };
    if remote.is_empty() || branch.is_empty() {
        return Err(pragma_fail("upstream must be in the form: `remote/branch`"));
    }
    Ok((remote.to_string(), branch.to_string()))
}

pub(super) fn parse_branch_create_arg(arg: &str) -> Result<(String, Option<String>), PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [name] => Ok(((*name).to_string(), None)),
        [name, start_point] => Ok(((*name).to_string(), Some((*start_point).to_string()))),
        _ => Err(pragma_fail(
            "argument must be in the form: `name [start-point]`",
        )),
    }
}

pub(super) fn parse_switch_branch_arg(arg: &str) -> Result<(String, bool), PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [name] => Ok(((*name).to_string(), false)),
        ["--force" | "-f", name] => Ok(((*name).to_string(), true)),
        _ => Err(pragma_fail(
            "argument must be in the form: `[--force] name`",
        )),
    }
}

pub(super) fn parse_switch_create_arg(
    arg: &str,
) -> Result<(String, Option<String>, bool), PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [name] => Ok(((*name).to_string(), None, false)),
        ["--force" | "-f", name] => Ok(((*name).to_string(), None, true)),
        ["--force" | "-f", name, start_point] => {
            Ok(((*name).to_string(), Some((*start_point).to_string()), true))
        }
        [name, start_point] => Ok(((*name).to_string(), Some((*start_point).to_string()), false)),
        _ => Err(pragma_fail(
            "argument must be in the form: `[--force] name [start-point]`",
        )),
    }
}

pub(super) fn parse_tag_create_arg(
    arg: &str,
) -> Result<(String, Option<String>, Option<String>), PragmaErr> {
    let arg = arg.trim();
    if let Some(rest) = arg
        .strip_prefix("--annotated ")
        .or_else(|| arg.strip_prefix("-a "))
    {
        let Some((spec, message)) = rest.split_once(" -- ") else {
            return Err(pragma_fail(
                "annotated tag argument must be in the form: `--annotated name [rev] -- message`",
            ));
        };
        let message = message.trim();
        if message.is_empty() {
            return Err(pragma_fail("annotated tag message cannot be empty"));
        }
        let parts: Vec<&str> = spec.split_whitespace().collect();
        return match parts.as_slice() {
            [name] => Ok(((*name).to_string(), None, Some(message.to_string()))),
            [name, target] => Ok((
                (*name).to_string(),
                Some((*target).to_string()),
                Some(message.to_string()),
            )),
            _ => Err(pragma_fail(
                "annotated tag argument must be in the form: `--annotated name [rev] -- message`",
            )),
        };
    }

    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [name] => Ok(((*name).to_string(), None, None)),
        [name, target] => Ok(((*name).to_string(), Some((*target).to_string()), None)),
        _ => Err(pragma_fail("argument must be in the form: `name [rev]`")),
    }
}

pub(super) fn parse_repo_reset_arg(arg: &str) -> Result<(ResetMode, String), PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [rev] => Ok((ResetMode::Mixed, (*rev).to_string())),
        ["--soft", rev] => Ok((ResetMode::Soft, (*rev).to_string())),
        ["--mixed", rev] => Ok((ResetMode::Mixed, (*rev).to_string())),
        ["--hard", rev] => Ok((ResetMode::Hard, (*rev).to_string())),
        _ => Err(pragma_fail(
            "argument must be in the form: `[--soft|--mixed|--hard] rev`",
        )),
    }
}
