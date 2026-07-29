use std::{
    borrow::Cow,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder},
};

use super::{
    CONTENT_CLASS_SAMPLE_BYTES, FileConfig, GIT_IGNORE_FILE, GRAFT_IGNORE_FILE, RepoPathStorage,
    RepoTrackedPathKind, Result, SQLITE_DATABASE_MAGIC,
};

#[derive(Debug, Clone)]
pub(super) struct IgnoreRules {
    worktree: PathBuf,
    root: Gitignore,
}

impl IgnoreRules {
    pub(super) fn load(worktree: &Path) -> Result<Self> {
        Ok(Self {
            worktree: worktree.to_path_buf(),
            root: Self::load_directory(worktree)?,
        })
    }

    pub(super) fn is_ignored(&self, key: &str, is_dir: bool) -> Result<bool> {
        if key.is_empty() {
            return Ok(false);
        }

        let mut matchers = vec![self.root.clone()];
        let mut path = self.worktree.clone();
        let mut components = Path::new(key).components().peekable();
        while let Some(component) = components.next() {
            path.push(component);
            let has_descendants = components.peek().is_some();
            let component_is_dir = has_descendants || is_dir;
            if Self::matches(&matchers, &path, component_is_dir) {
                return Ok(true);
            }
            if has_descendants {
                matchers.push(Self::load_directory(&path)?);
            }
        }
        Ok(false)
    }

    pub(super) fn root(&self) -> Gitignore {
        self.root.clone()
    }

    pub(super) fn rules_for_directory(&self, directory: &Path) -> Result<Gitignore> {
        Self::load_directory(directory)
    }

    pub(super) fn matches(matchers: &[Gitignore], path: &Path, is_dir: bool) -> bool {
        for matcher in matchers.iter().rev() {
            match matcher.matched(path, is_dir) {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {}
            }
        }
        false
    }

    fn load_directory(directory: &Path) -> Result<Gitignore> {
        let mut builder = GitignoreBuilder::new(directory);
        for file_name in [GIT_IGNORE_FILE, GRAFT_IGNORE_FILE] {
            let path = directory.join(file_name);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() => {
                    if let Some(err) = builder.add(path) {
                        return Err(err.into());
                    }
                }
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(builder.build()?)
    }
}

pub(super) fn wildcard_match(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == text;
    }

    let parts = pattern.split('*').collect::<Vec<_>>();
    let mut rest = text;
    if let Some(first) = parts.first().filter(|part| !part.is_empty()) {
        let Some(stripped) = rest.strip_prefix(first) else {
            return false;
        };
        rest = stripped;
    }

    for part in parts
        .iter()
        .skip(1)
        .take(parts.len().saturating_sub(2))
        .filter(|part| !part.is_empty())
    {
        let Some(index) = rest.find(part) else {
            return false;
        };
        rest = &rest[index + part.len()..];
    }

    if let Some(last) = parts.last().filter(|part| !part.is_empty()) {
        rest.ends_with(last)
    } else {
        true
    }
}

pub(super) fn normalize_repo_path(path: &str) -> String {
    let path = path.trim().trim_start_matches("./");
    let path = normalize_repo_path_separators(path);
    let path = path.trim_end_matches('/');
    if path == "." {
        String::new()
    } else {
        path.to_string()
    }
}

pub fn validate_repo_path_identity(path: &str) -> Result<()> {
    #[cfg(not(windows))]
    if path.contains('\\') {
        return Err(super::RepoErr::UnsupportedPathIdentity {
            path: path.to_string(),
            reason: "backslashes are not supported in POSIX repository paths",
        });
    }

    let path = normalize_repo_path_separators(path);
    if path
        .split('/')
        .any(|component| component.trim() != component)
    {
        return Err(super::RepoErr::UnsupportedPathIdentity {
            path: path.into_owned(),
            reason: "path components must not start or end with whitespace",
        });
    }
    Ok(())
}

pub(super) fn normalize_repo_path_key(path: &str) -> Result<String> {
    validate_repo_path_identity(path)?;
    Ok(normalize_repo_path(path))
}

fn normalize_repo_path_separators(path: &str) -> Cow<'_, str> {
    #[cfg(windows)]
    {
        Cow::Owned(path.replace('\\', "/"))
    }
    #[cfg(not(windows))]
    {
        Cow::Borrowed(path)
    }
}

pub(super) fn is_sqlite_database_file(path: &Path) -> Result<bool> {
    let mut file = fs::File::open(path)?;
    let mut magic = [0; SQLITE_DATABASE_MAGIC.len()];
    match file.read_exact(&mut magic) {
        Ok(()) => Ok(&magic == SQLITE_DATABASE_MAGIC),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(err.into()),
    }
}

pub(super) fn classify_artifact_path(path: &Path) -> Result<RepoTrackedPathKind> {
    let mut file = fs::File::open(path)?;
    let mut sample = vec![0; CONTENT_CLASS_SAMPLE_BYTES];
    let len = file.read(&mut sample)?;
    sample.truncate(len);
    Ok(classify_artifact_bytes(&sample))
}

pub(super) fn classify_artifact_bytes(bytes: &[u8]) -> RepoTrackedPathKind {
    if is_text_bytes(bytes) {
        RepoTrackedPathKind::TextFile
    } else {
        RepoTrackedPathKind::BinaryFile
    }
}

pub(super) fn artifact_storage_for_path(
    key: &str,
    kind: RepoTrackedPathKind,
    size: u64,
    config: &FileConfig,
) -> RepoPathStorage {
    match kind {
        RepoTrackedPathKind::SqliteDatabase => RepoPathStorage::SqliteSnapshot,
        RepoTrackedPathKind::BinaryFile => RepoPathStorage::External,
        RepoTrackedPathKind::TextFile => {
            if config_path_patterns_match(&config.external_paths, key)
                || size > config.inline_text_threshold.as_u64()
            {
                RepoPathStorage::External
            } else {
                RepoPathStorage::Inline
            }
        }
    }
}

pub(super) fn config_path_patterns_match(patterns: &[String], key: &str) -> bool {
    patterns
        .iter()
        .any(|pattern| config_path_pattern_matches(pattern, key))
}

pub(super) fn config_path_pattern_matches(pattern: &str, key: &str) -> bool {
    let pattern = normalize_repo_path(pattern.trim().trim_start_matches("./"));
    if pattern.is_empty() {
        return false;
    }
    if wildcard_match(&pattern, key) {
        return true;
    }
    pattern
        .strip_suffix("/**")
        .is_some_and(|prefix| key == prefix || key.starts_with(&format!("{prefix}/")))
        || (!pattern.contains('*')
            && key
                .strip_prefix(&pattern)
                .is_some_and(|suffix| suffix.starts_with('/')))
}

pub(super) fn is_text_bytes(bytes: &[u8]) -> bool {
    let sample = if bytes.len() > CONTENT_CLASS_SAMPLE_BYTES {
        &bytes[..CONTENT_CLASS_SAMPLE_BYTES]
    } else {
        bytes
    };
    if sample.is_empty() {
        return true;
    }
    if sample.contains(&0) || std::str::from_utf8(sample).is_err() {
        return false;
    }
    sample
        .iter()
        .all(|byte| !byte.is_ascii_control() || matches!(*byte, b'\n' | b'\r' | b'\t'))
}

pub(super) fn is_sqlite_sidecar_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.ends_with("-wal") || name.ends_with("-shm") || name.ends_with("-journal")
        })
}
