use std::{
    borrow::Cow,
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
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
    root_sources: DirectoryIgnoreFingerprint,
}

/// Reusable nested `.gitignore` / `.graftignore` matcher for bounded SDK scans.
#[derive(Debug, Clone)]
pub struct RepoIgnoreMatcher {
    worktree: PathBuf,
    root: Gitignore,
    directory_rules: BTreeMap<PathBuf, Gitignore>,
    rule_sources: BTreeMap<PathBuf, DirectoryIgnoreFingerprint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IgnoreFileFingerprint {
    len: u64,
    modified_ns: Option<u128>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
}

type DirectoryIgnoreFingerprint = [Option<IgnoreFileFingerprint>; 2];

impl RepoIgnoreMatcher {
    pub fn is_ignored(&mut self, key: &str, is_dir: bool) -> Result<bool> {
        let key = normalize_repo_path_key(key)?;
        if key.is_empty() {
            return Ok(false);
        }
        let mut rule_directories = Vec::new();
        let mut path = self.worktree.clone();
        let mut components = Path::new(&key).components().peekable();
        while let Some(component) = components.next() {
            path.push(component);
            let has_descendants = components.peek().is_some();
            let component_is_dir = has_descendants || is_dir;
            if self.matches(&rule_directories, &path, component_is_dir) {
                return Ok(true);
            }
            if has_descendants {
                if !self.directory_rules.contains_key(&path) {
                    let (rules, sources) = IgnoreRules::load_directory(&path)?;
                    self.directory_rules.insert(path.clone(), rules);
                    self.rule_sources.insert(path.clone(), sources);
                }
                rule_directories.push(path.clone());
            }
        }
        Ok(false)
    }

    /// Returns false when an ignore source loaded by this matcher changed on disk.
    pub fn rules_unchanged(&self) -> Result<bool> {
        for (directory, expected) in &self.rule_sources {
            super::cancellation_checkpoint()?;
            if &IgnoreRules::source_fingerprint(directory)? != expected {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn matches(&self, rule_directories: &[PathBuf], path: &Path, is_dir: bool) -> bool {
        for directory in rule_directories.iter().rev() {
            let matcher = self
                .directory_rules
                .get(directory)
                .expect("visited ignore directory has loaded rules");
            match matcher.matched(path, is_dir) {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {}
            }
        }
        matches!(self.root.matched(path, is_dir), Match::Ignore(_))
    }
}

impl IgnoreRules {
    pub(super) fn load(worktree: &Path) -> Result<Self> {
        let (root, root_sources) = Self::load_directory(worktree)?;
        Ok(Self {
            worktree: worktree.to_path_buf(),
            root,
            root_sources,
        })
    }

    pub(super) fn is_ignored(&self, key: &str, is_dir: bool) -> Result<bool> {
        self.matcher().is_ignored(key, is_dir)
    }

    pub(super) fn matcher(&self) -> RepoIgnoreMatcher {
        let mut rule_sources = BTreeMap::new();
        rule_sources.insert(self.worktree.clone(), self.root_sources.clone());
        RepoIgnoreMatcher {
            worktree: self.worktree.clone(),
            root: self.root.clone(),
            directory_rules: BTreeMap::new(),
            rule_sources,
        }
    }

    pub(super) fn root(&self) -> Gitignore {
        self.root.clone()
    }

    pub(super) fn rules_for_directory(&self, directory: &Path) -> Result<Gitignore> {
        Self::load_directory(directory).map(|(rules, _)| rules)
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

    fn load_directory(directory: &Path) -> Result<(Gitignore, DirectoryIgnoreFingerprint)> {
        let mut builder = GitignoreBuilder::new(directory);
        let sources = Self::source_fingerprint(directory)?;
        for (file_name, source) in [GIT_IGNORE_FILE, GRAFT_IGNORE_FILE]
            .into_iter()
            .zip(sources.iter())
        {
            if source.is_none() {
                continue;
            }
            let path = directory.join(file_name);
            if let Some(err) = builder.add(path) {
                return Err(err.into());
            }
        }
        Ok((builder.build()?, sources))
    }

    fn source_fingerprint(directory: &Path) -> Result<DirectoryIgnoreFingerprint> {
        let mut sources = [None, None];
        for (index, file_name) in [GIT_IGNORE_FILE, GRAFT_IGNORE_FILE].into_iter().enumerate() {
            let path = directory.join(file_name);
            sources[index] = match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.is_file() => Some(ignore_file_fingerprint(&metadata)),
                Ok(_) => None,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
        }
        Ok(sources)
    }
}

fn ignore_file_fingerprint(metadata: &fs::Metadata) -> IgnoreFileFingerprint {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;

    IgnoreFileFingerprint {
        len: metadata.len(),
        modified_ns: metadata.modified().ok().and_then(system_time_ns),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
        #[cfg(unix)]
        changed_seconds: metadata.ctime(),
        #[cfg(unix)]
        changed_nanoseconds: metadata.ctime_nsec(),
    }
}

fn system_time_ns(time: SystemTime) -> Option<u128> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|value| value.as_nanos())
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
