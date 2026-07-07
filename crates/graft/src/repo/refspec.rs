use super::{RepoErr, Result, validate_ref_name, validate_remote_name};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParsedRefspec {
    pub(super) source: Option<BranchPattern>,
    pub(super) destination: Option<BranchPattern>,
    pub(super) force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BranchPattern {
    Exact(String),
    Wildcard { prefix: String, suffix: String },
}

impl BranchPattern {
    pub(super) fn is_wildcard(&self) -> bool {
        matches!(self, Self::Wildcard { .. })
    }

    pub(super) fn exact(&self) -> Option<&str> {
        match self {
            Self::Exact(branch) => Some(branch),
            Self::Wildcard { .. } => None,
        }
    }

    pub(super) fn capture<'a>(&self, branch: &'a str) -> Result<Option<&'a str>> {
        match self {
            Self::Exact(pattern) => Ok((branch == pattern).then_some("")),
            Self::Wildcard { prefix, suffix } => {
                let Some(rest) = branch.strip_prefix(prefix) else {
                    return Ok(None);
                };
                let Some(capture) = rest.strip_suffix(suffix) else {
                    return Ok(None);
                };
                if capture.is_empty() {
                    return Ok(None);
                }
                validate_ref_name(capture)?;
                Ok(Some(capture))
            }
        }
    }

    pub(super) fn expand(&self, capture: &str) -> Result<String> {
        match self {
            Self::Exact(branch) => Ok(branch.clone()),
            Self::Wildcard { prefix, suffix } => {
                validate_ref_name(capture)?;
                let branch = format!("{prefix}{capture}{suffix}");
                validate_ref_name(&branch)?;
                Ok(branch)
            }
        }
    }
}

pub(super) fn parse_fetch_refspec(remote: &str, refspec: &str) -> Result<ParsedRefspec> {
    let parsed = parse_refspec(refspec, RefspecSide::FetchSource, |dst| {
        parse_fetch_destination(remote, dst)
    })?;
    if parsed.source.is_none() {
        return invalid_refspec(refspec, "fetch refspecs require a source");
    }
    validate_refspec_shape(refspec, &parsed)?;
    Ok(parsed)
}

pub(super) fn parse_push_refspec(refspec: &str) -> Result<ParsedRefspec> {
    let parsed = parse_refspec(refspec, RefspecSide::PushSource, |dst| {
        parse_branch_pattern_ref(dst, RefspecSide::PushDestination)
    })?;
    validate_refspec_shape(refspec, &parsed)?;
    Ok(parsed)
}

fn parse_refspec(
    refspec: &str,
    source_side: RefspecSide,
    parse_destination: impl FnOnce(&str) -> Result<BranchPattern>,
) -> Result<ParsedRefspec> {
    let refspec = refspec.trim();
    if refspec.is_empty() {
        return invalid_refspec(refspec, "empty refspec");
    }

    let (force, body) = if let Some(body) = refspec.strip_prefix('+') {
        (true, body)
    } else {
        (false, refspec)
    };
    if body.is_empty() {
        return invalid_refspec(refspec, "missing source ref");
    }
    if body.matches(':').count() > 1 {
        return invalid_refspec(refspec, "too many `:` separators");
    }

    let (source, destination) = match body.split_once(':') {
        Some((source, destination)) => {
            if destination.is_empty() {
                return invalid_refspec(refspec, "empty destination refs are not supported");
            }
            (
                if source.is_empty() {
                    None
                } else {
                    Some(parse_branch_pattern_ref(source, source_side)?)
                },
                Some(parse_destination(destination)?),
            )
        }
        None => (Some(parse_branch_pattern_ref(body, source_side)?), None),
    };

    Ok(ParsedRefspec { source, destination, force })
}

fn validate_refspec_shape(refspec: &str, parsed: &ParsedRefspec) -> Result<()> {
    let Some(source) = &parsed.source else {
        if parsed
            .destination
            .as_ref()
            .is_some_and(BranchPattern::is_wildcard)
        {
            return invalid_refspec(refspec, "wildcard delete refspecs are not supported");
        }
        return Ok(());
    };
    let destination = parsed.destination.as_ref().unwrap_or(source);
    if source.is_wildcard() != destination.is_wildcard() {
        return invalid_refspec(
            refspec,
            "wildcard refspecs must use `*` on both source and destination",
        );
    }
    if source.is_wildcard() && parsed.destination.is_none() {
        return invalid_refspec(
            refspec,
            "wildcard refspecs must include an explicit destination",
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum RefspecSide {
    FetchSource,
    FetchDestination,
    PushSource,
    PushDestination,
}

fn parse_fetch_destination(remote: &str, destination: &str) -> Result<BranchPattern> {
    if let Some(rest) = destination.strip_prefix("refs/remotes/") {
        let (destination_remote, branch) =
            rest.split_once('/')
                .ok_or_else(|| RepoErr::InvalidRefspec {
                    refspec: destination.to_string(),
                    message: "fetch destination must be under `refs/remotes/<remote>/`".to_string(),
                })?;
        validate_remote_name(destination_remote)?;
        if destination_remote != remote {
            return invalid_refspec(
                destination,
                "fetch destination remote must match the selected remote",
            );
        }
        return parse_branch_pattern(branch, RefspecSide::FetchDestination);
    }
    if destination.starts_with("refs/") {
        return invalid_refspec(
            destination,
            "fetch destination must be a branch name or `refs/remotes/<remote>/<branch>`",
        );
    }
    parse_branch_pattern(destination, RefspecSide::FetchDestination)
}

fn parse_branch_pattern_ref(value: &str, side: RefspecSide) -> Result<BranchPattern> {
    let branch = if let Some(branch) = value.strip_prefix("refs/heads/") {
        branch
    } else if value.starts_with("refs/") {
        return invalid_refspec(value, refspec_side_message(side));
    } else {
        value
    };
    parse_branch_pattern(branch, side)
}

fn parse_branch_pattern(value: &str, _side: RefspecSide) -> Result<BranchPattern> {
    if value.matches('*').count() > 1 {
        return invalid_refspec(value, "only one `*` wildcard is supported");
    }
    if let Some((prefix, suffix)) = value.split_once('*') {
        let sample = format!("{prefix}x{suffix}");
        validate_ref_name(&sample)?;
        Ok(BranchPattern::Wildcard {
            prefix: prefix.to_string(),
            suffix: suffix.to_string(),
        })
    } else {
        validate_ref_name(value)?;
        Ok(BranchPattern::Exact(value.to_string()))
    }
}

fn refspec_side_message(side: RefspecSide) -> &'static str {
    match side {
        RefspecSide::FetchSource | RefspecSide::PushSource => {
            "source must be a branch name or `refs/heads/<branch>`"
        }
        RefspecSide::FetchDestination => {
            "fetch destination must be a branch name or `refs/remotes/<remote>/<branch>`"
        }
        RefspecSide::PushDestination => {
            "push destination must be a branch name or `refs/heads/<branch>`"
        }
    }
}

fn invalid_refspec<T>(refspec: &str, message: impl Into<String>) -> Result<T> {
    Err(RepoErr::InvalidRefspec {
        refspec: refspec.to_string(),
        message: message.into(),
    })
}
