use std::{fs, io::Read, path::Path};

use super::{
    CONTENT_CLASS_SAMPLE_BYTES, FileConfig, GRAFT_IGNORE_FILE, RepoPathStorage,
    RepoTrackedPathKind, Result, SQLITE_DATABASE_MAGIC,
};

#[derive(Debug, Clone, Default)]
pub(super) struct IgnoreRules {
    patterns: Vec<IgnorePattern>,
}

impl IgnoreRules {
    pub(super) fn load(worktree: &Path) -> Result<Self> {
        let path = worktree.join(GRAFT_IGNORE_FILE);
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(err.into()),
        };
        Ok(Self {
            patterns: raw.lines().filter_map(IgnorePattern::parse).collect(),
        })
    }

    pub(super) fn is_ignored(&self, key: &str, is_dir: bool) -> bool {
        !key.is_empty()
            && self
                .patterns
                .iter()
                .any(|pattern| pattern.matches(key, is_dir))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IgnorePattern {
    pattern: String,
    dir_only: bool,
    anchored: bool,
    has_slash: bool,
}

impl IgnorePattern {
    fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            return None;
        }
        let dir_only = line.ends_with('/');
        let anchored = line.starts_with('/');
        let pattern = line
            .trim_start_matches('/')
            .trim_end_matches('/')
            .trim_start_matches("./");
        let pattern = normalize_repo_path(pattern);
        if pattern.is_empty() {
            return None;
        }
        let has_slash = pattern.contains('/');
        Some(Self { pattern, dir_only, anchored, has_slash })
    }

    fn matches(&self, key: &str, is_dir: bool) -> bool {
        if self.anchored || self.has_slash {
            return self.matches_anchored(key, is_dir);
        }

        let components = key.split('/').collect::<Vec<_>>();
        for (index, component) in components.iter().enumerate() {
            let component_is_dir = index + 1 < components.len() || is_dir;
            if wildcard_match(&self.pattern, component) && (!self.dir_only || component_is_dir) {
                return true;
            }
        }
        false
    }

    fn matches_anchored(&self, key: &str, is_dir: bool) -> bool {
        if wildcard_match(&self.pattern, key) && (!self.dir_only || is_dir) {
            return true;
        }
        key.strip_prefix(&self.pattern)
            .is_some_and(|suffix| suffix.starts_with('/'))
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
    let path = path.trim().trim_start_matches("./").replace('\\', "/");
    let path = path.trim_end_matches('/');
    if path == "." {
        String::new()
    } else {
        path.to_string()
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
