use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Display},
    fs,
    io::Read,
    path::{Path, PathBuf},
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt, stream};

pub mod index;
pub mod object;

pub use object::CommitTableSummary;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    core::{
        LogId, VolumeId, byte_unit::ByteUnit, commit_hash::CommitHash, lsn::LSN, lsn::LSNRangeExt,
        page_count::PageCount,
    },
    remote::{RemoteConfig, RemoteErr},
    snapshot::Snapshot,
};

pub const GRAFT_DIR: &str = ".graft";
pub const GRAFT_IGNORE_FILE: &str = ".graftignore";
pub const REPOSITORY_FORMAT_VERSION: u32 = 2;
pub const OBJECT_FORMAT: &str = "blake3";
const NULL_OBJECT_ID: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const REFLOG_ACTOR: &str = "Graft <graft@example.invalid>";
const DEFAULT_LARGE_FILE_THRESHOLD: ByteUnit = ByteUnit::MB;

const CONFIG_FILE: &str = "config.toml";
const HEAD_FILE: &str = "HEAD";
const MERGE_HEAD_FILE: &str = "MERGE_HEAD";
const ORIG_HEAD_FILE: &str = "ORIG_HEAD";
const DIR_REFS_HEADS: &str = "refs/heads";
const DIR_REFS_REMOTES: &str = "refs/remotes";
const DIR_REFS_TAGS: &str = "refs/tags";
const DIR_OBJECTS: &str = "objects";
const DIR_OBJECTS_PACK: &str = "objects/pack";
const DIR_STORE_FJALL: &str = "store/fjall";
const DIR_STORE_FILES: &str = "store/files";
const DIR_INDEX: &str = "index";
const DIR_LOCKS: &str = "locks";
const DIR_TMP: &str = "tmp";
const DIR_LOGS_REFS: &str = "logs/refs";
const DIR_LOGS_HEAD: &str = "logs/HEAD";
const SQLITE_DATABASE_MAGIC: &[u8; 16] = b"SQLite format 3\0";
const CONTENT_CLASS_SAMPLE_BYTES: usize = 8192;
const REMOTE_REF_READ_CONCURRENCY: usize = 5;
const REMOTE_OBJECT_PACK_VERSION: u32 = 1;
const REMOTE_OBJECT_PACK_MAGIC: &[u8] = b"graft-object-pack-v1\n";

#[derive(Debug, Error)]
pub enum RepoErr {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to decode TOML: {0}")]
    TomlDe(#[from] toml::de::Error),

    #[error("failed to encode TOML: {0}")]
    TomlSer(#[from] toml::ser::Error),

    #[error("no .graft repository found from {0}")]
    NotFound(PathBuf),

    #[error("unsupported repository format version {actual}; expected {expected}")]
    UnsupportedFormat { expected: u32, actual: u32 },

    #[error("unsupported object format `{actual}`; expected `{expected}`")]
    UnsupportedObjectFormat {
        expected: &'static str,
        actual: String,
    },

    #[error("invalid ref name `{0}`")]
    InvalidRefName(String),

    #[error("cannot create ref `{reference}` because `{existing}` already exists")]
    RefNameConflict { reference: String, existing: String },

    #[error("invalid remote name `{0}`")]
    InvalidRemoteName(String),

    #[error("invalid HEAD contents: {0}")]
    InvalidHead(String),

    #[error("branch `{0}` does not exist")]
    BranchNotFound(String),

    #[error("branch `{0}` already exists")]
    BranchExists(String),

    #[error("cannot delete current branch `{0}`")]
    BranchIsCurrent(String),

    #[error("branch `{branch}` is not fully merged")]
    BranchNotMerged { branch: String, target: String },

    #[error("tag `{0}` already exists")]
    TagExists(String),

    #[error("tag `{0}` does not exist")]
    TagNotFound(String),

    #[error("remote `{0}` already exists")]
    RemoteExists(String),

    #[error("HEAD does not point at a commit yet")]
    UnbornHead,

    #[error("no changes added to commit")]
    NoStagedChanges,

    #[error("cannot commit with unresolved index conflicts")]
    UnresolvedConflicts,

    #[error("merge already in progress")]
    MergeInProgress,

    #[error("no merge in progress")]
    NoMergeInProgress,

    #[error("pull target branch `{0}` is not the current branch")]
    NotCurrentBranch(String),

    #[error("commit `{0}` does not exist")]
    CommitNotFound(String),

    #[error("unknown revision `{0}`")]
    UnknownRevision(String),

    #[error("ambiguous revision `{0}`")]
    AmbiguousRevision(String),

    #[error("invalid revision `{0}`")]
    InvalidRevision(String),

    #[error("invalid refspec `{refspec}`: {message}")]
    InvalidRefspec { refspec: String, message: String },

    #[error("path `{path}` is outside repository worktree `{worktree}`")]
    PathOutsideWorktree { path: PathBuf, worktree: PathBuf },

    #[error("path `{0}` is not valid UTF-8")]
    NonUtf8Path(PathBuf),

    #[error("path `{path}` does not exist in revision `{rev}`")]
    PathNotFoundInRevision { path: String, rev: String },

    #[error("path `{0}` is not tracked")]
    PathNotTracked(String),

    #[error("path `{0}` is not conflicted")]
    PathNotConflicted(String),

    #[error("remote `{0}` does not exist")]
    RemoteNotFound(String),

    #[error("remote `{remote}` has no branch `{branch}`")]
    RemoteBranchNotFound { remote: String, branch: String },

    #[error(
        "push rejected because remote `{remote}/{remote_branch}` is not an ancestor of local `{local_branch}`"
    )]
    NonFastForward {
        remote: String,
        local_branch: String,
        remote_branch: String,
    },

    #[error("remote ref `{remote}/{branch}` changed during push; fetch and retry")]
    RemoteRefChanged { remote: String, branch: String },

    #[error("invalid remote object `{path}`: {message}")]
    InvalidRemoteObject { path: String, message: String },

    #[error("unknown repository config key `{0}`")]
    UnknownConfigKey(String),

    #[error("invalid repository config value `{value}` for `{key}`: {message}")]
    InvalidConfigValue {
        key: String,
        value: String,
        message: String,
    },

    #[error(transparent)]
    Object(#[from] object::ObjectErr),

    #[error(transparent)]
    Remote(#[from] RemoteErr),
}

pub type Result<T> = std::result::Result<T, RepoErr>;

pub const CONFIG_KEY_FILES_INLINE_TEXT_THRESHOLD: &str = "files.inline_text_threshold";
pub const CONFIG_KEY_FILES_EXTERNAL_PATHS: &str = "files.external_paths";
pub const CONFIG_KEY_MERGE_DEFAULT_SEMANTIC_KEYS: &str = "merge.default_semantic_keys";
pub const CONFIG_KEY_MERGE_SEMANTIC_KEYS_PREFIX: &str = "merge.semantic_keys.";
pub const CONFIG_KEY_MERGE_GENERATED_COLUMNS_PREFIX: &str = "merge.generated_columns.";
pub const CONFIG_KEY_MERGE_INTERNAL_RESOLVERS_PREFIX: &str = "merge.internal_resolvers.";
pub const CONFIG_KEY_MERGE_SCHEMA_RESOLVERS_PREFIX: &str = "merge.schema_resolvers.";

const DEFAULT_INTERNAL_RESOLVERS: &[(&str, &str)] = &[
    ("index_btree", "reindex"),
    ("sqlite_sequence", "sequence_max"),
    ("sqlite_stat1", "rebuild"),
    ("sqlite_stat2", "rebuild"),
    ("sqlite_stat3", "rebuild"),
    ("sqlite_stat4", "rebuild"),
];
const DEFAULT_SCHEMA_RESOLVERS: &[(&str, &str)] = &[("add_column", "alter_table_add_column")];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoConfigEntry {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepoConfig {
    pub core: CoreConfig,

    #[serde(default)]
    pub extensions: ExtensionsConfig,

    #[serde(default, skip_serializing_if = "MergeConfig::is_empty")]
    pub merge: MergeConfig,

    #[serde(default, skip_serializing_if = "FileConfig::is_default")]
    pub files: FileConfig,

    #[serde(default)]
    pub remotes: BTreeMap<String, RemoteConfig>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub branches: BTreeMap<String, BranchConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreConfig {
    pub repository_format_version: u32,
    pub default_branch: String,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            repository_format_version: REPOSITORY_FORMAT_VERSION,
            default_branch: "main".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionsConfig {
    pub object_format: String,
}

impl Default for ExtensionsConfig {
    fn default() -> Self {
        Self { object_format: OBJECT_FORMAT.to_string() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileConfig {
    pub inline_text_threshold: ByteUnit,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external_paths: Vec<String>,
}

impl FileConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            inline_text_threshold: DEFAULT_LARGE_FILE_THRESHOLD,
            external_paths: Vec::new(),
        }
    }
}

fn normalize_config_key(key: &str) -> Result<&str> {
    let key = key.trim();
    if key.is_empty() {
        return Err(RepoErr::UnknownConfigKey(key.to_string()));
    }
    Ok(key)
}

fn config_entry(config: &RepoConfig, key: &str) -> Result<RepoConfigEntry> {
    let value = if key == CONFIG_KEY_FILES_INLINE_TEXT_THRESHOLD {
        config.files.inline_text_threshold.to_string()
    } else if key == CONFIG_KEY_FILES_EXTERNAL_PATHS {
        format_config_string_list(&config.files.external_paths)
    } else if key == CONFIG_KEY_MERGE_DEFAULT_SEMANTIC_KEYS {
        format_config_string_list(&config.merge.default_semantic_keys)
    } else if let Some(table) = config_semantic_keys_table(key)? {
        config
            .merge
            .semantic_keys
            .get(table)
            .map(|keys| format_config_string_list(keys))
            .unwrap_or_default()
    } else if let Some(table) = config_generated_columns_table(key)? {
        config
            .merge
            .generated_columns
            .get(table)
            .map(|columns| format_config_string_list(columns))
            .unwrap_or_default()
    } else if let Some(subject) = config_internal_resolver_subject(config, key)? {
        config
            .merge
            .internal_resolvers
            .get(subject)
            .cloned()
            .or_else(|| default_internal_resolver(subject).map(str::to_string))
            .unwrap_or_default()
    } else if let Some(operation) = config_schema_resolver_operation(config, key)? {
        config
            .merge
            .schema_resolvers
            .get(operation)
            .cloned()
            .or_else(|| default_schema_resolver(operation).map(str::to_string))
            .unwrap_or_default()
    } else {
        return Err(RepoErr::UnknownConfigKey(key.to_string()));
    };
    Ok(RepoConfigEntry { key: key.to_string(), value })
}

fn config_entries(config: &RepoConfig) -> Vec<RepoConfigEntry> {
    let mut entries = vec![
        RepoConfigEntry {
            key: CONFIG_KEY_FILES_INLINE_TEXT_THRESHOLD.to_string(),
            value: config.files.inline_text_threshold.to_string(),
        },
        RepoConfigEntry {
            key: CONFIG_KEY_FILES_EXTERNAL_PATHS.to_string(),
            value: format_config_string_list(&config.files.external_paths),
        },
        RepoConfigEntry {
            key: CONFIG_KEY_MERGE_DEFAULT_SEMANTIC_KEYS.to_string(),
            value: format_config_string_list(&config.merge.default_semantic_keys),
        },
    ];

    entries.extend(
        config
            .merge
            .semantic_keys
            .iter()
            .map(|(table, keys)| RepoConfigEntry {
                key: format!("{CONFIG_KEY_MERGE_SEMANTIC_KEYS_PREFIX}{table}"),
                value: format_config_string_list(keys),
            }),
    );
    entries.extend(DEFAULT_INTERNAL_RESOLVERS.iter().map(|(subject, default)| {
        RepoConfigEntry {
            key: format!("{CONFIG_KEY_MERGE_INTERNAL_RESOLVERS_PREFIX}{subject}"),
            value: config
                .merge
                .internal_resolvers
                .get(*subject)
                .cloned()
                .unwrap_or_else(|| (*default).to_string()),
        }
    }));
    entries.extend(DEFAULT_SCHEMA_RESOLVERS.iter().map(|(operation, default)| {
        RepoConfigEntry {
            key: format!("{CONFIG_KEY_MERGE_SCHEMA_RESOLVERS_PREFIX}{operation}"),
            value: config
                .merge
                .schema_resolvers
                .get(*operation)
                .cloned()
                .unwrap_or_else(|| (*default).to_string()),
        }
    }));
    entries.extend(
        config
            .merge
            .generated_columns
            .iter()
            .map(|(table, columns)| RepoConfigEntry {
                key: format!("{CONFIG_KEY_MERGE_GENERATED_COLUMNS_PREFIX}{table}"),
                value: format_config_string_list(columns),
            }),
    );

    entries
}

fn config_semantic_keys_table(key: &str) -> Result<Option<&str>> {
    config_key_suffix(key, CONFIG_KEY_MERGE_SEMANTIC_KEYS_PREFIX)
}

fn config_generated_columns_table(key: &str) -> Result<Option<&str>> {
    config_key_suffix(key, CONFIG_KEY_MERGE_GENERATED_COLUMNS_PREFIX)
}

fn config_internal_resolver_subject<'a>(
    config: &RepoConfig,
    key: &'a str,
) -> Result<Option<&'a str>> {
    let Some(subject) = config_key_suffix(key, CONFIG_KEY_MERGE_INTERNAL_RESOLVERS_PREFIX)? else {
        return Ok(None);
    };
    if default_internal_resolver(subject).is_some()
        || config.merge.internal_resolvers.contains_key(subject)
    {
        Ok(Some(subject))
    } else {
        Err(RepoErr::UnknownConfigKey(key.to_string()))
    }
}

fn config_schema_resolver_operation<'a>(
    config: &RepoConfig,
    key: &'a str,
) -> Result<Option<&'a str>> {
    let Some(operation) = config_key_suffix(key, CONFIG_KEY_MERGE_SCHEMA_RESOLVERS_PREFIX)? else {
        return Ok(None);
    };
    if default_schema_resolver(operation).is_some()
        || config.merge.schema_resolvers.contains_key(operation)
    {
        Ok(Some(operation))
    } else {
        Err(RepoErr::UnknownConfigKey(key.to_string()))
    }
}

fn config_key_suffix<'a>(key: &'a str, prefix: &str) -> Result<Option<&'a str>> {
    let Some(suffix) = key.strip_prefix(prefix) else {
        return Ok(None);
    };
    if suffix.trim().is_empty() {
        return Err(RepoErr::UnknownConfigKey(key.to_string()));
    }
    Ok(Some(suffix))
}

fn format_config_string_list(values: &[String]) -> String {
    values.join(", ")
}

fn default_internal_resolver(subject: &str) -> Option<&'static str> {
    DEFAULT_INTERNAL_RESOLVERS
        .iter()
        .find_map(|(candidate, resolver)| (*candidate == subject).then_some(*resolver))
}

fn default_schema_resolver(operation: &str) -> Option<&'static str> {
    DEFAULT_SCHEMA_RESOLVERS
        .iter()
        .find_map(|(candidate, resolver)| (*candidate == operation).then_some(*resolver))
}

fn parse_config_byte_unit_value(key: &str, value: &str) -> Result<ByteUnit> {
    let token_count = value.split_ascii_whitespace().count();
    if token_count > 2 {
        return Err(RepoErr::InvalidConfigValue {
            key: key.to_string(),
            value: value.to_string(),
            message: "expected <number> [<unit>]".to_string(),
        });
    }

    value
        .parse::<ByteUnit>()
        .map_err(|err| RepoErr::InvalidConfigValue {
            key: key.to_string(),
            value: value.to_string(),
            message: err.to_string(),
        })
}

fn parse_config_string_list_value(key: &str, value: &str) -> Result<Vec<String>> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(Vec::new());
    }

    let mut values = Vec::new();
    let mut seen = BTreeSet::new();
    for segment in value.split(',') {
        let segment = segment.trim();
        if segment.is_empty() {
            return Err(RepoErr::InvalidConfigValue {
                key: key.to_string(),
                value: value.to_string(),
                message: "expected comma or whitespace separated values".to_string(),
            });
        }
        for item in segment.split_ascii_whitespace() {
            if !seen.insert(item.to_string()) {
                return Err(RepoErr::InvalidConfigValue {
                    key: key.to_string(),
                    value: value.to_string(),
                    message: format!("duplicate value `{item}`"),
                });
            }
            values.push(item.to_string());
        }
    }
    Ok(values)
}

fn parse_config_internal_resolver_value(key: &str, subject: &str, value: &str) -> Result<String> {
    let value = value.trim();
    let Some(expected) = default_internal_resolver(subject) else {
        return Err(RepoErr::UnknownConfigKey(key.to_string()));
    };
    if value != expected {
        return Err(RepoErr::InvalidConfigValue {
            key: key.to_string(),
            value: value.to_string(),
            message: format!("expected `{expected}` for `{subject}`"),
        });
    }
    Ok(value.to_string())
}

fn parse_config_schema_resolver_value(key: &str, operation: &str, value: &str) -> Result<String> {
    let value = value.trim();
    let Some(expected) = default_schema_resolver(operation) else {
        return Err(RepoErr::UnknownConfigKey(key.to_string()));
    };
    if value != expected {
        return Err(RepoErr::InvalidConfigValue {
            key: key.to_string(),
            value: value.to_string(),
            message: format!("expected `{expected}` for `{operation}`"),
        });
    }
    Ok(value.to_string())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_semantic_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub semantic_keys: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub internal_resolvers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub schema_resolvers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub generated_columns: BTreeMap<String, Vec<String>>,
}

impl MergeConfig {
    fn is_empty(&self) -> bool {
        self.default_semantic_keys.is_empty()
            && self.semantic_keys.is_empty()
            && self.internal_resolvers.is_empty()
            && self.schema_resolvers.is_empty()
            && self.generated_columns.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Head {
    Branch { name: String },
    Detached { commit: String },
}

impl Head {
    pub fn branch(name: impl Into<String>) -> Self {
        Self::Branch { name: name.into() }
    }

    fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if let Some(reference) = raw.strip_prefix("ref: ") {
            let branch = reference
                .strip_prefix("refs/heads/")
                .ok_or_else(|| RepoErr::InvalidHead(raw.to_string()))?;
            validate_ref_name(branch)?;
            Ok(Self::branch(branch))
        } else if raw.is_empty() {
            Err(RepoErr::InvalidHead(raw.to_string()))
        } else {
            Ok(Self::Detached { commit: raw.to_string() })
        }
    }

    fn serialize(&self) -> String {
        match self {
            Self::Branch { name } => format!("ref: refs/heads/{name}\n"),
            Self::Detached { commit } => format!("{commit}\n"),
        }
    }

    pub fn branch_name(&self) -> Option<&str> {
        match self {
            Self::Branch { name } => Some(name),
            Self::Detached { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchUpstream {
    pub remote: String,
    pub branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchInfo {
    pub name: String,
    pub target: Option<String>,
    pub current: bool,
    pub upstream: Option<BranchUpstream>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagInfo {
    pub name: String,
    pub object: String,
    pub target: String,
    pub annotated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteInfo {
    pub name: String,
    pub config: RemoteConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemotePruneOutcome {
    pub remote: String,
    pub branches: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchOutcome {
    pub remote: String,
    pub branch: String,
    pub head: String,
    pub commits: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchAllOutcome {
    pub remote: String,
    pub branches: Vec<FetchOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushOutcome {
    pub remote: String,
    pub local_branch: String,
    pub remote_branch: String,
    pub head: String,
    pub commits: usize,
    pub forced: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushAllOutcome {
    pub remote: String,
    pub branches: Vec<PushOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushRefspecBranch {
    pub local_branch: String,
    pub remote_branch: String,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteBranchRef {
    pub remote: String,
    pub branch: String,
    pub head: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteBranchHead {
    pub raw: Option<Bytes>,
    pub head: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemoteObjectPackIndex {
    version: u32,
    pack: String,
    objects: Vec<RemoteObjectPackEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemoteObjectPackEntry {
    id: object::ObjectId,
    offset: u64,
    len: u64,
}

#[derive(Debug, Default)]
struct RemoteObjectPackCache {
    indexes: Option<Vec<RemoteObjectPackIndex>>,
    packs: BTreeMap<String, Bytes>,
}

impl RemoteObjectPackCache {
    fn indexes(&mut self, remote: &crate::remote::Remote) -> Result<&[RemoteObjectPackIndex]> {
        if self.indexes.is_none() {
            self.indexes = Some(fetch_remote_object_pack_indexes(remote)?);
        }
        Ok(self.indexes.as_deref().expect("pack indexes initialized"))
    }

    fn pack_bytes(&mut self, remote: &crate::remote::Remote, pack: &str) -> Result<Bytes> {
        if let Some(bytes) = self.packs.get(pack) {
            return Ok(bytes.clone());
        }
        let bytes =
            block_on_remote(remote.get_raw(pack))?.ok_or_else(|| RepoErr::InvalidRemoteObject {
                path: pack.to_string(),
                message: "missing pack object".to_string(),
            })?;
        self.packs.insert(pack.to_string(), bytes.clone());
        Ok(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedRefspec {
    source: Option<BranchPattern>,
    destination: Option<BranchPattern>,
    force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BranchPattern {
    Exact(String),
    Wildcard { prefix: String, suffix: String },
}

impl BranchPattern {
    fn is_wildcard(&self) -> bool {
        matches!(self, Self::Wildcard { .. })
    }

    fn exact(&self) -> Option<&str> {
        match self {
            Self::Exact(branch) => Some(branch),
            Self::Wildcard { .. } => None,
        }
    }

    fn capture<'a>(&self, branch: &'a str) -> Result<Option<&'a str>> {
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

    fn expand(&self, capture: &str) -> Result<String> {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullOutcome {
    pub remote: String,
    pub remote_branch: String,
    pub local_branch: String,
    pub head: String,
    pub commits: usize,
    pub merge: MergeOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullPlan {
    pub remote: String,
    pub remote_branch: String,
    pub local_branch: String,
    pub fetch: FetchOutcome,
    pub merge: MergePlan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MergeOutcome {
    FastForward {
        from: Option<String>,
        to: String,
    },
    AlreadyUpToDate {
        head: String,
    },
    Merged {
        head: String,
        target: String,
        merge_base: Option<String>,
        staged: Vec<String>,
        conflicted: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePlan {
    pub rev: String,
    pub target: String,
    pub checkout: CheckoutPlan,
    pub outcome: MergeOutcome,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<index::Index>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeAbortPlan {
    pub target: String,
    pub checkout: CheckoutPlan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitObject {
    pub id: String,
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parents: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    pub message: String,
    pub timestamp_ms: u64,

    #[serde(default)]
    pub files: BTreeMap<String, CommitFileState>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifacts: BTreeMap<String, CommitArtifactState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<CommitPathChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tables: Vec<CommitTableSummary>,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub changed_tables: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitPathChange {
    pub path: String,
    pub change: RepoFileChange,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitFileState {
    pub volume: VolumeId,
    pub snapshot: RepoSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommitArtifactState {
    File {
        kind: RepoTrackedPathKind,
        oid: object::ObjectId,
        content_hash: object::ObjectId,
        size: u64,
    },
    LargeFile {
        kind: RepoTrackedPathKind,
        oid: object::ObjectId,
        content_hash: object::ObjectId,
        size: u64,
    },
}

impl CommitArtifactState {
    pub fn oid(&self) -> &object::ObjectId {
        match self {
            Self::File { oid, .. } | Self::LargeFile { oid, .. } => oid,
        }
    }

    pub fn content_hash(&self) -> &object::ObjectId {
        match self {
            Self::File { content_hash, .. } | Self::LargeFile { content_hash, .. } => content_hash,
        }
    }

    pub fn size(&self) -> u64 {
        match self {
            Self::File { size, .. } | Self::LargeFile { size, .. } => *size,
        }
    }

    pub fn is_large(&self) -> bool {
        matches!(self, Self::LargeFile { .. })
    }
}

fn artifact_tracked_path_kind(state: &CommitArtifactState) -> RepoTrackedPathKind {
    match state {
        CommitArtifactState::File { kind, .. } | CommitArtifactState::LargeFile { kind, .. } => {
            *kind
        }
    }
}

fn artifact_diff_kind(
    before: Option<&CommitArtifactState>,
    after: Option<&CommitArtifactState>,
) -> RepoTrackedPathKind {
    after
        .or(before)
        .map(artifact_tracked_path_kind)
        .unwrap_or(RepoTrackedPathKind::BinaryFile)
}

fn artifact_tracked_path_storage(state: &CommitArtifactState) -> RepoPathStorage {
    match state {
        CommitArtifactState::File { .. } => RepoPathStorage::Inline,
        CommitArtifactState::LargeFile { .. } => RepoPathStorage::External,
    }
}

fn artifact_diff_storage(
    before: Option<&CommitArtifactState>,
    after: Option<&CommitArtifactState>,
) -> RepoPathStorage {
    after
        .or(before)
        .map(artifact_tracked_path_storage)
        .unwrap_or(RepoPathStorage::Inline)
}

fn default_path_storage(kind: RepoTrackedPathKind) -> RepoPathStorage {
    match kind {
        RepoTrackedPathKind::SqliteDatabase => RepoPathStorage::SqliteSnapshot,
        RepoTrackedPathKind::TextFile => RepoPathStorage::Inline,
        RepoTrackedPathKind::BinaryFile => RepoPathStorage::External,
    }
}

fn repo_path_kind_from_object_kind(kind: object::FileContentKind) -> RepoTrackedPathKind {
    match kind {
        object::FileContentKind::TextFile => RepoTrackedPathKind::TextFile,
        object::FileContentKind::BinaryFile => RepoTrackedPathKind::BinaryFile,
    }
}

fn object_kind_from_repo_path_kind(kind: RepoTrackedPathKind) -> object::FileContentKind {
    match kind {
        RepoTrackedPathKind::TextFile => object::FileContentKind::TextFile,
        RepoTrackedPathKind::SqliteDatabase | RepoTrackedPathKind::BinaryFile => {
            object::FileContentKind::BinaryFile
        }
    }
}

fn tracked_file_entry(
    path: String,
    stage: index::IndexStage,
    file: &CommitFileState,
) -> RepoTrackedPathEntry {
    let blob = object::Object::Blob(object::BlobObject::SqliteSnapshot(sqlite_snapshot_blob(
        file,
    )));
    RepoTrackedPathEntry {
        path,
        stage,
        kind: RepoTrackedPathKind::SqliteDatabase,
        storage: RepoPathStorage::SqliteSnapshot,
        mode: Some(object::TreeEntryMode::SqliteDatabase),
        oid: Some(blob.id()),
        size: None,
        page_count: Some(file.snapshot.page_count),
    }
}

fn tracked_artifact_entry(
    path: String,
    stage: index::IndexStage,
    artifact: &CommitArtifactState,
) -> RepoTrackedPathEntry {
    let kind = artifact_tracked_path_kind(artifact);
    RepoTrackedPathEntry {
        path,
        stage,
        kind,
        storage: artifact_tracked_path_storage(artifact),
        mode: Some(object::TreeEntryMode::Regular),
        oid: Some(artifact.oid().clone()),
        size: Some(artifact.size()),
        page_count: None,
    }
}

fn tracked_index_entry(entry: &index::IndexEntry) -> Option<RepoTrackedPathEntry> {
    if let Some(file) = &entry.file {
        let mut tracked = tracked_file_entry(entry.path.clone(), entry.stage, file);
        tracked.mode = entry.mode;
        tracked.oid = entry.oid.clone().or(tracked.oid);
        Some(tracked)
    } else if let Some(artifact) = &entry.artifact {
        let mut tracked = tracked_artifact_entry(entry.path.clone(), entry.stage, artifact);
        tracked.mode = entry.mode;
        tracked.oid = entry.oid.clone().or(tracked.oid);
        Some(tracked)
    } else {
        None
    }
}

fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoDiff {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub paths: Vec<RepoPathDiff>,
    pub files: Vec<RepoFileDiff>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<RepoArtifactDiff>,
}

impl RepoDiff {
    pub fn refresh_paths(&mut self) {
        self.paths = repo_diff_paths(&self.files, &self.artifacts);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoPathDiff {
    pub path: String,
    pub change: RepoFileChange,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoFileDiff {
    pub path: String,
    pub change: RepoFileChange,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
    pub from: Option<CommitFileState>,
    pub to: Option<CommitFileState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoArtifactDiff {
    pub path: String,
    pub change: RepoFileChange,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
    pub from: Option<CommitArtifactState>,
    pub to: Option<CommitArtifactState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoFileChange {
    Added,
    Deleted,
    Modified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetOutcome {
    pub target: String,
    pub mode: ResetMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetPlan {
    pub rev: String,
    pub target: String,
    pub mode: ResetMode,
    pub checkout: CheckoutPlan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutFileOutcome {
    pub target: String,
    pub path: String,
    pub state: CommitFileState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutFilePlan {
    pub target: String,
    pub path: String,
    pub state: CommitFileState,
    pub entry: index::IndexEntry,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutArtifactOutcome {
    pub target: String,
    pub path: String,
    pub state: CommitArtifactState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutArtifactPlan {
    pub target: String,
    pub path: String,
    pub state: CommitArtifactState,
    pub entry: index::IndexEntry,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutPlan {
    pub target: Option<String>,
    pub files: BTreeMap<String, CommitFileState>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifacts: BTreeMap<String, CommitArtifactState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchNewBranchPlan {
    pub branch: BranchInfo,
    pub checkout: CheckoutPlan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSnapshot {
    pub page_count: PageCount,
    pub ranges: Vec<RepoLogRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoLogRange {
    pub log: LogId,
    pub start: LSN,
    pub end: LSN,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<RepoStorageCommit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoStorageCommit {
    pub lsn: LSN,
    pub commit_hash: CommitHash,
}

impl RepoSnapshot {
    pub fn from_snapshot(snapshot: &Snapshot) -> Self {
        Self {
            page_count: snapshot.page_count,
            ranges: snapshot
                .iter()
                .map(|range| RepoLogRange {
                    log: range.log.clone(),
                    start: *range.lsns.start(),
                    end: *range.lsns.end(),
                    commits: Vec::new(),
                })
                .collect(),
        }
    }

    pub fn to_snapshot(&self) -> Snapshot {
        let Some((first, rest)) = self.ranges.split_first() else {
            return Snapshot::empty();
        };

        let mut snapshot =
            Snapshot::new(first.log.clone(), first.start..=first.end, self.page_count);
        for range in rest {
            snapshot.append(range.log.clone(), range.start..=range.end);
        }
        snapshot
    }

    pub fn expected_commit_count(&self) -> u64 {
        self.ranges
            .iter()
            .map(|range| (range.start..=range.end).len())
            .sum()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoStatus {
    pub worktree: PathBuf,
    pub graft_dir: PathBuf,
    pub repository_format_version: u32,
    pub head: Head,
    pub head_target: Option<String>,
    pub merge_head: Option<String>,
    pub orig_head: Option<String>,
    pub dirty: bool,
    #[serde(default)]
    pub has_unstaged_changes: bool,
    #[serde(default)]
    pub has_staged_changes: bool,
    #[serde(default)]
    pub has_conflicts: bool,
    #[serde(default)]
    pub work_in_progress: bool,
    #[serde(default)]
    pub counts: RepoStatusCounts,
    #[serde(default)]
    pub paths: Vec<RepoStatusPath>,
    #[serde(default)]
    pub unstaged: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unstaged_changes: Vec<RepoWorktreeChange>,
    pub staged: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub staged_changes: Vec<RepoStagedChange>,
    pub conflicted: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicted_changes: Vec<RepoConflictChange>,
    pub branches: Vec<BranchInfo>,
    pub remotes: Vec<RemoteInfo>,
    pub upstream: Option<BranchUpstream>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_status: Option<RepoUpstreamStatus>,
    #[serde(default)]
    pub ahead: usize,
    #[serde(default)]
    pub behind: usize,
}

impl RepoStatus {
    pub fn refresh_summary_flags(&mut self) {
        self.counts = RepoStatusCounts::from_status_parts(
            self.unstaged.len(),
            self.unstaged_changes.len(),
            self.staged.len(),
            self.staged_changes.len(),
            self.conflicted.len(),
            self.conflicted_changes.len(),
        );
        self.has_unstaged_changes = self.counts.unstaged > 0;
        self.has_staged_changes = self.counts.staged > 0;
        self.has_conflicts = self.counts.conflicted > 0;
        self.work_in_progress = self.has_unstaged_changes
            || self.has_staged_changes
            || self.has_conflicts
            || self.merge_head.is_some();
        self.dirty = self.has_unstaged_changes;
        self.paths = Self::status_paths_from_changes(
            &self.unstaged_changes,
            &self.staged_changes,
            &self.conflicted_changes,
        );
    }

    fn status_paths_from_changes(
        unstaged_changes: &[RepoWorktreeChange],
        staged_changes: &[RepoStagedChange],
        conflicted_changes: &[RepoConflictChange],
    ) -> Vec<RepoStatusPath> {
        #[derive(Default)]
        struct Builder {
            kind: Option<RepoTrackedPathKind>,
            storage: Option<RepoPathStorage>,
            unstaged_change: Option<RepoWorktreeChangeKind>,
            staged_change: Option<RepoFileChange>,
            conflicted: bool,
        }

        fn kind_priority(kind: RepoTrackedPathKind) -> u8 {
            match kind {
                RepoTrackedPathKind::TextFile => 1,
                RepoTrackedPathKind::BinaryFile => 1,
                RepoTrackedPathKind::SqliteDatabase => 3,
            }
        }

        fn storage_priority(storage: RepoPathStorage) -> u8 {
            match storage {
                RepoPathStorage::Inline => 1,
                RepoPathStorage::External => 2,
                RepoPathStorage::SqliteSnapshot => 3,
            }
        }

        fn record_kind(target: &mut Option<RepoTrackedPathKind>, kind: RepoTrackedPathKind) {
            if target.is_none_or(|existing| kind_priority(kind) > kind_priority(existing)) {
                *target = Some(kind);
            }
        }

        fn record_storage(target: &mut Option<RepoPathStorage>, storage: RepoPathStorage) {
            if target.is_none_or(|existing| storage_priority(storage) > storage_priority(existing))
            {
                *target = Some(storage);
            }
        }

        let mut paths = BTreeMap::<String, Builder>::new();

        for change in unstaged_changes {
            let entry = paths.entry(change.path.clone()).or_default();
            record_kind(&mut entry.kind, change.kind);
            record_storage(&mut entry.storage, change.storage);
            entry.unstaged_change = Some(change.change);
        }

        for change in staged_changes {
            let entry = paths.entry(change.path.clone()).or_default();
            record_kind(&mut entry.kind, change.kind);
            record_storage(&mut entry.storage, change.storage);
            entry.staged_change = Some(change.change);
        }

        for change in conflicted_changes {
            let entry = paths.entry(change.path.clone()).or_default();
            record_kind(&mut entry.kind, change.kind);
            record_storage(&mut entry.storage, change.storage);
            entry.conflicted = true;
        }

        paths
            .into_iter()
            .filter_map(|(path, entry)| {
                entry.kind.map(|kind| {
                    let index_status = if entry.conflicted {
                        RepoStatusPathState::Unmerged
                    } else {
                        entry
                            .staged_change
                            .map(RepoStatusPathState::from_staged_change)
                            .unwrap_or(RepoStatusPathState::None)
                    };
                    let worktree_status = if entry.conflicted {
                        RepoStatusPathState::Unmerged
                    } else {
                        entry
                            .unstaged_change
                            .map(RepoStatusPathState::from_worktree_change)
                            .unwrap_or(RepoStatusPathState::None)
                    };
                    let code = RepoStatusPathState::code(index_status, worktree_status);
                    RepoStatusPath {
                        path,
                        kind,
                        storage: entry.storage.unwrap_or_else(|| default_path_storage(kind)),
                        index_status,
                        worktree_status,
                        code,
                        unstaged_change: entry.unstaged_change,
                        staged_change: entry.staged_change,
                        conflicted: entry.conflicted,
                    }
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoStatusCounts {
    pub unstaged: usize,
    pub staged: usize,
    pub conflicted: usize,
}

impl RepoStatusCounts {
    fn from_status_parts(
        unstaged: usize,
        unstaged_changes: usize,
        staged: usize,
        staged_changes: usize,
        conflicted: usize,
        conflicted_changes: usize,
    ) -> Self {
        Self {
            unstaged: unstaged.max(unstaged_changes),
            staged: staged.max(staged_changes),
            conflicted: conflicted.max(conflicted_changes),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoStatusPath {
    pub path: String,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
    pub index_status: RepoStatusPathState,
    pub worktree_status: RepoStatusPathState,
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unstaged_change: Option<RepoWorktreeChangeKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_change: Option<RepoFileChange>,
    #[serde(default)]
    pub conflicted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoStatusPathState {
    None,
    Added,
    Modified,
    Deleted,
    Untracked,
    Unmerged,
}

impl RepoStatusPathState {
    fn from_staged_change(change: RepoFileChange) -> Self {
        match change {
            RepoFileChange::Added => Self::Added,
            RepoFileChange::Deleted => Self::Deleted,
            RepoFileChange::Modified => Self::Modified,
        }
    }

    fn from_worktree_change(change: RepoWorktreeChangeKind) -> Self {
        match change {
            RepoWorktreeChangeKind::Deleted => Self::Deleted,
            RepoWorktreeChangeKind::Modified => Self::Modified,
            RepoWorktreeChangeKind::Untracked => Self::Untracked,
        }
    }

    fn code(index: Self, worktree: Self) -> String {
        if index == Self::Unmerged || worktree == Self::Unmerged {
            return "UU".to_string();
        }
        if index == Self::None && worktree == Self::Untracked {
            return "??".to_string();
        }
        format!("{}{}", index.index_code(), worktree.worktree_code())
    }

    fn index_code(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Deleted => 'D',
            Self::Modified => 'M',
            Self::None | Self::Untracked | Self::Unmerged => ' ',
        }
    }

    fn worktree_code(self) -> char {
        match self {
            Self::Deleted => 'D',
            Self::Modified => 'M',
            Self::Untracked => '?',
            Self::None | Self::Added | Self::Unmerged => ' ',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoArtifactAudit {
    pub artifacts: usize,
    pub external_payloads: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<RepoArtifactAuditIssue>,
}

impl RepoArtifactAudit {
    pub fn ok(&self) -> bool {
        self.issues.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoArtifactRepairOutcome {
    pub remote: String,
    pub fetched_objects: usize,
    pub fetched_external_payloads: usize,
    pub before: RepoArtifactAudit,
    pub after: RepoArtifactAudit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoLargeFileFetchOutcome {
    pub remote: String,
    pub target: String,
    pub external_payloads: usize,
    pub already_present_payloads: usize,
    pub fetched_payloads: usize,
    pub fetched_bytes: u64,
    pub files: Vec<RepoLargeFileFetchEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoLargeFileFetchEntry {
    pub content_hash: object::ObjectId,
    pub size: u64,
    pub store_path: String,
    pub status: RepoLargeFileFetchStatus,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoLargeFileFetchStatus {
    Present,
    Fetched,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoLargeFileStatusOutcome {
    pub target: String,
    pub external_payloads: usize,
    pub present_payloads: usize,
    pub missing_payloads: usize,
    pub invalid_payloads: usize,
    pub present_bytes: u64,
    pub missing_bytes: u64,
    pub invalid_bytes: u64,
    pub files: Vec<RepoLargeFileStatusEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoLargeFileStatusEntry {
    pub content_hash: object::ObjectId,
    pub size: u64,
    pub store_path: String,
    pub status: RepoLargeFileStatusState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoLargeFileStatusState {
    Present,
    Missing,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoLargeFilePruneOutcome {
    pub dry_run: bool,
    pub referenced_payloads: usize,
    pub candidate_payloads: usize,
    pub candidate_bytes: u64,
    pub pruned_payloads: usize,
    pub pruned_bytes: u64,
    pub files: Vec<RepoLargeFilePruneEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoLargeFilePruneEntry {
    pub content_hash: object::ObjectId,
    pub size: u64,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoArtifactAuditIssue {
    pub path: String,
    pub kind: RepoArtifactAuditIssueKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oid: Option<object::ObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<object::ObjectId>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoArtifactAuditIssueKind {
    MissingObject,
    InvalidObject,
    MissingExternalPayload,
    InvalidExternalPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoTrackedPath {
    pub path: String,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_count: Option<PageCount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoTrackedPathDetail {
    pub path: String,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_count: Option<PageCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oid: Option<object::ObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<object::ObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_present: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_payload_present: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoTrackedPathEntry {
    pub path: String,
    pub stage: index::IndexStage,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<object::TreeEntryMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oid: Option<object::ObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_count: Option<PageCount>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoTrackedPathKind {
    SqliteDatabase,
    TextFile,
    BinaryFile,
}

impl Display for RepoTrackedPathKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SqliteDatabase => f.write_str("sqlite_database"),
            Self::TextFile => f.write_str("text_file"),
            Self::BinaryFile => f.write_str("binary_file"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoPathStorage {
    SqliteSnapshot,
    Inline,
    External,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoUpstreamStatus {
    pub remote: String,
    pub branch: String,
    pub local: String,
    pub remote_target: String,
    pub ahead: usize,
    pub behind: usize,
    pub state: RepoUpstreamState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoUpstreamState {
    UpToDate,
    Ahead,
    Behind,
    Diverged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoWorktreeChange {
    pub path: String,
    pub change: RepoWorktreeChangeKind,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoStagedChange {
    pub path: String,
    pub change: RepoFileChange,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoConflictChange {
    pub path: String,
    pub kind: RepoTrackedPathKind,
    pub storage: RepoPathStorage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoWorktreeChangeKind {
    Modified,
    Deleted,
    Untracked,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct WorktreeState {
    #[serde(default)]
    dirty: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    deleted: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    worktree: PathBuf,
    graft_dir: PathBuf,
}

impl Repository {
    pub fn init(worktree: impl AsRef<Path>) -> Result<Self> {
        let worktree = worktree.as_ref();
        fs::create_dir_all(worktree)?;

        let worktree = fs::canonicalize(worktree)?;
        let graft_dir = worktree.join(GRAFT_DIR);
        let repo = Self { worktree, graft_dir };

        repo.create_layout()?;

        if !repo.config_path().exists() {
            repo.write_config(&RepoConfig::default())?;
        } else {
            repo.ensure_supported_format()?;
        }

        if !repo.head_path().exists() {
            let default_branch = repo.config()?.core.default_branch;
            repo.write_head(&Head::branch(default_branch))?;
        }

        Ok(repo)
    }

    pub fn init_for_file(path: impl AsRef<Path>) -> Result<Self> {
        Self::init(worktree_for_file(path.as_ref()))
    }

    pub fn open(worktree: impl AsRef<Path>) -> Result<Self> {
        let worktree = fs::canonicalize(worktree)?;
        let graft_dir = worktree.join(GRAFT_DIR);
        if !graft_dir.is_dir() {
            return Err(RepoErr::NotFound(worktree));
        }

        let repo = Self { worktree, graft_dir };
        repo.ensure_supported_format()?;
        Ok(repo)
    }

    pub fn discover(start: impl AsRef<Path>) -> Result<Self> {
        let original = start.as_ref().to_path_buf();
        let mut current = normalize_discovery_start(start.as_ref())?;

        loop {
            let graft_dir = current.join(GRAFT_DIR);
            if graft_dir.is_dir() {
                return Self::open(&current);
            }

            if !current.pop() {
                return Err(RepoErr::NotFound(original));
            }
        }
    }

    pub fn discover_for_file(path: impl AsRef<Path>) -> Result<Self> {
        Self::discover(worktree_for_file(path.as_ref()))
    }

    pub fn worktree(&self) -> &Path {
        &self.worktree
    }

    pub fn graft_dir(&self) -> &Path {
        &self.graft_dir
    }

    pub fn store_dir(&self) -> PathBuf {
        self.graft_dir.join(DIR_STORE_FJALL)
    }

    pub fn file_store_dir(&self) -> PathBuf {
        self.graft_dir.join(DIR_STORE_FILES)
    }

    pub fn object_store(&self) -> object::LooseObjectStore {
        object::LooseObjectStore::new(self.graft_dir.join("objects"))
    }

    pub fn config(&self) -> Result<RepoConfig> {
        let raw = fs::read_to_string(self.config_path())?;
        Ok(toml::from_str(&raw)?)
    }

    pub fn write_config(&self, config: &RepoConfig) -> Result<()> {
        let raw = toml::to_string_pretty(config)?;
        fs::write(self.config_path(), raw)?;
        Ok(())
    }

    pub fn config_get(&self, key: &str) -> Result<RepoConfigEntry> {
        let config = self.config()?;
        config_entry(&config, normalize_config_key(key)?)
    }

    pub fn config_list(&self) -> Result<Vec<RepoConfigEntry>> {
        Ok(config_entries(&self.config()?))
    }

    pub fn config_set(&self, key: &str, value: &str) -> Result<RepoConfigEntry> {
        let key = normalize_config_key(key)?;
        let value = value.trim();
        let mut config = self.config()?;

        match key {
            CONFIG_KEY_FILES_INLINE_TEXT_THRESHOLD => {
                config.files.inline_text_threshold = parse_config_byte_unit_value(key, value)?;
            }
            CONFIG_KEY_FILES_EXTERNAL_PATHS => {
                config.files.external_paths = parse_config_string_list_value(key, value)?
                    .into_iter()
                    .map(|path| normalize_repo_path(&path))
                    .collect();
            }
            CONFIG_KEY_MERGE_DEFAULT_SEMANTIC_KEYS => {
                config.merge.default_semantic_keys = parse_config_string_list_value(key, value)?;
            }
            _ => {
                if let Some(table) = config_semantic_keys_table(key)? {
                    let keys = parse_config_string_list_value(key, value)?;
                    if keys.is_empty() {
                        config.merge.semantic_keys.remove(table);
                    } else {
                        config.merge.semantic_keys.insert(table.to_string(), keys);
                    }
                } else if let Some(table) = config_generated_columns_table(key)? {
                    let columns = parse_config_string_list_value(key, value)?;
                    if columns.is_empty() {
                        config.merge.generated_columns.remove(table);
                    } else {
                        config
                            .merge
                            .generated_columns
                            .insert(table.to_string(), columns);
                    }
                } else if let Some(subject) = config_internal_resolver_subject(&config, key)? {
                    let resolver = parse_config_internal_resolver_value(key, subject, value)?;
                    config
                        .merge
                        .internal_resolvers
                        .insert(subject.to_string(), resolver);
                } else if let Some(operation) = config_schema_resolver_operation(&config, key)? {
                    let resolver = parse_config_schema_resolver_value(key, operation, value)?;
                    config
                        .merge
                        .schema_resolvers
                        .insert(operation.to_string(), resolver);
                } else {
                    return Err(RepoErr::UnknownConfigKey(key.to_string()));
                }
            }
        }

        self.write_config(&config)?;
        config_entry(&config, key)
    }

    pub fn config_unset(&self, key: &str) -> Result<RepoConfigEntry> {
        let key = normalize_config_key(key)?;
        let mut config = self.config()?;

        match key {
            CONFIG_KEY_FILES_INLINE_TEXT_THRESHOLD => {
                config.files.inline_text_threshold = FileConfig::default().inline_text_threshold;
            }
            CONFIG_KEY_FILES_EXTERNAL_PATHS => {
                config.files.external_paths.clear();
            }
            CONFIG_KEY_MERGE_DEFAULT_SEMANTIC_KEYS => {
                config.merge.default_semantic_keys.clear();
            }
            _ => {
                if let Some(table) = config_semantic_keys_table(key)? {
                    config.merge.semantic_keys.remove(table);
                } else if let Some(table) = config_generated_columns_table(key)? {
                    config.merge.generated_columns.remove(table);
                } else if let Some(subject) = config_internal_resolver_subject(&config, key)? {
                    config.merge.internal_resolvers.remove(subject);
                } else if let Some(operation) = config_schema_resolver_operation(&config, key)? {
                    config.merge.schema_resolvers.remove(operation);
                } else {
                    return Err(RepoErr::UnknownConfigKey(key.to_string()));
                }
            }
        }

        self.write_config(&config)?;
        config_entry(&config, key)
    }

    fn file_config(&self) -> Result<FileConfig> {
        Ok(self.config()?.files)
    }

    pub fn head(&self) -> Result<Head> {
        let raw = fs::read_to_string(self.head_path())?;
        Head::parse(&raw)
    }

    pub fn write_head(&self, head: &Head) -> Result<()> {
        self.write_head_with_message(head, "HEAD update")
    }

    fn write_head_with_message(&self, head: &Head, message: &str) -> Result<()> {
        if let Head::Branch { name } = head {
            validate_ref_name(name)?;
        }
        let old = self.current_head_for_reflog()?;
        let old_target = old
            .as_ref()
            .map(|head| self.head_reflog_target(head))
            .transpose()?
            .flatten();
        let new_target = self.head_reflog_target(head)?;
        write_file_atomic(&self.head_path(), head.serialize().as_bytes())?;
        self.append_head_reflog(old_target.as_deref(), new_target.as_deref(), message)?;
        Ok(())
    }

    pub fn status(&self) -> Result<RepoStatus> {
        let config = self.config()?;
        let head = self.head()?;
        let upstream = head
            .branch_name()
            .map(|branch| self.branch_upstream(branch))
            .transpose()?
            .flatten();
        let head_target = self.head_target()?;
        let index = self.read_index()?;
        let branches = self.branches()?;
        let remotes = self.remotes()?;
        let upstream_status = self.upstream_status(head_target.as_deref(), upstream.as_ref())?;
        let ahead = upstream_status.as_ref().map_or(0, |status| status.ahead);
        let behind = upstream_status.as_ref().map_or(0, |status| status.behind);
        let merge_head = self.merge_head()?;
        let orig_head = self.orig_head()?;
        let staged_changes = self.staged_changes_for_index(&index)?;
        let conflicted_changes = self.conflicted_changes_for_index(&index);
        let unstaged_changes = self.unstaged_changes_for_index(&index)?;
        let unstaged: Vec<String> = unstaged_changes
            .iter()
            .map(|change| change.path.clone())
            .collect();
        let staged = index.staged_paths();
        let conflicted = index.conflicted_paths();
        let counts = RepoStatusCounts::from_status_parts(
            unstaged.len(),
            unstaged_changes.len(),
            staged.len(),
            staged_changes.len(),
            conflicted.len(),
            conflicted_changes.len(),
        );
        let has_unstaged_changes = counts.unstaged > 0;
        let has_staged_changes = counts.staged > 0;
        let has_conflicts = counts.conflicted > 0;
        let work_in_progress =
            has_unstaged_changes || has_staged_changes || has_conflicts || merge_head.is_some();
        let dirty = has_unstaged_changes;
        let paths = RepoStatus::status_paths_from_changes(
            &unstaged_changes,
            &staged_changes,
            &conflicted_changes,
        );

        Ok(RepoStatus {
            worktree: self.worktree.clone(),
            graft_dir: self.graft_dir.clone(),
            repository_format_version: config.core.repository_format_version,
            head,
            head_target,
            merge_head,
            orig_head,
            dirty,
            has_unstaged_changes,
            has_staged_changes,
            has_conflicts,
            work_in_progress,
            counts,
            paths,
            unstaged,
            unstaged_changes,
            staged,
            staged_changes,
            conflicted,
            conflicted_changes,
            branches,
            remotes,
            upstream,
            upstream_status,
            ahead,
            behind,
        })
    }

    pub fn audit_artifacts(&self) -> Result<RepoArtifactAudit> {
        let artifacts = self.index_artifacts()?;
        let mut audit = RepoArtifactAudit {
            artifacts: artifacts.len(),
            external_payloads: artifacts.values().filter(|state| state.is_large()).count(),
            issues: Vec::new(),
        };

        for (path, state) in artifacts {
            self.audit_artifact_state(&path, &state, &mut audit);
        }

        Ok(audit)
    }

    pub fn repair_artifacts_from_remote(&self, remote: &str) -> Result<RepoArtifactRepairOutcome> {
        validate_remote_name(remote)?;
        let before = self.audit_artifacts()?;
        let remote_store = self.remote_store(remote)?;
        let artifacts = self.index_artifacts()?;
        let mut pack_cache = RemoteObjectPackCache::default();
        let mut fetched_objects = BTreeSet::new();
        let mut fetched_external_payloads = BTreeSet::new();

        for state in artifacts.values() {
            self.repair_artifact_state_from_remote(
                &remote_store,
                state,
                &mut pack_cache,
                &mut fetched_objects,
                &mut fetched_external_payloads,
            )?;
        }

        let after = self.audit_artifacts()?;
        Ok(RepoArtifactRepairOutcome {
            remote: remote.to_string(),
            fetched_objects: fetched_objects.len(),
            fetched_external_payloads: fetched_external_payloads.len(),
            before,
            after,
        })
    }

    pub fn fetch_large_file_payloads(
        &self,
        remote: &str,
        rev: Option<&str>,
    ) -> Result<RepoLargeFileFetchOutcome> {
        validate_remote_name(remote)?;
        let target = self.resolve_revision(rev.unwrap_or("HEAD"))?;
        let commit = self.read_commit(&target)?;
        let remote_store = self.remote_store(remote)?;
        let mut files = BTreeMap::<object::ObjectId, RepoLargeFileFetchEntry>::new();

        for (path, state) in &commit.artifacts {
            let CommitArtifactState::LargeFile { content_hash, size, .. } = state else {
                continue;
            };
            let entry =
                files
                    .entry(content_hash.clone())
                    .or_insert_with(|| RepoLargeFileFetchEntry {
                        content_hash: content_hash.clone(),
                        size: *size,
                        store_path: large_file_content_relative_path(content_hash),
                        status: RepoLargeFileFetchStatus::Present,
                        paths: Vec::new(),
                    });
            if entry.size != *size {
                return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                    kind: "large-file-pointer",
                    message: "same content hash referenced with different sizes".to_string(),
                }));
            }
            entry.paths.push(path.clone());
        }

        let mut already_present_payloads = 0;
        let mut fetched_payloads = 0;
        let mut fetched_bytes = 0;
        for entry in files.values_mut() {
            let present = self.large_file_content_path(&entry.content_hash).exists();
            self.fetch_large_file_content(&remote_store, &entry.content_hash, entry.size)?;
            if present {
                already_present_payloads += 1;
                entry.status = RepoLargeFileFetchStatus::Present;
            } else {
                fetched_payloads += 1;
                fetched_bytes += entry.size;
                entry.status = RepoLargeFileFetchStatus::Fetched;
            }
        }

        Ok(RepoLargeFileFetchOutcome {
            remote: remote.to_string(),
            target,
            external_payloads: files.len(),
            already_present_payloads,
            fetched_payloads,
            fetched_bytes,
            files: files.into_values().collect(),
        })
    }

    pub fn large_file_payloads_status(
        &self,
        rev: Option<&str>,
    ) -> Result<RepoLargeFileStatusOutcome> {
        let target = self.resolve_revision(rev.unwrap_or("HEAD"))?;
        let commit = self.read_commit(&target)?;
        let mut files = BTreeMap::<object::ObjectId, RepoLargeFileStatusEntry>::new();

        for (path, state) in &commit.artifacts {
            let CommitArtifactState::LargeFile { content_hash, size, .. } = state else {
                continue;
            };
            let entry =
                files
                    .entry(content_hash.clone())
                    .or_insert_with(|| RepoLargeFileStatusEntry {
                        content_hash: content_hash.clone(),
                        size: *size,
                        store_path: large_file_content_relative_path(content_hash),
                        status: RepoLargeFileStatusState::Missing,
                        message: None,
                        paths: Vec::new(),
                    });
            if entry.size != *size {
                return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                    kind: "large-file-pointer",
                    message: "same content hash referenced with different sizes".to_string(),
                }));
            }
            entry.paths.push(path.clone());
        }

        let mut present_payloads = 0;
        let mut missing_payloads = 0;
        let mut invalid_payloads = 0;
        let mut present_bytes = 0;
        let mut missing_bytes = 0;
        let mut invalid_bytes = 0;
        for entry in files.values_mut() {
            match fs::read(self.large_file_content_path(&entry.content_hash)) {
                Ok(bytes) => {
                    if let Err(err) =
                        validate_large_file_content(&entry.content_hash, entry.size, &bytes)
                    {
                        entry.status = RepoLargeFileStatusState::Invalid;
                        entry.message = Some(err.to_string());
                        invalid_payloads += 1;
                        invalid_bytes += entry.size;
                    } else {
                        entry.status = RepoLargeFileStatusState::Present;
                        present_payloads += 1;
                        present_bytes += entry.size;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    entry.status = RepoLargeFileStatusState::Missing;
                    entry.message =
                        Some(format!("missing external payload {}", entry.content_hash));
                    missing_payloads += 1;
                    missing_bytes += entry.size;
                }
                Err(err) => return Err(err.into()),
            }
        }

        Ok(RepoLargeFileStatusOutcome {
            target,
            external_payloads: files.len(),
            present_payloads,
            missing_payloads,
            invalid_payloads,
            present_bytes,
            missing_bytes,
            invalid_bytes,
            files: files.into_values().collect(),
        })
    }

    pub fn prune_large_file_payloads(&self, dry_run: bool) -> Result<RepoLargeFilePruneOutcome> {
        let referenced = self.referenced_large_file_payloads()?;
        let mut files = Vec::new();
        for payload in self.local_large_file_payloads()? {
            if referenced.contains(&payload.content_hash) {
                continue;
            }
            files.push(payload);
        }

        files.sort_by(|left, right| left.content_hash.cmp(&right.content_hash));
        let candidate_payloads = files.len();
        let candidate_bytes = files.iter().map(|file| file.size).sum();
        let mut pruned_payloads = 0;
        let mut pruned_bytes = 0;
        if !dry_run {
            for file in &files {
                let path = self.graft_dir.join(&file.path);
                fs::remove_file(&path)?;
                remove_empty_parent_dirs(path.parent(), &self.file_store_dir())?;
                pruned_payloads += 1;
                pruned_bytes += file.size;
            }
        }

        Ok(RepoLargeFilePruneOutcome {
            dry_run,
            referenced_payloads: referenced.len(),
            candidate_payloads,
            candidate_bytes,
            pruned_payloads,
            pruned_bytes,
            files,
        })
    }

    pub fn tracked_paths(&self) -> Result<Vec<RepoTrackedPath>> {
        let files = self.index_files()?;
        let artifacts = self.index_artifacts()?;
        let mut paths = Vec::with_capacity(files.len() + artifacts.len());

        for (path, file) in files {
            paths.push(RepoTrackedPath {
                path,
                kind: RepoTrackedPathKind::SqliteDatabase,
                storage: RepoPathStorage::SqliteSnapshot,
                size: None,
                page_count: Some(file.snapshot.page_count),
            });
        }
        for (path, artifact) in artifacts {
            let kind = artifact_tracked_path_kind(&artifact);
            paths.push(RepoTrackedPath {
                path,
                kind,
                storage: artifact_tracked_path_storage(&artifact),
                size: Some(artifact.size()),
                page_count: None,
            });
        }

        paths.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(paths)
    }

    pub fn untracked_paths(&self) -> Result<Vec<RepoTrackedPath>> {
        let index = self.read_index()?;
        self.untracked_paths_for_index(&index)
    }

    pub fn tracked_path_details(&self) -> Result<Vec<RepoTrackedPathDetail>> {
        let files = self.index_files()?;
        let artifacts = self.index_artifacts()?;
        let mut paths = Vec::with_capacity(files.len() + artifacts.len());

        for (path, file) in files {
            paths.push(RepoTrackedPathDetail {
                path,
                kind: RepoTrackedPathKind::SqliteDatabase,
                storage: RepoPathStorage::SqliteSnapshot,
                size: None,
                page_count: Some(file.snapshot.page_count),
                oid: None,
                content_hash: None,
                object_present: None,
                external_payload_present: None,
            });
        }
        for (path, artifact) in artifacts {
            let kind = artifact_tracked_path_kind(&artifact);
            let external_payload_present = match &artifact {
                CommitArtifactState::LargeFile { content_hash, .. } => {
                    Some(self.large_file_content_path(content_hash).exists())
                }
                CommitArtifactState::File { .. } => None,
            };
            paths.push(RepoTrackedPathDetail {
                path,
                kind,
                storage: artifact_tracked_path_storage(&artifact),
                size: Some(artifact.size()),
                page_count: None,
                oid: Some(artifact.oid().clone()),
                content_hash: Some(artifact.content_hash().clone()),
                object_present: Some(self.object_store().path_for(artifact.oid()).exists()),
                external_payload_present,
            });
        }

        paths.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(paths)
    }

    pub fn tracked_path_entries(&self) -> Result<Vec<RepoTrackedPathEntry>> {
        let index = self.read_index()?;
        let mut normal_entries = BTreeMap::<String, RepoTrackedPathEntry>::new();

        for (path, file) in self.head_files()? {
            normal_entries.insert(
                path.clone(),
                tracked_file_entry(path, index::IndexStage::Normal, &file),
            );
        }
        for (path, artifact) in self.head_artifacts()? {
            normal_entries.insert(
                path.clone(),
                tracked_artifact_entry(path, index::IndexStage::Normal, &artifact),
            );
        }

        for path in index.conflicted_paths() {
            normal_entries.remove(&path);
        }

        for entry in index.stage0_entries() {
            if let Some(entry) = tracked_index_entry(entry) {
                normal_entries.insert(entry.path.clone(), entry);
            } else {
                normal_entries.remove(&entry.path);
            }
        }

        let mut entries = normal_entries.into_values().collect::<Vec<_>>();
        entries.extend(
            index
                .entries
                .iter()
                .filter(|entry| entry.stage != index::IndexStage::Normal)
                .filter_map(tracked_index_entry),
        );
        entries.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| u8::from(left.stage).cmp(&u8::from(right.stage)))
        });
        Ok(entries)
    }

    fn upstream_status(
        &self,
        local: Option<&str>,
        upstream: Option<&BranchUpstream>,
    ) -> Result<Option<RepoUpstreamStatus>> {
        let Some(local) = local else {
            return Ok(None);
        };
        let Some(upstream) = upstream else {
            return Ok(None);
        };
        let Some(remote_target) = self.remote_tracking_ref(&upstream.remote, &upstream.branch)?
        else {
            return Ok(None);
        };

        let local_reachable = self.reachable_commits(local)?;
        let remote_reachable = self.reachable_commits(&remote_target)?;
        let ahead = local_reachable.difference(&remote_reachable).count();
        let behind = remote_reachable.difference(&local_reachable).count();
        let state = match (ahead, behind) {
            (0, 0) => RepoUpstreamState::UpToDate,
            (_, 0) => RepoUpstreamState::Ahead,
            (0, _) => RepoUpstreamState::Behind,
            _ => RepoUpstreamState::Diverged,
        };

        Ok(Some(RepoUpstreamStatus {
            remote: upstream.remote.clone(),
            branch: upstream.branch.clone(),
            local: local.to_string(),
            remote_target,
            ahead,
            behind,
            state,
        }))
    }

    fn reachable_commits(&self, start: &str) -> Result<BTreeSet<String>> {
        let mut reachable = BTreeSet::new();
        let mut stack = vec![start.to_string()];
        while let Some(id) = stack.pop() {
            if !reachable.insert(id.clone()) {
                continue;
            }
            for parent in commit_parent_ids(&self.read_commit(&id)?) {
                stack.push(parent);
            }
        }
        Ok(reachable)
    }

    pub fn branches(&self) -> Result<Vec<BranchInfo>> {
        let config = self.config()?;
        let head = self.head()?;
        let current = head.branch_name();
        let mut branches = BTreeMap::<String, Option<String>>::new();

        Self::collect_ref_files(&self.graft_dir.join(DIR_REFS_HEADS), "", &mut branches)?;

        if let Some(current) = current
            && !branches.contains_key(current)
        {
            branches.insert(current.to_string(), None);
        }

        branches
            .into_iter()
            .map(|(name, target)| {
                let upstream = branch_upstream_from_config(&config, &name)?;
                Ok(BranchInfo {
                    current: current == Some(name.as_str()),
                    name,
                    target,
                    upstream,
                })
            })
            .collect::<Result<Vec<_>>>()
    }

    pub fn remote_tracking_branches(&self) -> Result<Vec<RemoteBranchRef>> {
        let mut refs = BTreeMap::<String, Option<String>>::new();
        Self::collect_ref_files(&self.graft_dir.join(DIR_REFS_REMOTES), "", &mut refs)?;

        let mut branches = Vec::new();
        for (name, target) in refs {
            let Some((remote, branch)) = name.split_once('/') else {
                continue;
            };
            validate_remote_name(remote)?;
            validate_ref_name(branch)?;
            let Some(head) = target else {
                continue;
            };
            branches.push(RemoteBranchRef {
                remote: remote.to_string(),
                branch: branch.to_string(),
                head,
            });
        }
        Ok(branches)
    }

    pub fn branch_create(&self, name: &str, start_point: Option<&str>) -> Result<BranchInfo> {
        validate_ref_name(name)?;
        if self.branch_exists(name) {
            return Err(RepoErr::BranchExists(name.to_string()));
        }

        let target = match start_point {
            Some(target) => self.resolve_revision(target)?,
            None => self.head_target()?.ok_or(RepoErr::UnbornHead)?,
        };

        self.write_branch_ref(name, &target, "branch: create")?;
        Ok(BranchInfo {
            name: name.to_string(),
            target: Some(target),
            current: self
                .head()
                .ok()
                .and_then(|head| head.branch_name().map(str::to_string))
                == Some(name.to_string()),
            upstream: self.branch_upstream(name)?,
        })
    }

    pub fn branch_create_unborn(&self, name: &str) -> Result<BranchInfo> {
        validate_ref_name(name)?;
        if self.branch_exists(name) {
            return Err(RepoErr::BranchExists(name.to_string()));
        }
        self.write_ref_update(&format!("refs/heads/{name}"), "", "branch: create unborn")?;
        Ok(BranchInfo {
            name: name.to_string(),
            target: None,
            current: false,
            upstream: self.branch_upstream(name)?,
        })
    }

    pub fn branch_delete(&self, name: &str, force: bool) -> Result<BranchInfo> {
        validate_ref_name(name)?;
        if self.current_branch()?.as_deref() == Some(name) {
            return Err(RepoErr::BranchIsCurrent(name.to_string()));
        }

        if !self.branch_exists(name) {
            return Err(RepoErr::BranchNotFound(name.to_string()));
        }
        let target = self.read_branch_ref(name)?;

        if !force && let Some(target) = &target {
            let merged = if let Some(head) = self.head_target()? {
                self.is_ancestor(target, &head)?
            } else {
                false
            };
            if !merged {
                return Err(RepoErr::BranchNotMerged {
                    branch: name.to_string(),
                    target: target.clone(),
                });
            }
        }

        self.delete_ref(&format!("refs/heads/{name}"))?;
        self.delete_ref_log(&format!("refs/heads/{name}"))?;
        let mut repo_config = self.config()?;
        repo_config.branches.remove(name);
        self.write_config(&repo_config)?;
        Ok(BranchInfo {
            name: name.to_string(),
            target,
            current: false,
            upstream: None,
        })
    }

    pub fn branch_rename(&self, old: &str, new: &str, force: bool) -> Result<BranchInfo> {
        validate_ref_name(old)?;
        validate_ref_name(new)?;

        if old == new {
            return self.branch_info(old);
        }

        let current = self.current_branch()?;
        let old_is_current = current.as_deref() == Some(old);
        let new_is_current = current.as_deref() == Some(new);
        let old_exists = self.branch_exists(old);
        if !old_exists && !old_is_current {
            return Err(RepoErr::BranchNotFound(old.to_string()));
        }
        if new_is_current {
            return Err(RepoErr::BranchIsCurrent(new.to_string()));
        }

        let new_exists = self.branch_exists(new);
        if new_exists && !force {
            return Err(RepoErr::BranchExists(new.to_string()));
        }

        let old_ref = format!("refs/heads/{old}");
        let new_ref = format!("refs/heads/{new}");
        let target = if old_exists {
            self.read_branch_ref(old)?
        } else {
            None
        };
        let target_raw = target.as_deref().unwrap_or("");
        let message = format!("branch: renamed {old} to {new}");

        let mut repo_config = self.config()?;
        let old_branch_config = repo_config.branches.remove(old);
        if force {
            repo_config.branches.remove(new);
        }
        if let Some(old_branch_config) = old_branch_config {
            repo_config
                .branches
                .insert(new.to_string(), old_branch_config);
        }

        Self::ensure_path_namespace_available_for_rename(&self.graft_dir, &old_ref, &new_ref)?;
        let reflog_root = self.graft_dir.join(DIR_LOGS_REFS);
        if reflog_root.join(&old_ref).is_file() {
            Self::ensure_path_namespace_available_for_rename(&reflog_root, &old_ref, &new_ref)?;
        }

        if new_exists {
            self.delete_ref(&new_ref)?;
            self.delete_ref_log(&new_ref)?;
        }
        if old_exists {
            self.delete_ref(&old_ref)?;
        }

        self.ensure_ref_namespace_available(&new_ref)?;
        self.move_ref_log_for_rename(&old_ref, &new_ref)?;
        self.write_ref(&new_ref, target_raw)?;
        self.append_ref_reflog(&new_ref, target.as_deref(), target.as_deref(), &message)?;

        if old_is_current {
            write_file_atomic(&self.head_path(), Head::branch(new).serialize().as_bytes())?;
            self.append_head_reflog(target.as_deref(), target.as_deref(), &message)?;
        }

        self.write_config(&repo_config)?;
        self.branch_info(new)
    }

    pub fn switch_branch(&self, name: &str) -> Result<()> {
        let plan = self.plan_switch_branch(name)?;
        self.apply_switch_branch_plan(name, &plan)
    }

    pub fn plan_switch_branch(&self, name: &str) -> Result<CheckoutPlan> {
        validate_ref_name(name)?;

        let default_branch = self.config()?.core.default_branch;
        let target = self.read_branch_ref(name)?;
        if target.is_none() && name != default_branch && !self.branch_exists(name) {
            return Err(RepoErr::BranchNotFound(name.to_string()));
        }

        self.checkout_plan_for_target(target)
    }

    pub fn apply_switch_branch_plan(&self, name: &str, _plan: &CheckoutPlan) -> Result<()> {
        validate_ref_name(name)?;
        self.write_head_with_message(&Head::branch(name), &format!("checkout: moving to {name}"))
    }

    pub fn switch_new_branch(&self, name: &str, start_point: Option<&str>) -> Result<BranchInfo> {
        let plan = self.plan_switch_new_branch(name, start_point)?;
        self.apply_switch_new_branch_plan(&plan)
    }

    pub fn plan_switch_new_branch(
        &self,
        name: &str,
        start_point: Option<&str>,
    ) -> Result<SwitchNewBranchPlan> {
        validate_ref_name(name)?;
        if self.branch_exists(name) {
            return Err(RepoErr::BranchExists(name.to_string()));
        }
        self.ensure_ref_namespace_available(&format!("refs/heads/{name}"))?;

        let target = match start_point {
            Some(target) => Some(self.resolve_revision(target)?),
            None => self.head_target()?,
        };
        let checkout = self.checkout_plan_for_target(target.clone())?;
        let branch = BranchInfo {
            name: name.to_string(),
            target,
            current: true,
            upstream: self.branch_upstream(name)?,
        };
        Ok(SwitchNewBranchPlan { branch, checkout })
    }

    pub fn apply_switch_new_branch_plan(&self, plan: &SwitchNewBranchPlan) -> Result<BranchInfo> {
        if let Some(target) = &plan.branch.target {
            self.write_branch_ref(&plan.branch.name, target, "branch: create")?;
        } else {
            self.write_ref_update(
                &format!("refs/heads/{}", plan.branch.name),
                "",
                "branch: create unborn",
            )?;
        }
        self.write_head_with_message(
            &Head::branch(plan.branch.name.clone()),
            &format!("checkout: moving to {}", plan.branch.name),
        )?;
        Ok(plan.branch.clone())
    }

    pub fn tags(&self) -> Result<Vec<TagInfo>> {
        let mut tags = BTreeMap::<String, Option<String>>::new();
        Self::collect_ref_files(&self.graft_dir.join(DIR_REFS_TAGS), "", &mut tags)?;
        tags.into_iter()
            .filter_map(|(name, target)| target.map(|target| self.tag_info_from_ref(name, target)))
            .collect()
    }

    pub fn tag_create(&self, name: &str, target: Option<&str>) -> Result<TagInfo> {
        validate_ref_name(name)?;
        if self.tag_exists(name) {
            return Err(RepoErr::TagExists(name.to_string()));
        }

        let target = match target {
            Some(target) => self.resolve_revision(target)?,
            None => self.head_target()?.ok_or(RepoErr::UnbornHead)?,
        };

        self.write_tag_ref(name, &target, "tag: create")?;
        Ok(TagInfo {
            name: name.to_string(),
            object: target.clone(),
            target,
            annotated: false,
            message: None,
        })
    }

    pub fn tag_create_annotated(
        &self,
        name: &str,
        target: Option<&str>,
        message: impl Into<String>,
    ) -> Result<TagInfo> {
        validate_ref_name(name)?;
        if self.tag_exists(name) {
            return Err(RepoErr::TagExists(name.to_string()));
        }

        let target = match target {
            Some(target) => self.resolve_revision(target)?,
            None => self.head_target()?.ok_or(RepoErr::UnbornHead)?,
        };
        let target_id = object::ObjectId::from_str(&target)?;
        let message = message.into();
        let tag_object = object::TagObject {
            object: target_id,
            object_type: object::ObjectKind::Commit,
            name: name.to_string(),
            tagger: object::Signature::new("Graft", "graft@example.invalid", now_ms(), "+0000"),
            message: message.clone(),
        };
        let object = self
            .object_store()
            .write(&object::Object::Tag(tag_object))?;
        let object = object.to_string();

        self.write_tag_ref(name, &object, "tag: create annotated")?;
        Ok(TagInfo {
            name: name.to_string(),
            object,
            target,
            annotated: true,
            message: Some(message),
        })
    }

    pub fn tag_delete(&self, name: &str) -> Result<TagInfo> {
        validate_ref_name(name)?;
        let object = self
            .read_tag_ref(name)?
            .ok_or_else(|| RepoErr::TagNotFound(name.to_string()))?;
        let tag = self.tag_info_from_ref(name.to_string(), object)?;
        self.delete_tag_ref(name)?;
        self.delete_ref_log(&format!("refs/tags/{name}"))?;
        Ok(tag)
    }

    pub fn remote_add(&self, name: &str, config: RemoteConfig) -> Result<RemoteInfo> {
        validate_remote_name(name)?;
        let mut repo_config = self.config()?;
        if repo_config.remotes.contains_key(name) {
            return Err(RepoErr::RemoteExists(name.to_string()));
        }
        repo_config.remotes.insert(name.to_string(), config.clone());
        self.write_config(&repo_config)?;
        fs::create_dir_all(self.graft_dir.join(DIR_REFS_REMOTES).join(name))?;
        Ok(RemoteInfo { name: name.to_string(), config })
    }

    pub fn remote_remove(&self, name: &str) -> Result<RemoteInfo> {
        validate_remote_name(name)?;
        let mut repo_config = self.config()?;
        let Some(config) = repo_config.remotes.remove(name) else {
            return Err(RepoErr::RemoteNotFound(name.to_string()));
        };
        repo_config
            .branches
            .retain(|_, branch| branch.remote.as_deref() != Some(name));
        self.write_config(&repo_config)?;

        remove_path_if_exists(self.graft_dir.join(DIR_REFS_REMOTES).join(name))?;
        remove_path_if_exists(
            self.graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs")
                .join("remotes")
                .join(name),
        )?;
        Ok(RemoteInfo { name: name.to_string(), config })
    }

    pub fn remote_rename(&self, old: &str, new: &str) -> Result<RemoteInfo> {
        validate_remote_name(old)?;
        validate_remote_name(new)?;
        if old == new {
            let config = self
                .config()?
                .remotes
                .remove(old)
                .ok_or_else(|| RepoErr::RemoteNotFound(old.to_string()))?;
            return Ok(RemoteInfo { name: new.to_string(), config });
        }

        let mut repo_config = self.config()?;
        let Some(config) = repo_config.remotes.remove(old) else {
            return Err(RepoErr::RemoteNotFound(old.to_string()));
        };
        if repo_config.remotes.contains_key(new) {
            return Err(RepoErr::RemoteExists(new.to_string()));
        }
        if self.graft_dir.join(DIR_REFS_REMOTES).join(new).exists()
            || self
                .graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs")
                .join("remotes")
                .join(new)
                .exists()
        {
            return Err(RepoErr::RemoteExists(new.to_string()));
        }

        for branch in repo_config.branches.values_mut() {
            if branch.remote.as_deref() == Some(old) {
                branch.remote = Some(new.to_string());
            }
        }
        repo_config.remotes.insert(new.to_string(), config.clone());
        self.write_config(&repo_config)?;

        move_path_if_exists(
            self.graft_dir.join(DIR_REFS_REMOTES).join(old),
            self.graft_dir.join(DIR_REFS_REMOTES).join(new),
        )?;
        move_path_if_exists(
            self.graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs")
                .join("remotes")
                .join(old),
            self.graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs")
                .join("remotes")
                .join(new),
        )?;

        Ok(RemoteInfo { name: new.to_string(), config })
    }

    pub fn remote_get_url(&self, name: &str) -> Result<RemoteInfo> {
        validate_remote_name(name)?;
        let config = self
            .config()?
            .remotes
            .get(name)
            .cloned()
            .ok_or_else(|| RepoErr::RemoteNotFound(name.to_string()))?;
        Ok(RemoteInfo { name: name.to_string(), config })
    }

    pub fn remote_set_url(&self, name: &str, config: RemoteConfig) -> Result<RemoteInfo> {
        validate_remote_name(name)?;
        let mut repo_config = self.config()?;
        let Some(remote_config) = repo_config.remotes.get_mut(name) else {
            return Err(RepoErr::RemoteNotFound(name.to_string()));
        };
        *remote_config = config.clone();
        self.write_config(&repo_config)?;
        Ok(RemoteInfo { name: name.to_string(), config })
    }

    pub fn remotes(&self) -> Result<Vec<RemoteInfo>> {
        Ok(self
            .config()?
            .remotes
            .into_iter()
            .map(|(name, config)| RemoteInfo { name, config })
            .collect())
    }

    pub fn set_remote_tracking_ref(&self, remote: &str, branch: &str, target: &str) -> Result<()> {
        validate_remote_name(remote)?;
        validate_ref_name(branch)?;
        self.write_ref_update(
            &format!("refs/remotes/{remote}/{branch}"),
            target,
            &format!("fetch {remote}/{branch}"),
        )
    }

    pub fn remote_tracking_ref(&self, remote: &str, branch: &str) -> Result<Option<String>> {
        validate_remote_name(remote)?;
        validate_ref_name(branch)?;
        self.read_ref(&format!("refs/remotes/{remote}/{branch}"))
    }

    pub fn remote_default_branch(&self, remote: &str) -> Result<Option<String>> {
        validate_remote_name(remote)?;
        let remote_store = self.remote_store(remote)?;
        let Some(head) = block_on_remote(remote_store.get_raw(HEAD_FILE))? else {
            return Ok(None);
        };
        parse_remote_head_branch(HEAD_FILE, head)
    }

    pub fn remote_branch_refs(&self, remote: &str) -> Result<Vec<RemoteBranchRef>> {
        validate_remote_name(remote)?;
        let remote_store = self.remote_store(remote)?;
        self.remote_branch_refs_from_store(remote, &remote_store)
    }

    pub fn remote_branch_head(&self, remote: &str, branch: &str) -> Result<Option<String>> {
        Ok(self.remote_branch_head_state(remote, branch)?.head)
    }

    pub fn remote_branch_head_state(&self, remote: &str, branch: &str) -> Result<RemoteBranchHead> {
        validate_remote_name(remote)?;
        validate_ref_name(branch)?;
        let remote_store = self.remote_store(remote)?;
        Self::remote_branch_head_from_store(&remote_store, branch)
    }

    fn remote_branch_head_from_store(
        remote_store: &crate::remote::Remote,
        branch: &str,
    ) -> Result<RemoteBranchHead> {
        let head_path = format!("refs/heads/{branch}");
        let raw = block_on_remote(remote_store.get_raw(&head_path))?;
        let head = raw
            .as_ref()
            .map(|bytes| parse_remote_ref(&head_path, bytes.clone()))
            .transpose()?;
        Ok(RemoteBranchHead { raw, head })
    }

    pub fn remote_prune(&self, remote: &str) -> Result<RemotePruneOutcome> {
        validate_remote_name(remote)?;
        let remote_store = self.remote_store(remote)?;
        let remote_branches = self
            .remote_branch_refs_from_store(remote, &remote_store)?
            .into_iter()
            .map(|reference| reference.branch)
            .collect::<BTreeSet<_>>();
        let mut local_tracking = BTreeMap::<String, Option<String>>::new();
        Self::collect_ref_files(
            &self.graft_dir.join(DIR_REFS_REMOTES).join(remote),
            "",
            &mut local_tracking,
        )?;

        let mut branches = Vec::new();
        for branch in local_tracking.keys() {
            validate_ref_name(branch)?;
            if remote_branches.contains(branch) {
                continue;
            }
            let reference = format!("refs/remotes/{remote}/{branch}");
            self.delete_ref_if_exists(&reference)?;
            self.delete_ref_log(&reference)?;
            branches.push(branch.clone());
        }

        Ok(RemotePruneOutcome { remote: remote.to_string(), branches })
    }

    pub fn current_branch(&self) -> Result<Option<String>> {
        Ok(self.head()?.branch_name().map(ToString::to_string))
    }

    pub fn default_branch(&self) -> Result<String> {
        Ok(self.config()?.core.default_branch)
    }

    pub fn branch_target(&self, branch: &str) -> Result<Option<String>> {
        validate_ref_name(branch)?;
        self.read_branch_ref(branch)
    }

    pub fn branch_upstream(&self, branch: &str) -> Result<Option<BranchUpstream>> {
        validate_ref_name(branch)?;
        branch_upstream_from_config(&self.config()?, branch)
    }

    pub fn set_branch_upstream(
        &self,
        branch: &str,
        remote: &str,
        remote_branch: &str,
    ) -> Result<BranchInfo> {
        self.ensure_local_branch_for_config(branch)?;
        validate_remote_name(remote)?;
        validate_ref_name(remote_branch)?;

        let mut repo_config = self.config()?;
        if !repo_config.remotes.contains_key(remote) {
            return Err(RepoErr::RemoteNotFound(remote.to_string()));
        }

        repo_config.branches.insert(
            branch.to_string(),
            BranchConfig {
                remote: Some(remote.to_string()),
                merge: Some(branch_merge_ref(remote_branch)),
            },
        );
        self.write_config(&repo_config)?;
        self.branch_info(branch)
    }

    pub fn unset_branch_upstream(&self, branch: &str) -> Result<BranchInfo> {
        self.ensure_local_branch_for_config(branch)?;
        let mut repo_config = self.config()?;
        repo_config.branches.remove(branch);
        self.write_config(&repo_config)?;
        self.branch_info(branch)
    }

    pub fn default_remote_branch(
        &self,
        remote: Option<&str>,
        branch: Option<&str>,
    ) -> Result<BranchUpstream> {
        if let Some(remote) = remote {
            validate_remote_name(remote)?;
        }
        if let Some(branch) = branch {
            validate_ref_name(branch)?;
        }

        let current_branch = self.current_branch()?;
        let current_upstream = current_branch
            .as_deref()
            .map(|branch| self.branch_upstream(branch))
            .transpose()?
            .flatten();

        let resolved_remote = remote
            .map(ToString::to_string)
            .or_else(|| {
                current_upstream
                    .as_ref()
                    .map(|upstream| upstream.remote.clone())
            })
            .unwrap_or_else(|| "origin".to_string());
        let resolved_branch = branch
            .map(ToString::to_string)
            .or_else(|| {
                if remote.is_none() {
                    current_upstream
                        .as_ref()
                        .map(|upstream| upstream.branch.clone())
                } else {
                    None
                }
            })
            .or(current_branch)
            .unwrap_or_else(|| self.default_branch().unwrap_or_else(|_| "main".to_string()));

        Ok(BranchUpstream {
            remote: resolved_remote,
            branch: resolved_branch,
        })
    }

    pub fn fetch(&self, remote: &str, branch: &str) -> Result<FetchOutcome> {
        validate_remote_name(remote)?;
        validate_ref_name(branch)?;
        let remote_store = self.remote_store(remote)?;
        let head_path = format!("refs/heads/{branch}");
        let Some(head) = block_on_remote(remote_store.get_raw(&head_path))? else {
            return Err(RepoErr::RemoteBranchNotFound {
                remote: remote.to_string(),
                branch: branch.to_string(),
            });
        };
        let head = parse_remote_ref(&head_path, head)?;
        let commits = self.fetch_commit_chain(&remote_store, &head)?;
        self.set_remote_tracking_ref(remote, branch, &head)?;
        Ok(FetchOutcome {
            remote: remote.to_string(),
            branch: branch.to_string(),
            head,
            commits,
        })
    }

    pub fn fetch_all(&self, remote: &str) -> Result<FetchAllOutcome> {
        validate_remote_name(remote)?;
        let remote_store = self.remote_store(remote)?;
        let remote_refs = self.remote_branch_refs_from_store(remote, &remote_store)?;
        let mut branches = Vec::with_capacity(remote_refs.len());

        for remote_ref in remote_refs {
            let commits = self.fetch_commit_chain(&remote_store, &remote_ref.head)?;
            self.set_remote_tracking_ref(remote, &remote_ref.branch, &remote_ref.head)?;
            branches.push(FetchOutcome {
                remote: remote.to_string(),
                branch: remote_ref.branch,
                head: remote_ref.head,
                commits,
            });
        }

        Ok(FetchAllOutcome { remote: remote.to_string(), branches })
    }

    pub fn fetch_refspec(&self, remote: &str, refspec: &str) -> Result<FetchAllOutcome> {
        validate_remote_name(remote)?;
        let parsed = parse_fetch_refspec(remote, refspec)?;
        let remote_store = self.remote_store(remote)?;
        let branches = self.fetch_refspec_with_store(remote, &remote_store, refspec, &parsed)?;
        Ok(FetchAllOutcome { remote: remote.to_string(), branches })
    }

    fn fetch_refspec_with_store(
        &self,
        remote: &str,
        remote_store: &crate::remote::Remote,
        refspec: &str,
        parsed: &ParsedRefspec,
    ) -> Result<Vec<FetchOutcome>> {
        let source = parsed
            .source
            .as_ref()
            .expect("fetch refspec parser rejects delete refspecs");
        let destination = parsed.destination.as_ref().unwrap_or(source);
        let mut outcomes = Vec::new();

        if let Some(source_branch) = source.exact() {
            let head_path = format!("refs/heads/{source_branch}");
            let Some(head) = block_on_remote(remote_store.get_raw(&head_path))? else {
                return Err(RepoErr::RemoteBranchNotFound {
                    remote: remote.to_string(),
                    branch: source_branch.to_string(),
                });
            };
            let head = parse_remote_ref(&head_path, head)?;
            let destination_branch = destination.expand("")?;
            let commits = self.fetch_commit_chain(remote_store, &head)?;
            self.set_remote_tracking_ref(remote, &destination_branch, &head)?;
            outcomes.push(FetchOutcome {
                remote: remote.to_string(),
                branch: destination_branch,
                head,
                commits,
            });
            return Ok(outcomes);
        }

        let remote_refs = self.remote_branch_refs_from_store(remote, remote_store)?;
        for remote_ref in remote_refs {
            let Some(capture) = source.capture(&remote_ref.branch)? else {
                continue;
            };
            let destination_branch = destination.expand(capture)?;
            let commits = self.fetch_commit_chain(remote_store, &remote_ref.head)?;
            self.set_remote_tracking_ref(remote, &destination_branch, &remote_ref.head)?;
            outcomes.push(FetchOutcome {
                remote: remote.to_string(),
                branch: destination_branch,
                head: remote_ref.head,
                commits,
            });
        }

        if outcomes.is_empty() {
            return Err(RepoErr::InvalidRefspec {
                refspec: refspec.to_string(),
                message: "wildcard matched no remote branches".to_string(),
            });
        }
        Ok(outcomes)
    }

    pub fn push(&self, remote: &str, branch: &str) -> Result<PushOutcome> {
        self.push_branch(remote, branch, branch)
    }

    pub fn push_all(&self, remote: &str) -> Result<PushAllOutcome> {
        self.push_all_with_force(remote, false)
    }

    pub fn push_all_with_force(&self, remote: &str, force: bool) -> Result<PushAllOutcome> {
        validate_remote_name(remote)?;
        let mut branches = Vec::new();

        for branch in self.branches()? {
            if branch.target.is_none() {
                continue;
            }
            branches.push(self.push_branch_with_force(
                remote,
                &branch.name,
                &branch.name,
                force,
            )?);
        }

        Ok(PushAllOutcome { remote: remote.to_string(), branches })
    }

    pub fn push_refspec_with_force(
        &self,
        remote: &str,
        refspec: &str,
        force: bool,
    ) -> Result<PushAllOutcome> {
        validate_remote_name(remote)?;
        let parsed = parse_push_refspec(refspec)?;
        let force = force || parsed.force;
        if parsed.source.is_none() {
            let Some(destination) = &parsed.destination else {
                return Err(RepoErr::InvalidRefspec {
                    refspec: refspec.to_string(),
                    message: "delete refspecs require a destination".to_string(),
                });
            };
            if destination.is_wildcard() {
                return Err(RepoErr::InvalidRefspec {
                    refspec: refspec.to_string(),
                    message: "wildcard delete refspecs are not supported".to_string(),
                });
            }
            let remote_branch = destination.expand("")?;
            let outcome = self.push_delete_branch_with_force(remote, &remote_branch, force)?;
            return Ok(PushAllOutcome {
                remote: remote.to_string(),
                branches: vec![outcome],
            });
        }

        let source = parsed.source.as_ref().expect("handled delete refspec");
        let destination = parsed.destination.as_ref().unwrap_or(source);
        let mut branches = Vec::new();

        if let Some(local_branch) = source.exact() {
            let remote_branch = destination.expand("")?;
            branches.push(self.push_branch_with_force(
                remote,
                local_branch,
                &remote_branch,
                force,
            )?);
            return Ok(PushAllOutcome { remote: remote.to_string(), branches });
        }

        for branch in self.branches()? {
            if branch.target.is_none() {
                continue;
            }
            let Some(capture) = source.capture(&branch.name)? else {
                continue;
            };
            let remote_branch = destination.expand(capture)?;
            branches.push(self.push_branch_with_force(
                remote,
                &branch.name,
                &remote_branch,
                force,
            )?);
        }

        if branches.is_empty() {
            return Err(RepoErr::InvalidRefspec {
                refspec: refspec.to_string(),
                message: "wildcard matched no local branches".to_string(),
            });
        }
        Ok(PushAllOutcome { remote: remote.to_string(), branches })
    }

    pub fn push_refspec_branches(&self, refspec: &str) -> Result<Vec<PushRefspecBranch>> {
        let parsed = parse_push_refspec(refspec)?;
        let Some(source) = parsed.source.as_ref() else {
            return Ok(Vec::new());
        };
        let destination = parsed.destination.as_ref().unwrap_or(source);

        if let Some(local_branch) = source.exact() {
            let remote_branch = destination.expand("")?;
            return Ok(vec![PushRefspecBranch {
                local_branch: local_branch.to_string(),
                remote_branch,
            }]);
        }

        let mut branches = Vec::new();
        for branch in self.branches()? {
            if branch.target.is_none() {
                continue;
            }
            let Some(capture) = source.capture(&branch.name)? else {
                continue;
            };
            let remote_branch = destination.expand(capture)?;
            branches.push(PushRefspecBranch { local_branch: branch.name, remote_branch });
        }

        if branches.is_empty() {
            return Err(RepoErr::InvalidRefspec {
                refspec: refspec.to_string(),
                message: "wildcard matched no local branches".to_string(),
            });
        }
        Ok(branches)
    }

    pub fn push_branch(
        &self,
        remote: &str,
        local_branch: &str,
        remote_branch: &str,
    ) -> Result<PushOutcome> {
        self.push_branch_with_force(remote, local_branch, remote_branch, false)
    }

    pub fn push_branch_with_force(
        &self,
        remote: &str,
        local_branch: &str,
        remote_branch: &str,
        force: bool,
    ) -> Result<PushOutcome> {
        let remote_head = self.remote_branch_head_state(remote, remote_branch)?;
        self.push_branch_with_force_and_remote_head(
            remote,
            local_branch,
            remote_branch,
            force,
            remote_head,
        )
    }

    pub fn push_branch_with_force_and_remote_head(
        &self,
        remote: &str,
        local_branch: &str,
        remote_branch: &str,
        force: bool,
        remote_head: RemoteBranchHead,
    ) -> Result<PushOutcome> {
        validate_remote_name(remote)?;
        validate_ref_name(local_branch)?;
        validate_ref_name(remote_branch)?;
        let Some(head) = self.branch_target(local_branch)? else {
            return Err(RepoErr::UnbornHead);
        };

        let remote_store = self.remote_store(remote)?;
        let RemoteBranchHead { raw: remote_head_raw, head: remote_head } = remote_head;
        let remote_branch_existed = remote_head_raw.is_some();

        if remote_head.as_deref() == Some(head.as_str()) {
            self.set_remote_tracking_ref(remote, remote_branch, &head)?;
            return Ok(PushOutcome {
                remote: remote.to_string(),
                local_branch: local_branch.to_string(),
                remote_branch: remote_branch.to_string(),
                head,
                commits: 0,
                forced: force,
                deleted: false,
            });
        }

        if let Some(remote_head) = &remote_head
            && !force
            && !self.is_ancestor(remote_head, &head)?
        {
            return Err(RepoErr::NonFastForward {
                remote: remote.to_string(),
                local_branch: local_branch.to_string(),
                remote_branch: remote_branch.to_string(),
            });
        }

        let commits = self.push_commit_chain(&remote_store, &head, remote_head.as_deref())?;
        let head_path = format!("refs/heads/{remote_branch}");
        match block_on_remote(remote_store.compare_and_swap_raw(
            &head_path,
            remote_head_raw.as_deref(),
            format!("{head}\n"),
        )) {
            Ok(()) => {}
            Err(RepoErr::Remote(RemoteErr::CompareAndSwap { .. } | RemoteErr::LockBusy { .. })) => {
                return Err(RepoErr::RemoteRefChanged {
                    remote: remote.to_string(),
                    branch: remote_branch.to_string(),
                });
            }
            Err(err) => return Err(err),
        }
        if !remote_branch_existed {
            self.set_remote_head_if_absent(&remote_store, remote_branch)?;
        }
        self.set_remote_tracking_ref(remote, remote_branch, &head)?;

        Ok(PushOutcome {
            remote: remote.to_string(),
            local_branch: local_branch.to_string(),
            remote_branch: remote_branch.to_string(),
            head,
            commits,
            forced: force,
            deleted: false,
        })
    }

    pub fn push_delete_branch_with_force(
        &self,
        remote: &str,
        remote_branch: &str,
        force: bool,
    ) -> Result<PushOutcome> {
        validate_remote_name(remote)?;
        validate_ref_name(remote_branch)?;

        let remote_store = self.remote_store(remote)?;
        let head_path = format!("refs/heads/{remote_branch}");
        let remote_head_raw =
            block_on_remote(remote_store.get_raw(&head_path))?.ok_or_else(|| {
                RepoErr::RemoteBranchNotFound {
                    remote: remote.to_string(),
                    branch: remote_branch.to_string(),
                }
            })?;
        let remote_head = parse_remote_ref(&head_path, remote_head_raw.clone())?;

        if !force
            && let Some(local_tracking) = self.remote_tracking_ref(remote, remote_branch)?
            && local_tracking != remote_head
        {
            return Err(RepoErr::RemoteRefChanged {
                remote: remote.to_string(),
                branch: remote_branch.to_string(),
            });
        }

        match block_on_remote(
            remote_store.compare_and_delete_raw(&head_path, Some(remote_head_raw.as_ref())),
        ) {
            Ok(()) => {}
            Err(RepoErr::Remote(RemoteErr::CompareAndSwap { .. } | RemoteErr::LockBusy { .. })) => {
                return Err(RepoErr::RemoteRefChanged {
                    remote: remote.to_string(),
                    branch: remote_branch.to_string(),
                });
            }
            Err(err) => return Err(err),
        }

        self.delete_ref_if_exists(&format!("refs/remotes/{remote}/{remote_branch}"))?;
        self.delete_ref_log(&format!("refs/remotes/{remote}/{remote_branch}"))?;

        Ok(PushOutcome {
            remote: remote.to_string(),
            local_branch: String::new(),
            remote_branch: remote_branch.to_string(),
            head: remote_head,
            commits: 0,
            forced: force,
            deleted: true,
        })
    }

    pub fn pull(
        &self,
        remote: &str,
        remote_branch: &str,
        local_branch: &str,
    ) -> Result<PullOutcome> {
        let plan = self.plan_pull(remote, remote_branch, local_branch)?;
        self.apply_pull_plan(&plan)
    }

    pub fn plan_pull(
        &self,
        remote: &str,
        remote_branch: &str,
        local_branch: &str,
    ) -> Result<PullPlan> {
        validate_remote_name(remote)?;
        validate_ref_name(remote_branch)?;
        validate_ref_name(local_branch)?;
        if self.current_branch()?.as_deref() != Some(local_branch) {
            return Err(RepoErr::NotCurrentBranch(local_branch.to_string()));
        }
        if self.merge_head()?.is_some() {
            return Err(RepoErr::MergeInProgress);
        }

        let fetch = self.fetch(remote, remote_branch)?;
        let merge = self.plan_merge_revision(&format!("refs/remotes/{remote}/{remote_branch}"))?;
        Ok(PullPlan {
            remote: remote.to_string(),
            remote_branch: remote_branch.to_string(),
            local_branch: local_branch.to_string(),
            fetch,
            merge,
        })
    }

    pub fn plan_pull_refspec(
        &self,
        remote: &str,
        refspec: &str,
        local_branch: &str,
    ) -> Result<PullPlan> {
        validate_remote_name(remote)?;
        validate_ref_name(local_branch)?;
        if self.current_branch()?.as_deref() != Some(local_branch) {
            return Err(RepoErr::NotCurrentBranch(local_branch.to_string()));
        }
        if self.merge_head()?.is_some() {
            return Err(RepoErr::MergeInProgress);
        }

        let mut fetch = self.fetch_refspec(remote, refspec)?.branches;
        if fetch.len() != 1 {
            return Err(RepoErr::InvalidRefspec {
                refspec: refspec.to_string(),
                message: "pull refspec must update exactly one remote-tracking branch".to_string(),
            });
        }
        let fetch = fetch.pop().expect("length checked");
        let merge = self.plan_merge_revision(&format!("refs/remotes/{remote}/{}", fetch.branch))?;
        Ok(PullPlan {
            remote: remote.to_string(),
            remote_branch: fetch.branch.clone(),
            local_branch: local_branch.to_string(),
            fetch,
            merge,
        })
    }

    pub fn apply_pull_plan(&self, plan: &PullPlan) -> Result<PullOutcome> {
        let merge = self.apply_merge_plan(&plan.merge)?;
        Ok(PullOutcome {
            remote: plan.remote.clone(),
            remote_branch: plan.remote_branch.clone(),
            local_branch: plan.local_branch.clone(),
            head: plan.fetch.head.clone(),
            commits: plan.fetch.commits,
            merge,
        })
    }

    pub fn merge_revision(&self, rev: &str) -> Result<MergeOutcome> {
        let plan = self.plan_merge_revision(rev)?;
        self.apply_merge_plan(&plan)
    }

    pub fn plan_merge_revision(&self, rev: &str) -> Result<MergePlan> {
        if self.merge_head()?.is_some() {
            return Err(RepoErr::MergeInProgress);
        }
        let target = self.resolve_revision(rev)?;
        let checkout = self.checkout_plan_for_target(Some(target.clone()))?;
        let head = self.head_target()?;

        let Some(head) = head else {
            let outcome = MergeOutcome::FastForward { from: None, to: target.clone() };
            return Ok(MergePlan {
                rev: rev.to_string(),
                target,
                checkout,
                outcome,
                index: None,
            });
        };

        if self.is_ancestor(&target, &head)? {
            let outcome = MergeOutcome::AlreadyUpToDate { head };
            return Ok(MergePlan {
                rev: rev.to_string(),
                target,
                checkout,
                outcome,
                index: None,
            });
        }

        if self.is_ancestor(&head, &target)? {
            let outcome = MergeOutcome::FastForward { from: Some(head), to: target.clone() };
            return Ok(MergePlan {
                rev: rev.to_string(),
                target,
                checkout,
                outcome,
                index: None,
            });
        }

        let merge_base = self.merge_base(&head, &target)?;
        let base_files = self.files_for_commit(merge_base.as_deref())?;
        let ours_files = self.files_for_commit(Some(&head))?;
        let theirs_files = self.files_for_commit(Some(&target))?;
        let base_artifacts = self.artifacts_for_commit(merge_base.as_deref())?;
        let ours_artifacts = self.artifacts_for_commit(Some(&head))?;
        let theirs_artifacts = self.artifacts_for_commit(Some(&target))?;
        let mut index = self.read_index()?;
        let mut staged = Vec::new();
        let mut conflicted = Vec::new();

        let mut keys = BTreeMap::<String, ()>::new();
        for key in base_files
            .keys()
            .chain(ours_files.keys())
            .chain(theirs_files.keys())
        {
            keys.insert(key.clone(), ());
        }

        for key in keys.keys() {
            let base = base_files.get(key);
            let ours = ours_files.get(key);
            let theirs = theirs_files.get(key);

            if ours == theirs || base == theirs {
                continue;
            }

            if base == ours {
                index.remove_path(key);
                if let Some(theirs) = theirs {
                    index.stage(self.index_entry_for_state(
                        key.clone(),
                        index::IndexStage::Normal,
                        theirs.clone(),
                    )?);
                } else {
                    index.stage(index::IndexEntry {
                        path: key.clone(),
                        mode: None,
                        oid: None,
                        stage: index::IndexStage::Normal,
                        file: None,
                        artifact: None,
                    });
                }
                staged.push(key.clone());
                continue;
            }

            self.stage_merge_conflict(key, base, ours, theirs, &mut index)?;
            conflicted.push(key.clone());
        }

        let mut artifact_keys = BTreeMap::<String, ()>::new();
        for key in base_artifacts
            .keys()
            .chain(ours_artifacts.keys())
            .chain(theirs_artifacts.keys())
        {
            artifact_keys.insert(key.clone(), ());
        }

        for key in artifact_keys.keys() {
            let base = base_artifacts.get(key);
            let ours = ours_artifacts.get(key);
            let theirs = theirs_artifacts.get(key);

            if ours == theirs || base == theirs {
                continue;
            }

            if base == ours {
                index.remove_path(key);
                if let Some(theirs) = theirs {
                    index.stage(self.index_entry_for_artifact_state(
                        key.clone(),
                        index::IndexStage::Normal,
                        theirs.clone(),
                    ));
                } else {
                    index.stage(index::IndexEntry {
                        path: key.clone(),
                        mode: None,
                        oid: None,
                        stage: index::IndexStage::Normal,
                        file: None,
                        artifact: None,
                    });
                }
                staged.push(key.clone());
                continue;
            }

            self.stage_merge_artifact_conflict(key, base, ours, theirs, &mut index);
            conflicted.push(key.clone());
        }

        let outcome = MergeOutcome::Merged {
            head,
            target: target.clone(),
            merge_base,
            staged,
            conflicted,
        };
        Ok(MergePlan {
            rev: rev.to_string(),
            target,
            checkout,
            outcome,
            index: Some(index),
        })
    }

    pub fn apply_merge_plan(&self, plan: &MergePlan) -> Result<MergeOutcome> {
        if self.merge_head()?.is_some() {
            return Err(RepoErr::MergeInProgress);
        }

        match &plan.outcome {
            MergeOutcome::FastForward { to, .. } => {
                self.move_head_to(to, &format!("merge {}: fast-forward", plan.rev))?;
            }
            MergeOutcome::AlreadyUpToDate { .. } => {}
            MergeOutcome::Merged { head, target, .. } => {
                let index = plan.index.as_ref().ok_or(RepoErr::UnresolvedConflicts)?;
                self.write_index(index)?;
                self.write_merge_state(head, target)?;
            }
        }

        Ok(plan.outcome.clone())
    }

    pub fn merge_abort(&self) -> Result<String> {
        let plan = self.plan_merge_abort()?;
        self.apply_merge_abort_plan(&plan)
    }

    pub fn plan_merge_abort(&self) -> Result<MergeAbortPlan> {
        let target = self.orig_head()?.ok_or(RepoErr::NoMergeInProgress)?;
        let checkout = self.checkout_plan_for_target(Some(target.clone()))?;
        Ok(MergeAbortPlan { target, checkout })
    }

    pub fn apply_merge_abort_plan(&self, plan: &MergeAbortPlan) -> Result<String> {
        if self.orig_head()?.is_none() && self.merge_head()?.is_none() {
            return Err(RepoErr::NoMergeInProgress);
        }
        self.move_head_to(&plan.target, "merge: abort")?;
        self.clear_index()?;
        self.clear_dirty()?;
        self.clear_merge_state()?;
        Ok(plan.target.clone())
    }

    pub fn commit(&self, message: impl Into<String>) -> Result<CommitObject> {
        let commit = self.commit_with_files(message, BTreeMap::new(), Vec::new())?;
        self.clear_dirty()?;
        Ok(commit)
    }

    #[cfg(test)]
    fn stage_file(
        &self,
        path: impl AsRef<Path>,
        volume: VolumeId,
        snapshot: &Snapshot,
    ) -> Result<index::IndexEntry> {
        let key = self.file_key(path)?;
        let file = CommitFileState {
            volume,
            snapshot: repo_snapshot_with_test_hashes(snapshot),
        };
        self.stage_file_state(key, file)
    }

    fn stage_file_state(&self, key: String, file: CommitFileState) -> Result<index::IndexEntry> {
        let entry = self.index_entry_for_state(key.clone(), index::IndexStage::Normal, file)?;
        let mut index = self.read_index()?;
        index.stage(entry.clone());
        self.write_index(&index)?;
        self.clear_dirty_key(&key)?;
        Ok(entry)
    }

    pub fn stage_file_state_path(
        &self,
        path: impl AsRef<Path>,
        file: CommitFileState,
    ) -> Result<index::IndexEntry> {
        validate_commit_file_state(&file)?;
        let key = self.file_key(path)?;
        self.stage_file_state(key, file)
    }

    pub fn stage_artifact_path(&self, path: impl AsRef<Path>) -> Result<index::IndexEntry> {
        let key = self.file_key(path)?;
        let physical_path = self.worktree.join(&key);
        let artifact = self.write_artifact_state_from_path(&key, &physical_path)?;
        self.stage_artifact_state(key, artifact)
    }

    #[cfg(test)]
    fn stage_artifact_path_with_inline_text_threshold(
        &self,
        path: impl AsRef<Path>,
        inline_text_threshold: u64,
    ) -> Result<index::IndexEntry> {
        let key = self.file_key(path)?;
        let physical_path = self.worktree.join(&key);
        let config = FileConfig {
            inline_text_threshold: ByteUnit::new(inline_text_threshold),
            external_paths: Vec::new(),
        };
        let artifact =
            self.write_artifact_state_from_path_with_file_config(&key, &physical_path, &config)?;
        self.stage_artifact_state(key, artifact)
    }

    fn stage_artifact_state(
        &self,
        key: String,
        artifact: CommitArtifactState,
    ) -> Result<index::IndexEntry> {
        let entry =
            self.index_entry_for_artifact_state(key.clone(), index::IndexStage::Normal, artifact);
        let mut index = self.read_index()?;
        index.stage(entry.clone());
        self.write_index(&index)?;
        self.clear_dirty_key(&key)?;
        Ok(entry)
    }

    pub fn stage_file_removal(&self, path: impl AsRef<Path>) -> Result<index::IndexEntry> {
        let key = self.file_key(path)?;
        self.stage_file_removal_key(key)
    }

    pub fn stage_file_removal_key(&self, key: impl Into<String>) -> Result<index::IndexEntry> {
        let key = normalize_repo_path(&key.into());
        if !self.head_files()?.contains_key(&key) && !self.head_artifacts()?.contains_key(&key) {
            return Err(RepoErr::PathNotTracked(key));
        }
        let entry = index::IndexEntry {
            path: key,
            mode: None,
            oid: None,
            stage: index::IndexStage::Normal,
            file: None,
            artifact: None,
        };
        let mut index = self.read_index()?;
        index.stage(entry.clone());
        self.write_index(&index)?;
        self.clear_dirty_key(&entry.path)?;
        Ok(entry)
    }

    pub fn resolve_file_conflict(
        &self,
        path: impl AsRef<Path>,
        file: Option<CommitFileState>,
    ) -> Result<index::IndexEntry> {
        let key = self.file_key(path)?;
        let mut index = self.read_index()?;
        if !index.conflicted_paths().iter().any(|path| path == &key) {
            return Err(RepoErr::PathNotConflicted(key));
        }

        let entry = if let Some(file) = file {
            self.index_entry_for_state(key.clone(), index::IndexStage::Normal, file)?
        } else {
            index::IndexEntry {
                path: key.clone(),
                mode: None,
                oid: None,
                stage: index::IndexStage::Normal,
                file: None,
                artifact: None,
            }
        };
        index.stage(entry.clone());
        self.write_index(&index)?;
        self.clear_dirty_key(&key)?;
        Ok(entry)
    }

    pub fn resolve_artifact_conflict(
        &self,
        path: impl AsRef<Path>,
        artifact: Option<CommitArtifactState>,
    ) -> Result<index::IndexEntry> {
        let key = self.file_key(path)?;
        self.resolve_artifact_conflict_key(key, artifact)
    }

    pub fn resolve_artifact_conflict_from_path(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<index::IndexEntry> {
        let key = self.file_key(path)?;
        let physical_path = self.worktree.join(&key);
        let artifact = self.write_artifact_state_from_path(&key, &physical_path)?;
        self.resolve_artifact_conflict_key(key, Some(artifact))
    }

    fn resolve_artifact_conflict_key(
        &self,
        key: String,
        artifact: Option<CommitArtifactState>,
    ) -> Result<index::IndexEntry> {
        let mut index = self.read_index()?;
        if !index.conflicted_paths().iter().any(|path| path == &key) {
            return Err(RepoErr::PathNotConflicted(key));
        }

        let entry = if let Some(artifact) = artifact {
            self.index_entry_for_artifact_state(key.clone(), index::IndexStage::Normal, artifact)
        } else {
            index::IndexEntry {
                path: key.clone(),
                mode: None,
                oid: None,
                stage: index::IndexStage::Normal,
                file: None,
                artifact: None,
            }
        };
        index.stage(entry.clone());
        self.write_index(&index)?;
        self.clear_dirty_key(&key)?;
        Ok(entry)
    }

    fn index_entry_for_state(
        &self,
        key: String,
        stage: index::IndexStage,
        file: CommitFileState,
    ) -> Result<index::IndexEntry> {
        let blob = object::Object::Blob(object::BlobObject::SqliteSnapshot(sqlite_snapshot_blob(
            &file,
        )));
        let oid = self.object_store().write(&blob)?;
        Ok(index::IndexEntry {
            path: key,
            mode: Some(object::TreeEntryMode::SqliteDatabase),
            oid: Some(oid),
            stage,
            file: Some(file),
            artifact: None,
        })
    }

    fn index_entry_for_artifact_state(
        &self,
        key: String,
        stage: index::IndexStage,
        artifact: CommitArtifactState,
    ) -> index::IndexEntry {
        index::IndexEntry {
            path: key,
            mode: Some(object::TreeEntryMode::Regular),
            oid: Some(artifact.oid().clone()),
            stage,
            file: None,
            artifact: Some(artifact),
        }
    }

    pub fn commit_staged(&self, message: impl Into<String>) -> Result<CommitObject> {
        self.commit_staged_with_table_summary(message, Vec::new())
    }

    pub fn commit_staged_with_table_summary(
        &self,
        message: impl Into<String>,
        tables: Vec<CommitTableSummary>,
    ) -> Result<CommitObject> {
        let index = self.read_index()?;
        if index.has_conflicts() {
            return Err(RepoErr::UnresolvedConflicts);
        }
        if !index.has_staged_changes() && self.merge_head()?.is_none() {
            return Err(RepoErr::NoStagedChanges);
        }

        let mut files = self.head_files()?;
        let mut artifacts = self.head_artifacts()?;
        for entry in index.stage0_entries() {
            if let Some(file) = &entry.file {
                files.insert(entry.path.clone(), file.clone());
                artifacts.remove(&entry.path);
            } else if let Some(artifact) = &entry.artifact {
                artifacts.insert(entry.path.clone(), artifact.clone());
                files.remove(&entry.path);
            } else {
                files.remove(&entry.path);
                artifacts.remove(&entry.path);
            }
        }
        let commit = self.commit_with_files_and_artifacts(message, files, artifacts, tables)?;
        self.clear_index()?;
        Ok(commit)
    }

    #[cfg(test)]
    fn commit_file(
        &self,
        path: impl AsRef<Path>,
        message: impl Into<String>,
        volume: VolumeId,
        snapshot: &Snapshot,
    ) -> Result<CommitObject> {
        self.stage_file(path, volume, snapshot)?;
        self.commit_staged(message)
    }

    fn commit_with_files(
        &self,
        message: impl Into<String>,
        files: BTreeMap<String, CommitFileState>,
        tables: Vec<CommitTableSummary>,
    ) -> Result<CommitObject> {
        self.commit_with_files_and_artifacts(message, files, BTreeMap::new(), tables)
    }

    fn commit_with_files_and_artifacts(
        &self,
        message: impl Into<String>,
        files: BTreeMap<String, CommitFileState>,
        artifacts: BTreeMap<String, CommitArtifactState>,
        tables: Vec<CommitTableSummary>,
    ) -> Result<CommitObject> {
        let head = self.head()?;
        let parents = self.commit_parents()?;
        let parent = parents.first().cloned();
        let timestamp_ms = now_ms();
        let message = message.into();
        let tables = normalize_commit_table_summary(tables);
        let changed_tables = tables.len();
        let changes =
            self.commit_changes(parents.first().map(String::as_str), &files, &artifacts)?;
        let object_store = self.object_store();
        let tree = self.write_tree_object(&object_store, &files, &artifacts)?;
        let commit_object = self.canonical_commit_object(
            tree.clone(),
            &parents,
            &message,
            timestamp_ms,
            tables.clone(),
        )?;
        let id = object_store.write(&object::Object::Commit(commit_object))?;
        let commit = CommitObject {
            id: id.to_string(),
            parent,
            parents,
            tree: Some(tree.to_string()),
            message,
            timestamp_ms,
            files,
            artifacts,
            changes,
            tables,
            changed_tables,
        };

        match head {
            Head::Branch { name } => {
                self.write_branch_ref(&name, &commit.id, &format!("commit: {}", commit.message))?
            }
            Head::Detached { .. } => self.write_head_with_message(
                &Head::Detached { commit: commit.id.clone() },
                &format!("commit: {}", commit.message),
            )?,
        }

        self.clear_merge_state()?;
        Ok(commit)
    }

    fn commit_changes(
        &self,
        parent: Option<&str>,
        files: &BTreeMap<String, CommitFileState>,
        artifacts: &BTreeMap<String, CommitArtifactState>,
    ) -> Result<Vec<CommitPathChange>> {
        let Some(parent) = parent else {
            return Ok(commit_path_changes(
                &BTreeMap::new(),
                files,
                &BTreeMap::new(),
                artifacts,
            ));
        };
        let Some((parent_files, parent_artifacts)) = self.commit_tree_state(parent)? else {
            return Ok(Vec::new());
        };
        Ok(commit_path_changes(
            &parent_files,
            files,
            &parent_artifacts,
            artifacts,
        ))
    }

    pub fn log(&self) -> Result<Vec<CommitObject>> {
        let mut commits = vec![];
        let mut frontier = self.head_target()?.into_iter().collect::<Vec<_>>();
        let mut seen = BTreeSet::<String>::new();
        let mut cache = BTreeMap::<String, CommitObject>::new();

        while let Some((idx, id)) = self.next_log_frontier_commit(&frontier, &seen, &mut cache)? {
            frontier.remove(idx);
            if !seen.insert(id.clone()) {
                continue;
            }
            let commit = cache
                .remove(&id)
                .unwrap_or_else(|| unreachable!("commit was cached"));
            for parent in commit_parent_ids(&commit) {
                if !seen.contains(&parent) {
                    frontier.push(parent);
                }
            }
            commits.push(commit);
        }

        Ok(commits)
    }

    fn next_log_frontier_commit(
        &self,
        frontier: &[String],
        seen: &BTreeSet<String>,
        cache: &mut BTreeMap<String, CommitObject>,
    ) -> Result<Option<(usize, String)>> {
        let mut selected = None;
        let mut selected_timestamp = 0;

        for (idx, id) in frontier.iter().enumerate() {
            if seen.contains(id) {
                continue;
            }
            if !cache.contains_key(id) {
                cache.insert(id.clone(), self.read_commit(id)?);
            }
            let timestamp = cache
                .get(id)
                .map(|commit| commit.timestamp_ms)
                .unwrap_or_default();
            if selected.is_none() || timestamp > selected_timestamp {
                selected = Some((idx, id.clone()));
                selected_timestamp = timestamp;
            }
        }

        Ok(selected)
    }

    pub fn resolve_revision(&self, rev: &str) -> Result<String> {
        let rev = rev.trim();
        if rev.is_empty() {
            return Err(RepoErr::InvalidRevision(rev.to_string()));
        }

        let (base, ops) = split_revision_ops(rev)?;
        let mut id = self.resolve_revision_base(base)?;
        for op in ops {
            id = self.apply_revision_op(&id, op, rev)?;
        }
        Ok(id)
    }

    pub fn diff_revisions(&self, from: &str, to: &str, path: Option<&str>) -> Result<RepoDiff> {
        let from_id = self.resolve_revision(from)?;
        let to_id = self.resolve_revision(to)?;
        let from_commit = self.read_commit(&from_id)?;
        let to_commit = self.read_commit(&to_id)?;

        Ok(diff_repo_maps(
            from_id,
            to_id,
            &from_commit.files,
            &to_commit.files,
            &from_commit.artifacts,
            &to_commit.artifacts,
            path,
        ))
    }

    pub fn diff_staged(&self, path: Option<&str>) -> Result<RepoDiff> {
        let from = self.head_target()?.unwrap_or_else(|| "HEAD".to_string());
        let head_files = self.head_files()?;
        let index_files = self.index_files()?;
        let head_artifacts = self.head_artifacts()?;
        let index_artifacts = self.index_artifacts()?;
        Ok(diff_repo_maps(
            from,
            "index",
            &head_files,
            &index_files,
            &head_artifacts,
            &index_artifacts,
            path,
        ))
    }

    pub fn diff_worktree_file(
        &self,
        path: impl AsRef<Path>,
        state: CommitFileState,
        filter: Option<&str>,
    ) -> Result<RepoDiff> {
        let key = self.file_key(path)?;
        let mut worktree_files = self.index_files()?;
        worktree_files.insert(key.clone(), state);
        let mut worktree_artifacts = self.index_artifacts()?;
        worktree_artifacts.remove(&key);
        Ok(diff_repo_maps(
            "index",
            "worktree",
            &self.index_files()?,
            &worktree_files,
            &self.index_artifacts()?,
            &worktree_artifacts,
            filter,
        ))
    }

    pub fn diff_worktree_file_removal(
        &self,
        path: impl AsRef<Path>,
        filter: Option<&str>,
    ) -> Result<RepoDiff> {
        let mut worktree_files = self.index_files()?;
        worktree_files.remove(&self.file_key(path)?);
        Ok(diff_repo_maps(
            "index",
            "worktree",
            &self.index_files()?,
            &worktree_files,
            &self.index_artifacts()?,
            &self.index_artifacts()?,
            filter,
        ))
    }

    pub fn diff_worktree_artifact(
        &self,
        path: impl AsRef<Path>,
        filter: Option<&str>,
    ) -> Result<RepoDiff> {
        let key = self.file_key(path)?;
        let artifact = self.write_artifact_state_from_path(&key, &self.worktree.join(&key))?;
        let index_files = self.index_files()?;
        let mut worktree_files = index_files.clone();
        worktree_files.remove(&key);
        let index_artifacts = self.index_artifacts()?;
        let mut worktree_artifacts = index_artifacts.clone();
        worktree_artifacts.insert(key, artifact);
        Ok(diff_repo_maps(
            "index",
            "worktree",
            &index_files,
            &worktree_files,
            &index_artifacts,
            &worktree_artifacts,
            filter,
        ))
    }

    pub fn diff_worktree_artifact_removal(
        &self,
        path: impl AsRef<Path>,
        filter: Option<&str>,
    ) -> Result<RepoDiff> {
        let key = self.file_key(path)?;
        let index_files = self.index_files()?;
        let index_artifacts = self.index_artifacts()?;
        let mut worktree_artifacts = index_artifacts.clone();
        worktree_artifacts.remove(&key);
        Ok(diff_repo_maps(
            "index",
            "worktree",
            &index_files,
            &index_files,
            &index_artifacts,
            &worktree_artifacts,
            filter,
        ))
    }

    pub fn diff_revision_to_worktree_file(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
        state: CommitFileState,
        filter: Option<&str>,
    ) -> Result<RepoDiff> {
        let from_id = self.resolve_revision(rev)?;
        let from_commit = self.read_commit(&from_id)?;
        let from_files = from_commit.files;
        let from_artifacts = from_commit.artifacts;
        let mut worktree_files = from_files.clone();
        let key = self.file_key(path)?;
        worktree_files.insert(key.clone(), state);
        let mut worktree_artifacts = from_artifacts.clone();
        worktree_artifacts.remove(&key);
        Ok(diff_repo_maps(
            from_id,
            "worktree",
            &from_files,
            &worktree_files,
            &from_artifacts,
            &worktree_artifacts,
            filter,
        ))
    }

    pub fn diff_revision_to_worktree_file_removal(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
        filter: Option<&str>,
    ) -> Result<RepoDiff> {
        let from_id = self.resolve_revision(rev)?;
        let from_commit = self.read_commit(&from_id)?;
        let from_files = from_commit.files;
        let from_artifacts = from_commit.artifacts;
        let mut worktree_files = from_files.clone();
        worktree_files.remove(&self.file_key(path)?);
        Ok(diff_repo_maps(
            from_id,
            "worktree",
            &from_files,
            &worktree_files,
            &from_artifacts,
            &from_artifacts,
            filter,
        ))
    }

    pub fn diff_revision_to_worktree_artifact(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
        filter: Option<&str>,
    ) -> Result<RepoDiff> {
        let from_id = self.resolve_revision(rev)?;
        let from_commit = self.read_commit(&from_id)?;
        let from_files = from_commit.files;
        let from_artifacts = from_commit.artifacts;
        let key = self.file_key(path)?;
        let artifact = self.write_artifact_state_from_path(&key, &self.worktree.join(&key))?;
        let mut worktree_files = from_files.clone();
        worktree_files.remove(&key);
        let mut worktree_artifacts = from_artifacts.clone();
        worktree_artifacts.insert(key, artifact);
        Ok(diff_repo_maps(
            from_id,
            "worktree",
            &from_files,
            &worktree_files,
            &from_artifacts,
            &worktree_artifacts,
            filter,
        ))
    }

    pub fn diff_revision_to_worktree_artifact_removal(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
        filter: Option<&str>,
    ) -> Result<RepoDiff> {
        let from_id = self.resolve_revision(rev)?;
        let from_commit = self.read_commit(&from_id)?;
        let from_files = from_commit.files;
        let from_artifacts = from_commit.artifacts;
        let mut worktree_artifacts = from_artifacts.clone();
        worktree_artifacts.remove(&self.file_key(path)?);
        Ok(diff_repo_maps(
            from_id,
            "worktree",
            &from_files,
            &from_files,
            &from_artifacts,
            &worktree_artifacts,
            filter,
        ))
    }

    pub fn show_revision(&self, rev: &str) -> Result<CommitObject> {
        let id = self.resolve_revision(rev)?;
        self.read_commit(&id)
    }

    pub fn detach(&self, rev: &str) -> Result<String> {
        let plan = self.plan_detach(rev)?;
        self.apply_detach_plan(rev, &plan)
    }

    pub fn plan_detach(&self, rev: &str) -> Result<CheckoutPlan> {
        self.plan_revision_checkout(rev)
    }

    pub fn plan_revision_checkout(&self, rev: &str) -> Result<CheckoutPlan> {
        let id = self.resolve_revision(rev)?;
        self.checkout_plan_for_target(Some(id))
    }

    pub fn apply_detach_plan(&self, rev: &str, plan: &CheckoutPlan) -> Result<String> {
        let id = plan.target.clone().ok_or(RepoErr::UnbornHead)?;
        self.write_head_with_message(
            &Head::Detached { commit: id.clone() },
            &format!("checkout: moving to {rev}"),
        )?;
        Ok(id)
    }

    pub fn checkout_file_from_revision(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
    ) -> Result<CheckoutFileOutcome> {
        let path = self.file_key(path)?;
        self.checkout_file_key_from_revision(rev, path)
    }

    pub fn checkout_file_key_from_revision(
        &self,
        rev: &str,
        path: impl Into<String>,
    ) -> Result<CheckoutFileOutcome> {
        let plan = self.plan_checkout_file_key_from_revision(rev, path)?;
        self.apply_checkout_file_plan(&plan)
    }

    pub fn plan_checkout_file_from_revision(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
    ) -> Result<CheckoutFilePlan> {
        let path = self.file_key(path)?;
        self.plan_checkout_file_key_from_revision(rev, path)
    }

    pub fn plan_checkout_file_key_from_revision(
        &self,
        rev: &str,
        path: impl Into<String>,
    ) -> Result<CheckoutFilePlan> {
        let target = self.resolve_revision(rev)?;
        let path = normalize_repo_path(&path.into());
        let commit = self.read_commit(&target)?;
        let state =
            commit
                .files
                .get(&path)
                .cloned()
                .ok_or_else(|| RepoErr::PathNotFoundInRevision {
                    path: path.clone(),
                    rev: rev.to_string(),
                })?;
        let entry =
            self.index_entry_for_state(path.clone(), index::IndexStage::Normal, state.clone())?;
        Ok(CheckoutFilePlan { target, path, state, entry })
    }

    pub fn apply_checkout_file_plan(&self, plan: &CheckoutFilePlan) -> Result<CheckoutFileOutcome> {
        let mut index = self.read_index()?;
        index.stage(plan.entry.clone());
        self.write_index(&index)?;
        self.clear_dirty_key(&plan.path)?;
        Ok(CheckoutFileOutcome {
            target: plan.target.clone(),
            path: plan.path.clone(),
            state: plan.state.clone(),
        })
    }

    pub fn checkout_artifact_from_revision(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
    ) -> Result<CheckoutArtifactOutcome> {
        let path = self.file_key(path)?;
        self.checkout_artifact_key_from_revision(rev, path)
    }

    pub fn checkout_artifact_key_from_revision(
        &self,
        rev: &str,
        path: impl Into<String>,
    ) -> Result<CheckoutArtifactOutcome> {
        let plan = self.plan_checkout_artifact_key_from_revision(rev, path)?;
        self.apply_checkout_artifact_plan(&plan)
    }

    pub fn plan_checkout_artifact_from_revision(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
    ) -> Result<CheckoutArtifactPlan> {
        let path = self.file_key(path)?;
        self.plan_checkout_artifact_key_from_revision(rev, path)
    }

    pub fn plan_checkout_artifact_key_from_revision(
        &self,
        rev: &str,
        path: impl Into<String>,
    ) -> Result<CheckoutArtifactPlan> {
        let target = self.resolve_revision(rev)?;
        let path = normalize_repo_path(&path.into());
        let commit = self.read_commit(&target)?;
        let state = commit.artifacts.get(&path).cloned().ok_or_else(|| {
            RepoErr::PathNotFoundInRevision { path: path.clone(), rev: rev.to_string() }
        })?;
        let entry = self.index_entry_for_artifact_state(
            path.clone(),
            index::IndexStage::Normal,
            state.clone(),
        );
        Ok(CheckoutArtifactPlan { target, path, state, entry })
    }

    pub fn apply_checkout_artifact_plan(
        &self,
        plan: &CheckoutArtifactPlan,
    ) -> Result<CheckoutArtifactOutcome> {
        let mut index = self.read_index()?;
        index.stage(plan.entry.clone());
        self.write_index(&index)?;
        self.clear_dirty_key(&plan.path)?;
        Ok(CheckoutArtifactOutcome {
            target: plan.target.clone(),
            path: plan.path.clone(),
            state: plan.state.clone(),
        })
    }

    pub fn reset(&self, rev: &str, mode: ResetMode) -> Result<ResetOutcome> {
        let plan = self.plan_reset(rev, mode)?;
        self.apply_reset_plan(&plan)
    }

    pub fn plan_reset(&self, rev: &str, mode: ResetMode) -> Result<ResetPlan> {
        let target = self.resolve_revision(rev)?;
        let checkout = self.checkout_plan_for_target(Some(target.clone()))?;
        Ok(ResetPlan {
            rev: rev.to_string(),
            target,
            mode,
            checkout,
        })
    }

    pub fn apply_reset_plan(&self, plan: &ResetPlan) -> Result<ResetOutcome> {
        self.move_head_to(&plan.target, &format!("reset: moving to {}", plan.rev))?;
        match plan.mode {
            ResetMode::Soft => {}
            ResetMode::Mixed => self.clear_index()?,
            ResetMode::Hard => {
                self.clear_index()?;
                self.clear_dirty()?;
            }
        }
        self.clear_merge_state()?;
        Ok(ResetOutcome {
            target: plan.target.clone(),
            mode: plan.mode,
        })
    }

    pub fn mark_dirty_path(&self, path: impl AsRef<Path>) -> Result<()> {
        let key = self.file_key(path)?;
        self.mark_dirty_key(key)
    }

    pub fn mark_dirty_key(&self, key: impl Into<String>) -> Result<()> {
        let key = normalize_repo_path(&key.into());
        let mut state = self.read_worktree_state()?;
        let mut dirty = state.dirty.into_iter().collect::<BTreeSet<_>>();
        dirty.insert(key.clone());
        state.dirty = dirty.into_iter().collect();
        state.deleted.retain(|path| path != &key);
        self.write_worktree_state(&state)
    }

    pub fn mark_deleted_path(&self, path: impl AsRef<Path>) -> Result<()> {
        let key = self.file_key(path)?;
        self.mark_deleted_key(key)
    }

    pub fn mark_deleted_key(&self, key: impl Into<String>) -> Result<()> {
        let key = normalize_repo_path(&key.into());
        let mut state = self.read_worktree_state()?;
        state.dirty.retain(|path| path != &key);
        let mut deleted = state.deleted.into_iter().collect::<BTreeSet<_>>();
        deleted.insert(key);
        state.deleted = deleted.into_iter().collect();
        self.write_worktree_state(&state)
    }

    pub fn clear_dirty_path(&self, path: impl AsRef<Path>) -> Result<()> {
        let key = self.file_key(path)?;
        self.clear_dirty_key(&key)
    }

    pub fn clear_dirty_key(&self, key: &str) -> Result<()> {
        let key = normalize_repo_path(key);
        let mut state = self.read_worktree_state()?;
        state.dirty.retain(|path| path != &key);
        state.deleted.retain(|path| path != &key);
        self.write_worktree_state(&state)
    }

    pub fn clear_dirty(&self) -> Result<()> {
        let path = self.worktree_state_path();
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    pub fn dirty_paths(&self) -> Result<Vec<String>> {
        let state = self.read_worktree_state()?;
        let mut paths = state.dirty.into_iter().collect::<BTreeSet<_>>();
        paths.extend(state.deleted);
        Ok(paths.into_iter().collect())
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty_paths()
            .map(|paths| !paths.is_empty())
            .unwrap_or(false)
    }

    pub fn has_staged_changes(&self) -> Result<bool> {
        Ok(self.read_index()?.has_staged_changes())
    }

    pub fn has_work_in_progress(&self) -> Result<bool> {
        let index = self.read_index()?;
        Ok(!self.dirty_paths()?.is_empty()
            || index.has_staged_changes()
            || index.has_conflicts()
            || self.merge_head()?.is_some())
    }

    pub fn discard_work_in_progress(&self) -> Result<()> {
        self.clear_index()?;
        self.clear_dirty()?;
        self.clear_merge_state()
    }

    pub fn head_file(&self, path: impl AsRef<Path>) -> Result<Option<CommitFileState>> {
        let key = self.file_key(path)?;
        Ok(self
            .head_target()?
            .map(|commit| self.read_commit(&commit))
            .transpose()?
            .and_then(|commit| commit.files.get(&key).cloned()))
    }

    pub fn head_artifact(&self, path: impl AsRef<Path>) -> Result<Option<CommitArtifactState>> {
        let key = self.file_key(path)?;
        Ok(self
            .head_target()?
            .map(|commit| self.read_commit(&commit))
            .transpose()?
            .and_then(|commit| commit.artifacts.get(&key).cloned()))
    }

    pub fn index_file(&self, path: impl AsRef<Path>) -> Result<Option<CommitFileState>> {
        let key = self.file_key(path)?;
        Ok(self.index_files()?.remove(&key))
    }

    pub fn index_artifact(&self, path: impl AsRef<Path>) -> Result<Option<CommitArtifactState>> {
        let key = self.file_key(path)?;
        Ok(self.index_artifacts()?.remove(&key))
    }

    pub fn index_has_entry(&self, path: impl AsRef<Path>) -> Result<bool> {
        let key = self.file_key(path)?;
        self.index_has_key(key)
    }

    pub fn index_has_key(&self, key: impl Into<String>) -> Result<bool> {
        let key = normalize_repo_path(&key.into());
        Ok(self
            .read_index()?
            .stage0_entries()
            .any(|entry| entry.path == key))
    }

    pub fn restore_index_path_from_head(&self, path: impl AsRef<Path>) -> Result<String> {
        let key = self.file_key(path)?;
        self.restore_index_key_from_head(key)
    }

    pub fn restore_index_key_from_head(&self, key: impl Into<String>) -> Result<String> {
        let key = normalize_repo_path(&key.into());
        let mut index = self.read_index()?;
        if index.conflicted_paths().iter().any(|path| path == &key) {
            return Err(RepoErr::UnresolvedConflicts);
        }
        let had_index_entry = index.entries.iter().any(|entry| entry.path == key);
        let is_tracked_at_head =
            self.head_files()?.contains_key(&key) || self.head_artifacts()?.contains_key(&key);
        if !had_index_entry && !is_tracked_at_head {
            return Err(RepoErr::PathNotTracked(key));
        }
        index.remove_path(&key);
        self.write_index(&index)?;
        Ok(key)
    }

    pub fn restore_index_path_from_revision(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
    ) -> Result<String> {
        let key = self.file_key(path)?;
        self.restore_index_key_from_revision(rev, key)
    }

    pub fn restore_index_key_from_revision(
        &self,
        rev: &str,
        key: impl Into<String>,
    ) -> Result<String> {
        let key = normalize_repo_path(&key.into());
        let target = self.resolve_revision(rev)?;
        let source_commit = self.read_commit(&target)?;
        let source_files = source_commit.files;
        let source_artifacts = source_commit.artifacts;
        let source_state = source_files.get(&key).cloned();
        let source_artifact = source_artifacts.get(&key).cloned();
        let head_files = self.head_files()?;
        let head_artifacts = self.head_artifacts()?;
        let head_state = head_files.get(&key);
        let head_artifact = head_artifacts.get(&key);
        let head_has_path = head_state.is_some() || head_artifact.is_some();
        let mut index = self.read_index()?;
        if index.conflicted_paths().iter().any(|path| path == &key) {
            return Err(RepoErr::UnresolvedConflicts);
        }
        let had_index_entry = index.entries.iter().any(|entry| entry.path == key);

        if source_state.is_none() && source_artifact.is_none() && !head_has_path && !had_index_entry
        {
            return Err(RepoErr::PathNotFoundInRevision { path: key, rev: rev.to_string() });
        }

        index.remove_path(&key);
        if source_state.as_ref() == head_state && source_artifact.as_ref() == head_artifact {
            // Resetting the index to HEAD is represented by the absence of an index entry.
        } else if let Some(file) = source_state {
            index.stage(self.index_entry_for_state(
                key.clone(),
                index::IndexStage::Normal,
                file,
            )?);
        } else if let Some(artifact) = source_artifact {
            index.stage(self.index_entry_for_artifact_state(
                key.clone(),
                index::IndexStage::Normal,
                artifact,
            ));
        } else if head_has_path {
            index.stage(index::IndexEntry {
                path: key.clone(),
                mode: None,
                oid: None,
                stage: index::IndexStage::Normal,
                file: None,
                artifact: None,
            });
        }
        self.write_index(&index)?;
        Ok(key)
    }

    pub fn file_from_revision(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
    ) -> Result<Option<CommitFileState>> {
        let target = self.resolve_revision(rev)?;
        let key = self.file_key(path)?;
        Ok(self.read_commit(&target)?.files.get(&key).cloned())
    }

    pub fn artifact_from_revision(
        &self,
        rev: &str,
        path: impl AsRef<Path>,
    ) -> Result<Option<CommitArtifactState>> {
        let target = self.resolve_revision(rev)?;
        let key = self.file_key(path)?;
        Ok(self.read_commit(&target)?.artifacts.get(&key).cloned())
    }

    pub fn materialize_artifact_state(
        &self,
        path: impl AsRef<Path>,
        state: &CommitArtifactState,
    ) -> Result<()> {
        let key = self.file_key(path)?;
        self.materialize_artifact_key(&key, state)
    }

    pub fn materialize_artifact_key(&self, key: &str, state: &CommitArtifactState) -> Result<()> {
        let path = self.worktree.join(key);
        let bytes = self.artifact_bytes(state)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_file_atomic(&path, &bytes)?;
        Ok(())
    }

    pub fn materialize_artifact_checkout(
        &self,
        artifacts: &BTreeMap<String, CommitArtifactState>,
        previous_artifacts: &BTreeMap<String, CommitArtifactState>,
        replacement_files: &BTreeMap<String, CommitFileState>,
    ) -> Result<()> {
        for (path, state) in artifacts {
            self.materialize_artifact_key(path, state)?;
        }
        for path in previous_artifacts.keys() {
            if artifacts.contains_key(path) || replacement_files.contains_key(path) {
                continue;
            }
            let physical_path = self.worktree.join(path);
            if physical_path.is_file() {
                fs::remove_file(&physical_path)?;
                remove_empty_parent_dirs(physical_path.parent(), &self.worktree)?;
            }
        }
        Ok(())
    }

    pub fn artifact_path_matches_state(
        &self,
        path: impl AsRef<Path>,
        expected: &CommitArtifactState,
    ) -> Result<Option<bool>> {
        let key = self.file_key(path)?;
        artifact_file_matches(&self.worktree.join(key), expected)
    }

    pub fn file_key(&self, path: impl AsRef<Path>) -> Result<String> {
        let path = path.as_ref();
        let parent = worktree_for_file(path);
        let parent = fs::canonicalize(parent)?;
        let Some(file_name) = path.file_name() else {
            return Err(RepoErr::PathOutsideWorktree {
                path: path.to_path_buf(),
                worktree: self.worktree.clone(),
            });
        };
        let absolute = parent.join(file_name);
        let relative =
            absolute
                .strip_prefix(&self.worktree)
                .map_err(|_| RepoErr::PathOutsideWorktree {
                    path: absolute.clone(),
                    worktree: self.worktree.clone(),
                })?;
        relative
            .to_str()
            .map(|path| path.replace('\\', "/"))
            .ok_or_else(|| RepoErr::NonUtf8Path(relative.to_path_buf()))
    }

    pub fn is_ignored_worktree_path(&self, path: impl AsRef<Path>) -> Result<bool> {
        let path = path.as_ref();
        let key = self.worktree_key_for_path(path)?;
        let is_dir = path.is_dir();
        Ok(self.ignore_rules()?.is_ignored(&key, is_dir))
    }

    fn worktree_key_for_path(&self, path: &Path) -> Result<String> {
        let relative =
            path.strip_prefix(&self.worktree)
                .map_err(|_| RepoErr::PathOutsideWorktree {
                    path: path.to_path_buf(),
                    worktree: self.worktree.clone(),
                })?;
        relative
            .to_str()
            .map(|path| normalize_repo_path(path))
            .ok_or_else(|| RepoErr::NonUtf8Path(relative.to_path_buf()))
    }

    fn ignore_rules(&self) -> Result<IgnoreRules> {
        IgnoreRules::load(&self.worktree)
    }

    fn create_layout(&self) -> Result<()> {
        for dir in [
            DIR_REFS_HEADS,
            DIR_REFS_REMOTES,
            DIR_REFS_TAGS,
            DIR_OBJECTS,
            DIR_OBJECTS_PACK,
            DIR_STORE_FJALL,
            DIR_STORE_FILES,
            DIR_INDEX,
            DIR_LOCKS,
            DIR_TMP,
            DIR_LOGS_REFS,
            DIR_LOGS_HEAD,
        ] {
            fs::create_dir_all(self.graft_dir.join(dir))?;
        }
        Ok(())
    }

    fn ensure_supported_format(&self) -> Result<()> {
        let config = self.config()?;
        let actual = config.core.repository_format_version;
        if actual != REPOSITORY_FORMAT_VERSION {
            return Err(RepoErr::UnsupportedFormat {
                expected: REPOSITORY_FORMAT_VERSION,
                actual,
            });
        }
        let actual = config.extensions.object_format;
        if actual != OBJECT_FORMAT {
            return Err(RepoErr::UnsupportedObjectFormat { expected: OBJECT_FORMAT, actual });
        }
        Ok(())
    }

    fn config_path(&self) -> PathBuf {
        self.graft_dir.join(CONFIG_FILE)
    }

    fn head_path(&self) -> PathBuf {
        self.graft_dir.join(HEAD_FILE)
    }

    fn current_head_for_reflog(&self) -> Result<Option<Head>> {
        if !self.head_path().is_file() {
            return Ok(None);
        }
        self.head().map(Some)
    }

    fn head_reflog_target(&self, head: &Head) -> Result<Option<String>> {
        match head {
            Head::Branch { name } => self.read_branch_ref(name),
            Head::Detached { commit } => Ok(Some(commit.clone())),
        }
    }

    fn merge_head_path(&self) -> PathBuf {
        self.graft_dir.join(MERGE_HEAD_FILE)
    }

    fn orig_head_path(&self) -> PathBuf {
        self.graft_dir.join(ORIG_HEAD_FILE)
    }

    fn worktree_state_path(&self) -> PathBuf {
        self.graft_dir.join(DIR_INDEX).join("worktree.toml")
    }

    fn index_path(&self) -> PathBuf {
        self.graft_dir.join(DIR_INDEX).join("state.toml")
    }

    fn head_target(&self) -> Result<Option<String>> {
        match self.head()? {
            Head::Branch { name } => self.read_branch_ref(&name),
            Head::Detached { commit } => Ok(Some(commit)),
        }
    }

    fn move_head_to(&self, id: &str, message: &str) -> Result<()> {
        match self.head()? {
            Head::Branch { name } => self.write_branch_ref(&name, id, message)?,
            Head::Detached { .. } => {
                self.write_head_with_message(&Head::Detached { commit: id.to_string() }, message)?
            }
        }
        Ok(())
    }

    fn merge_head(&self) -> Result<Option<String>> {
        let path = self.merge_head_path();
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(path)?;
        let target = raw.trim();
        if target.is_empty() {
            return Ok(None);
        }
        Ok(Some(target.to_string()))
    }

    fn orig_head(&self) -> Result<Option<String>> {
        let path = self.orig_head_path();
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(path)?;
        let target = raw.trim();
        if target.is_empty() {
            return Ok(None);
        }
        Ok(Some(target.to_string()))
    }

    fn write_merge_state(&self, orig_head: &str, merge_head: &str) -> Result<()> {
        fs::write(self.orig_head_path(), format!("{orig_head}\n"))?;
        fs::write(self.merge_head_path(), format!("{merge_head}\n"))?;
        Ok(())
    }

    fn clear_merge_state(&self) -> Result<()> {
        for path in [self.merge_head_path(), self.orig_head_path()] {
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    fn commit_parents(&self) -> Result<Vec<String>> {
        let mut parents = Vec::new();
        if let Some(head) = self.head_target()? {
            parents.push(head);
        }
        if let Some(merge_head) = self.merge_head()?
            && !parents.iter().any(|parent| parent == &merge_head)
        {
            parents.push(merge_head);
        }
        Ok(parents)
    }

    pub fn read_commit(&self, id: &str) -> Result<CommitObject> {
        let id = object::ObjectId::from_str(id)?;
        let commit = self
            .read_commit_object(&id)?
            .ok_or_else(|| RepoErr::CommitNotFound(id.to_string()))?;
        self.commit_from_object(&id, commit)
    }

    fn read_commit_object(&self, id: &object::ObjectId) -> Result<Option<object::CommitObject>> {
        let Some(bytes) = self.object_store().read_raw(id)? else {
            return Ok(None);
        };
        let object = object::Object::decode(&bytes)?;
        let actual = object.id();
        if actual != *id {
            return Err(RepoErr::Object(object::ObjectErr::ObjectIdMismatch {
                expected: id.clone(),
                actual,
            }));
        }
        match object {
            object::Object::Commit(commit) => Ok(Some(commit)),
            object => Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "commit",
                message: format!("object {id} is a {}", object.kind()),
            })),
        }
    }

    fn commit_tree_state(
        &self,
        id: &str,
    ) -> Result<
        Option<(
            BTreeMap<String, CommitFileState>,
            BTreeMap<String, CommitArtifactState>,
        )>,
    > {
        let id = object::ObjectId::from_str(id)?;
        let Some(commit) = self.read_commit_object(&id)? else {
            return Ok(None);
        };
        self.tree_state_from_object(&commit.tree).map(Some)
    }

    fn commit_from_object(
        &self,
        id: &object::ObjectId,
        commit: object::CommitObject,
    ) -> Result<CommitObject> {
        let parents = commit
            .parents
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let tables = commit.tables;
        let changed_tables = tables.len();
        let (files, artifacts) = self.tree_state_from_object(&commit.tree)?;
        let changes =
            self.commit_changes(parents.first().map(String::as_str), &files, &artifacts)?;
        Ok(CommitObject {
            id: id.to_string(),
            parent: parents.first().cloned(),
            parents,
            tree: Some(commit.tree.to_string()),
            message: commit.message,
            timestamp_ms: commit.committer.timestamp_ms,
            files,
            artifacts,
            changes,
            tables,
            changed_tables,
        })
    }

    fn tree_state_from_object(
        &self,
        id: &object::ObjectId,
    ) -> Result<(
        BTreeMap<String, CommitFileState>,
        BTreeMap<String, CommitArtifactState>,
    )> {
        let object = self.object_store().read(id)?;
        let object::Object::Tree(tree) = object else {
            return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "tree",
                message: format!("object {id} is not a tree"),
            }));
        };

        let mut files = BTreeMap::new();
        let mut artifacts = BTreeMap::new();
        for entry in tree.entries {
            match entry.mode {
                object::TreeEntryMode::SqliteDatabase => {
                    let object = self.object_store().read(&entry.oid)?;
                    let object::Object::Blob(object::BlobObject::SqliteSnapshot(blob)) = object
                    else {
                        return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                            kind: "blob",
                            message: format!(
                                "tree entry `{}` is not a sqlite snapshot",
                                entry.path
                            ),
                        }));
                    };
                    files.insert(entry.path, file_state_from_sqlite_snapshot_blob(blob));
                }
                object::TreeEntryMode::Regular => {
                    let object = self.object_store().read(&entry.oid)?;
                    let object::Object::Blob(blob) = object else {
                        return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                            kind: "blob",
                            message: format!("tree entry `{}` is not a blob", entry.path),
                        }));
                    };
                    artifacts.insert(entry.path, artifact_state_from_blob(entry.oid, blob)?);
                }
            }
        }
        Ok((files, artifacts))
    }

    pub fn read_index(&self) -> Result<index::Index> {
        let path = self.index_path();
        if !path.exists() {
            return Ok(index::Index::default());
        }
        let raw = fs::read_to_string(path)?;
        Ok(toml::from_str(&raw)?)
    }

    pub fn index_files(&self) -> Result<BTreeMap<String, CommitFileState>> {
        let index = self.read_index()?;
        if index.has_conflicts() {
            return Err(RepoErr::UnresolvedConflicts);
        }

        let mut files = self.head_files()?;
        for entry in index.stage0_entries() {
            if let Some(file) = &entry.file {
                files.insert(entry.path.clone(), file.clone());
            } else if entry.artifact.is_some() {
                files.remove(&entry.path);
            } else {
                files.remove(&entry.path);
            }
        }
        Ok(files)
    }

    pub fn index_artifacts(&self) -> Result<BTreeMap<String, CommitArtifactState>> {
        let index = self.read_index()?;
        if index.has_conflicts() {
            return Err(RepoErr::UnresolvedConflicts);
        }

        let mut artifacts = self.head_artifacts()?;
        for entry in index.stage0_entries() {
            if let Some(artifact) = &entry.artifact {
                artifacts.insert(entry.path.clone(), artifact.clone());
            } else if entry.file.is_some() {
                artifacts.remove(&entry.path);
            } else {
                artifacts.remove(&entry.path);
            }
        }
        Ok(artifacts)
    }

    fn files_for_worktree_status(
        &self,
        index: &index::Index,
    ) -> Result<BTreeMap<String, CommitFileState>> {
        let mut files = self.head_files()?;
        for entry in index.stage0_entries() {
            if let Some(file) = &entry.file {
                files.insert(entry.path.clone(), file.clone());
            } else if entry.artifact.is_some() {
                files.remove(&entry.path);
            } else {
                files.remove(&entry.path);
            }
        }
        Ok(files)
    }

    fn artifacts_for_worktree_status(
        &self,
        index: &index::Index,
    ) -> Result<BTreeMap<String, CommitArtifactState>> {
        let mut artifacts = self.head_artifacts()?;
        for entry in index.stage0_entries() {
            if let Some(artifact) = &entry.artifact {
                artifacts.insert(entry.path.clone(), artifact.clone());
            } else if entry.file.is_some() {
                artifacts.remove(&entry.path);
            } else {
                artifacts.remove(&entry.path);
            }
        }
        Ok(artifacts)
    }

    fn staged_changes_for_index(&self, index: &index::Index) -> Result<Vec<RepoStagedChange>> {
        let head_files = self.head_files()?;
        let head_artifacts = self.head_artifacts()?;
        let mut changes = Vec::new();

        for entry in index.stage0_entries() {
            let was_tracked =
                head_files.contains_key(&entry.path) || head_artifacts.contains_key(&entry.path);
            let (change, kind, storage) = if entry.file.is_some() {
                (
                    if was_tracked {
                        RepoFileChange::Modified
                    } else {
                        RepoFileChange::Added
                    },
                    RepoTrackedPathKind::SqliteDatabase,
                    RepoPathStorage::SqliteSnapshot,
                )
            } else if let Some(artifact) = &entry.artifact {
                (
                    if was_tracked {
                        RepoFileChange::Modified
                    } else {
                        RepoFileChange::Added
                    },
                    artifact_tracked_path_kind(artifact),
                    artifact_tracked_path_storage(artifact),
                )
            } else {
                let (kind, storage) = if head_files.contains_key(&entry.path) {
                    (
                        RepoTrackedPathKind::SqliteDatabase,
                        RepoPathStorage::SqliteSnapshot,
                    )
                } else if let Some(artifact) = head_artifacts.get(&entry.path) {
                    (
                        artifact_tracked_path_kind(artifact),
                        artifact_tracked_path_storage(artifact),
                    )
                } else {
                    (RepoTrackedPathKind::BinaryFile, RepoPathStorage::Inline)
                };
                (RepoFileChange::Deleted, kind, storage)
            };

            changes.push(RepoStagedChange {
                path: entry.path.clone(),
                change,
                kind,
                storage,
            });
        }

        Ok(changes)
    }

    fn conflicted_changes_for_index(&self, index: &index::Index) -> Vec<RepoConflictChange> {
        fn kind_priority(kind: RepoTrackedPathKind) -> u8 {
            match kind {
                RepoTrackedPathKind::TextFile => 1,
                RepoTrackedPathKind::BinaryFile => 1,
                RepoTrackedPathKind::SqliteDatabase => 3,
            }
        }

        let mut by_path = BTreeMap::<String, (RepoTrackedPathKind, RepoPathStorage)>::new();

        for entry in index
            .entries
            .iter()
            .filter(|entry| entry.stage != index::IndexStage::Normal)
        {
            let (kind, storage) = if entry.file.is_some() {
                (
                    RepoTrackedPathKind::SqliteDatabase,
                    RepoPathStorage::SqliteSnapshot,
                )
            } else if let Some(artifact) = &entry.artifact {
                (
                    artifact_tracked_path_kind(artifact),
                    artifact_tracked_path_storage(artifact),
                )
            } else {
                (RepoTrackedPathKind::BinaryFile, RepoPathStorage::Inline)
            };
            by_path
                .entry(entry.path.clone())
                .and_modify(|existing| {
                    if kind_priority(kind) > kind_priority(existing.0) {
                        *existing = (kind, storage);
                    }
                })
                .or_insert((kind, storage));
        }

        by_path
            .into_iter()
            .map(|(path, (kind, storage))| RepoConflictChange { path, kind, storage })
            .collect()
    }

    fn unstaged_changes_for_index(&self, index: &index::Index) -> Result<Vec<RepoWorktreeChange>> {
        let tracked = self.files_for_worktree_status(index)?;
        let tracked_artifacts = self.artifacts_for_worktree_status(index)?;
        let state = self.read_worktree_state()?;
        let mut changes = BTreeMap::<
            String,
            (RepoWorktreeChangeKind, RepoTrackedPathKind, RepoPathStorage),
        >::new();
        for path in state.dirty {
            let (change, kind, storage) = if tracked.contains_key(&path) {
                (
                    RepoWorktreeChangeKind::Modified,
                    RepoTrackedPathKind::SqliteDatabase,
                    RepoPathStorage::SqliteSnapshot,
                )
            } else if let Some(artifact) = tracked_artifacts.get(&path) {
                (
                    RepoWorktreeChangeKind::Modified,
                    artifact_tracked_path_kind(artifact),
                    artifact_tracked_path_storage(artifact),
                )
            } else {
                let (kind, storage) = self.worktree_path_descriptor(&path)?;
                (RepoWorktreeChangeKind::Untracked, kind, storage)
            };
            changes.insert(path, (change, kind, storage));
        }
        for path in state.deleted {
            if tracked.contains_key(&path) {
                changes.insert(
                    path,
                    (
                        RepoWorktreeChangeKind::Deleted,
                        RepoTrackedPathKind::SqliteDatabase,
                        RepoPathStorage::SqliteSnapshot,
                    ),
                );
            } else if let Some(artifact) = tracked_artifacts.get(&path) {
                changes.insert(
                    path,
                    (
                        RepoWorktreeChangeKind::Deleted,
                        artifact_tracked_path_kind(artifact),
                        artifact_tracked_path_storage(artifact),
                    ),
                );
            }
        }
        for (path, expected) in &tracked_artifacts {
            if changes.contains_key(path) {
                continue;
            }
            let physical_path = self.worktree.join(&path);
            match artifact_file_matches(&physical_path, expected)? {
                Some(true) => {}
                Some(false) => {
                    changes.insert(
                        path.clone(),
                        (
                            RepoWorktreeChangeKind::Modified,
                            artifact_tracked_path_kind(expected),
                            artifact_tracked_path_storage(expected),
                        ),
                    );
                }
                None => {
                    changes.insert(
                        path.clone(),
                        (
                            RepoWorktreeChangeKind::Deleted,
                            artifact_tracked_path_kind(expected),
                            artifact_tracked_path_storage(expected),
                        ),
                    );
                }
            }
        }
        for path in self.untracked_paths_for_index(index)? {
            changes.entry(path.path).or_insert((
                RepoWorktreeChangeKind::Untracked,
                path.kind,
                path.storage,
            ));
        }
        Ok(changes
            .into_iter()
            .map(|(path, (change, kind, storage))| RepoWorktreeChange {
                path,
                change,
                kind,
                storage,
            })
            .collect())
    }

    fn untracked_paths_for_index(&self, index: &index::Index) -> Result<Vec<RepoTrackedPath>> {
        let tracked = self.files_for_worktree_status(index)?;
        let tracked_artifacts = self.artifacts_for_worktree_status(index)?;
        let mut paths = BTreeMap::<String, RepoTrackedPath>::new();

        for path in self.scan_untracked_sqlite_files()? {
            if !tracked.contains_key(&path) && !tracked_artifacts.contains_key(&path) {
                let size = self.worktree_path_size(&path)?;
                paths.insert(
                    path.clone(),
                    RepoTrackedPath {
                        path,
                        kind: RepoTrackedPathKind::SqliteDatabase,
                        storage: RepoPathStorage::SqliteSnapshot,
                        size,
                        page_count: None,
                    },
                );
            }
        }

        for path in self.scan_untracked_artifact_files()? {
            if !tracked.contains_key(&path) && !tracked_artifacts.contains_key(&path) {
                let (kind, storage) = self.worktree_path_descriptor(&path)?;
                let size = self.worktree_path_size(&path)?;
                paths.entry(path.clone()).or_insert(RepoTrackedPath {
                    path,
                    kind,
                    storage,
                    size,
                    page_count: None,
                });
            }
        }

        Ok(paths.into_values().collect())
    }

    fn worktree_path_size(&self, key: &str) -> Result<Option<u64>> {
        match fs::metadata(self.worktree.join(key)) {
            Ok(metadata) => Ok(Some(metadata.len())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    fn worktree_path_descriptor(
        &self,
        key: &str,
    ) -> Result<(RepoTrackedPathKind, RepoPathStorage)> {
        let path = self.worktree.join(key);
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok((RepoTrackedPathKind::BinaryFile, RepoPathStorage::External));
            }
            Err(err) => return Err(err.into()),
        };
        if is_sqlite_database_file(&path)? {
            return Ok((
                RepoTrackedPathKind::SqliteDatabase,
                RepoPathStorage::SqliteSnapshot,
            ));
        }
        let kind = classify_artifact_path(&path)?;
        let storage = artifact_storage_for_path(key, kind, metadata.len(), &self.file_config()?);
        Ok((kind, storage))
    }

    fn scan_untracked_sqlite_files(&self) -> Result<Vec<String>> {
        let mut paths = BTreeSet::new();
        let ignore = self.ignore_rules()?;
        self.collect_sqlite_worktree_files(&self.worktree, &ignore, &mut paths)?;
        Ok(paths.into_iter().collect())
    }

    fn scan_untracked_artifact_files(&self) -> Result<Vec<String>> {
        let mut paths = BTreeSet::new();
        let ignore = self.ignore_rules()?;
        self.collect_artifact_worktree_files(&self.worktree, &ignore, &mut paths)?;
        Ok(paths.into_iter().collect())
    }

    fn collect_sqlite_worktree_files(
        &self,
        dir: &Path,
        ignore: &IgnoreRules,
        out: &mut BTreeSet<String>,
    ) -> Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                if entry.file_name() == GRAFT_DIR {
                    continue;
                }
                let key = self.worktree_key_for_path(&path)?;
                if ignore.is_ignored(&key, true) {
                    continue;
                }
                self.collect_sqlite_worktree_files(&path, ignore, out)?;
            } else if file_type.is_file() {
                let key = self.worktree_key_for_path(&path)?;
                if ignore.is_ignored(&key, false) {
                    continue;
                }
                if is_sqlite_database_file(&path)? {
                    out.insert(key);
                }
            }
        }
        Ok(())
    }

    fn collect_artifact_worktree_files(
        &self,
        dir: &Path,
        ignore: &IgnoreRules,
        out: &mut BTreeSet<String>,
    ) -> Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                if entry.file_name() == GRAFT_DIR {
                    continue;
                }
                let key = self.worktree_key_for_path(&path)?;
                if ignore.is_ignored(&key, true) {
                    continue;
                }
                self.collect_artifact_worktree_files(&path, ignore, out)?;
            } else if file_type.is_file()
                && !is_sqlite_sidecar_file(&path)
                && !is_sqlite_database_file(&path)?
            {
                let key = self.worktree_key_for_path(&path)?;
                if ignore.is_ignored(&key, false) {
                    continue;
                }
                out.insert(key);
            }
        }
        Ok(())
    }

    fn files_for_commit(&self, id: Option<&str>) -> Result<BTreeMap<String, CommitFileState>> {
        id.map(|id| self.read_commit(id).map(|commit| commit.files))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    fn artifacts_for_commit(
        &self,
        id: Option<&str>,
    ) -> Result<BTreeMap<String, CommitArtifactState>> {
        id.map(|id| self.read_commit(id).map(|commit| commit.artifacts))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    fn checkout_plan_for_target(&self, target: Option<String>) -> Result<CheckoutPlan> {
        let files = self.files_for_commit(target.as_deref())?;
        let artifacts = self.artifacts_for_commit(target.as_deref())?;
        Ok(CheckoutPlan { target, files, artifacts })
    }

    fn stage_merge_conflict(
        &self,
        key: &str,
        base: Option<&CommitFileState>,
        ours: Option<&CommitFileState>,
        theirs: Option<&CommitFileState>,
        index: &mut index::Index,
    ) -> Result<()> {
        index.remove_path(key);
        for (stage, state) in [
            (index::IndexStage::Base, base),
            (index::IndexStage::Ours, ours),
            (index::IndexStage::Theirs, theirs),
        ] {
            if let Some(state) = state {
                index.stage(self.index_entry_for_state(key.to_string(), stage, state.clone())?);
            }
        }
        Ok(())
    }

    fn stage_merge_artifact_conflict(
        &self,
        key: &str,
        base: Option<&CommitArtifactState>,
        ours: Option<&CommitArtifactState>,
        theirs: Option<&CommitArtifactState>,
        index: &mut index::Index,
    ) {
        index.remove_path(key);
        for (stage, state) in [
            (index::IndexStage::Base, base),
            (index::IndexStage::Ours, ours),
            (index::IndexStage::Theirs, theirs),
        ] {
            if let Some(state) = state {
                index.stage(self.index_entry_for_artifact_state(
                    key.to_string(),
                    stage,
                    state.clone(),
                ));
            }
        }
    }

    fn write_index(&self, index: &index::Index) -> Result<()> {
        let path = self.index_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, toml::to_string_pretty(index)?)?;
        Ok(())
    }

    fn clear_index(&self) -> Result<()> {
        let path = self.index_path();
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    fn read_worktree_state(&self) -> Result<WorktreeState> {
        let path = self.worktree_state_path();
        if !path.exists() {
            return Ok(WorktreeState::default());
        }
        let raw = fs::read_to_string(path)?;
        let mut state: WorktreeState = toml::from_str(&raw)?;
        let dirty = state.dirty.into_iter().collect::<BTreeSet<_>>();
        state.dirty = dirty.into_iter().collect();
        let deleted = state.deleted.into_iter().collect::<BTreeSet<_>>();
        state.deleted = deleted.into_iter().collect();
        Ok(state)
    }

    fn write_worktree_state(&self, state: &WorktreeState) -> Result<()> {
        let path = self.worktree_state_path();
        if state.dirty.is_empty() && state.deleted.is_empty() {
            if path.exists() {
                fs::remove_file(path)?;
            }
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_file_atomic(&path, toml::to_string_pretty(state)?.as_bytes())
    }

    pub fn read_object(&self, id: &str) -> Result<object::Object> {
        let id = object::ObjectId::from_str(id)?;
        Ok(self.object_store().read(&id)?)
    }

    pub fn remote_store(&self, remote: &str) -> Result<crate::remote::Remote> {
        validate_remote_name(remote)?;
        let config = self
            .config()?
            .remotes
            .get(remote)
            .cloned()
            .ok_or_else(|| RepoErr::RemoteNotFound(remote.to_string()))?;
        Ok(config.build()?)
    }

    fn remote_branch_refs_from_store(
        &self,
        remote: &str,
        remote_store: &crate::remote::Remote,
    ) -> Result<Vec<RemoteBranchRef>> {
        validate_remote_name(remote)?;
        let prefix = "refs/heads/";
        let mut refs = BTreeMap::<String, String>::new();
        let mut paths = Vec::new();
        for path in block_on_remote(remote_store.list_raw(prefix))? {
            if path == prefix || path.ends_with('/') {
                continue;
            }
            let Some(branch) = path.strip_prefix(prefix) else {
                continue;
            };
            validate_ref_name(branch)?;
            let branch = branch.to_string();
            paths.push((path, branch));
        }

        let remote_refs = block_on_remote(async {
            stream::iter(paths)
                .map(|(path, branch)| async move {
                    let bytes = remote_store.get_raw(&path).await?;
                    Ok::<_, RemoteErr>((path, branch, bytes))
                })
                .buffer_unordered(REMOTE_REF_READ_CONCURRENCY)
                .try_collect::<Vec<_>>()
                .await
        })?;

        for (path, branch, bytes) in remote_refs {
            let Some(bytes) = bytes else {
                continue;
            };
            refs.insert(branch, parse_remote_ref(&path, bytes)?);
        }

        Ok(refs
            .into_iter()
            .map(|(branch, head)| RemoteBranchRef { remote: remote.to_string(), branch, head })
            .collect())
    }

    fn set_remote_head_if_absent(
        &self,
        remote_store: &crate::remote::Remote,
        branch: &str,
    ) -> Result<()> {
        if branch != self.default_branch()? {
            return Ok(());
        }

        match block_on_remote(
            remote_store.put_raw_if_not_exists(HEAD_FILE, Head::branch(branch).serialize()),
        ) {
            Ok(()) => Ok(()),
            Err(RepoErr::Remote(err)) if err.precondition_failed() => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn remote_object_ids(
        &self,
        remote: &crate::remote::Remote,
    ) -> Result<BTreeSet<object::ObjectId>> {
        let mut objects = BTreeSet::new();
        for path in block_on_remote(remote.list_raw(DIR_OBJECTS))? {
            if let Some(id) = remote_loose_object_id(&path)? {
                objects.insert(id);
            }
        }

        for index in fetch_remote_object_pack_indexes(remote)? {
            for entry in index.objects {
                objects.insert(entry.id);
            }
        }

        Ok(objects)
    }

    fn fetch_packed_object_bytes(
        &self,
        remote: &crate::remote::Remote,
        id: &object::ObjectId,
        pack_cache: &mut RemoteObjectPackCache,
    ) -> Result<Bytes> {
        let hit = pack_cache.indexes(remote)?.iter().find_map(|index| {
            index
                .objects
                .iter()
                .find(|entry| &entry.id == id)
                .map(|entry| (index.pack.clone(), entry.offset, entry.len))
        });
        let Some((pack, offset, len)) = hit else {
            return Err(RepoErr::InvalidRemoteObject {
                path: object::LooseObjectStore::relative_path(id),
                message: "missing object".to_string(),
            });
        };
        let end = offset
            .checked_add(len)
            .ok_or_else(|| RepoErr::InvalidRemoteObject {
                path: pack.clone(),
                message: format!("pack entry for object {id} overflows u64 range"),
            })?;
        let pack_bytes = pack_cache.pack_bytes(remote, &pack)?;
        let offset = usize::try_from(offset).map_err(|_| RepoErr::InvalidRemoteObject {
            path: pack.clone(),
            message: format!("pack entry for object {id} offset does not fit in usize"),
        })?;
        let end = usize::try_from(end).map_err(|_| RepoErr::InvalidRemoteObject {
            path: pack.clone(),
            message: format!("pack entry for object {id} end does not fit in usize"),
        })?;
        if end > pack_bytes.len() {
            return Err(RepoErr::InvalidRemoteObject {
                path: pack,
                message: format!("pack entry for object {id} extends past pack length"),
            });
        }
        Ok(pack_bytes.slice(offset..end))
    }

    fn fetch_commit_chain(&self, remote: &crate::remote::Remote, head: &str) -> Result<usize> {
        let mut count = 0;
        let mut stack = vec![head.to_string()];
        let mut seen = BTreeMap::<String, ()>::new();
        let mut pack_cache = RemoteObjectPackCache::default();
        while let Some(id) = stack.pop() {
            if seen.insert(id.clone(), ()).is_some() {
                continue;
            }

            let object_id = object::ObjectId::from_str(&id)?;
            let commit = match self.read_commit_object(&object_id)? {
                Some(commit) => commit,
                None => {
                    let object = self.fetch_remote_object(remote, &object_id, &mut pack_cache)?;
                    let object::Object::Commit(commit) = object else {
                        return Err(RepoErr::InvalidRemoteObject {
                            path: object::LooseObjectStore::relative_path(&object_id),
                            message: "expected commit object".to_string(),
                        });
                    };
                    count += 1;
                    commit
                }
            };

            self.fetch_object_graph(remote, &commit.tree, &mut pack_cache)?;
            for parent in commit.parents {
                stack.push(parent.to_string());
            }
        }
        Ok(count)
    }

    fn push_commit_chain(
        &self,
        remote: &crate::remote::Remote,
        head: &str,
        stop_at: Option<&str>,
    ) -> Result<usize> {
        let remote_objects = if stop_at.is_some() {
            BTreeSet::new()
        } else {
            self.remote_object_ids(remote)?
        };
        let stop_commits = stop_at
            .map(|id| self.commit_ancestors_inclusive(id))
            .transpose()?
            .unwrap_or_default();
        let mut commits = Vec::new();
        let mut objects = BTreeMap::<object::ObjectId, Vec<u8>>::new();
        let mut external_payloads = BTreeMap::<object::ObjectId, u64>::new();
        let mut stack = vec![head.to_string()];
        let mut seen = BTreeMap::<String, ()>::new();
        while let Some(id) = stack.pop() {
            if seen.insert(id.clone(), ()).is_some() {
                continue;
            }
            if stop_commits.contains(&id) {
                continue;
            }
            let object_id = object::ObjectId::from_str(&id)?;
            if remote_objects.contains(&object_id) {
                continue;
            }

            let Some(bytes) = self.object_store().read_raw(&object_id)? else {
                return Err(RepoErr::CommitNotFound(id.clone()));
            };
            let object = object::Object::decode(&bytes)?;
            let actual = object.id();
            if actual != object_id {
                return Err(RepoErr::Object(object::ObjectErr::ObjectIdMismatch {
                    expected: object_id,
                    actual,
                }));
            }
            let object::Object::Commit(commit) = object else {
                return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                    kind: "commit",
                    message: format!("object {id} is not a commit"),
                }));
            };
            let commit_id = object::ObjectId::from_str(&id)?;
            objects.insert(commit_id.clone(), bytes);
            self.collect_object_graph_for_pack(
                &commit.tree,
                &remote_objects,
                &mut objects,
                &mut external_payloads,
            )?;
            for parent in &commit.parents {
                stack.push(parent.to_string());
            }
            commits.push(commit_id);
        }

        let count = commits.len();
        self.push_large_file_contents(remote, external_payloads)?;
        self.push_object_pack(remote, objects)?;
        Ok(count)
    }

    fn commit_ancestors_inclusive(&self, head: &str) -> Result<BTreeSet<String>> {
        let mut ancestors = BTreeSet::new();
        let mut stack = vec![head.to_string()];
        while let Some(id) = stack.pop() {
            if !ancestors.insert(id.clone()) {
                continue;
            }
            let commit = match self.read_commit(&id) {
                Ok(commit) => commit,
                Err(RepoErr::CommitNotFound(_)) => continue,
                Err(err) => return Err(err),
            };
            for parent in commit_parent_ids(&commit) {
                stack.push(parent);
            }
        }
        Ok(ancestors)
    }

    fn collect_object_graph_for_pack(
        &self,
        id: &object::ObjectId,
        remote_objects: &BTreeSet<object::ObjectId>,
        objects: &mut BTreeMap<object::ObjectId, Vec<u8>>,
        external_payloads: &mut BTreeMap<object::ObjectId, u64>,
    ) -> Result<()> {
        if remote_objects.contains(id) || objects.contains_key(id) {
            return Ok(());
        }

        let Some(bytes) = self.object_store().read_raw(id)? else {
            return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "object",
                message: format!("missing local object {id}"),
            }));
        };
        let object = object::Object::decode(&bytes)?;
        let actual = object.id();
        if actual != *id {
            return Err(RepoErr::Object(object::ObjectErr::ObjectIdMismatch {
                expected: id.clone(),
                actual,
            }));
        }

        objects.insert(id.clone(), bytes);
        match object {
            object::Object::Commit(commit) => {
                self.collect_object_graph_for_pack(
                    &commit.tree,
                    remote_objects,
                    objects,
                    external_payloads,
                )?;
            }
            object::Object::Tree(tree) => {
                for entry in tree.entries {
                    self.collect_object_graph_for_pack(
                        &entry.oid,
                        remote_objects,
                        objects,
                        external_payloads,
                    )?;
                }
            }
            object::Object::Blob(object::BlobObject::LargeFilePointer(pointer)) => {
                match external_payloads.insert(pointer.content_hash, pointer.size) {
                    Some(size) if size != pointer.size => {
                        return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                            kind: "large-file-pointer",
                            message: "same content hash referenced with different sizes"
                                .to_string(),
                        }));
                    }
                    _ => {}
                }
            }
            object::Object::Blob(_) | object::Object::Tag(_) => {}
        }
        Ok(())
    }

    fn push_large_file_contents(
        &self,
        remote: &crate::remote::Remote,
        external_payloads: BTreeMap<object::ObjectId, u64>,
    ) -> Result<()> {
        for (id, size) in external_payloads {
            let bytes = self.read_large_file_content(&id, size)?;
            let path = large_file_content_relative_path(&id);
            match block_on_remote(remote.put_raw_if_not_exists(&path, bytes)) {
                Ok(()) => {}
                Err(RepoErr::Remote(err)) if err.precondition_failed() => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    fn push_object_pack(
        &self,
        remote: &crate::remote::Remote,
        objects: BTreeMap<object::ObjectId, Vec<u8>>,
    ) -> Result<()> {
        if objects.is_empty() {
            return Ok(());
        }

        let mut pack = REMOTE_OBJECT_PACK_MAGIC.to_vec();
        let mut entries = Vec::with_capacity(objects.len());
        for (id, bytes) in objects {
            let offset = pack.len() as u64;
            let len = bytes.len() as u64;
            pack.extend_from_slice(&bytes);
            entries.push(RemoteObjectPackEntry { id, offset, len });
        }

        let pack_id = blake3::hash(&pack).to_hex().to_string();
        let pack_path = format!("{DIR_OBJECTS_PACK}/{pack_id}.pack");
        let index_path = format!("{DIR_OBJECTS_PACK}/{pack_id}.idx");
        let index = RemoteObjectPackIndex {
            version: REMOTE_OBJECT_PACK_VERSION,
            pack: pack_path.clone(),
            objects: entries,
        };
        let index_bytes =
            serde_json::to_vec(&index).map_err(|err| RepoErr::InvalidRemoteObject {
                path: index_path.clone(),
                message: format!("failed to encode pack index: {err}"),
            })?;

        match block_on_remote(remote.put_raw_if_not_exists(&pack_path, pack)) {
            Ok(()) => {}
            Err(RepoErr::Remote(err)) if err.precondition_failed() => {}
            Err(err) => return Err(err),
        }
        match block_on_remote(remote.put_raw_if_not_exists(&index_path, index_bytes)) {
            Ok(()) => {}
            Err(RepoErr::Remote(err)) if err.precondition_failed() => {}
            Err(err) => return Err(err),
        }
        Ok(())
    }

    fn fetch_object_graph(
        &self,
        remote: &crate::remote::Remote,
        id: &object::ObjectId,
        pack_cache: &mut RemoteObjectPackCache,
    ) -> Result<()> {
        let object = match self.object_store().read_raw(id)? {
            Some(bytes) => {
                let object = object::Object::decode(&bytes)?;
                let actual = object.id();
                if actual != *id {
                    return Err(RepoErr::Object(object::ObjectErr::ObjectIdMismatch {
                        expected: id.clone(),
                        actual,
                    }));
                }
                object
            }
            None => self.fetch_remote_object(remote, id, pack_cache)?,
        };

        match object {
            object::Object::Commit(commit) => {
                self.fetch_object_graph(remote, &commit.tree, pack_cache)?;
                for parent in commit.parents {
                    self.fetch_object_graph(remote, &parent, pack_cache)?;
                }
            }
            object::Object::Tree(tree) => {
                for entry in tree.entries {
                    self.fetch_object_graph(remote, &entry.oid, pack_cache)?;
                }
            }
            object::Object::Blob(object::BlobObject::LargeFilePointer(pointer)) => {
                self.fetch_large_file_content(remote, &pointer.content_hash, pointer.size)?;
            }
            object::Object::Blob(_) | object::Object::Tag(_) => {}
        }
        Ok(())
    }

    fn fetch_remote_object(
        &self,
        remote: &crate::remote::Remote,
        id: &object::ObjectId,
        pack_cache: &mut RemoteObjectPackCache,
    ) -> Result<object::Object> {
        let path = object::LooseObjectStore::relative_path(id);
        let bytes = match block_on_remote(remote.get_raw(&path))? {
            Some(bytes) => bytes,
            None => self.fetch_packed_object_bytes(remote, id, pack_cache)?,
        };
        Ok(self.object_store().write_raw_validated(id, &bytes)?)
    }

    fn fetch_large_file_content(
        &self,
        remote: &crate::remote::Remote,
        id: &object::ObjectId,
        size: u64,
    ) -> Result<()> {
        if self.large_file_content_path(id).exists() {
            self.read_large_file_content(id, size)?;
            return Ok(());
        }

        let path = large_file_content_relative_path(id);
        let bytes = block_on_remote(remote.get_raw(&path))?.ok_or_else(|| {
            RepoErr::InvalidRemoteObject {
                path: path.clone(),
                message: format!("missing external payload {id}"),
            }
        })?;
        let bytes = bytes.to_vec();
        validate_large_file_content(id, size, &bytes)?;
        self.write_large_file_content(id, &bytes)?;
        Ok(())
    }

    fn repair_artifact_state_from_remote(
        &self,
        remote: &crate::remote::Remote,
        state: &CommitArtifactState,
        pack_cache: &mut RemoteObjectPackCache,
        fetched_objects: &mut BTreeSet<object::ObjectId>,
        fetched_external_payloads: &mut BTreeSet<object::ObjectId>,
    ) -> Result<()> {
        let oid = state.oid();
        let missing_object = match self.object_store().read(oid) {
            Ok(_) => false,
            Err(object::ObjectErr::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
        };
        if missing_object {
            let object = self.fetch_remote_object(remote, oid, pack_cache)?;
            validate_artifact_object_matches_state(state, &object)?;
            fetched_objects.insert(oid.clone());
        }

        if let CommitArtifactState::LargeFile { content_hash, size, .. } = state
            && !self.large_file_content_path(content_hash).exists()
        {
            self.fetch_large_file_content(remote, content_hash, *size)?;
            fetched_external_payloads.insert(content_hash.clone());
        }

        Ok(())
    }

    fn referenced_large_file_payloads(&self) -> Result<BTreeSet<object::ObjectId>> {
        let mut referenced = BTreeSet::new();
        for entry in self.read_index()?.entries {
            if let Some(artifact) = entry.artifact {
                collect_large_file_payload_from_artifact(&artifact, &mut referenced);
            }
        }

        let mut starts = BTreeSet::new();
        if let Some(head) = self.head_target()? {
            starts.insert(head);
        }
        for branch in self.branches()? {
            if let Some(target) = branch.target {
                starts.insert(target);
            }
        }
        for branch in self.remote_tracking_branches()? {
            starts.insert(branch.head);
        }
        for tag in self.tags()? {
            starts.insert(tag.target);
        }

        let mut seen = BTreeSet::new();
        for start in starts {
            self.collect_reachable_large_file_payloads(&start, &mut seen, &mut referenced)?;
        }
        Ok(referenced)
    }

    fn collect_reachable_large_file_payloads(
        &self,
        start: &str,
        seen: &mut BTreeSet<String>,
        referenced: &mut BTreeSet<object::ObjectId>,
    ) -> Result<()> {
        let mut stack = vec![start.to_string()];
        while let Some(id) = stack.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            let commit = self.read_commit(&id)?;
            for artifact in commit.artifacts.values() {
                collect_large_file_payload_from_artifact(artifact, referenced);
            }
            for parent in commit_parent_ids(&commit) {
                stack.push(parent);
            }
        }
        Ok(())
    }

    fn local_large_file_payloads(&self) -> Result<Vec<RepoLargeFilePruneEntry>> {
        let root = self.file_store_dir();
        if !root.exists() {
            return Ok(Vec::new());
        }

        let mut files = Vec::new();
        for dir in fs::read_dir(&root)? {
            let dir = dir?;
            if !dir.file_type()?.is_dir() {
                continue;
            }
            let fanout = dir.file_name().to_string_lossy().into_owned();
            if fanout.len() != 2 || !fanout.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }

            for file in fs::read_dir(dir.path())? {
                let file = file?;
                if !file.file_type()?.is_file() {
                    continue;
                }
                let suffix = file.file_name().to_string_lossy().into_owned();
                if suffix.len() != 62 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    continue;
                }
                let content_hash = object::ObjectId::from_str(&format!("{fanout}{suffix}"))?;
                let size = file.metadata()?.len();
                let path = large_file_content_relative_path(&content_hash);
                files.push(RepoLargeFilePruneEntry { content_hash, size, path });
            }
        }
        Ok(files)
    }

    fn write_tree_object(
        &self,
        object_store: &object::LooseObjectStore,
        files: &BTreeMap<String, CommitFileState>,
        artifacts: &BTreeMap<String, CommitArtifactState>,
    ) -> Result<object::ObjectId> {
        let mut entries = Vec::with_capacity(files.len() + artifacts.len());
        for (path, state) in files {
            let blob = object::Object::Blob(object::BlobObject::SqliteSnapshot(
                sqlite_snapshot_blob(state),
            ));
            let oid = object_store.write(&blob)?;
            entries.push(object::TreeEntry {
                mode: object::TreeEntryMode::SqliteDatabase,
                oid,
                path: path.clone(),
            });
        }
        for (path, state) in artifacts {
            entries.push(object::TreeEntry {
                mode: object::TreeEntryMode::Regular,
                oid: state.oid().clone(),
                path: path.clone(),
            });
        }
        let tree = object::TreeObject::new(entries)?;
        Ok(object_store.write(&object::Object::Tree(tree))?)
    }

    fn canonical_commit_object(
        &self,
        tree: object::ObjectId,
        parents: &[String],
        message: &str,
        timestamp_ms: u64,
        tables: Vec<CommitTableSummary>,
    ) -> Result<object::CommitObject> {
        let parents = parents
            .iter()
            .map(|parent| object::ObjectId::from_str(parent))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let signature =
            object::Signature::new("Graft", "graft@example.invalid", timestamp_ms, "+0000");
        Ok(object::CommitObject {
            tree,
            parents,
            author: signature.clone(),
            committer: signature,
            repo_format_version: REPOSITORY_FORMAT_VERSION,
            tables,
            message: message.to_string(),
        })
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let mut stack = vec![descendant.to_string()];
        let mut seen = BTreeMap::<String, ()>::new();
        while let Some(id) = stack.pop() {
            if seen.insert(id.clone(), ()).is_some() {
                continue;
            }
            if id == ancestor {
                return Ok(true);
            }
            for parent in commit_parent_ids(&self.read_commit(&id)?) {
                stack.push(parent);
            }
        }
        Ok(false)
    }

    fn merge_base(&self, left: &str, right: &str) -> Result<Option<String>> {
        let mut left_ancestors = BTreeMap::<String, ()>::new();
        let mut stack = vec![left.to_string()];
        while let Some(id) = stack.pop() {
            if left_ancestors.insert(id.clone(), ()).is_some() {
                continue;
            }
            for parent in commit_parent_ids(&self.read_commit(&id)?) {
                stack.push(parent);
            }
        }

        let mut stack = vec![right.to_string()];
        let mut seen = BTreeMap::<String, ()>::new();
        while let Some(id) = stack.pop() {
            if seen.insert(id.clone(), ()).is_some() {
                continue;
            }
            if left_ancestors.contains_key(&id) {
                return Ok(Some(id));
            }
            for parent in commit_parent_ids(&self.read_commit(&id)?) {
                stack.push(parent);
            }
        }

        Ok(None)
    }

    fn head_files(&self) -> Result<BTreeMap<String, CommitFileState>> {
        Ok(self
            .head_target()?
            .map(|commit| self.read_commit(&commit))
            .transpose()?
            .map(|commit| commit.files)
            .unwrap_or_default())
    }

    fn head_artifacts(&self) -> Result<BTreeMap<String, CommitArtifactState>> {
        Ok(self
            .head_target()?
            .map(|commit| self.read_commit(&commit))
            .transpose()?
            .map(|commit| commit.artifacts)
            .unwrap_or_default())
    }

    fn read_branch_ref(&self, name: &str) -> Result<Option<String>> {
        self.read_ref(&format!("refs/heads/{name}"))
    }

    fn branch_info(&self, name: &str) -> Result<BranchInfo> {
        self.ensure_local_branch_for_config(name)?;
        let current = self.current_branch()?.as_deref() == Some(name);
        Ok(BranchInfo {
            name: name.to_string(),
            target: self.read_branch_ref(name)?,
            current,
            upstream: self.branch_upstream(name)?,
        })
    }

    fn resolve_revision_base(&self, rev: &str) -> Result<String> {
        match rev {
            "HEAD" | "@" => return self.head_target()?.ok_or(RepoErr::UnbornHead),
            _ => {}
        }

        if let Some(target) = self.resolve_refish(rev)? {
            return Ok(target);
        }

        self.resolve_commit_prefix(rev)
    }

    fn apply_revision_op(&self, id: &str, op: RevisionOp, rev: &str) -> Result<String> {
        match op {
            RevisionOp::FirstParent(ancestors) => {
                let mut id = id.to_string();
                for _ in 0..ancestors {
                    let parents = commit_parent_ids(&self.read_commit(&id)?);
                    id = parents
                        .into_iter()
                        .next()
                        .ok_or_else(|| RepoErr::UnknownRevision(rev.to_string()))?;
                }
                Ok(id)
            }
            RevisionOp::Parent(parent) => {
                if parent == 0 {
                    return Ok(id.to_string());
                }
                let parents = commit_parent_ids(&self.read_commit(id)?);
                parents
                    .get(parent - 1)
                    .cloned()
                    .ok_or_else(|| RepoErr::UnknownRevision(rev.to_string()))
            }
        }
    }

    fn resolve_refish(&self, rev: &str) -> Result<Option<String>> {
        if rev.starts_with("refs/") {
            return self
                .read_ref(rev)?
                .map(|target| {
                    if rev.starts_with("refs/tags/") {
                        self.peel_object_to_commit(&target, rev)
                    } else {
                        Ok(target)
                    }
                })
                .transpose();
        }

        if let Some(target) = self.read_ref(&format!("refs/heads/{rev}"))? {
            return Ok(Some(target));
        }

        if let Some(target) = self.read_ref(&format!("refs/tags/{rev}"))? {
            return Ok(Some(self.peel_object_to_commit(&target, rev)?));
        }

        if let Some((remote, branch)) = rev.split_once('/')
            && validate_remote_name(remote).is_ok()
            && validate_ref_name(branch).is_ok()
            && let Some(target) = self.read_ref(&format!("refs/remotes/{remote}/{branch}"))?
        {
            return Ok(Some(target));
        }

        Ok(None)
    }

    fn resolve_commit_prefix(&self, rev: &str) -> Result<String> {
        if rev.len() < 4 || rev.len() > 64 || !rev.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(RepoErr::UnknownRevision(rev.to_string()));
        }

        if rev.len() == 64 {
            let id = object::ObjectId::from_str(rev)?;
            return self.peel_object_id_to_commit(&id, rev);
        }

        let mut matches = self.commitish_object_ids_with_prefix(rev)?;

        match matches.len() {
            0 => Err(RepoErr::UnknownRevision(rev.to_string())),
            1 => {
                let id = object::ObjectId::from_str(&matches.pop().expect("one match"))?;
                self.peel_object_id_to_commit(&id, rev)
            }
            _ => Err(RepoErr::AmbiguousRevision(rev.to_string())),
        }
    }

    fn commitish_object_ids_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let mut matches = Vec::new();
        let root = self.object_store().root().to_path_buf();
        if !root.exists() {
            return Ok(matches);
        }

        for dir in fs::read_dir(root)? {
            let dir = dir?;
            if !dir.file_type()?.is_dir() {
                continue;
            }
            let fanout = dir.file_name().to_string_lossy().into_owned();
            if fanout.len() != 2 || !fanout.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }

            for file in fs::read_dir(dir.path())? {
                let file = file?;
                if !file.file_type()?.is_file() {
                    continue;
                }
                let suffix = file.file_name().to_string_lossy().into_owned();
                if suffix.len() != 62 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    continue;
                }

                let id = format!("{fanout}{suffix}");
                if !id.starts_with(prefix) {
                    continue;
                }

                let object_id = object::ObjectId::from_str(&id)?;
                let Some(bytes) = self.object_store().read_raw(&object_id)? else {
                    continue;
                };
                let object = object::Object::decode(&bytes)?;
                let actual = object.id();
                if actual != object_id {
                    return Err(RepoErr::Object(object::ObjectErr::ObjectIdMismatch {
                        expected: object_id,
                        actual,
                    }));
                }
                if matches!(object, object::Object::Commit(_) | object::Object::Tag(_)) {
                    matches.push(id);
                }
            }
        }

        matches.sort();
        Ok(matches)
    }

    fn peel_object_to_commit(&self, id: &str, rev: &str) -> Result<String> {
        let id = object::ObjectId::from_str(id)?;
        self.peel_object_id_to_commit(&id, rev)
    }

    fn peel_object_id_to_commit(&self, id: &object::ObjectId, rev: &str) -> Result<String> {
        let mut current = id.clone();
        let mut seen = BTreeMap::<String, ()>::new();

        loop {
            if seen.insert(current.to_string(), ()).is_some() {
                return Err(RepoErr::UnknownRevision(rev.to_string()));
            }

            let Some(bytes) = self.object_store().read_raw(&current)? else {
                return Err(RepoErr::UnknownRevision(rev.to_string()));
            };
            let object = object::Object::decode(&bytes)?;
            let actual = object.id();
            if actual != current {
                return Err(RepoErr::Object(object::ObjectErr::ObjectIdMismatch {
                    expected: current,
                    actual,
                }));
            }

            match object {
                object::Object::Commit(_) => return Ok(current.to_string()),
                object::Object::Tag(tag) => {
                    if !matches!(
                        tag.object_type,
                        object::ObjectKind::Commit | object::ObjectKind::Tag
                    ) {
                        return Err(RepoErr::UnknownRevision(rev.to_string()));
                    }
                    current = tag.object;
                }
                _ => return Err(RepoErr::UnknownRevision(rev.to_string())),
            }
        }
    }

    fn branch_exists(&self, name: &str) -> bool {
        self.graft_dir.join(DIR_REFS_HEADS).join(name).is_file()
    }

    fn ensure_local_branch_for_config(&self, name: &str) -> Result<()> {
        validate_ref_name(name)?;
        if self.branch_exists(name) || self.current_branch()?.as_deref() == Some(name) {
            Ok(())
        } else {
            Err(RepoErr::BranchNotFound(name.to_string()))
        }
    }

    fn tag_exists(&self, name: &str) -> bool {
        self.graft_dir.join(DIR_REFS_TAGS).join(name).is_file()
    }

    fn write_branch_ref(&self, name: &str, target: &str, message: &str) -> Result<()> {
        self.write_ref_update(&format!("refs/heads/{name}"), target, message)
    }

    fn read_tag_ref(&self, name: &str) -> Result<Option<String>> {
        validate_ref_name(name)?;
        self.read_ref(&format!("refs/tags/{name}"))
    }

    fn tag_info_from_ref(&self, name: String, object: String) -> Result<TagInfo> {
        let object_id = object::ObjectId::from_str(&object)?;
        match self.object_store().read(&object_id)? {
            object::Object::Commit(_) => Ok(TagInfo {
                name,
                object: object.clone(),
                target: object,
                annotated: false,
                message: None,
            }),
            object::Object::Tag(tag) => {
                let target = self.peel_object_id_to_commit(&tag.object, &name)?;
                Ok(TagInfo {
                    name,
                    object,
                    target,
                    annotated: true,
                    message: Some(tag.message),
                })
            }
            object => Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "tag",
                message: format!("tag ref `{name}` points at a {}", object.kind()),
            })),
        }
    }

    fn write_tag_ref(&self, name: &str, target: &str, message: &str) -> Result<()> {
        self.write_ref_update(&format!("refs/tags/{name}"), target, message)
    }

    fn delete_tag_ref(&self, name: &str) -> Result<()> {
        validate_ref_name(name)?;
        let path = self.graft_dir.join(DIR_REFS_TAGS).join(name);
        if !path.is_file() {
            return Err(RepoErr::TagNotFound(name.to_string()));
        }
        fs::remove_file(&path)?;
        remove_empty_parent_dirs(path.parent(), &self.graft_dir.join(DIR_REFS_TAGS))?;
        Ok(())
    }

    fn read_ref(&self, reference: &str) -> Result<Option<String>> {
        validate_full_ref(reference)?;
        let path = self.graft_dir.join(reference);
        if !path.exists() {
            return Ok(None);
        }
        if !path.is_file() {
            return Err(RepoErr::BranchNotFound(reference.to_string()));
        }

        let raw = fs::read_to_string(path)?;
        let target = raw.trim();
        if target.is_empty() {
            Ok(None)
        } else {
            Ok(Some(target.to_string()))
        }
    }

    fn write_ref_update(&self, reference: &str, target: &str, message: &str) -> Result<()> {
        validate_full_ref(reference)?;
        self.ensure_ref_namespace_available(reference)?;
        let old = self.read_ref(reference)?;
        self.write_ref(reference, target)?;
        self.append_ref_reflog(reference, old.as_deref(), Some(target), message)?;
        Ok(())
    }

    fn write_ref(&self, reference: &str, target: &str) -> Result<()> {
        validate_full_ref(reference)?;
        self.ensure_ref_namespace_available(reference)?;
        let path = self.graft_dir.join(reference);
        write_file_atomic(&path, format!("{target}\n").as_bytes())?;
        Ok(())
    }

    fn ensure_ref_namespace_available(&self, reference: &str) -> Result<()> {
        validate_full_ref(reference)?;
        let path = self.graft_dir.join(reference);
        if path.is_dir() {
            return Err(RepoErr::RefNameConflict {
                reference: reference.to_string(),
                existing: reference.to_string(),
            });
        }

        let mut current = path.parent();
        while let Some(parent) = current {
            if parent == self.graft_dir {
                break;
            }
            if parent.is_file() {
                let existing = parent.strip_prefix(&self.graft_dir).map_or_else(
                    |_| parent.display().to_string(),
                    |path| path.to_string_lossy().replace('\\', "/"),
                );
                return Err(RepoErr::RefNameConflict {
                    reference: reference.to_string(),
                    existing,
                });
            }
            current = parent.parent();
        }

        Ok(())
    }

    fn ensure_path_namespace_available_for_rename(
        root: &Path,
        old_reference: &str,
        new_reference: &str,
    ) -> Result<()> {
        validate_full_ref(old_reference)?;
        validate_full_ref(new_reference)?;

        let old_path = root.join(old_reference);
        let new_path = root.join(new_reference);
        if new_path.is_dir() && !path_tree_contains_only_file(&new_path, &old_path)? {
            return Err(RepoErr::RefNameConflict {
                reference: new_reference.to_string(),
                existing: new_reference.to_string(),
            });
        }

        let mut current = new_path.parent();
        while let Some(parent) = current {
            if parent == root {
                break;
            }
            if parent.is_file() && parent != old_path {
                let existing = parent.strip_prefix(root).map_or_else(
                    |_| parent.display().to_string(),
                    |path| path.to_string_lossy().replace('\\', "/"),
                );
                return Err(RepoErr::RefNameConflict {
                    reference: new_reference.to_string(),
                    existing,
                });
            }
            current = parent.parent();
        }

        Ok(())
    }

    fn delete_ref(&self, reference: &str) -> Result<()> {
        validate_full_ref(reference)?;
        let path = self.graft_dir.join(reference);
        if !path.is_file() {
            return Err(RepoErr::BranchNotFound(reference.to_string()));
        }
        fs::remove_file(&path)?;
        remove_empty_parent_dirs(path.parent(), &self.graft_dir.join(DIR_REFS_HEADS))?;
        Ok(())
    }

    fn delete_ref_if_exists(&self, reference: &str) -> Result<()> {
        validate_full_ref(reference)?;
        let path = self.graft_dir.join(reference);
        if path.is_file() {
            fs::remove_file(&path)?;
            remove_empty_parent_dirs(path.parent(), &self.graft_dir.join("refs"))?;
        }
        Ok(())
    }

    fn collect_ref_files(
        dir: &Path,
        prefix: &str,
        out: &mut BTreeMap<String, Option<String>>,
    ) -> Result<()> {
        if !dir.exists() {
            return Ok(());
        }

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let name = if prefix.is_empty() {
                file_name
            } else {
                format!("{prefix}/{file_name}")
            };

            if entry.file_type()?.is_dir() {
                Self::collect_ref_files(&entry.path(), &name, out)?;
            } else {
                let raw = fs::read_to_string(entry.path())?;
                let target = raw.trim();
                out.insert(
                    name,
                    if target.is_empty() {
                        None
                    } else {
                        Some(target.to_string())
                    },
                );
            }
        }

        Ok(())
    }

    fn delete_ref_log(&self, reference: &str) -> Result<()> {
        validate_full_ref(reference)?;
        let path = self.graft_dir.join(DIR_LOGS_REFS).join(reference);
        if path.is_file() {
            fs::remove_file(&path)?;
            remove_empty_parent_dirs(path.parent(), &self.graft_dir.join(DIR_LOGS_REFS))?;
        }
        Ok(())
    }

    fn move_ref_log_for_rename(&self, old_reference: &str, new_reference: &str) -> Result<()> {
        validate_full_ref(old_reference)?;
        validate_full_ref(new_reference)?;

        let root = self.graft_dir.join(DIR_LOGS_REFS);
        let old_path = root.join(old_reference);
        if !old_path.is_file() {
            return Ok(());
        }

        let bytes = fs::read(&old_path)?;
        fs::remove_file(&old_path)?;
        remove_empty_parent_dirs(old_path.parent(), &root)?;

        let new_path = root.join(new_reference);
        write_file_atomic(&new_path, &bytes)?;
        Ok(())
    }

    fn append_head_reflog(
        &self,
        old: Option<&str>,
        new: Option<&str>,
        message: &str,
    ) -> Result<()> {
        fs::create_dir_all(self.graft_dir.join(DIR_LOGS_HEAD))?;
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.graft_dir.join(DIR_LOGS_HEAD).join("HEAD"))?
            .write_all(reflog_line(old, new, message).as_bytes())?;
        Ok(())
    }

    fn append_ref_reflog(
        &self,
        reference: &str,
        old: Option<&str>,
        new: Option<&str>,
        message: &str,
    ) -> Result<()> {
        validate_full_ref(reference)?;
        let path = self.graft_dir.join(DIR_LOGS_REFS).join(reference);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(reflog_line(old, new, message).as_bytes())?;
        Ok(())
    }
}

impl Display for Head {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Head::Branch { name } => write!(f, "refs/heads/{name}"),
            Head::Detached { commit } => write!(f, "{commit}"),
        }
    }
}

fn branch_upstream_from_config(
    config: &RepoConfig,
    branch: &str,
) -> Result<Option<BranchUpstream>> {
    validate_ref_name(branch)?;
    let Some(branch_config) = config.branches.get(branch) else {
        return Ok(None);
    };
    let Some(remote) = &branch_config.remote else {
        return Ok(None);
    };
    let Some(merge) = &branch_config.merge else {
        return Ok(None);
    };

    validate_remote_name(remote)?;
    let branch = branch_from_merge_ref(merge)?;
    Ok(Some(BranchUpstream { remote: remote.clone(), branch }))
}

fn branch_merge_ref(branch: &str) -> String {
    format!("refs/heads/{branch}")
}

fn branch_from_merge_ref(merge: &str) -> Result<String> {
    let branch = merge.strip_prefix("refs/heads/").unwrap_or(merge);
    validate_ref_name(branch)?;
    Ok(branch.to_string())
}

fn parse_fetch_refspec(remote: &str, refspec: &str) -> Result<ParsedRefspec> {
    let parsed = parse_refspec(refspec, RefspecSide::FetchSource, |dst| {
        parse_fetch_destination(remote, dst)
    })?;
    if parsed.source.is_none() {
        return invalid_refspec(refspec, "fetch refspecs require a source");
    }
    validate_refspec_shape(refspec, &parsed)?;
    Ok(parsed)
}

fn parse_push_refspec(refspec: &str) -> Result<ParsedRefspec> {
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn normalize_commit_table_summary(tables: Vec<CommitTableSummary>) -> Vec<CommitTableSummary> {
    let mut by_name = BTreeMap::<String, CommitTableSummary>::new();
    for table in tables {
        if table.name.is_empty() || table.inserts + table.deletes + table.updates == 0 {
            continue;
        }
        by_name
            .entry(table.name.clone())
            .and_modify(|entry| {
                entry.inserts += table.inserts;
                entry.deletes += table.deletes;
                entry.updates += table.updates;
            })
            .or_insert(table);
    }
    by_name.into_values().collect()
}

fn write_file_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        for attempt in 0..100 {
            let tmp = parent.join(format!(
                ".graft-tmp-{}-{}-{attempt}",
                now_ms(),
                std::process::id()
            ));
            if tmp.exists() {
                continue;
            }
            fs::write(&tmp, bytes)?;
            return match fs::rename(&tmp, path) {
                Ok(()) => Ok(()),
                Err(err) => {
                    let _ = fs::remove_file(&tmp);
                    Err(err.into())
                }
            };
        }
    }
    fs::write(path, bytes)?;
    Ok(())
}

fn reflog_line(old: Option<&str>, new: Option<&str>, message: &str) -> String {
    format!(
        "{} {} {} {} +0000\t{}\n",
        reflog_value(old),
        reflog_value(new),
        REFLOG_ACTOR,
        now_ms(),
        sanitize_reflog_message(message)
    )
}

fn reflog_value(value: Option<&str>) -> &str {
    match value {
        Some(value) if !value.is_empty() => value,
        _ => NULL_OBJECT_ID,
    }
}

fn sanitize_reflog_message(message: &str) -> String {
    message
        .chars()
        .map(|ch| match ch {
            '\n' | '\r' | '\t' => ' ',
            ch => ch,
        })
        .collect()
}

fn sqlite_snapshot_blob(state: &CommitFileState) -> object::SqliteSnapshotBlob {
    object::SqliteSnapshotBlob {
        volume: state.volume.clone(),
        page_count: state.snapshot.page_count,
        ranges: state
            .snapshot
            .ranges
            .iter()
            .map(|range| object::SqliteSnapshotRange {
                log: range.log.clone(),
                start: range.start,
                end: range.end,
                commits: range
                    .commits
                    .iter()
                    .map(|commit| object::SqliteSnapshotCommit {
                        lsn: commit.lsn,
                        commit_hash: commit.commit_hash.clone(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn file_state_from_sqlite_snapshot_blob(blob: object::SqliteSnapshotBlob) -> CommitFileState {
    CommitFileState {
        volume: blob.volume,
        snapshot: RepoSnapshot {
            page_count: blob.page_count,
            ranges: blob
                .ranges
                .into_iter()
                .map(|range| RepoLogRange {
                    log: range.log,
                    start: range.start,
                    end: range.end,
                    commits: range
                        .commits
                        .into_iter()
                        .map(|commit| RepoStorageCommit {
                            lsn: commit.lsn,
                            commit_hash: commit.commit_hash,
                        })
                        .collect(),
                })
                .collect(),
        },
    }
}

fn artifact_state_from_blob(
    oid: object::ObjectId,
    blob: object::BlobObject,
) -> Result<CommitArtifactState> {
    match blob {
        object::BlobObject::File(blob) => {
            let content_hash = object::ObjectId::for_bytes(&blob.bytes);
            Ok(CommitArtifactState::File {
                kind: repo_path_kind_from_object_kind(blob.kind),
                oid,
                content_hash,
                size: blob.bytes.len() as u64,
            })
        }
        object::BlobObject::LargeFilePointer(blob) => Ok(CommitArtifactState::LargeFile {
            kind: repo_path_kind_from_object_kind(blob.kind),
            oid,
            content_hash: blob.content_hash,
            size: blob.size,
        }),
        object::BlobObject::SqliteSnapshot(_) => {
            Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "blob",
                message: "sqlite snapshot cannot be used as a regular artifact".to_string(),
            }))
        }
    }
}

fn validate_artifact_object_matches_state(
    state: &CommitArtifactState,
    object: &object::Object,
) -> Result<()> {
    match (state, object) {
        (
            CommitArtifactState::File { kind, content_hash, size, .. },
            object::Object::Blob(object::BlobObject::File(blob)),
        ) => {
            let actual_hash = object::ObjectId::for_bytes(&blob.bytes);
            if repo_path_kind_from_object_kind(blob.kind) == *kind
                && &actual_hash == content_hash
                && blob.bytes.len() as u64 == *size
            {
                Ok(())
            } else {
                Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                    kind: "blob",
                    message: format!(
                        "file blob metadata mismatch: expected kind {kind}, {size} byte(s), and hash {content_hash}, got kind {}, {} byte(s), and hash {actual_hash}",
                        repo_path_kind_from_object_kind(blob.kind),
                        blob.bytes.len()
                    ),
                }))
            }
        }
        (
            CommitArtifactState::LargeFile { kind, content_hash, size, .. },
            object::Object::Blob(object::BlobObject::LargeFilePointer(pointer)),
        ) => {
            if repo_path_kind_from_object_kind(pointer.kind) == *kind
                && &pointer.content_hash == content_hash
                && pointer.size == *size
            {
                Ok(())
            } else {
                Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                    kind: "blob",
                    message: format!(
                        "large file pointer mismatch: expected kind {kind}, {size} byte(s), and hash {content_hash}, got kind {}, {} byte(s), and hash {}",
                        repo_path_kind_from_object_kind(pointer.kind),
                        pointer.size,
                        pointer.content_hash
                    ),
                }))
            }
        }
        (CommitArtifactState::File { .. }, _) => {
            Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "blob",
                message: "artifact object is not a file blob".to_string(),
            }))
        }
        (CommitArtifactState::LargeFile { .. }, _) => {
            Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "blob",
                message: "artifact object is not a large file pointer".to_string(),
            }))
        }
    }
}

fn collect_large_file_payload_from_artifact(
    state: &CommitArtifactState,
    out: &mut BTreeSet<object::ObjectId>,
) {
    if let CommitArtifactState::LargeFile { content_hash, .. } = state {
        out.insert(content_hash.clone());
    }
}

impl Repository {
    fn write_artifact_state_from_path(
        &self,
        key: &str,
        path: &Path,
    ) -> Result<CommitArtifactState> {
        self.write_artifact_state_from_path_with_file_config(key, path, &self.file_config()?)
    }

    fn write_artifact_state_from_path_with_file_config(
        &self,
        key: &str,
        path: &Path,
        config: &FileConfig,
    ) -> Result<CommitArtifactState> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() {
            return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "file",
                message: format!("path `{}` is not a regular file", path.display()),
            }));
        }

        let bytes = fs::read(path)?;
        let size = bytes.len() as u64;
        let kind = classify_artifact_bytes(&bytes);
        let object_kind = object_kind_from_repo_path_kind(kind);
        let content_hash = object::ObjectId::for_bytes(&bytes);
        if artifact_storage_for_path(key, kind, size, config) == RepoPathStorage::External {
            self.write_large_file_content(&content_hash, &bytes)?;
            let pointer = object::LargeFilePointerBlob {
                kind: object_kind,
                content_hash: content_hash.clone(),
                size,
            };
            let oid = self.object_store().write(&object::Object::Blob(
                object::BlobObject::LargeFilePointer(pointer),
            ))?;
            Ok(CommitArtifactState::LargeFile { kind, oid, content_hash, size })
        } else {
            let oid =
                self.object_store()
                    .write(&object::Object::Blob(object::BlobObject::File(
                        object::FileBlob { kind: object_kind, bytes },
                    )))?;
            Ok(CommitArtifactState::File { kind, oid, content_hash, size })
        }
    }

    fn write_large_file_content(&self, id: &object::ObjectId, bytes: &[u8]) -> Result<()> {
        let path = self.large_file_content_path(id);
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_file_atomic(&path, bytes)?;
        Ok(())
    }

    fn large_file_content_path(&self, id: &object::ObjectId) -> PathBuf {
        self.graft_dir.join(large_file_content_relative_path(id))
    }

    fn read_large_file_content(&self, id: &object::ObjectId, size: u64) -> Result<Vec<u8>> {
        let bytes = fs::read(self.large_file_content_path(id))?;
        validate_large_file_content(id, size, &bytes)?;
        Ok(bytes)
    }

    fn artifact_bytes(&self, state: &CommitArtifactState) -> Result<Vec<u8>> {
        match state {
            CommitArtifactState::File { oid, .. } => {
                let object = self.object_store().read(oid)?;
                let object::Object::Blob(object::BlobObject::File(blob)) = object else {
                    return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                        kind: "blob",
                        message: format!("artifact object {oid} is not a file blob"),
                    }));
                };
                Ok(blob.bytes)
            }
            CommitArtifactState::LargeFile { content_hash, size, .. } => {
                self.read_large_file_content(content_hash, *size)
            }
        }
    }

    fn audit_artifact_state(
        &self,
        path: &str,
        state: &CommitArtifactState,
        audit: &mut RepoArtifactAudit,
    ) {
        match state {
            CommitArtifactState::File { kind, oid, content_hash, size } => {
                match self.object_store().read(oid) {
                    Ok(object::Object::Blob(object::BlobObject::File(blob))) => {
                        let actual_hash = object::ObjectId::for_bytes(&blob.bytes);
                        let actual_kind = repo_path_kind_from_object_kind(blob.kind);
                        if actual_kind != *kind
                            || &actual_hash != content_hash
                            || blob.bytes.len() as u64 != *size
                        {
                            audit.issues.push(RepoArtifactAuditIssue {
                                path: path.to_string(),
                                kind: RepoArtifactAuditIssueKind::InvalidObject,
                                oid: Some(oid.clone()),
                                content_hash: Some(content_hash.clone()),
                                message: format!(
                                    "file blob metadata mismatch: expected kind {kind}, {size} byte(s), and hash {content_hash}, got kind {actual_kind}, {} byte(s), and hash {actual_hash}",
                                    blob.bytes.len()
                                ),
                            });
                        }
                    }
                    Ok(_) => audit.issues.push(RepoArtifactAuditIssue {
                        path: path.to_string(),
                        kind: RepoArtifactAuditIssueKind::InvalidObject,
                        oid: Some(oid.clone()),
                        content_hash: Some(content_hash.clone()),
                        message: "artifact object is not a file blob".to_string(),
                    }),
                    Err(object::ObjectErr::Io(err))
                        if err.kind() == std::io::ErrorKind::NotFound =>
                    {
                        audit.issues.push(RepoArtifactAuditIssue {
                            path: path.to_string(),
                            kind: RepoArtifactAuditIssueKind::MissingObject,
                            oid: Some(oid.clone()),
                            content_hash: Some(content_hash.clone()),
                            message: format!("missing artifact object {oid}"),
                        });
                    }
                    Err(err) => audit.issues.push(RepoArtifactAuditIssue {
                        path: path.to_string(),
                        kind: RepoArtifactAuditIssueKind::InvalidObject,
                        oid: Some(oid.clone()),
                        content_hash: Some(content_hash.clone()),
                        message: err.to_string(),
                    }),
                }
            }
            CommitArtifactState::LargeFile { kind, oid, content_hash, size } => {
                match self.object_store().read(oid) {
                    Ok(object::Object::Blob(object::BlobObject::LargeFilePointer(pointer))) => {
                        let actual_kind = repo_path_kind_from_object_kind(pointer.kind);
                        if actual_kind != *kind
                            || &pointer.content_hash != content_hash
                            || pointer.size != *size
                        {
                            audit.issues.push(RepoArtifactAuditIssue {
                                path: path.to_string(),
                                kind: RepoArtifactAuditIssueKind::InvalidObject,
                                oid: Some(oid.clone()),
                                content_hash: Some(content_hash.clone()),
                                message: format!(
                                    "large file pointer mismatch: expected kind {kind}, {size} byte(s), and hash {content_hash}, got kind {actual_kind}, {} byte(s), and hash {}",
                                    pointer.size,
                                    pointer.content_hash
                                ),
                            });
                        }
                    }
                    Ok(_) => audit.issues.push(RepoArtifactAuditIssue {
                        path: path.to_string(),
                        kind: RepoArtifactAuditIssueKind::InvalidObject,
                        oid: Some(oid.clone()),
                        content_hash: Some(content_hash.clone()),
                        message: "artifact object is not a large file pointer".to_string(),
                    }),
                    Err(object::ObjectErr::Io(err))
                        if err.kind() == std::io::ErrorKind::NotFound =>
                    {
                        audit.issues.push(RepoArtifactAuditIssue {
                            path: path.to_string(),
                            kind: RepoArtifactAuditIssueKind::MissingObject,
                            oid: Some(oid.clone()),
                            content_hash: Some(content_hash.clone()),
                            message: format!("missing large file pointer object {oid}"),
                        });
                    }
                    Err(err) => audit.issues.push(RepoArtifactAuditIssue {
                        path: path.to_string(),
                        kind: RepoArtifactAuditIssueKind::InvalidObject,
                        oid: Some(oid.clone()),
                        content_hash: Some(content_hash.clone()),
                        message: err.to_string(),
                    }),
                }

                match fs::read(self.large_file_content_path(content_hash)) {
                    Ok(bytes) => {
                        if let Err(err) = validate_large_file_content(content_hash, *size, &bytes) {
                            audit.issues.push(RepoArtifactAuditIssue {
                                path: path.to_string(),
                                kind: RepoArtifactAuditIssueKind::InvalidExternalPayload,
                                oid: Some(oid.clone()),
                                content_hash: Some(content_hash.clone()),
                                message: err.to_string(),
                            });
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        audit.issues.push(RepoArtifactAuditIssue {
                            path: path.to_string(),
                            kind: RepoArtifactAuditIssueKind::MissingExternalPayload,
                            oid: Some(oid.clone()),
                            content_hash: Some(content_hash.clone()),
                            message: format!("missing external payload {content_hash}"),
                        });
                    }
                    Err(err) => audit.issues.push(RepoArtifactAuditIssue {
                        path: path.to_string(),
                        kind: RepoArtifactAuditIssueKind::InvalidExternalPayload,
                        oid: Some(oid.clone()),
                        content_hash: Some(content_hash.clone()),
                        message: err.to_string(),
                    }),
                }
            }
        }
    }
}

fn large_file_content_relative_path(id: &object::ObjectId) -> String {
    let raw = id.as_str();
    format!("{DIR_STORE_FILES}/{}/{}", &raw[..2], &raw[2..])
}

fn validate_large_file_content(id: &object::ObjectId, size: u64, bytes: &[u8]) -> Result<()> {
    if bytes.len() as u64 != size {
        return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
            kind: "large-file",
            message: format!(
                "external payload {id} size mismatch: expected {size}, got {}",
                bytes.len()
            ),
        }));
    }
    let actual = object::ObjectId::for_bytes(bytes);
    if actual != *id {
        return Err(RepoErr::Object(object::ObjectErr::ObjectIdMismatch {
            expected: id.clone(),
            actual,
        }));
    }
    Ok(())
}

fn artifact_file_matches(path: &Path, expected: &CommitArtifactState) -> Result<Option<bool>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Ok(Some(false));
            }
            if metadata.len() != expected.size() {
                return Ok(Some(false));
            }
            let bytes = fs::read(path)?;
            Ok(Some(
                object::ObjectId::for_bytes(&bytes) == *expected.content_hash(),
            ))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn validate_commit_file_state(state: &CommitFileState) -> Result<()> {
    for range in &state.snapshot.ranges {
        let expected_count = (range.start..=range.end).len();
        if range.commits.len() as u64 != expected_count {
            return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "sqlite-snapshot",
                message: format!(
                    "range {:?} {}..={} has {} storage commit hashes; expected {}",
                    range.log,
                    range.start,
                    range.end,
                    range.commits.len(),
                    expected_count
                ),
            }));
        }

        for (commit, expected_lsn) in range.commits.iter().zip((range.start..=range.end).iter()) {
            if commit.lsn != expected_lsn {
                return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                    kind: "sqlite-snapshot",
                    message: format!(
                        "range {:?} {}..={} has storage commit hash for LSN {}; expected {}",
                        range.log, range.start, range.end, commit.lsn, expected_lsn
                    ),
                }));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn repo_snapshot_with_test_hashes(snapshot: &Snapshot) -> RepoSnapshot {
    RepoSnapshot {
        page_count: snapshot.page_count,
        ranges: snapshot
            .iter()
            .map(|range| RepoLogRange {
                log: range.log.clone(),
                start: *range.lsns.start(),
                end: *range.lsns.end(),
                commits: range
                    .lsns
                    .iter()
                    .map(|lsn| RepoStorageCommit {
                        lsn,
                        commit_hash: CommitHash::testonly_random(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn commit_parent_ids(commit: &CommitObject) -> Vec<String> {
    if commit.parents.is_empty() {
        commit.parent.iter().cloned().collect()
    } else {
        commit.parents.clone()
    }
}

fn block_on_remote<T>(
    future: impl std::future::Future<Output = std::result::Result<T, RemoteErr>>,
) -> Result<T> {
    thread_local! {
        static REMOTE_RUNTIME: RefCell<Option<tokio::runtime::Runtime>> = const { RefCell::new(None) };
    }

    REMOTE_RUNTIME.with(|runtime| {
        let mut runtime = runtime.borrow_mut();
        if runtime.is_none() {
            *runtime = Some(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?,
            );
        }
        Ok(runtime
            .as_ref()
            .expect("runtime initialized")
            .block_on(future)?)
    })
}

fn parse_remote_ref(path: &str, bytes: bytes::Bytes) -> Result<String> {
    let raw = String::from_utf8(bytes.to_vec()).map_err(|err| RepoErr::InvalidRemoteObject {
        path: path.to_string(),
        message: err.to_string(),
    })?;
    let target = raw.trim();
    if target.is_empty() {
        return Err(RepoErr::InvalidRemoteObject {
            path: path.to_string(),
            message: "empty ref".to_string(),
        });
    }
    Ok(target.to_string())
}

fn remote_loose_object_id(path: &str) -> Result<Option<object::ObjectId>> {
    let Some(rest) = path.strip_prefix("objects/") else {
        return Ok(None);
    };
    if rest.starts_with("pack/") {
        return Ok(None);
    }
    let Some((fanout, suffix)) = rest.split_once('/') else {
        return Ok(None);
    };
    if fanout.len() != 2 || suffix.len() != 62 || suffix.contains('/') {
        return Ok(None);
    }
    let id = format!("{fanout}{suffix}");
    if !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(None);
    }
    Ok(Some(object::ObjectId::from_str(&id)?))
}

fn fetch_remote_object_pack_indexes(
    remote: &crate::remote::Remote,
) -> Result<Vec<RemoteObjectPackIndex>> {
    let mut indexes = Vec::new();
    for path in block_on_remote(remote.list_raw(DIR_OBJECTS_PACK))? {
        if !path.ends_with(".idx") {
            continue;
        }
        let Some(bytes) = block_on_remote(remote.get_raw(&path))? else {
            continue;
        };
        indexes.push(decode_remote_object_pack_index(&path, &bytes)?);
    }
    Ok(indexes)
}

fn decode_remote_object_pack_index(path: &str, bytes: &[u8]) -> Result<RemoteObjectPackIndex> {
    let index: RemoteObjectPackIndex =
        serde_json::from_slice(bytes).map_err(|err| RepoErr::InvalidRemoteObject {
            path: path.to_string(),
            message: format!("invalid pack index JSON: {err}"),
        })?;
    if index.version != REMOTE_OBJECT_PACK_VERSION {
        return Err(RepoErr::InvalidRemoteObject {
            path: path.to_string(),
            message: format!(
                "unsupported pack index version {}; expected {}",
                index.version, REMOTE_OBJECT_PACK_VERSION
            ),
        });
    }
    if !index.pack.starts_with(&format!("{DIR_OBJECTS_PACK}/")) || !index.pack.ends_with(".pack") {
        return Err(RepoErr::InvalidRemoteObject {
            path: path.to_string(),
            message: format!("pack path `{}` is outside {DIR_OBJECTS_PACK}", index.pack),
        });
    }
    let min_offset = REMOTE_OBJECT_PACK_MAGIC.len() as u64;
    for entry in &index.objects {
        if entry.len == 0 {
            return Err(RepoErr::InvalidRemoteObject {
                path: path.to_string(),
                message: format!("pack entry for object {} is empty", entry.id),
            });
        }
        if entry.offset < min_offset {
            return Err(RepoErr::InvalidRemoteObject {
                path: path.to_string(),
                message: format!(
                    "pack entry for object {} starts inside pack header",
                    entry.id
                ),
            });
        }
        entry
            .offset
            .checked_add(entry.len)
            .ok_or_else(|| RepoErr::InvalidRemoteObject {
                path: path.to_string(),
                message: format!("pack entry for object {} overflows u64 range", entry.id),
            })?;
    }
    Ok(index)
}

fn parse_remote_head_branch(path: &str, bytes: bytes::Bytes) -> Result<Option<String>> {
    let raw = String::from_utf8(bytes.to_vec()).map_err(|err| RepoErr::InvalidRemoteObject {
        path: path.to_string(),
        message: err.to_string(),
    })?;
    let target = raw.trim();
    if target.is_empty() {
        return Err(RepoErr::InvalidRemoteObject {
            path: path.to_string(),
            message: "empty HEAD".to_string(),
        });
    }
    let Some(reference) = target.strip_prefix("ref: ") else {
        return Ok(None);
    };
    let Some(branch) = reference.strip_prefix("refs/heads/") else {
        return Err(RepoErr::InvalidRemoteObject {
            path: path.to_string(),
            message: format!("HEAD points outside refs/heads: {reference}"),
        });
    };
    validate_ref_name(branch)?;
    Ok(Some(branch.to_string()))
}

fn worktree_for_file(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[derive(Debug, Clone, Default)]
struct IgnoreRules {
    patterns: Vec<IgnorePattern>,
}

impl IgnoreRules {
    fn load(worktree: &Path) -> Result<Self> {
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

    fn is_ignored(&self, key: &str, is_dir: bool) -> bool {
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

fn wildcard_match(pattern: &str, text: &str) -> bool {
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

fn remove_empty_parent_dirs(mut dir: Option<&Path>, root: &Path) -> Result<()> {
    while let Some(current) = dir {
        if current == root || !current.starts_with(root) {
            break;
        }
        match fs::remove_dir(current) {
            Ok(()) => dir = current.parent(),
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotFound
                ) =>
            {
                break;
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn path_tree_contains_only_file(dir: &Path, allowed_file: &Path) -> Result<bool> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if !path_tree_contains_only_file(&path, allowed_file)? {
                return Ok(false);
            }
        } else if path != allowed_file {
            return Ok(false);
        }
    }
    Ok(true)
}

fn move_path_if_exists(from: PathBuf, to: PathBuf) -> Result<()> {
    if !from.exists() {
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(from, to)?;
    Ok(())
}

fn remove_path_if_exists(path: PathBuf) -> Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)?;
    } else if path.is_file() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn normalize_discovery_start(start: &Path) -> Result<PathBuf> {
    if start.exists() {
        let start = fs::canonicalize(start)?;
        if start.is_file() {
            Ok(start
                .parent()
                .map_or_else(|| PathBuf::from("/"), Path::to_path_buf))
        } else {
            Ok(start)
        }
    } else {
        let base = if start.extension().is_some() {
            worktree_for_file(start)
        } else {
            start
        };
        Ok(fs::canonicalize(base)?)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RevisionOp {
    FirstParent(usize),
    Parent(usize),
}

fn split_revision_ops(rev: &str) -> Result<(&str, Vec<RevisionOp>)> {
    let Some(first_op) = rev.find(['~', '^']) else {
        return Ok((rev, Vec::new()));
    };
    if first_op == 0 {
        return Err(RepoErr::InvalidRevision(rev.to_string()));
    }

    let base = &rev[..first_op];
    let suffix = &rev[first_op..];
    let bytes = suffix.as_bytes();
    let mut ops = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        if op != b'~' && op != b'^' {
            return Err(RepoErr::InvalidRevision(rev.to_string()));
        }
        i += 1;
        let digits_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let value = if digits_start == i {
            1
        } else {
            suffix[digits_start..i]
                .parse()
                .map_err(|_| RepoErr::InvalidRevision(rev.to_string()))?
        };
        ops.push(match op {
            b'~' => RevisionOp::FirstParent(value),
            b'^' => RevisionOp::Parent(value),
            _ => unreachable!("validated op"),
        });
    }

    Ok((base, ops))
}

fn normalize_repo_path(path: &str) -> String {
    let path = path.trim().trim_start_matches("./").replace('\\', "/");
    let path = path.trim_end_matches('/');
    if path == "." {
        String::new()
    } else {
        path.to_string()
    }
}

fn is_sqlite_database_file(path: &Path) -> Result<bool> {
    let mut file = fs::File::open(path)?;
    let mut magic = [0; SQLITE_DATABASE_MAGIC.len()];
    match file.read_exact(&mut magic) {
        Ok(()) => Ok(&magic == SQLITE_DATABASE_MAGIC),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn classify_artifact_path(path: &Path) -> Result<RepoTrackedPathKind> {
    let mut file = fs::File::open(path)?;
    let mut sample = vec![0; CONTENT_CLASS_SAMPLE_BYTES];
    let len = file.read(&mut sample)?;
    sample.truncate(len);
    Ok(classify_artifact_bytes(&sample))
}

fn classify_artifact_bytes(bytes: &[u8]) -> RepoTrackedPathKind {
    if is_text_bytes(bytes) {
        RepoTrackedPathKind::TextFile
    } else {
        RepoTrackedPathKind::BinaryFile
    }
}

fn artifact_storage_for_path(
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

fn config_path_patterns_match(patterns: &[String], key: &str) -> bool {
    patterns
        .iter()
        .any(|pattern| config_path_pattern_matches(pattern, key))
}

fn config_path_pattern_matches(pattern: &str, key: &str) -> bool {
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

fn is_text_bytes(bytes: &[u8]) -> bool {
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

fn is_sqlite_sidecar_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.ends_with("-wal") || name.ends_with("-shm") || name.ends_with("-journal")
        })
}

fn diff_repo_maps(
    from: impl Into<String>,
    to: impl Into<String>,
    from_files: &BTreeMap<String, CommitFileState>,
    to_files: &BTreeMap<String, CommitFileState>,
    from_artifacts: &BTreeMap<String, CommitArtifactState>,
    to_artifacts: &BTreeMap<String, CommitArtifactState>,
    path: Option<&str>,
) -> RepoDiff {
    let path = path.map(normalize_repo_path);
    let mut keys = BTreeMap::<String, ()>::new();
    for key in from_files.keys().chain(to_files.keys()) {
        if repo_path_matches_filter(key, path.as_deref()) {
            keys.insert(key.clone(), ());
        }
    }

    let mut files = Vec::new();
    for key in keys.keys() {
        let before = from_files.get(key).cloned();
        let after = to_files.get(key).cloned();
        let change = match (&before, &after) {
            (None, Some(_)) => Some(RepoFileChange::Added),
            (Some(_), None) => Some(RepoFileChange::Deleted),
            (Some(before), Some(after)) if before != after => Some(RepoFileChange::Modified),
            _ => None,
        };
        if let Some(change) = change {
            files.push(RepoFileDiff {
                path: key.clone(),
                change,
                kind: RepoTrackedPathKind::SqliteDatabase,
                storage: RepoPathStorage::SqliteSnapshot,
                from: before,
                to: after,
            });
        }
    }

    let mut artifact_keys = BTreeMap::<String, ()>::new();
    for key in from_artifacts.keys().chain(to_artifacts.keys()) {
        if repo_path_matches_filter(key, path.as_deref()) {
            artifact_keys.insert(key.clone(), ());
        }
    }

    let mut artifacts = Vec::new();
    for key in artifact_keys.keys() {
        let before = from_artifacts.get(key).cloned();
        let after = to_artifacts.get(key).cloned();
        let change = match (&before, &after) {
            (None, Some(_)) => Some(RepoFileChange::Added),
            (Some(_), None) => Some(RepoFileChange::Deleted),
            (Some(before), Some(after)) if before != after => Some(RepoFileChange::Modified),
            _ => None,
        };
        if let Some(change) = change {
            artifacts.push(RepoArtifactDiff {
                path: key.clone(),
                change,
                kind: artifact_diff_kind(before.as_ref(), after.as_ref()),
                storage: artifact_diff_storage(before.as_ref(), after.as_ref()),
                from: before,
                to: after,
            });
        }
    }

    let paths = repo_diff_paths(&files, &artifacts);
    RepoDiff {
        from: from.into(),
        to: to.into(),
        paths,
        files,
        artifacts,
    }
}

fn repo_diff_paths(files: &[RepoFileDiff], artifacts: &[RepoArtifactDiff]) -> Vec<RepoPathDiff> {
    let mut paths = Vec::with_capacity(files.len() + artifacts.len());
    paths.extend(files.iter().map(|file| RepoPathDiff {
        path: file.path.clone(),
        change: file.change,
        kind: file.kind,
        storage: file.storage,
    }));
    paths.extend(artifacts.iter().map(|artifact| RepoPathDiff {
        path: artifact.path.clone(),
        change: artifact.change,
        kind: artifact.kind,
        storage: artifact.storage,
    }));
    paths.sort_by(|left, right| left.path.cmp(&right.path));
    paths
}

fn commit_path_changes(
    from_files: &BTreeMap<String, CommitFileState>,
    to_files: &BTreeMap<String, CommitFileState>,
    from_artifacts: &BTreeMap<String, CommitArtifactState>,
    to_artifacts: &BTreeMap<String, CommitArtifactState>,
) -> Vec<CommitPathChange> {
    let diff = diff_repo_maps(
        "parent",
        "commit",
        from_files,
        to_files,
        from_artifacts,
        to_artifacts,
        None,
    );
    diff.paths
        .into_iter()
        .map(|path| CommitPathChange {
            path: path.path,
            change: path.change,
            kind: path.kind,
            storage: path.storage,
        })
        .collect()
}

fn repo_path_matches_filter(key: &str, path: Option<&str>) -> bool {
    path.is_none_or(|path| {
        path.is_empty()
            || key == path
            || key
                .strip_prefix(path)
                .is_some_and(|suffix| suffix.starts_with('/'))
    })
}

fn validate_remote_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.contains("@{")
        || name == "@"
        || name.starts_with('-')
        || name.ends_with('.')
        || name.ends_with(".lock")
        || name.chars().any(is_invalid_ref_char)
    {
        return Err(RepoErr::InvalidRemoteName(name.to_string()));
    }
    Ok(())
}

fn validate_ref_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('/')
        || name.ends_with('/')
        || name.contains("//")
        || name.contains("..")
        || name.contains("@{")
        || name == "@"
        || name.starts_with('-')
        || name.ends_with('.')
        || name.chars().any(is_invalid_ref_char)
    {
        return Err(RepoErr::InvalidRefName(name.to_string()));
    }

    if name.split('/').any(|part| {
        part == "."
            || part == ".."
            || part.is_empty()
            || part.starts_with('.')
            || part.ends_with(".lock")
    }) {
        return Err(RepoErr::InvalidRefName(name.to_string()));
    }

    Ok(())
}

fn is_invalid_ref_char(ch: char) -> bool {
    ch.is_control() || ch.is_whitespace() || matches!(ch, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
}

fn validate_full_ref(reference: &str) -> Result<()> {
    if let Some(name) = reference.strip_prefix("refs/heads/") {
        validate_ref_name(name)
    } else if let Some(rest) = reference.strip_prefix("refs/remotes/") {
        let (remote, branch) = rest
            .split_once('/')
            .ok_or_else(|| RepoErr::InvalidRefName(reference.to_string()))?;
        validate_remote_name(remote)?;
        validate_ref_name(branch)
    } else if let Some(name) = reference.strip_prefix("refs/tags/") {
        validate_ref_name(name)
    } else {
        Err(RepoErr::InvalidRefName(reference.to_string()))
    }
}

trait WriteAll {
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()>;
}

impl WriteAll for fs::File {
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        Write::write_all(self, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_repo_layout_and_unborn_main() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        assert!(repo.graft_dir().join(CONFIG_FILE).exists());
        assert!(repo.graft_dir().join(DIR_OBJECTS).is_dir());
        assert!(repo.graft_dir().join(DIR_OBJECTS_PACK).is_dir());
        assert!(!repo.graft_dir().join("objects/commits").exists());
        assert!(repo.graft_dir().join(DIR_STORE_FJALL).is_dir());
        assert_eq!(
            repo.config().unwrap().extensions.object_format,
            OBJECT_FORMAT
        );
        assert_eq!(
            repo.config().unwrap().files.inline_text_threshold,
            ByteUnit::MB
        );
        assert_eq!(
            fs::read_to_string(repo.graft_dir().join(HEAD_FILE)).unwrap(),
            "ref: refs/heads/main\n"
        );

        let status = repo.status().unwrap();
        assert_eq!(status.repository_format_version, REPOSITORY_FORMAT_VERSION);
        assert_eq!(status.head, Head::branch("main"));
        assert_eq!(status.head_target, None);
        assert!(!status.dirty);
        assert_eq!(status.branches.len(), 1);
        assert_eq!(status.branches[0].name, "main");
        assert_eq!(status.branches[0].target, None);
        assert!(status.branches[0].current);
    }

    #[test]
    fn config_get_set_manages_files_inline_text_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        assert_eq!(
            repo.config_get(CONFIG_KEY_FILES_INLINE_TEXT_THRESHOLD)
                .unwrap(),
            RepoConfigEntry {
                key: CONFIG_KEY_FILES_INLINE_TEXT_THRESHOLD.to_string(),
                value: "1 MB".to_string()
            }
        );

        assert_eq!(
            repo.config_set(CONFIG_KEY_FILES_INLINE_TEXT_THRESHOLD, "4 B")
                .unwrap(),
            RepoConfigEntry {
                key: CONFIG_KEY_FILES_INLINE_TEXT_THRESHOLD.to_string(),
                value: "4 B".to_string()
            }
        );
        assert_eq!(
            repo.config().unwrap().files.inline_text_threshold,
            ByteUnit::new(4)
        );

        let raw_config = fs::read_to_string(repo.graft_dir().join(CONFIG_FILE)).unwrap();
        assert!(raw_config.contains("[files]"));
        assert!(raw_config.contains("inline_text_threshold = \"4 B\""));

        assert!(matches!(
            repo.config_get("core.default_branch"),
            Err(RepoErr::UnknownConfigKey(key)) if key == "core.default_branch"
        ));
        assert!(matches!(
            repo.config_set(CONFIG_KEY_FILES_INLINE_TEXT_THRESHOLD, "4 B extra"),
            Err(RepoErr::InvalidConfigValue { key, value, .. })
                if key == CONFIG_KEY_FILES_INLINE_TEXT_THRESHOLD && value == "4 B extra"
        ));
        assert_eq!(
            repo.config_unset(CONFIG_KEY_FILES_INLINE_TEXT_THRESHOLD)
                .unwrap(),
            RepoConfigEntry {
                key: CONFIG_KEY_FILES_INLINE_TEXT_THRESHOLD.to_string(),
                value: "1 MB".to_string()
            }
        );
        assert_eq!(
            repo.config().unwrap().files.inline_text_threshold,
            ByteUnit::MB
        );
    }

    #[test]
    fn config_get_set_manages_files_external_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        assert_eq!(
            repo.config_get(CONFIG_KEY_FILES_EXTERNAL_PATHS).unwrap(),
            RepoConfigEntry {
                key: CONFIG_KEY_FILES_EXTERNAL_PATHS.to_string(),
                value: String::new()
            }
        );

        assert_eq!(
            repo.config_set(
                CONFIG_KEY_FILES_EXTERNAL_PATHS,
                "assets/**, ./attachments/**"
            )
            .unwrap(),
            RepoConfigEntry {
                key: CONFIG_KEY_FILES_EXTERNAL_PATHS.to_string(),
                value: "assets/**, attachments/**".to_string()
            }
        );
        assert_eq!(
            repo.config().unwrap().files.external_paths,
            vec!["assets/**".to_string(), "attachments/**".to_string()]
        );

        let raw_config = fs::read_to_string(repo.graft_dir().join(CONFIG_FILE)).unwrap();
        assert!(raw_config.contains("[files]"));
        assert!(raw_config.contains("external_paths = ["));
        assert!(raw_config.contains(r#""assets/**""#));
        assert!(raw_config.contains(r#""attachments/**""#));

        assert!(matches!(
            repo.config_set(CONFIG_KEY_FILES_EXTERNAL_PATHS, "assets/** assets/**"),
            Err(RepoErr::InvalidConfigValue { key, value, .. })
                if key == CONFIG_KEY_FILES_EXTERNAL_PATHS && value == "assets/** assets/**"
        ));
        assert_eq!(
            repo.config_unset(CONFIG_KEY_FILES_EXTERNAL_PATHS).unwrap(),
            RepoConfigEntry {
                key: CONFIG_KEY_FILES_EXTERNAL_PATHS.to_string(),
                value: String::new()
            }
        );
        assert!(repo.config().unwrap().files.external_paths.is_empty());
    }

    #[test]
    fn config_get_set_manages_merge_semantic_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        assert_eq!(
            repo.config_get(CONFIG_KEY_MERGE_DEFAULT_SEMANTIC_KEYS)
                .unwrap(),
            RepoConfigEntry {
                key: CONFIG_KEY_MERGE_DEFAULT_SEMANTIC_KEYS.to_string(),
                value: String::new()
            }
        );
        assert_eq!(
            repo.config_set(CONFIG_KEY_MERGE_DEFAULT_SEMANTIC_KEYS, "_id, slug")
                .unwrap(),
            RepoConfigEntry {
                key: CONFIG_KEY_MERGE_DEFAULT_SEMANTIC_KEYS.to_string(),
                value: "_id, slug".to_string()
            }
        );
        assert_eq!(
            repo.config().unwrap().merge.default_semantic_keys,
            vec!["_id".to_string(), "slug".to_string()]
        );

        let table_key = "merge.semantic_keys.policy_entities";
        assert_eq!(
            repo.config_get(table_key).unwrap(),
            RepoConfigEntry {
                key: table_key.to_string(),
                value: String::new()
            }
        );
        assert_eq!(
            repo.config_set(table_key, "name entity_id").unwrap(),
            RepoConfigEntry {
                key: table_key.to_string(),
                value: "name, entity_id".to_string()
            }
        );
        assert_eq!(
            repo.config().unwrap().merge.semantic_keys["policy_entities"],
            vec!["name".to_string(), "entity_id".to_string()]
        );

        let raw_config = fs::read_to_string(repo.graft_dir().join(CONFIG_FILE)).unwrap();
        assert!(raw_config.contains("[merge]"));
        assert!(raw_config.contains("default_semantic_keys = ["));
        assert!(raw_config.contains(r#""_id""#));
        assert!(raw_config.contains(r#""slug""#));
        assert!(raw_config.contains("[merge.semantic_keys]"));
        assert!(raw_config.contains("policy_entities = ["));
        assert!(raw_config.contains(r#""name""#));
        assert!(raw_config.contains(r#""entity_id""#));

        assert_eq!(repo.config_set(table_key, "").unwrap().value, "");
        repo.config_set(table_key, "name").unwrap();
        assert_eq!(
            repo.config_unset(table_key).unwrap(),
            RepoConfigEntry {
                key: table_key.to_string(),
                value: String::new()
            }
        );
        assert!(
            !repo
                .config()
                .unwrap()
                .merge
                .semantic_keys
                .contains_key("policy_entities")
        );
        assert_eq!(
            repo.config_unset(CONFIG_KEY_MERGE_DEFAULT_SEMANTIC_KEYS)
                .unwrap(),
            RepoConfigEntry {
                key: CONFIG_KEY_MERGE_DEFAULT_SEMANTIC_KEYS.to_string(),
                value: String::new()
            }
        );
        assert!(
            repo.config()
                .unwrap()
                .merge
                .default_semantic_keys
                .is_empty()
        );

        assert!(matches!(
            repo.config_get("merge.semantic_keys."),
            Err(RepoErr::UnknownConfigKey(key)) if key == "merge.semantic_keys."
        ));
        assert!(matches!(
            repo.config_set(CONFIG_KEY_MERGE_DEFAULT_SEMANTIC_KEYS, "_id,,slug"),
            Err(RepoErr::InvalidConfigValue { key, value, .. })
                if key == CONFIG_KEY_MERGE_DEFAULT_SEMANTIC_KEYS && value == "_id,,slug"
        ));
        assert!(matches!(
            repo.config_set(CONFIG_KEY_MERGE_DEFAULT_SEMANTIC_KEYS, "_id _id"),
            Err(RepoErr::InvalidConfigValue { key, value, .. })
                if key == CONFIG_KEY_MERGE_DEFAULT_SEMANTIC_KEYS && value == "_id _id"
        ));
    }

    #[test]
    fn config_get_set_manages_remaining_merge_policy_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        let generated_key = "merge.generated_columns.generated_merge_surface";
        assert_eq!(repo.config_get(generated_key).unwrap().value, "");
        assert_eq!(
            repo.config_set(generated_key, "body_len body_hash")
                .unwrap(),
            RepoConfigEntry {
                key: generated_key.to_string(),
                value: "body_len, body_hash".to_string()
            }
        );
        assert_eq!(
            repo.config().unwrap().merge.generated_columns["generated_merge_surface"],
            vec!["body_len".to_string(), "body_hash".to_string()]
        );
        assert_eq!(repo.config_unset(generated_key).unwrap().value, "");
        assert!(
            !repo
                .config()
                .unwrap()
                .merge
                .generated_columns
                .contains_key("generated_merge_surface")
        );

        let internal_key = "merge.internal_resolvers.sqlite_sequence";
        assert_eq!(repo.config_get(internal_key).unwrap().value, "sequence_max");
        assert_eq!(
            repo.config_set(internal_key, "sequence_max").unwrap(),
            RepoConfigEntry {
                key: internal_key.to_string(),
                value: "sequence_max".to_string()
            }
        );
        assert_eq!(
            repo.config().unwrap().merge.internal_resolvers["sqlite_sequence"],
            "sequence_max"
        );
        assert_eq!(
            repo.config_unset(internal_key).unwrap(),
            RepoConfigEntry {
                key: internal_key.to_string(),
                value: "sequence_max".to_string()
            }
        );
        assert!(
            !repo
                .config()
                .unwrap()
                .merge
                .internal_resolvers
                .contains_key("sqlite_sequence")
        );
        assert!(matches!(
            repo.config_set(internal_key, "rebuild"),
            Err(RepoErr::InvalidConfigValue { key, value, .. })
                if key == internal_key && value == "rebuild"
        ));
        assert!(matches!(
            repo.config_get("merge.internal_resolvers.unknown"),
            Err(RepoErr::UnknownConfigKey(key)) if key == "merge.internal_resolvers.unknown"
        ));

        let schema_key = "merge.schema_resolvers.add_column";
        assert_eq!(
            repo.config_get(schema_key).unwrap().value,
            "alter_table_add_column"
        );
        assert_eq!(
            repo.config_set(schema_key, "alter_table_add_column")
                .unwrap(),
            RepoConfigEntry {
                key: schema_key.to_string(),
                value: "alter_table_add_column".to_string()
            }
        );
        assert_eq!(
            repo.config().unwrap().merge.schema_resolvers["add_column"],
            "alter_table_add_column"
        );
        assert_eq!(
            repo.config_unset(schema_key).unwrap(),
            RepoConfigEntry {
                key: schema_key.to_string(),
                value: "alter_table_add_column".to_string()
            }
        );
        assert!(
            !repo
                .config()
                .unwrap()
                .merge
                .schema_resolvers
                .contains_key("add_column")
        );
        assert!(matches!(
            repo.config_set(schema_key, "manual"),
            Err(RepoErr::InvalidConfigValue { key, value, .. })
                if key == schema_key && value == "manual"
        ));
    }

    #[test]
    fn config_list_reports_effective_supported_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        assert_eq!(
            repo.config_list().unwrap(),
            vec![
                RepoConfigEntry {
                    key: CONFIG_KEY_FILES_INLINE_TEXT_THRESHOLD.to_string(),
                    value: "1 MB".to_string()
                },
                RepoConfigEntry {
                    key: CONFIG_KEY_FILES_EXTERNAL_PATHS.to_string(),
                    value: String::new()
                },
                RepoConfigEntry {
                    key: CONFIG_KEY_MERGE_DEFAULT_SEMANTIC_KEYS.to_string(),
                    value: String::new()
                },
                RepoConfigEntry {
                    key: "merge.internal_resolvers.index_btree".to_string(),
                    value: "reindex".to_string()
                },
                RepoConfigEntry {
                    key: "merge.internal_resolvers.sqlite_sequence".to_string(),
                    value: "sequence_max".to_string()
                },
                RepoConfigEntry {
                    key: "merge.internal_resolvers.sqlite_stat1".to_string(),
                    value: "rebuild".to_string()
                },
                RepoConfigEntry {
                    key: "merge.internal_resolvers.sqlite_stat2".to_string(),
                    value: "rebuild".to_string()
                },
                RepoConfigEntry {
                    key: "merge.internal_resolvers.sqlite_stat3".to_string(),
                    value: "rebuild".to_string()
                },
                RepoConfigEntry {
                    key: "merge.internal_resolvers.sqlite_stat4".to_string(),
                    value: "rebuild".to_string()
                },
                RepoConfigEntry {
                    key: "merge.schema_resolvers.add_column".to_string(),
                    value: "alter_table_add_column".to_string()
                }
            ]
        );

        repo.config_set(CONFIG_KEY_FILES_INLINE_TEXT_THRESHOLD, "8 MB")
            .unwrap();
        repo.config_set(CONFIG_KEY_MERGE_DEFAULT_SEMANTIC_KEYS, "_id slug")
            .unwrap();
        repo.config_set("merge.semantic_keys.documents", "doc_id")
            .unwrap();
        repo.config_set("merge.semantic_keys.assets", "asset_id")
            .unwrap();
        repo.config_set("merge.generated_columns.documents", "body_len")
            .unwrap();

        assert_eq!(
            repo.config_list().unwrap(),
            vec![
                RepoConfigEntry {
                    key: CONFIG_KEY_FILES_INLINE_TEXT_THRESHOLD.to_string(),
                    value: "8 MB".to_string()
                },
                RepoConfigEntry {
                    key: CONFIG_KEY_FILES_EXTERNAL_PATHS.to_string(),
                    value: String::new()
                },
                RepoConfigEntry {
                    key: CONFIG_KEY_MERGE_DEFAULT_SEMANTIC_KEYS.to_string(),
                    value: "_id, slug".to_string()
                },
                RepoConfigEntry {
                    key: "merge.semantic_keys.assets".to_string(),
                    value: "asset_id".to_string()
                },
                RepoConfigEntry {
                    key: "merge.semantic_keys.documents".to_string(),
                    value: "doc_id".to_string()
                },
                RepoConfigEntry {
                    key: "merge.internal_resolvers.index_btree".to_string(),
                    value: "reindex".to_string()
                },
                RepoConfigEntry {
                    key: "merge.internal_resolvers.sqlite_sequence".to_string(),
                    value: "sequence_max".to_string()
                },
                RepoConfigEntry {
                    key: "merge.internal_resolvers.sqlite_stat1".to_string(),
                    value: "rebuild".to_string()
                },
                RepoConfigEntry {
                    key: "merge.internal_resolvers.sqlite_stat2".to_string(),
                    value: "rebuild".to_string()
                },
                RepoConfigEntry {
                    key: "merge.internal_resolvers.sqlite_stat3".to_string(),
                    value: "rebuild".to_string()
                },
                RepoConfigEntry {
                    key: "merge.internal_resolvers.sqlite_stat4".to_string(),
                    value: "rebuild".to_string()
                },
                RepoConfigEntry {
                    key: "merge.schema_resolvers.add_column".to_string(),
                    value: "alter_table_add_column".to_string()
                },
                RepoConfigEntry {
                    key: "merge.generated_columns.documents".to_string(),
                    value: "body_len".to_string()
                }
            ]
        );
    }

    #[test]
    fn open_rejects_unsupported_object_format() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let mut config = repo.config().unwrap();
        config.extensions.object_format = "sha1".to_string();
        repo.write_config(&config).unwrap();

        assert!(matches!(
            Repository::open(tmp.path()),
            Err(RepoErr::UnsupportedObjectFormat { expected, actual })
                if expected == OBJECT_FORMAT && actual == "sha1"
        ));
    }

    #[test]
    fn commit_updates_current_branch_and_log() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let app = tmp.path().join("app.db");

        repo.mark_dirty_path(&app).unwrap();
        let status = repo.status().unwrap();
        assert!(status.dirty);
        assert_eq!(status.unstaged, vec!["app.db".to_string()]);

        let first = repo.commit("initial database").unwrap();
        assert!(!repo.status().unwrap().dirty);
        assert_eq!(repo.status().unwrap().head_target, Some(first.id.clone()));

        repo.mark_dirty_path(&app).unwrap();
        let second = repo.commit("add table").unwrap();

        let log = repo.log().unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0], second);
        assert_eq!(log[1], first);
    }

    #[test]
    fn status_scans_worktree_files_as_untracked() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let nested = tmp.path().join("nested");
        fs::create_dir_all(&nested).unwrap();
        let ignored_dir = tmp.path().join("ignored_dir");
        fs::create_dir_all(&ignored_dir).unwrap();

        fs::write(
            tmp.path().join(GRAFT_IGNORE_FILE),
            "*.tmp\nignored_dir/\nignored.db\n.graftignore\n",
        )
        .unwrap();
        write_sqlite_magic(tmp.path().join("app.db"));
        fs::write(tmp.path().join("app.db-wal"), b"sqlite sidecar").unwrap();
        write_sqlite_magic(tmp.path().join("ignored.db"));
        fs::write(tmp.path().join("scratch.tmp"), b"ignored").unwrap();
        fs::write(ignored_dir.join("notes.txt"), b"ignored").unwrap();
        fs::write(tmp.path().join("notes.txt"), b"not sqlite").unwrap();
        write_sqlite_magic(repo.graft_dir().join("ignored.db"));
        fs::write(repo.graft_dir().join("ignored.txt"), b"ignored").unwrap();
        fs::write(nested.join("config.json"), br#"{"theme":"dark"}"#).unwrap();
        write_sqlite_magic(nested.join("data.sqlite"));

        let status = repo.status().unwrap();

        assert_eq!(
            status.unstaged_changes,
            vec![
                RepoWorktreeChange {
                    path: "app.db".to_string(),
                    change: RepoWorktreeChangeKind::Untracked,
                    kind: RepoTrackedPathKind::SqliteDatabase,
                    storage: RepoPathStorage::SqliteSnapshot,
                },
                RepoWorktreeChange {
                    path: "nested/config.json".to_string(),
                    change: RepoWorktreeChangeKind::Untracked,
                    kind: RepoTrackedPathKind::TextFile,
                    storage: RepoPathStorage::Inline,
                },
                RepoWorktreeChange {
                    path: "nested/data.sqlite".to_string(),
                    change: RepoWorktreeChangeKind::Untracked,
                    kind: RepoTrackedPathKind::SqliteDatabase,
                    storage: RepoPathStorage::SqliteSnapshot,
                },
                RepoWorktreeChange {
                    path: "notes.txt".to_string(),
                    change: RepoWorktreeChangeKind::Untracked,
                    kind: RepoTrackedPathKind::TextFile,
                    storage: RepoPathStorage::Inline,
                },
            ]
        );
    }

    #[test]
    fn untracked_paths_lists_worktree_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let mut config = repo.config().unwrap();
        config.files.inline_text_threshold = ByteUnit::new(4);
        repo.write_config(&config).unwrap();

        let assets = tmp.path().join("assets");
        let ignored_dir = tmp.path().join("ignored_dir");
        fs::create_dir_all(&assets).unwrap();
        fs::create_dir_all(&ignored_dir).unwrap();

        fs::write(
            tmp.path().join(GRAFT_IGNORE_FILE),
            "*.tmp\nignored_dir/\nignored.db\n.graftignore\n",
        )
        .unwrap();
        write_sqlite_magic(tmp.path().join("app.db"));
        fs::write(tmp.path().join("app.db-wal"), b"sqlite sidecar").unwrap();
        write_sqlite_magic(tmp.path().join("ignored.db"));
        fs::write(tmp.path().join("scratch.tmp"), b"ignored").unwrap();
        fs::write(ignored_dir.join("secret.txt"), b"ignored").unwrap();
        fs::write(assets.join("model.bin"), b"large model payload").unwrap();
        fs::write(assets.join("note.txt"), b"note").unwrap();
        fs::write(repo.graft_dir().join("ignored.txt"), b"ignored").unwrap();

        let paths = repo.untracked_paths().unwrap();

        assert_eq!(
            paths,
            vec![
                RepoTrackedPath {
                    path: "app.db".to_string(),
                    kind: RepoTrackedPathKind::SqliteDatabase,
                    storage: RepoPathStorage::SqliteSnapshot,
                    size: Some(SQLITE_DATABASE_MAGIC.len() as u64),
                    page_count: None,
                },
                RepoTrackedPath {
                    path: "assets/model.bin".to_string(),
                    kind: RepoTrackedPathKind::TextFile,
                    storage: RepoPathStorage::External,
                    size: Some(19),
                    page_count: None,
                },
                RepoTrackedPath {
                    path: "assets/note.txt".to_string(),
                    kind: RepoTrackedPathKind::TextFile,
                    storage: RepoPathStorage::Inline,
                    size: Some(4),
                    page_count: None,
                },
            ]
        );

        repo.stage_artifact_path(assets.join("note.txt")).unwrap();
        let paths = repo.untracked_paths().unwrap();
        assert_eq!(
            paths
                .iter()
                .map(|path| path.path.as_str())
                .collect::<Vec<_>>(),
            vec!["app.db", "assets/model.bin"]
        );
    }

    #[test]
    fn status_classifies_unstaged_modified_deleted_and_untracked_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(3), PageCount::new(7));
        let app = tmp.path().join("app.db");
        let notes = tmp.path().join("notes.db");

        fs::write(&app, b"tracked database").unwrap();
        repo.commit_file(&app, "initial database", volume, &snapshot)
            .unwrap();

        fs::write(&app, b"modified database").unwrap();
        repo.mark_dirty_path(&app).unwrap();
        let status = repo.status().unwrap();
        assert_eq!(
            status.unstaged_changes,
            vec![RepoWorktreeChange {
                path: "app.db".to_string(),
                change: RepoWorktreeChangeKind::Modified,
                kind: RepoTrackedPathKind::SqliteDatabase,
                storage: RepoPathStorage::SqliteSnapshot,
            }]
        );

        fs::remove_file(&app).unwrap();
        repo.mark_deleted_path(&app).unwrap();
        let status = repo.status().unwrap();
        assert_eq!(
            status.unstaged_changes,
            vec![RepoWorktreeChange {
                path: "app.db".to_string(),
                change: RepoWorktreeChangeKind::Deleted,
                kind: RepoTrackedPathKind::SqliteDatabase,
                storage: RepoPathStorage::SqliteSnapshot,
            }]
        );

        repo.clear_dirty().unwrap();
        fs::write(&notes, b"new database").unwrap();
        repo.mark_dirty_path(&notes).unwrap();
        let status = repo.status().unwrap();
        assert_eq!(
            status.unstaged_changes,
            vec![RepoWorktreeChange {
                path: "notes.db".to_string(),
                change: RepoWorktreeChangeKind::Untracked,
                kind: RepoTrackedPathKind::TextFile,
                storage: RepoPathStorage::Inline,
            }]
        );
        assert_eq!(status.unstaged, vec!["notes.db".to_string()]);
    }

    #[test]
    fn branch_reflog_records_old_and_new_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        let first = repo.commit("initial database").unwrap();
        let second = repo.commit("add table").unwrap();

        let reflog =
            fs::read_to_string(repo.graft_dir().join(DIR_LOGS_REFS).join("refs/heads/main"))
                .unwrap();
        let lines = reflog.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with(&format!("{NULL_OBJECT_ID} {}", first.id)));
        assert!(lines[0].contains("\tcommit: initial database"));
        assert!(lines[1].starts_with(&format!("{} {}", first.id, second.id)));
        assert!(lines[1].contains("\tcommit: add table"));
    }

    #[test]
    fn head_reflog_records_branch_switch_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        let main = repo.commit("initial database").unwrap();
        repo.switch_new_branch("feature/search", None).unwrap();
        let feature = repo.commit("feature work").unwrap();
        repo.switch_branch("main").unwrap();

        let reflog = fs::read_to_string(repo.graft_dir().join(DIR_LOGS_HEAD).join("HEAD")).unwrap();
        let last = reflog.lines().last().unwrap();
        assert!(last.starts_with(&format!("{} {}", feature.id, main.id)));
        assert!(last.contains("\tcheckout: moving to main"));
    }

    #[test]
    fn commit_file_records_snapshot_state() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(3), PageCount::new(7));

        let commit = repo
            .commit_file(
                tmp.path().join("app.db"),
                "initial database",
                volume.clone(),
                &snapshot,
            )
            .unwrap();

        let file = commit.files.get("app.db").unwrap();
        assert_eq!(file.volume, volume);
        assert_eq!(file.snapshot.to_snapshot().head(), snapshot.head());
        assert_eq!(
            repo.head_file(tmp.path().join("app.db")).unwrap(),
            Some(file.clone())
        );
    }

    #[test]
    fn stage_file_updates_index_and_commit_clears_it() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(3), PageCount::new(7));

        repo.mark_dirty_path(tmp.path().join("app.db")).unwrap();
        let entry = repo
            .stage_file(tmp.path().join("app.db"), volume, &snapshot)
            .unwrap();
        assert_eq!(entry.path, "app.db");
        assert!(!repo.is_dirty());
        assert!(repo.has_staged_changes().unwrap());

        let status = repo.status().unwrap();
        assert_eq!(status.staged, vec!["app.db".to_string()]);
        assert!(status.conflicted.is_empty());

        let commit = repo.commit_staged("initial database").unwrap();
        assert_eq!(
            repo.head_file(tmp.path().join("app.db")).unwrap(),
            entry.file
        );
        assert!(!repo.has_staged_changes().unwrap());
        assert!(repo.read_index().unwrap().is_empty());
        assert_eq!(repo.status().unwrap().head_target, Some(commit.id));
    }

    #[test]
    fn staging_one_file_preserves_other_unstaged_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(3), PageCount::new(7));
        let app = tmp.path().join("app.db");
        let notes = tmp.path().join("notes.db");

        repo.mark_dirty_path(&notes).unwrap();
        repo.mark_dirty_path(&app).unwrap();

        let status = repo.status().unwrap();
        assert_eq!(
            status.unstaged,
            vec!["app.db".to_string(), "notes.db".to_string()]
        );

        repo.stage_file(&app, volume, &snapshot).unwrap();
        let status = repo.status().unwrap();
        assert_eq!(status.staged, vec!["app.db".to_string()]);
        assert_eq!(status.unstaged, vec!["notes.db".to_string()]);

        repo.commit_staged("stage app only").unwrap();
        let status = repo.status().unwrap();
        assert!(status.staged.is_empty());
        assert_eq!(status.unstaged, vec!["notes.db".to_string()]);
        assert!(status.dirty);
    }

    #[test]
    fn stage_file_removal_commits_deleted_path() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let app = tmp.path().join("app.db");
        let notes = tmp.path().join("notes.db");
        let app_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let notes_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(3), PageCount::new(4));

        repo.stage_file(&app, volume.clone(), &app_snapshot)
            .unwrap();
        repo.stage_file(&notes, volume, &notes_snapshot).unwrap();
        let base = repo.commit_staged("base").unwrap();

        let removal = repo.stage_file_removal(&notes).unwrap();

        assert_eq!(removal.path, "notes.db");
        assert!(removal.file.is_none());
        let staged = repo.diff_staged(None).unwrap();
        assert_eq!(staged.from, base.id);
        assert_eq!(staged.files.len(), 1);
        assert_eq!(staged.files[0].path, "notes.db");
        assert_eq!(staged.files[0].change, RepoFileChange::Deleted);
        assert!(staged.files[0].to.is_none());

        let commit = repo.commit_staged("remove notes").unwrap();

        assert!(repo.head_file(&app).unwrap().is_some());
        assert!(repo.head_file(&notes).unwrap().is_none());
        assert!(
            !repo
                .read_commit(&commit.id)
                .unwrap()
                .files
                .contains_key("notes.db")
        );
        assert!(repo.read_index().unwrap().is_empty());
    }

    #[test]
    fn commit_staged_rejects_empty_index() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        assert!(matches!(
            repo.commit_staged("nothing to commit"),
            Err(RepoErr::NoStagedChanges)
        ));
    }

    #[test]
    fn stage_file_state_path_requires_storage_commit_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let log = LogId::random();
        let state = CommitFileState {
            volume: VolumeId::random(),
            snapshot: RepoSnapshot {
                page_count: PageCount::new(3),
                ranges: vec![RepoLogRange {
                    log,
                    start: LSN::FIRST,
                    end: LSN::new(2),
                    commits: vec![],
                }],
            },
        };

        let err = repo
            .stage_file_state_path(tmp.path().join("app.db"), state)
            .expect_err("missing storage commit hashes should be rejected");
        assert!(matches!(
            err,
            RepoErr::Object(object::ObjectErr::InvalidObject { kind: "sqlite-snapshot", .. })
        ));
    }

    #[test]
    fn commit_file_writes_content_addressed_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(3), PageCount::new(7));

        let commit = repo
            .commit_file(
                tmp.path().join("app.db"),
                "initial database",
                volume.clone(),
                &snapshot,
            )
            .unwrap();

        assert!(!repo.graft_dir().join("objects/commits").exists());
        let object::Object::Commit(commit_object) = repo.read_object(&commit.id).unwrap() else {
            panic!("repo commit id should point at a commit object");
        };
        assert_eq!(commit.tree.as_deref(), Some(commit_object.tree.as_str()));
        assert!(commit_object.parents.is_empty());

        let object::Object::Tree(tree) = repo.read_object(commit_object.tree.as_str()).unwrap()
        else {
            panic!("commit tree should point at a tree object");
        };
        assert_eq!(tree.entries.len(), 1);
        assert_eq!(tree.entries[0].path, "app.db");
        assert_eq!(tree.entries[0].mode, object::TreeEntryMode::SqliteDatabase);

        let object::Object::Blob(object::BlobObject::SqliteSnapshot(blob)) =
            repo.read_object(tree.entries[0].oid.as_str()).unwrap()
        else {
            panic!("tree entry should point at a sqlite snapshot blob");
        };
        assert_eq!(blob.volume, volume);
        assert_eq!(blob.page_count, PageCount::new(7));
        assert_eq!(blob.ranges.len(), 1);
        assert_eq!(blob.ranges[0].log, log);
        assert_eq!(blob.ranges[0].start, LSN::FIRST);
        assert_eq!(blob.ranges[0].end, LSN::new(3));

        let reconstructed = repo.read_commit(&commit.id).unwrap();
        assert_eq!(reconstructed.id, commit.id);
        assert_eq!(reconstructed.tree, commit.tree);
        assert_eq!(reconstructed.files, commit.files);
    }

    #[test]
    fn stage_artifact_path_commits_regular_file_and_status_tracks_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let notes = tmp.path().join("notes.txt");
        fs::write(&notes, b"hello app state").unwrap();

        let entry = repo.stage_artifact_path(&notes).unwrap();
        assert_eq!(entry.path, "notes.txt");
        assert!(entry.file.is_none());
        let state = entry.artifact.clone().expect("artifact staged");
        assert_eq!(state.size(), 15);
        assert_eq!(
            *state.content_hash(),
            object::ObjectId::for_bytes(b"hello app state")
        );

        let commit = repo.commit_staged("track notes").unwrap();
        assert!(commit.files.is_empty());
        assert_eq!(commit.artifacts.get("notes.txt"), Some(&state));
        assert_eq!(repo.head_artifact(&notes).unwrap(), Some(state.clone()));
        assert!(!repo.status().unwrap().dirty);

        let object::Object::Tree(tree) = repo
            .read_object(commit.tree.as_deref().expect("commit tree"))
            .unwrap()
        else {
            panic!("commit tree should point at a tree object");
        };
        assert_eq!(tree.entries.len(), 1);
        assert_eq!(tree.entries[0].mode, object::TreeEntryMode::Regular);

        let object::Object::Blob(object::BlobObject::File(blob)) =
            repo.read_object(tree.entries[0].oid.as_str()).unwrap()
        else {
            panic!("artifact tree entry should point at a file blob");
        };
        assert_eq!(blob.kind, object::FileContentKind::TextFile);
        assert_eq!(blob.bytes, b"hello app state");

        fs::write(&notes, b"changed").unwrap();
        let status = repo.status().unwrap();
        assert_eq!(
            status.unstaged_changes,
            vec![RepoWorktreeChange {
                path: "notes.txt".to_string(),
                change: RepoWorktreeChangeKind::Modified,
                kind: RepoTrackedPathKind::TextFile,
                storage: RepoPathStorage::Inline,
            }]
        );

        fs::remove_file(&notes).unwrap();
        let status = repo.status().unwrap();
        assert_eq!(
            status.unstaged_changes,
            vec![RepoWorktreeChange {
                path: "notes.txt".to_string(),
                change: RepoWorktreeChangeKind::Deleted,
                kind: RepoTrackedPathKind::TextFile,
                storage: RepoPathStorage::Inline,
            }]
        );
    }

    #[test]
    fn large_artifact_uses_pointer_blob_and_materializes_content() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let asset = tmp.path().join("asset.bin");
        let bytes = b"large artifact payload";
        fs::write(&asset, bytes).unwrap();

        let entry = repo
            .stage_artifact_path_with_inline_text_threshold(&asset, 4)
            .unwrap();
        let state = entry.artifact.clone().expect("artifact staged");
        assert!(state.is_large());
        assert_eq!(state.size(), bytes.len() as u64);
        assert_eq!(*state.content_hash(), object::ObjectId::for_bytes(bytes));

        let object::Object::Blob(object::BlobObject::LargeFilePointer(pointer)) =
            repo.read_object(state.oid().as_str()).unwrap()
        else {
            panic!("large artifact should be represented by a pointer blob");
        };
        assert_eq!(pointer.kind, object::FileContentKind::TextFile);
        assert_eq!(pointer.content_hash, object::ObjectId::for_bytes(bytes));
        assert_eq!(pointer.size, bytes.len() as u64);

        fs::remove_file(&asset).unwrap();
        repo.materialize_artifact_state(&asset, &state).unwrap();
        assert_eq!(fs::read(&asset).unwrap(), bytes);
    }

    #[test]
    fn stage_artifact_path_uses_configured_inline_text_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let mut config = repo.config().unwrap();
        config.files.inline_text_threshold = ByteUnit::new(4);
        repo.write_config(&config).unwrap();

        let asset = tmp.path().join("asset.bin");
        let bytes = b"configured large payload";
        fs::write(&asset, bytes).unwrap();

        let state = repo
            .stage_artifact_path(&asset)
            .unwrap()
            .artifact
            .expect("artifact staged");
        assert!(state.is_large());
        assert_eq!(state.size(), bytes.len() as u64);
        assert_eq!(*state.content_hash(), object::ObjectId::for_bytes(bytes));

        let diff = repo.diff_staged(None).unwrap();
        assert_eq!(diff.artifacts.len(), 1);
        assert_eq!(diff.artifacts[0].path, "asset.bin");
        assert_eq!(diff.artifacts[0].change, RepoFileChange::Added);
        assert_eq!(diff.artifacts[0].kind, RepoTrackedPathKind::TextFile);
        assert_eq!(diff.artifacts[0].storage, RepoPathStorage::External);

        let raw_config = fs::read_to_string(repo.graft_dir().join(CONFIG_FILE)).unwrap();
        assert!(raw_config.contains("[files]"));
        assert!(raw_config.contains("inline_text_threshold = \"4 B\""));
    }

    #[test]
    fn stage_artifact_path_uses_kind_and_external_path_storage_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let mut config = repo.config().unwrap();
        config.files.external_paths = vec!["assets/**".to_string()];
        repo.write_config(&config).unwrap();

        let assets = tmp.path().join("assets");
        fs::create_dir_all(&assets).unwrap();
        let icon = assets.join("icon.txt");
        let tiny_png = tmp.path().join("tiny.png");
        fs::write(&icon, b"small text asset").unwrap();
        fs::write(&tiny_png, b"\x89PNG\r\n\x1a\n").unwrap();

        let untracked = repo.untracked_paths().unwrap();
        assert_eq!(
            untracked
                .iter()
                .find(|path| path.path == "tiny.png")
                .map(|path| (&path.kind, &path.storage)),
            Some((&RepoTrackedPathKind::BinaryFile, &RepoPathStorage::External))
        );

        let icon_state = repo
            .stage_artifact_path(&icon)
            .unwrap()
            .artifact
            .expect("text asset staged");
        assert_eq!(
            artifact_tracked_path_kind(&icon_state),
            RepoTrackedPathKind::TextFile
        );
        assert_eq!(
            artifact_tracked_path_storage(&icon_state),
            RepoPathStorage::External
        );

        let png_state = repo
            .stage_artifact_path(&tiny_png)
            .unwrap()
            .artifact
            .expect("binary asset staged");
        assert_eq!(
            artifact_tracked_path_kind(&png_state),
            RepoTrackedPathKind::BinaryFile
        );
        assert_eq!(
            artifact_tracked_path_storage(&png_state),
            RepoPathStorage::External
        );
    }

    #[test]
    fn audit_artifacts_reports_missing_external_payloads() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let asset = tmp.path().join("asset.bin");
        let bytes = b"large artifact payload";
        fs::write(&asset, bytes).unwrap();
        let state = repo
            .stage_artifact_path_with_inline_text_threshold(&asset, 4)
            .unwrap()
            .artifact
            .expect("large artifact staged");
        repo.commit_staged("track asset").unwrap();

        let clean = repo.audit_artifacts().unwrap();
        assert!(clean.ok());
        assert_eq!(clean.artifacts, 1);
        assert_eq!(clean.external_payloads, 1);

        fs::remove_file(repo.large_file_content_path(state.content_hash())).unwrap();

        let audit = repo.audit_artifacts().unwrap();
        assert!(!audit.ok());
        assert_eq!(audit.issues.len(), 1);
        assert_eq!(audit.issues[0].path, "asset.bin");
        assert_eq!(
            audit.issues[0].kind,
            RepoArtifactAuditIssueKind::MissingExternalPayload
        );
        assert_eq!(
            audit.issues[0].content_hash,
            Some(state.content_hash().clone())
        );
    }

    #[test]
    fn tracked_paths_lists_sqlite_files_and_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let app = tmp.path().join("app.db");
        let notes = tmp.path().join("notes.txt");
        let model = tmp.path().join("model.bin");
        fs::write(&notes, b"notes").unwrap();
        fs::write(&model, b"large model payload").unwrap();

        let snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(2), PageCount::new(3));
        repo.stage_file(&app, volume, &snapshot).unwrap();
        repo.stage_artifact_path(&notes).unwrap();
        repo.stage_artifact_path_with_inline_text_threshold(&model, 4)
            .unwrap();

        let tracked = repo.tracked_paths().unwrap();
        assert_eq!(tracked.len(), 3);
        assert_eq!(tracked[0].path, "app.db");
        assert_eq!(tracked[0].kind, RepoTrackedPathKind::SqliteDatabase);
        assert_eq!(tracked[0].storage, RepoPathStorage::SqliteSnapshot);
        assert_eq!(tracked[0].page_count, Some(PageCount::new(3)));
        assert_eq!(tracked[1].path, "model.bin");
        assert_eq!(tracked[1].kind, RepoTrackedPathKind::TextFile);
        assert_eq!(tracked[1].storage, RepoPathStorage::External);
        assert_eq!(tracked[1].size, Some(19));
        assert_eq!(tracked[2].path, "notes.txt");
        assert_eq!(tracked[2].kind, RepoTrackedPathKind::TextFile);
        assert_eq!(tracked[2].storage, RepoPathStorage::Inline);
        assert_eq!(tracked[2].size, Some(5));

        let details = repo.tracked_path_details().unwrap();
        assert_eq!(details.len(), 3);
        assert_eq!(details[0].path, "app.db");
        assert_eq!(details[0].kind, RepoTrackedPathKind::SqliteDatabase);
        assert_eq!(details[0].storage, RepoPathStorage::SqliteSnapshot);
        assert_eq!(details[0].page_count, Some(PageCount::new(3)));
        assert_eq!(details[0].oid, None);
        assert_eq!(details[1].path, "model.bin");
        assert_eq!(details[1].kind, RepoTrackedPathKind::TextFile);
        assert_eq!(details[1].storage, RepoPathStorage::External);
        assert_eq!(details[1].size, Some(19));
        assert!(details[1].oid.is_some());
        assert_eq!(
            details[1].content_hash,
            Some(object::ObjectId::for_bytes(b"large model payload"))
        );
        assert_eq!(details[1].object_present, Some(true));
        assert_eq!(details[1].external_payload_present, Some(true));
        assert_eq!(details[2].path, "notes.txt");
        assert_eq!(details[2].kind, RepoTrackedPathKind::TextFile);
        assert_eq!(details[2].storage, RepoPathStorage::Inline);
        assert_eq!(details[2].size, Some(5));
        assert!(details[2].oid.is_some());
        assert_eq!(
            details[2].content_hash,
            Some(object::ObjectId::for_bytes(b"notes"))
        );
        assert_eq!(details[2].object_present, Some(true));
        assert_eq!(details[2].external_payload_present, None);

        let entries = repo.tracked_path_entries().unwrap();
        assert_eq!(entries.len(), 3);
        assert!(
            entries
                .iter()
                .all(|entry| entry.stage == index::IndexStage::Normal)
        );
        assert_eq!(entries[0].path, "app.db");
        assert_eq!(entries[0].mode, Some(object::TreeEntryMode::SqliteDatabase));
        assert!(entries[0].oid.is_some());
        assert_eq!(entries[1].path, "model.bin");
        assert_eq!(entries[1].mode, Some(object::TreeEntryMode::Regular));
        assert!(entries[1].oid.is_some());
        assert_eq!(entries[2].path, "notes.txt");
        assert_eq!(entries[2].mode, Some(object::TreeEntryMode::Regular));
        assert!(entries[2].oid.is_some());
    }

    #[test]
    fn checkout_artifact_from_revision_stages_path_without_moving_head() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let notes = tmp.path().join("notes.txt");

        fs::write(&notes, b"first").unwrap();
        let first_state = repo
            .stage_artifact_path(&notes)
            .unwrap()
            .artifact
            .expect("artifact staged");
        let first = repo.commit_staged("first notes").unwrap();

        fs::write(&notes, b"second").unwrap();
        let second_state = repo
            .stage_artifact_path(&notes)
            .unwrap()
            .artifact
            .expect("artifact staged");
        let second = repo.commit_staged("second notes").unwrap();

        let outcome = repo
            .checkout_artifact_from_revision("HEAD~1", &notes)
            .unwrap();

        assert_eq!(outcome.target, first.id);
        assert_eq!(outcome.path, "notes.txt");
        assert_eq!(outcome.state, first_state);
        assert_eq!(repo.status().unwrap().head_target, Some(second.id));
        let index = repo.read_index().unwrap();
        let staged: Vec<_> = index.stage0_entries().collect();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].path, "notes.txt");
        assert_eq!(staged[0].artifact, Some(first_state));
        assert_eq!(repo.index_artifact(&notes).unwrap(), staged[0].artifact);
        assert_eq!(repo.head_artifact(&notes).unwrap(), Some(second_state));
    }

    #[test]
    fn restore_index_path_from_revision_handles_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let notes = tmp.path().join("notes.txt");

        fs::write(&notes, b"first").unwrap();
        let first_state = repo
            .stage_artifact_path(&notes)
            .unwrap()
            .artifact
            .expect("artifact staged");
        repo.commit_staged("first notes").unwrap();

        fs::write(&notes, b"second").unwrap();
        let second_state = repo
            .stage_artifact_path(&notes)
            .unwrap()
            .artifact
            .expect("artifact staged");
        repo.commit_staged("second notes").unwrap();

        let restored = repo
            .restore_index_path_from_revision("HEAD~1", &notes)
            .unwrap();

        assert_eq!(restored, "notes.txt");
        assert_eq!(repo.index_artifact(&notes).unwrap(), Some(first_state));
        assert_eq!(repo.head_artifact(&notes).unwrap(), Some(second_state));
        let diff = repo.diff_staged(Some("notes.txt")).unwrap();
        assert!(diff.files.is_empty());
        assert_eq!(diff.artifacts.len(), 1);
        assert_eq!(diff.artifacts[0].path, "notes.txt");
        assert_eq!(diff.artifacts[0].change, RepoFileChange::Modified);
    }

    #[test]
    fn tree_id_changes_when_sqlite_snapshot_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let first_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let second_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(4));

        let first = repo
            .commit_file(
                tmp.path().join("app.db"),
                "first",
                volume.clone(),
                &first_snapshot,
            )
            .unwrap();
        let second = repo
            .commit_file(
                tmp.path().join("app.db"),
                "second",
                volume,
                &second_snapshot,
            )
            .unwrap();

        assert_ne!(first.tree, second.tree);
        assert_ne!(first.id, second.id);
    }

    #[test]
    fn resolve_revision_supports_head_branch_parent_and_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let first_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let second_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(4));

        let first = repo
            .commit_file(
                tmp.path().join("app.db"),
                "first",
                volume.clone(),
                &first_snapshot,
            )
            .unwrap();
        let second = repo
            .commit_file(
                tmp.path().join("app.db"),
                "second",
                volume,
                &second_snapshot,
            )
            .unwrap();
        let prefix = &second.id[..12];

        assert_eq!(repo.resolve_revision("HEAD").unwrap(), second.id);
        assert_eq!(repo.resolve_revision("@").unwrap(), second.id);
        assert_eq!(repo.resolve_revision("main").unwrap(), second.id);
        assert_eq!(repo.resolve_revision("HEAD~1").unwrap(), first.id);
        assert_eq!(repo.resolve_revision("HEAD^").unwrap(), first.id);
        assert_eq!(repo.resolve_revision("HEAD^1").unwrap(), first.id);
        assert_eq!(repo.resolve_revision("HEAD^0").unwrap(), second.id);
        assert_eq!(repo.resolve_revision(prefix).unwrap(), second.id);
    }

    #[test]
    fn tags_create_list_resolve_and_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let first_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let second_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(4));

        let first = repo
            .commit_file(
                tmp.path().join("app.db"),
                "first",
                volume.clone(),
                &first_snapshot,
            )
            .unwrap();
        let second = repo
            .commit_file(
                tmp.path().join("app.db"),
                "second",
                volume,
                &second_snapshot,
            )
            .unwrap();

        let tag = repo.tag_create("v1.0", Some("HEAD~1")).unwrap();
        assert_eq!(tag.name, "v1.0");
        assert_eq!(tag.target, first.id);
        assert_eq!(repo.resolve_revision("v1.0").unwrap(), tag.target);

        let latest = repo.tag_create("latest", None).unwrap();
        assert_eq!(latest.target, second.id);
        assert_eq!(repo.resolve_revision("latest").unwrap(), latest.target);
        assert!(repo.tags().unwrap().iter().any(|tag| tag.name == "v1.0"));
        assert!(matches!(
            repo.tag_create("latest", None),
            Err(RepoErr::TagExists(name)) if name == "latest"
        ));

        let deleted = repo.tag_delete("v1.0").unwrap();
        assert_eq!(deleted.name, "v1.0");
        assert!(repo.tags().unwrap().iter().all(|tag| tag.name != "v1.0"));
        assert!(matches!(
            repo.resolve_revision("v1.0"),
            Err(RepoErr::UnknownRevision(rev)) if rev == "v1.0"
        ));
        assert!(matches!(
            repo.tag_delete("v1.0"),
            Err(RepoErr::TagNotFound(name)) if name == "v1.0"
        ));
    }

    #[test]
    fn annotated_tags_point_refs_at_tag_objects_and_peel_to_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let commit = repo.commit("initial database").unwrap();

        let tag = repo
            .tag_create_annotated("v1.0", None, "release 1.0")
            .unwrap();
        assert_eq!(tag.name, "v1.0");
        assert_eq!(tag.target, commit.id);
        assert_ne!(tag.object, tag.target);
        assert!(tag.annotated);
        assert_eq!(tag.message.as_deref(), Some("release 1.0"));

        let tag_object = repo.read_object(&tag.object).unwrap();
        let object::Object::Tag(tag_object) = tag_object else {
            panic!("expected tag object");
        };
        assert_eq!(tag_object.name, "v1.0");
        assert_eq!(tag_object.message, "release 1.0");
        assert_eq!(tag_object.object.to_string(), commit.id);

        assert_eq!(repo.resolve_revision("v1.0").unwrap(), commit.id);
        assert_eq!(
            repo.resolve_revision(&format!("refs/tags/{}", tag.name))
                .unwrap(),
            commit.id
        );
        assert_eq!(repo.resolve_revision(&tag.object).unwrap(), commit.id);

        let listed = repo.tags().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0], tag);
    }

    #[test]
    fn diff_revisions_reports_changed_sqlite_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let first_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let second_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(4));

        let first = repo
            .commit_file(
                tmp.path().join("app.db"),
                "first",
                volume.clone(),
                &first_snapshot,
            )
            .unwrap();
        let second = repo
            .commit_file(
                tmp.path().join("app.db"),
                "second",
                volume,
                &second_snapshot,
            )
            .unwrap();

        let diff = repo.diff_revisions(&first.id, &second.id, None).unwrap();
        assert_eq!(diff.from, first.id);
        assert_eq!(diff.to, second.id);
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].path, "app.db");
        assert_eq!(diff.files[0].change, RepoFileChange::Modified);
        assert_eq!(diff.files[0].kind, RepoTrackedPathKind::SqliteDatabase);

        let empty = repo
            .diff_revisions("HEAD~1", "HEAD", Some("missing.db"))
            .unwrap();
        assert!(empty.files.is_empty());
    }

    #[test]
    fn diff_path_filter_matches_directory_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let assets = tmp.path().join("assets");
        let docs = tmp.path().join("docs");
        fs::create_dir_all(&assets).unwrap();
        fs::create_dir_all(&docs).unwrap();
        let app = assets.join("app.db");
        let asset_notes = assets.join("notes.txt");
        let docs_notes = docs.join("notes.txt");
        let first_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let second_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(5));

        fs::write(&asset_notes, b"asset notes v1").unwrap();
        fs::write(&docs_notes, b"docs notes v1").unwrap();
        repo.stage_file(&app, volume.clone(), &first_snapshot)
            .unwrap();
        repo.stage_artifact_path(&asset_notes).unwrap();
        repo.stage_artifact_path(&docs_notes).unwrap();
        let first = repo.commit_staged("first").unwrap();

        fs::write(&asset_notes, b"asset notes v2").unwrap();
        fs::write(&docs_notes, b"docs notes v2").unwrap();
        repo.stage_file(&app, volume, &second_snapshot).unwrap();
        repo.stage_artifact_path(&asset_notes).unwrap();
        repo.stage_artifact_path(&docs_notes).unwrap();
        let second = repo.commit_staged("second").unwrap();

        let diff = repo
            .diff_revisions(&first.id, &second.id, Some("assets"))
            .unwrap();
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].path, "assets/app.db");
        assert_eq!(diff.files[0].kind, RepoTrackedPathKind::SqliteDatabase);
        assert_eq!(diff.artifacts.len(), 1);
        assert_eq!(diff.artifacts[0].path, "assets/notes.txt");
        assert_eq!(diff.artifacts[0].kind, RepoTrackedPathKind::TextFile);
        assert_eq!(diff.artifacts[0].storage, RepoPathStorage::Inline);

        let slash_diff = repo
            .diff_revisions(&first.id, &second.id, Some("assets/"))
            .unwrap();
        assert_eq!(slash_diff.files, diff.files);
        assert_eq!(slash_diff.artifacts, diff.artifacts);

        let exact_prefix_miss = repo
            .diff_revisions(&first.id, &second.id, Some("asset"))
            .unwrap();
        assert!(exact_prefix_miss.files.is_empty());
        assert!(exact_prefix_miss.artifacts.is_empty());
    }

    #[test]
    fn diff_staged_and_worktree_file_reports_git_like_states() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let first_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let staged_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(4), PageCount::new(4));
        let worktree_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(6), PageCount::new(5));
        let db = tmp.path().join("app.db");

        let first = repo
            .commit_file(&db, "first", volume.clone(), &first_snapshot)
            .unwrap();
        let staged = repo
            .stage_file(&db, volume.clone(), &staged_snapshot)
            .unwrap();
        let worktree = CommitFileState {
            volume,
            snapshot: RepoSnapshot::from_snapshot(&worktree_snapshot),
        };

        let staged_diff = repo.diff_staged(None).unwrap();
        assert_eq!(staged_diff.from, first.id);
        assert_eq!(staged_diff.to, "index");
        assert_eq!(staged_diff.files.len(), 1);
        assert_eq!(staged_diff.files[0].change, RepoFileChange::Modified);
        assert_eq!(staged_diff.files[0].to, staged.file);

        let worktree_diff = repo
            .diff_worktree_file(&db, worktree.clone(), Some("app.db"))
            .unwrap();
        assert_eq!(worktree_diff.from, "index");
        assert_eq!(worktree_diff.to, "worktree");
        assert_eq!(worktree_diff.files.len(), 1);
        assert_eq!(worktree_diff.files[0].change, RepoFileChange::Modified);
        assert_eq!(worktree_diff.files[0].to, Some(worktree.clone()));

        let rev_worktree_diff = repo
            .diff_revision_to_worktree_file("HEAD", &db, worktree, None)
            .unwrap();
        assert_eq!(rev_worktree_diff.from, first.id);
        assert_eq!(rev_worktree_diff.to, "worktree");
        assert_eq!(rev_worktree_diff.files.len(), 1);
        assert_eq!(rev_worktree_diff.files[0].change, RepoFileChange::Modified);
    }

    #[test]
    fn detach_moves_head_to_resolved_revision() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let first_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let second_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(4));

        let first = repo
            .commit_file(
                tmp.path().join("app.db"),
                "first",
                volume.clone(),
                &first_snapshot,
            )
            .unwrap();
        let second = repo
            .commit_file(
                tmp.path().join("app.db"),
                "second",
                volume,
                &second_snapshot,
            )
            .unwrap();

        let detached = repo.detach("HEAD~1").unwrap();
        assert_eq!(detached, first.id);
        assert_eq!(
            repo.head().unwrap(),
            Head::Detached { commit: first.id.clone() }
        );
        assert_eq!(repo.resolve_revision("HEAD").unwrap(), first.id);
        assert_eq!(repo.resolve_revision(&second.id[..12]).unwrap(), second.id);
    }

    #[test]
    fn checkout_plan_freezes_target_files_before_refs_move() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let first_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let second_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(4), PageCount::new(4));
        let third_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(6), PageCount::new(5));
        let db = tmp.path().join("app.db");

        repo.commit_file(&db, "first", volume.clone(), &first_snapshot)
            .unwrap();
        let second = repo
            .commit_file(&db, "second", volume.clone(), &second_snapshot)
            .unwrap();
        let plan = repo.plan_revision_checkout("HEAD").unwrap();
        let third = repo
            .commit_file(&db, "third", volume, &third_snapshot)
            .unwrap();

        assert_eq!(plan.target, Some(second.id));
        assert_eq!(
            plan.files
                .get("app.db")
                .expect("planned app.db")
                .snapshot
                .to_snapshot()
                .head(),
            second_snapshot.head()
        );
        assert_eq!(repo.status().unwrap().head_target, Some(third.id));
    }

    #[test]
    fn checkout_file_from_revision_stages_path_without_moving_head() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let first_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let second_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(4));
        let db = tmp.path().join("app.db");

        let first = repo
            .commit_file(&db, "first", volume.clone(), &first_snapshot)
            .unwrap();
        let second = repo
            .commit_file(&db, "second", volume, &second_snapshot)
            .unwrap();

        let outcome = repo.checkout_file_from_revision("HEAD~1", &db).unwrap();

        assert_eq!(outcome.target, first.id);
        assert_eq!(outcome.path, "app.db");
        assert_eq!(
            outcome.state.snapshot.to_snapshot().head(),
            first_snapshot.head()
        );
        assert_eq!(repo.status().unwrap().head_target, Some(second.id));
        let index = repo.read_index().unwrap();
        let staged: Vec<_> = index.stage0_entries().collect();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].path, "app.db");
        assert_eq!(staged[0].file, Some(outcome.state));
    }

    #[test]
    fn checkout_file_plan_freezes_target_before_branch_moves() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let base_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let feature_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(4), PageCount::new(4));
        let later_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(6), PageCount::new(5));
        let db = tmp.path().join("app.db");

        repo.commit_file(&db, "base", volume.clone(), &base_snapshot)
            .unwrap();
        repo.switch_new_branch("feature/search", None).unwrap();
        let feature = repo
            .commit_file(&db, "feature", volume.clone(), &feature_snapshot)
            .unwrap();
        repo.switch_branch("main").unwrap();

        let plan = repo
            .plan_checkout_file_from_revision("feature/search", &db)
            .unwrap();
        assert_eq!(plan.target, feature.id);
        assert_eq!(plan.path, "app.db");
        assert!(repo.read_index().unwrap().is_empty());

        repo.switch_branch("feature/search").unwrap();
        let later = repo
            .commit_file(&db, "feature later", volume, &later_snapshot)
            .unwrap();
        repo.switch_branch("main").unwrap();

        let outcome = repo.apply_checkout_file_plan(&plan).unwrap();

        assert_eq!(outcome.target, feature.id);
        assert_eq!(
            outcome.state.snapshot.to_snapshot().head(),
            feature_snapshot.head()
        );
        assert_eq!(
            repo.branch_target("feature/search").unwrap(),
            Some(later.id)
        );
        let staged = repo.diff_staged(Some("app.db")).unwrap();
        assert_eq!(staged.to, "index");
        assert_eq!(staged.files.len(), 1);
        assert_eq!(
            staged.files[0]
                .to
                .as_ref()
                .unwrap()
                .snapshot
                .to_snapshot()
                .head(),
            feature_snapshot.head()
        );
    }

    #[test]
    fn reset_modes_update_head_index_and_dirty_state() {
        for (mode, expect_staged, expect_dirty) in [
            (ResetMode::Soft, true, true),
            (ResetMode::Mixed, false, true),
            (ResetMode::Hard, false, false),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let repo = Repository::init(tmp.path()).unwrap();
            let volume = VolumeId::random();
            let log = LogId::random();
            let first_snapshot =
                Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
            let second_snapshot =
                Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(4), PageCount::new(4));
            let staged_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(6), PageCount::new(5));

            let first = repo
                .commit_file(
                    tmp.path().join("app.db"),
                    "first",
                    volume.clone(),
                    &first_snapshot,
                )
                .unwrap();
            repo.commit_file(
                tmp.path().join("app.db"),
                "second",
                volume.clone(),
                &second_snapshot,
            )
            .unwrap();
            repo.stage_file(tmp.path().join("app.db"), volume, &staged_snapshot)
                .unwrap();
            repo.mark_dirty_path(tmp.path().join("app.db")).unwrap();

            let outcome = repo.reset("HEAD~1", mode).unwrap();

            assert_eq!(outcome.target, first.id);
            assert_eq!(outcome.mode, mode);
            assert_eq!(repo.status().unwrap().head_target, Some(outcome.target));
            assert_eq!(repo.has_staged_changes().unwrap(), expect_staged);
            assert_eq!(repo.is_dirty(), expect_dirty);
        }
    }

    #[test]
    fn reset_plan_freezes_target_before_branch_moves() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let app = tmp.path().join("app.db");
        let base_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let feature_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(4), PageCount::new(4));
        let later_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(6), PageCount::new(5));

        let base = repo
            .commit_file(&app, "base", volume.clone(), &base_snapshot)
            .unwrap();
        repo.switch_new_branch("feature/search", None).unwrap();
        let feature = repo
            .commit_file(&app, "feature", volume.clone(), &feature_snapshot)
            .unwrap();
        repo.switch_branch("main").unwrap();

        let plan = repo.plan_reset("feature/search", ResetMode::Hard).unwrap();
        assert_eq!(plan.target, feature.id);
        assert_eq!(plan.checkout.target, Some(feature.id.clone()));

        repo.switch_branch("feature/search").unwrap();
        let later = repo
            .commit_file(&app, "feature later", volume, &later_snapshot)
            .unwrap();
        repo.switch_branch("main").unwrap();

        let outcome = repo.apply_reset_plan(&plan).unwrap();
        assert_eq!(outcome.target, feature.id);
        assert_eq!(repo.branch_target("main").unwrap(), Some(feature.id));
        assert_eq!(
            repo.branch_target("feature/search").unwrap(),
            Some(later.id)
        );
        assert_ne!(repo.branch_target("main").unwrap(), Some(base.id));
    }

    #[test]
    fn branch_switch_and_remote_tracking_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let first = repo.commit("initial database").unwrap();

        let branch = repo.switch_new_branch("feature/search", None).unwrap();
        assert!(branch.current);
        assert_eq!(branch.target, Some(first.id.clone()));
        assert_eq!(repo.head().unwrap(), Head::branch("feature/search"));

        repo.switch_branch("main").unwrap();
        assert_eq!(repo.head().unwrap(), Head::branch("main"));

        repo.remote_add(
            "origin",
            RemoteConfig::Fs {
                root: tmp.path().join("remote").to_string_lossy().into_owned(),
            },
        )
        .unwrap();
        repo.set_remote_tracking_ref("origin", "main", &first.id)
            .unwrap();

        assert_eq!(
            repo.remote_tracking_ref("origin", "main").unwrap(),
            Some(first.id)
        );
        assert_eq!(repo.remotes().unwrap().len(), 1);
    }

    #[test]
    fn branch_upstream_config_drives_default_remote_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        repo.commit("initial database").unwrap();
        repo.remote_add(
            "origin",
            RemoteConfig::Fs {
                root: tmp.path().join("remote").to_string_lossy().into_owned(),
            },
        )
        .unwrap();

        let branch = repo.set_branch_upstream("main", "origin", "trunk").unwrap();
        assert_eq!(
            branch.upstream,
            Some(BranchUpstream {
                remote: "origin".to_string(),
                branch: "trunk".to_string(),
            })
        );
        assert_eq!(repo.status().unwrap().upstream, branch.upstream);
        assert_eq!(
            repo.default_remote_branch(None, None).unwrap(),
            BranchUpstream {
                remote: "origin".to_string(),
                branch: "trunk".to_string(),
            }
        );
        assert_eq!(
            repo.default_remote_branch(Some("origin"), None).unwrap(),
            BranchUpstream {
                remote: "origin".to_string(),
                branch: "main".to_string(),
            }
        );

        let config = repo.config().unwrap();
        assert_eq!(
            config.branches["main"].merge.as_deref(),
            Some("refs/heads/trunk")
        );

        let branch = repo.unset_branch_upstream("main").unwrap();
        assert_eq!(branch.upstream, None);
        assert_eq!(
            repo.default_remote_branch(None, None).unwrap(),
            BranchUpstream {
                remote: "origin".to_string(),
                branch: "main".to_string(),
            }
        );
    }

    #[test]
    fn branch_create_resolves_start_point_and_rejects_existing_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let first = repo.commit("initial database").unwrap();
        let second = repo.commit("second database").unwrap();

        let branch = repo.branch_create("release/1.0", Some("HEAD~1")).unwrap();
        assert_eq!(branch.name, "release/1.0");
        assert_eq!(branch.target, Some(first.id.clone()));
        assert_eq!(repo.resolve_revision("release/1.0").unwrap(), first.id);
        assert_eq!(repo.resolve_revision("HEAD").unwrap(), second.id);
        assert!(matches!(
            repo.branch_create("release/1.0", None),
            Err(RepoErr::BranchExists(name)) if name == "release/1.0"
        ));
    }

    #[test]
    fn branch_rename_moves_current_branch_config_and_reflog() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let first = repo.commit("initial database").unwrap();

        repo.branch_create("feature/search", None).unwrap();
        repo.remote_add(
            "origin",
            RemoteConfig::Fs {
                root: tmp.path().join("remote").to_string_lossy().into_owned(),
            },
        )
        .unwrap();
        repo.set_branch_upstream("feature/search", "origin", "feature/search")
            .unwrap();
        repo.switch_branch("feature/search").unwrap();

        let renamed = repo
            .branch_rename("feature/search", "topic/search", false)
            .unwrap();
        assert!(renamed.current);
        assert_eq!(renamed.name, "topic/search");
        assert_eq!(renamed.target, Some(first.id.clone()));
        assert_eq!(
            renamed.upstream,
            Some(BranchUpstream {
                remote: "origin".to_string(),
                branch: "feature/search".to_string(),
            })
        );
        assert_eq!(repo.head().unwrap(), Head::branch("topic/search"));
        assert_eq!(repo.branch_target("topic/search").unwrap(), Some(first.id));
        assert!(
            !repo
                .graft_dir
                .join(DIR_REFS_HEADS)
                .join("feature/search")
                .exists()
        );
        assert!(
            repo.graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs/heads/topic/search")
                .is_file()
        );
        assert!(
            !repo
                .graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs/heads/feature/search")
                .exists()
        );
    }

    #[test]
    fn branch_rename_force_overwrites_existing_branch_and_ref_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let first = repo.commit("initial database").unwrap();
        let second = repo.commit("second database").unwrap();

        repo.branch_create("feature", Some(&first.id)).unwrap();
        repo.branch_rename("feature", "feature/search", false)
            .unwrap();
        assert_eq!(
            repo.branch_target("feature/search").unwrap(),
            Some(first.id.clone())
        );

        repo.branch_create("release/next", Some(&second.id))
            .unwrap();
        repo.branch_rename("feature/search", "release/next", true)
            .unwrap();
        assert_eq!(repo.branch_target("release/next").unwrap(), Some(first.id));
        assert!(!repo.graft_dir.join(DIR_REFS_HEADS).join("feature").exists());
        assert!(
            !repo
                .graft_dir
                .join(DIR_REFS_HEADS)
                .join("feature/search")
                .exists()
        );
    }

    #[test]
    fn branch_rename_current_unborn_branch_updates_head() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        let renamed = repo.branch_rename("main", "trunk", false).unwrap();
        assert_eq!(renamed.name, "trunk");
        assert!(renamed.current);
        assert_eq!(renamed.target, None);
        assert_eq!(repo.head().unwrap(), Head::branch("trunk"));
    }

    #[test]
    fn refs_reject_file_directory_namespace_conflicts() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let first = repo.commit("initial database").unwrap();

        repo.branch_create("feature/search", None).unwrap();
        assert!(matches!(
            repo.branch_create("feature", None),
            Err(RepoErr::RefNameConflict { reference, existing })
                if reference == "refs/heads/feature" && existing == "refs/heads/feature"
        ));

        repo.branch_create("release", None).unwrap();
        assert!(matches!(
            repo.branch_create("release/1.0", None),
            Err(RepoErr::RefNameConflict { reference, existing })
                if reference == "refs/heads/release/1.0" && existing == "refs/heads/release"
        ));

        repo.tag_create("v1/rc1", None).unwrap();
        assert!(matches!(
            repo.tag_create("v1", None),
            Err(RepoErr::RefNameConflict { reference, existing })
                if reference == "refs/tags/v1" && existing == "refs/tags/v1"
        ));

        repo.tag_create("stable", None).unwrap();
        assert!(matches!(
            repo.tag_create("stable/rc1", None),
            Err(RepoErr::RefNameConflict { reference, existing })
                if reference == "refs/tags/stable/rc1" && existing == "refs/tags/stable"
        ));

        repo.set_remote_tracking_ref("origin", "topic/search", &first.id)
            .unwrap();
        assert!(matches!(
            repo.set_remote_tracking_ref("origin", "topic", &first.id),
            Err(RepoErr::RefNameConflict { reference, existing })
                if reference == "refs/remotes/origin/topic" && existing == "refs/remotes/origin/topic"
        ));

        repo.set_remote_tracking_ref("origin", "main", &first.id)
            .unwrap();
        assert!(matches!(
            repo.set_remote_tracking_ref("origin", "main/v2", &first.id),
            Err(RepoErr::RefNameConflict { reference, existing })
                if reference == "refs/remotes/origin/main/v2" && existing == "refs/remotes/origin/main"
        ));
    }

    #[test]
    fn remote_remove_deletes_config_and_tracking_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let first = repo.commit("initial database").unwrap();

        let config = RemoteConfig::Fs {
            root: tmp.path().join("remote").to_string_lossy().into_owned(),
        };
        repo.remote_add("origin", config).unwrap();
        repo.set_branch_upstream("main", "origin", "main").unwrap();
        assert!(repo.branch_upstream("main").unwrap().is_some());
        repo.set_remote_tracking_ref("origin", "main", &first.id)
            .unwrap();
        assert_eq!(
            repo.remote_tracking_ref("origin", "main").unwrap(),
            Some(first.id)
        );

        let removed = repo.remote_remove("origin").unwrap();
        assert_eq!(removed.name, "origin");
        assert!(matches!(removed.config, RemoteConfig::Fs { .. }));
        assert!(repo.remotes().unwrap().is_empty());
        assert_eq!(repo.branch_upstream("main").unwrap(), None);
        assert_eq!(repo.remote_tracking_ref("origin", "main").unwrap(), None);
        assert!(matches!(
            repo.remote_store("origin"),
            Err(RepoErr::RemoteNotFound(name)) if name == "origin"
        ));
        assert!(matches!(
            repo.remote_remove("origin"),
            Err(RepoErr::RemoteNotFound(name)) if name == "origin"
        ));
    }

    #[test]
    fn remote_rename_moves_config_tracking_refs_reflogs_and_upstreams() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let first = repo.commit("initial database").unwrap();

        let config = RemoteConfig::Fs {
            root: tmp.path().join("remote").to_string_lossy().into_owned(),
        };
        repo.remote_add("origin", config).unwrap();
        repo.set_branch_upstream("main", "origin", "main").unwrap();
        repo.set_remote_tracking_ref("origin", "main", &first.id)
            .unwrap();
        assert!(
            repo.graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs/remotes/origin/main")
                .is_file()
        );

        let renamed = repo.remote_rename("origin", "upstream").unwrap();
        assert_eq!(renamed.name, "upstream");
        assert!(matches!(renamed.config, RemoteConfig::Fs { .. }));
        assert!(repo.remote_store("upstream").is_ok());
        assert!(matches!(
            repo.remote_store("origin"),
            Err(RepoErr::RemoteNotFound(name)) if name == "origin"
        ));
        assert_eq!(
            repo.branch_upstream("main").unwrap(),
            Some(BranchUpstream {
                remote: "upstream".to_string(),
                branch: "main".to_string(),
            })
        );
        assert_eq!(
            repo.remote_tracking_ref("upstream", "main").unwrap(),
            Some(first.id)
        );
        assert_eq!(repo.remote_tracking_ref("origin", "main").unwrap(), None);
        assert!(
            repo.graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs/remotes/upstream/main")
                .is_file()
        );
        assert!(
            !repo
                .graft_dir
                .join(DIR_LOGS_REFS)
                .join("refs/remotes/origin")
                .exists()
        );
        assert!(matches!(
            repo.remote_rename("upstream", "upstream"),
            Ok(RemoteInfo { name, .. }) if name == "upstream"
        ));
        repo.remote_add("backup", RemoteConfig::Memory).unwrap();
        assert!(matches!(
            repo.remote_rename("upstream", "backup"),
            Err(RepoErr::RemoteExists(name)) if name == "backup"
        ));
    }

    #[test]
    fn remote_set_url_updates_config_without_touching_refs_or_upstreams() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let first = repo.commit("initial database").unwrap();

        let original = RemoteConfig::Fs {
            root: tmp.path().join("remote-a").to_string_lossy().into_owned(),
        };
        let updated = RemoteConfig::Fs {
            root: tmp.path().join("remote-b").to_string_lossy().into_owned(),
        };
        repo.remote_add("origin", original.clone()).unwrap();
        repo.set_branch_upstream("main", "origin", "main").unwrap();
        repo.set_remote_tracking_ref("origin", "main", &first.id)
            .unwrap();

        assert_eq!(repo.remote_get_url("origin").unwrap().config, original);
        let info = repo.remote_set_url("origin", updated.clone()).unwrap();
        assert_eq!(info.name, "origin");
        assert_eq!(info.config, updated);
        assert_eq!(repo.remote_get_url("origin").unwrap().config, updated);
        assert_eq!(
            repo.branch_upstream("main").unwrap(),
            Some(BranchUpstream {
                remote: "origin".to_string(),
                branch: "main".to_string(),
            })
        );
        assert_eq!(
            repo.remote_tracking_ref("origin", "main").unwrap(),
            Some(first.id)
        );
        assert!(matches!(
            repo.remote_set_url("missing", RemoteConfig::Memory),
            Err(RepoErr::RemoteNotFound(name)) if name == "missing"
        ));
        assert!(matches!(
            repo.remote_get_url("../origin"),
            Err(RepoErr::InvalidRemoteName(name)) if name == "../origin"
        ));
        assert!(matches!(
            repo.remote_store("../origin"),
            Err(RepoErr::InvalidRemoteName(name)) if name == "../origin"
        ));
    }

    #[test]
    fn branch_delete_removes_merged_branch_and_rejects_current_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let first = repo.commit("initial database").unwrap();

        repo.branch_create("feature/search", None).unwrap();
        assert!(repo.branch_exists("feature/search"));

        assert!(matches!(
            repo.branch_delete("main", false),
            Err(RepoErr::BranchIsCurrent(name)) if name == "main"
        ));

        let deleted = repo.branch_delete("feature/search", false).unwrap();
        assert_eq!(deleted.name, "feature/search");
        assert_eq!(deleted.target, Some(first.id));
        assert!(!repo.branch_exists("feature/search"));
        assert!(matches!(
            repo.switch_branch("feature/search"),
            Err(RepoErr::BranchNotFound(name)) if name == "feature/search"
        ));
        assert!(matches!(
            repo.switch_branch("feature"),
            Err(RepoErr::BranchNotFound(name)) if name == "feature"
        ));
    }

    #[test]
    fn branch_delete_requires_force_for_unmerged_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let app = tmp.path().join("app.db");
        let base_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let feature_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(3), PageCount::new(4));

        repo.commit_file(&app, "base", volume.clone(), &base_snapshot)
            .unwrap();
        repo.switch_new_branch("feature/search", None).unwrap();
        let feature = repo
            .commit_file(&app, "feature", volume, &feature_snapshot)
            .unwrap();
        repo.switch_branch("main").unwrap();

        assert!(matches!(
            repo.branch_delete("feature/search", false),
            Err(RepoErr::BranchNotMerged { branch, target })
                if branch == "feature/search" && target == feature.id
        ));

        let deleted = repo.branch_delete("feature/search", true).unwrap();
        assert_eq!(deleted.target, Some(feature.id));
        assert!(!repo.branch_exists("feature/search"));
    }

    #[test]
    fn ref_names_reject_git_revision_syntax_and_path_hazards() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        for name in [
            "-topic",
            ".topic",
            "topic.",
            "topic.lock",
            "feature/.hidden",
            "feature/topic.lock",
            "feature//topic",
            "feature/../topic",
            "topic..next",
            "topic name",
            "topic\tname",
            "topic~1",
            "topic^1",
            "topic:bad",
            "topic?bad",
            "topic*bad",
            "topic[bad",
            "topic\\bad",
            "topic@{1}",
            "@",
        ] {
            assert!(
                matches!(repo.branch_create_unborn(name), Err(RepoErr::InvalidRefName(actual)) if actual == name),
                "expected invalid branch name `{name}`"
            );
        }
    }

    #[test]
    fn remote_names_reject_ref_path_and_revision_hazards() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        for name in [
            "-origin",
            "origin.",
            "origin.lock",
            "up/stream",
            "up\\stream",
            "up..stream",
            "up stream",
            "up\tstream",
            "origin~1",
            "origin^1",
            "origin:bad",
            "origin?bad",
            "origin*bad",
            "origin[bad",
            "origin@{1}",
            "@",
        ] {
            assert!(
                matches!(
                    repo.remote_add(name, RemoteConfig::Memory),
                    Err(RepoErr::InvalidRemoteName(actual)) if actual == name
                ),
                "expected invalid remote name `{name}`"
            );
        }
    }

    #[test]
    fn merge_revision_stages_clean_three_way_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let app = tmp.path().join("app.db");
        let notes = tmp.path().join("notes.db");
        let app_base = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let app_main = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(3), PageCount::new(4));
        let notes_base = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(4), PageCount::new(5));
        let notes_feature = Snapshot::new(log, LSN::FIRST..=LSN::new(5), PageCount::new(6));

        repo.stage_file(&app, volume.clone(), &app_base).unwrap();
        repo.stage_file(&notes, volume.clone(), &notes_base)
            .unwrap();
        let base = repo.commit_staged("base").unwrap();
        repo.switch_new_branch("feature/search", None).unwrap();
        let feature = repo
            .commit_file(&notes, "feature notes", volume.clone(), &notes_feature)
            .unwrap();
        repo.switch_branch("main").unwrap();
        let main = repo
            .commit_file(&app, "main app", volume, &app_main)
            .unwrap();

        let outcome = repo.merge_revision("feature/search").unwrap();

        assert_eq!(
            outcome,
            MergeOutcome::Merged {
                head: main.id,
                target: feature.id,
                merge_base: Some(base.id),
                staged: vec!["notes.db".to_string()],
                conflicted: vec![],
            }
        );
        let status = repo.status().unwrap();
        assert_eq!(status.staged, vec!["notes.db".to_string()]);
        assert!(status.conflicted.is_empty());
        assert!(!repo.read_index().unwrap().has_conflicts());
    }

    #[test]
    fn merge_plan_freezes_target_before_branch_moves() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let app = tmp.path().join("app.db");
        let base_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let feature_snapshot =
            Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(4), PageCount::new(4));
        let later_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(6), PageCount::new(5));

        let base = repo
            .commit_file(&app, "base", volume.clone(), &base_snapshot)
            .unwrap();
        repo.switch_new_branch("feature/search", None).unwrap();
        let feature = repo
            .commit_file(&app, "feature", volume.clone(), &feature_snapshot)
            .unwrap();
        repo.switch_branch("main").unwrap();

        let plan = repo.plan_merge_revision("feature/search").unwrap();
        assert_eq!(plan.target, feature.id);
        assert_eq!(
            plan.outcome,
            MergeOutcome::FastForward {
                from: Some(base.id.clone()),
                to: feature.id.clone()
            }
        );

        repo.switch_branch("feature/search").unwrap();
        let later = repo
            .commit_file(&app, "feature later", volume, &later_snapshot)
            .unwrap();
        repo.switch_branch("main").unwrap();

        let outcome = repo.apply_merge_plan(&plan).unwrap();
        assert_eq!(
            outcome,
            MergeOutcome::FastForward {
                from: Some(base.id),
                to: feature.id.clone()
            }
        );
        assert_eq!(repo.branch_target("main").unwrap(), Some(feature.id));
        assert_eq!(
            repo.branch_target("feature/search").unwrap(),
            Some(later.id)
        );
    }

    #[test]
    fn merge_revision_stages_clean_delete_from_theirs() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let app = tmp.path().join("app.db");
        let notes = tmp.path().join("notes.db");
        let app_base = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let notes_base = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(3), PageCount::new(4));
        let app_main = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(5));

        repo.stage_file(&app, volume.clone(), &app_base).unwrap();
        repo.stage_file(&notes, volume.clone(), &notes_base)
            .unwrap();
        let base = repo.commit_staged("base").unwrap();
        repo.switch_new_branch("feature/delete-notes", None)
            .unwrap();
        let removal = repo.stage_file_removal(&notes).unwrap();
        assert!(removal.file.is_none());
        let feature = repo.commit_staged("remove notes").unwrap();
        repo.switch_branch("main").unwrap();
        let main = repo
            .commit_file(&app, "main app", volume, &app_main)
            .unwrap();

        let outcome = repo.merge_revision("feature/delete-notes").unwrap();

        assert_eq!(
            outcome,
            MergeOutcome::Merged {
                head: main.id,
                target: feature.id,
                merge_base: Some(base.id),
                staged: vec!["notes.db".to_string()],
                conflicted: vec![],
            }
        );
        let staged = repo.diff_staged(Some("notes.db")).unwrap();
        assert_eq!(staged.files.len(), 1);
        assert_eq!(staged.files[0].change, RepoFileChange::Deleted);
        assert!(staged.files[0].to.is_none());
    }

    #[test]
    fn merge_revision_writes_conflict_stages_and_blocks_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let app = tmp.path().join("app.db");
        let base = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let ours = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(3), PageCount::new(4));
        let theirs = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(5));

        let base_commit = repo
            .commit_file(&app, "base", volume.clone(), &base)
            .unwrap();
        repo.switch_new_branch("feature/search", None).unwrap();
        let feature = repo
            .commit_file(&app, "feature", volume.clone(), &theirs)
            .unwrap();
        repo.switch_branch("main").unwrap();
        let main = repo.commit_file(&app, "main", volume, &ours).unwrap();

        let outcome = repo.merge_revision("feature/search").unwrap();

        assert_eq!(
            outcome,
            MergeOutcome::Merged {
                head: main.id.clone(),
                target: feature.id.clone(),
                merge_base: Some(base_commit.id.clone()),
                staged: vec![],
                conflicted: vec!["app.db".to_string()],
            }
        );
        let index = repo.read_index().unwrap();
        assert!(index.has_conflicts());
        assert_eq!(index.conflicted_paths(), vec!["app.db".to_string()]);
        let stages: Vec<_> = index.entries.iter().map(|entry| entry.stage).collect();
        assert_eq!(
            stages,
            vec![
                index::IndexStage::Base,
                index::IndexStage::Ours,
                index::IndexStage::Theirs,
            ]
        );
        assert!(matches!(
            repo.commit_staged("merge feature"),
            Err(RepoErr::UnresolvedConflicts)
        ));

        repo.stage_file(&app, VolumeId::random(), &theirs).unwrap();
        assert!(!repo.read_index().unwrap().has_conflicts());
        let merge_commit = repo.commit_staged("merge feature").unwrap();
        assert_eq!(merge_commit.parent, Some(main.id.clone()));
        assert_eq!(
            merge_commit.parents,
            vec![main.id.clone(), feature.id.clone()]
        );
        assert_eq!(repo.resolve_revision("HEAD^").unwrap(), main.id);
        assert_eq!(repo.resolve_revision("HEAD^1").unwrap(), main.id);
        assert_eq!(repo.resolve_revision("HEAD^2").unwrap(), feature.id);
        assert_eq!(repo.resolve_revision("HEAD^0").unwrap(), merge_commit.id);
        let log_ids: Vec<_> = repo
            .log()
            .unwrap()
            .into_iter()
            .map(|commit| commit.id)
            .collect();
        assert_eq!(
            log_ids,
            vec![merge_commit.id.clone(), main.id, feature.id, base_commit.id]
        );
        let object::Object::Commit(commit_object) = repo.read_object(&merge_commit.id).unwrap()
        else {
            panic!("merge commit id should point at a commit object");
        };
        assert_eq!(commit_object.parents.len(), 2);
        assert!(repo.merge_head().unwrap().is_none());
    }

    #[test]
    fn merge_abort_restores_orig_head_and_clears_index() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let app = tmp.path().join("app.db");
        let base = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let ours = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(3), PageCount::new(4));
        let theirs = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(5));

        repo.commit_file(&app, "base", volume.clone(), &base)
            .unwrap();
        repo.switch_new_branch("feature/search", None).unwrap();
        repo.commit_file(&app, "feature", volume.clone(), &theirs)
            .unwrap();
        repo.switch_branch("main").unwrap();
        let main = repo.commit_file(&app, "main", volume, &ours).unwrap();

        repo.merge_revision("feature/search").unwrap();
        assert!(repo.read_index().unwrap().has_conflicts());

        let restored = repo.merge_abort().unwrap();

        assert_eq!(restored, main.id);
        assert_eq!(repo.status().unwrap().head_target, Some(restored));
        assert!(repo.read_index().unwrap().is_empty());
        assert!(repo.merge_head().unwrap().is_none());
    }

    #[test]
    fn merge_abort_plan_freezes_orig_head_before_merge_state_moves() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let volume = VolumeId::random();
        let log = LogId::random();
        let app = tmp.path().join("app.db");
        let base = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let ours = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(3), PageCount::new(4));
        let theirs = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(5));

        let base_commit = repo
            .commit_file(&app, "base", volume.clone(), &base)
            .unwrap();
        repo.switch_new_branch("feature/search", None).unwrap();
        let feature = repo
            .commit_file(&app, "feature", volume.clone(), &theirs)
            .unwrap();
        repo.switch_branch("main").unwrap();
        let main = repo.commit_file(&app, "main", volume, &ours).unwrap();

        repo.merge_revision("feature/search").unwrap();
        let plan = repo.plan_merge_abort().unwrap();
        assert_eq!(plan.target, main.id);
        assert_eq!(
            plan.checkout
                .files
                .get("app.db")
                .expect("planned app.db")
                .snapshot
                .to_snapshot()
                .head(),
            ours.head()
        );

        repo.write_merge_state(&feature.id, &base_commit.id)
            .unwrap();
        let restored = repo.apply_merge_abort_plan(&plan).unwrap();

        assert_eq!(restored, main.id);
        assert_eq!(repo.status().unwrap().head_target, Some(restored));
        assert!(repo.read_index().unwrap().is_empty());
        assert!(repo.merge_head().unwrap().is_none());
    }

    #[test]
    fn push_and_fetch_roundtrip_named_remote_refs() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote.clone()).unwrap();
        let first = source.commit("initial database").unwrap();
        let second = source.commit("add table").unwrap();

        let push = source.push("origin", "main").unwrap();
        assert_eq!(push.head, second.id);
        assert_eq!(push.commits, 2);
        assert_eq!(
            source.remote_tracking_ref("origin", "main").unwrap(),
            Some(second.id.clone())
        );
        assert_eq!(
            fs::read_to_string(remote_dir.path().join("HEAD")).unwrap(),
            "ref: refs/heads/main\n"
        );
        assert_eq!(
            source.remote_default_branch("origin").unwrap().as_deref(),
            Some("main")
        );
        let second_oid = object::ObjectId::from_str(&second.id).unwrap();
        assert!(
            !remote_dir
                .path()
                .join(object::LooseObjectStore::relative_path(&second_oid))
                .is_file()
        );
        let pack_dir = remote_dir.path().join(DIR_OBJECTS_PACK);
        assert!(fs::read_dir(&pack_dir).unwrap().any(|entry| {
            entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "pack")
        }));
        assert!(fs::read_dir(&pack_dir).unwrap().any(|entry| {
            entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "idx")
        }));
        assert!(!remote_dir.path().join("objects/commits").exists());

        let clone_dir = tempfile::tempdir().unwrap();
        let clone = Repository::init(clone_dir.path()).unwrap();
        clone.remote_add("origin", remote).unwrap();

        let fetch = clone.fetch("origin", "main").unwrap();
        assert_eq!(fetch.head, second.id);
        assert_eq!(fetch.commits, 2);
        assert_eq!(
            clone.remote_tracking_ref("origin", "main").unwrap(),
            Some(second.id.clone())
        );
        assert_eq!(
            clone.read_commit(&first.id).unwrap().message,
            "initial database"
        );
        assert_eq!(
            clone.read_commit(&second.id).unwrap().parent,
            Some(first.id)
        );
        assert!(clone.object_store().path_for(&second_oid).is_file());
        let object::Object::Commit(commit_object) = clone.read_object(&second.id).unwrap() else {
            panic!("fetch should hydrate canonical commit object");
        };
        let object::Object::Tree(_) = clone.read_object(commit_object.tree.as_str()).unwrap()
        else {
            panic!("fetch should hydrate canonical tree object");
        };
        assert!(!clone.graft_dir().join("objects/commits").exists());
    }

    #[test]
    fn push_and_fetch_roundtrip_large_artifact_payloads() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote.clone()).unwrap();
        let asset = source_dir.path().join("assets/model.bin");
        fs::create_dir_all(asset.parent().unwrap()).unwrap();
        let bytes = b"large model payload";
        fs::write(&asset, bytes).unwrap();
        let state = source
            .stage_artifact_path_with_inline_text_threshold(&asset, 4)
            .unwrap()
            .artifact
            .expect("large artifact staged");
        assert!(state.is_large());
        let commit = source.commit_staged("track model").unwrap();

        let push = source.push("origin", "main").unwrap();
        assert_eq!(push.head, commit.id);
        assert_eq!(push.commits, 1);
        assert_eq!(
            fs::read(
                remote_dir
                    .path()
                    .join(large_file_content_relative_path(state.content_hash()))
            )
            .unwrap(),
            bytes
        );

        let clone_dir = tempfile::tempdir().unwrap();
        let clone = Repository::init(clone_dir.path()).unwrap();
        clone.remote_add("origin", remote).unwrap();
        let fetch = clone.fetch("origin", "main").unwrap();
        assert_eq!(fetch.head, commit.id);
        assert_eq!(fetch.commits, 1);
        let cloned_state = clone
            .read_commit(&commit.id)
            .unwrap()
            .artifacts
            .get("assets/model.bin")
            .cloned()
            .expect("fetched artifact state");
        assert_eq!(cloned_state, state);
        assert!(
            clone
                .file_store_dir()
                .join(&state.content_hash().as_str()[..2])
                .join(&state.content_hash().as_str()[2..])
                .is_file()
        );

        let materialized = clone_dir.path().join("assets/model.bin");
        clone
            .materialize_artifact_key("assets/model.bin", &cloned_state)
            .unwrap();
        assert_eq!(fs::read(materialized).unwrap(), bytes);
    }

    #[test]
    fn repair_artifacts_from_remote_hydrates_missing_large_artifact_parts() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote.clone()).unwrap();
        let asset = source_dir.path().join("assets/model.bin");
        fs::create_dir_all(asset.parent().unwrap()).unwrap();
        let bytes = b"large repair payload";
        fs::write(&asset, bytes).unwrap();
        let state = source
            .stage_artifact_path_with_inline_text_threshold(&asset, 4)
            .unwrap()
            .artifact
            .expect("large artifact staged");
        let commit = source.commit_staged("track model").unwrap();
        source.push("origin", "main").unwrap();

        let clone_dir = tempfile::tempdir().unwrap();
        let clone = Repository::init(clone_dir.path()).unwrap();
        clone.remote_add("origin", remote).unwrap();
        clone.fetch("origin", "main").unwrap();
        let checkout = clone
            .checkout_artifact_key_from_revision("origin/main", "assets/model.bin")
            .unwrap();
        assert_eq!(checkout.target, commit.id);

        fs::remove_file(clone.object_store().path_for(state.oid())).unwrap();
        fs::remove_file(clone.large_file_content_path(state.content_hash())).unwrap();
        let broken = clone.audit_artifacts().unwrap();
        assert_eq!(broken.issues.len(), 2);
        assert!(broken.issues.iter().any(|issue| {
            issue.path == "assets/model.bin"
                && issue.kind == RepoArtifactAuditIssueKind::MissingObject
        }));
        assert!(broken.issues.iter().any(|issue| {
            issue.path == "assets/model.bin"
                && issue.kind == RepoArtifactAuditIssueKind::MissingExternalPayload
        }));

        let repaired = clone.repair_artifacts_from_remote("origin").unwrap();
        assert_eq!(repaired.remote, "origin");
        assert_eq!(repaired.fetched_objects, 1);
        assert_eq!(repaired.fetched_external_payloads, 1);
        assert_eq!(repaired.before, broken);
        assert!(repaired.after.ok());

        clone
            .materialize_artifact_key("assets/model.bin", &state)
            .unwrap();
        assert_eq!(
            fs::read(clone_dir.path().join("assets/model.bin")).unwrap(),
            bytes
        );
    }

    #[test]
    fn fetch_large_file_payloads_hydrates_missing_payloads_for_revision() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote.clone()).unwrap();
        let asset = source_dir.path().join("assets/model.bin");
        fs::create_dir_all(asset.parent().unwrap()).unwrap();
        let bytes = b"external fetch payload";
        fs::write(&asset, bytes).unwrap();
        let state = source
            .stage_artifact_path_with_inline_text_threshold(&asset, 4)
            .unwrap()
            .artifact
            .expect("large artifact staged");
        let commit = source.commit_staged("track model").unwrap();
        source.push("origin", "main").unwrap();

        let clone_dir = tempfile::tempdir().unwrap();
        let clone = Repository::init(clone_dir.path()).unwrap();
        clone.remote_add("origin", remote).unwrap();
        clone.fetch("origin", "main").unwrap();
        fs::remove_file(clone.large_file_content_path(state.content_hash())).unwrap();

        let fetched = clone
            .fetch_large_file_payloads("origin", Some("origin/main"))
            .unwrap();
        assert_eq!(fetched.remote, "origin");
        assert_eq!(fetched.target, commit.id);
        assert_eq!(fetched.external_payloads, 1);
        assert_eq!(fetched.already_present_payloads, 0);
        assert_eq!(fetched.fetched_payloads, 1);
        assert_eq!(fetched.fetched_bytes, bytes.len() as u64);
        assert_eq!(fetched.files[0].content_hash, *state.content_hash());
        assert_eq!(fetched.files[0].size, bytes.len() as u64);
        assert_eq!(fetched.files[0].status, RepoLargeFileFetchStatus::Fetched);
        assert_eq!(fetched.files[0].paths, vec!["assets/model.bin"]);
        assert_eq!(
            fs::read(clone.large_file_content_path(state.content_hash())).unwrap(),
            bytes
        );

        let present = clone
            .fetch_large_file_payloads("origin", Some("origin/main"))
            .unwrap();
        assert_eq!(present.already_present_payloads, 1);
        assert_eq!(present.fetched_payloads, 0);
        assert_eq!(present.fetched_bytes, 0);
        assert_eq!(present.files[0].status, RepoLargeFileFetchStatus::Present);
    }

    #[test]
    fn large_file_payloads_status_reports_present_missing_and_invalid_payloads() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let assets = tmp.path().join("assets");
        fs::create_dir_all(&assets).unwrap();
        fs::write(assets.join("present.bin"), b"large present payload").unwrap();
        fs::write(assets.join("missing.bin"), b"large missing payload").unwrap();
        fs::write(assets.join("invalid.bin"), b"large invalid payload").unwrap();

        let present_state = repo
            .stage_artifact_path_with_inline_text_threshold(assets.join("present.bin"), 4)
            .unwrap()
            .artifact
            .expect("present artifact staged");
        let missing_state = repo
            .stage_artifact_path_with_inline_text_threshold(assets.join("missing.bin"), 4)
            .unwrap()
            .artifact
            .expect("missing artifact staged");
        let invalid_state = repo
            .stage_artifact_path_with_inline_text_threshold(assets.join("invalid.bin"), 4)
            .unwrap()
            .artifact
            .expect("invalid artifact staged");
        let commit = repo.commit_staged("track payloads").unwrap();

        fs::remove_file(repo.large_file_content_path(missing_state.content_hash())).unwrap();
        fs::write(
            repo.large_file_content_path(invalid_state.content_hash()),
            b"corrupt payload",
        )
        .unwrap();

        let status = repo.large_file_payloads_status(Some("HEAD")).unwrap();
        assert_eq!(status.target, commit.id);
        assert_eq!(status.external_payloads, 3);
        assert_eq!(status.present_payloads, 1);
        assert_eq!(status.missing_payloads, 1);
        assert_eq!(status.invalid_payloads, 1);
        assert_eq!(status.present_bytes, present_state.size());
        assert_eq!(status.missing_bytes, missing_state.size());
        assert_eq!(status.invalid_bytes, invalid_state.size());

        let present = status
            .files
            .iter()
            .find(|entry| entry.paths.iter().any(|path| path == "assets/present.bin"))
            .unwrap();
        assert_eq!(present.status, RepoLargeFileStatusState::Present);
        assert_eq!(present.message, None);

        let missing = status
            .files
            .iter()
            .find(|entry| entry.paths.iter().any(|path| path == "assets/missing.bin"))
            .unwrap();
        assert_eq!(missing.status, RepoLargeFileStatusState::Missing);
        assert!(
            missing
                .message
                .as_deref()
                .unwrap()
                .contains("missing external payload")
        );

        let invalid = status
            .files
            .iter()
            .find(|entry| entry.paths.iter().any(|path| path == "assets/invalid.bin"))
            .unwrap();
        assert_eq!(invalid.status, RepoLargeFileStatusState::Invalid);
        assert!(
            invalid
                .message
                .as_deref()
                .unwrap()
                .contains("external payload")
        );
    }

    #[test]
    fn prune_large_file_payloads_removes_only_unreferenced_payloads() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let asset = tmp.path().join("assets/model.bin");
        fs::create_dir_all(asset.parent().unwrap()).unwrap();

        fs::write(&asset, b"large model v1").unwrap();
        let first_state = repo
            .stage_artifact_path_with_inline_text_threshold(&asset, 4)
            .unwrap()
            .artifact
            .expect("first large artifact staged");
        repo.commit_staged("track first model").unwrap();

        fs::write(&asset, b"large model v2").unwrap();
        let second_state = repo
            .stage_artifact_path_with_inline_text_threshold(&asset, 4)
            .unwrap()
            .artifact
            .expect("second large artifact staged");
        repo.commit_staged("track second model").unwrap();

        let staged = tmp.path().join("assets/staged.bin");
        fs::write(&staged, b"large staged payload").unwrap();
        let staged_state = repo
            .stage_artifact_path_with_inline_text_threshold(&staged, 4)
            .unwrap()
            .artifact
            .expect("staged large artifact");

        let orphan_bytes = b"orphan large payload";
        let orphan = object::ObjectId::for_bytes(orphan_bytes);
        repo.write_large_file_content(&orphan, orphan_bytes)
            .unwrap();

        let dry_run = repo.prune_large_file_payloads(true).unwrap();
        assert!(dry_run.dry_run);
        assert_eq!(dry_run.referenced_payloads, 3);
        assert_eq!(dry_run.candidate_payloads, 1);
        assert_eq!(dry_run.candidate_bytes, orphan_bytes.len() as u64);
        assert_eq!(dry_run.pruned_payloads, 0);
        assert_eq!(dry_run.files[0].content_hash, orphan);
        assert!(repo.large_file_content_path(&orphan).is_file());

        let pruned = repo.prune_large_file_payloads(false).unwrap();
        assert!(!pruned.dry_run);
        assert_eq!(pruned.referenced_payloads, 3);
        assert_eq!(pruned.candidate_payloads, 1);
        assert_eq!(pruned.pruned_payloads, 1);
        assert_eq!(pruned.pruned_bytes, orphan_bytes.len() as u64);
        assert!(!repo.large_file_content_path(&orphan).exists());
        assert!(
            repo.large_file_content_path(first_state.content_hash())
                .is_file()
        );
        assert!(
            repo.large_file_content_path(second_state.content_hash())
                .is_file()
        );
        assert!(
            repo.large_file_content_path(staged_state.content_hash())
                .is_file()
        );
    }

    #[test]
    fn push_noop_skips_remote_ref_lock() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote).unwrap();
        let commit = source.commit("initial database").unwrap();

        let first = source.push("origin", "main").unwrap();
        assert_eq!(first.head, commit.id);
        assert_eq!(first.commits, 1);

        let lock_path = remote_dir.path().join("locks/refs/heads/main.lock");
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        fs::write(&lock_path, "held\n").unwrap();

        let second = source.push("origin", "main").unwrap();
        assert_eq!(second.head, commit.id);
        assert_eq!(second.commits, 0);
        assert_eq!(
            source.remote_tracking_ref("origin", "main").unwrap(),
            Some(commit.id)
        );
    }

    #[test]
    fn force_push_overwrites_non_fast_forward_remote_ref() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote.clone()).unwrap();
        let base = source.commit("base").unwrap();
        source.push("origin", "main").unwrap();

        let other_dir = tempfile::tempdir().unwrap();
        let other = Repository::init(other_dir.path()).unwrap();
        other.remote_add("origin", remote).unwrap();
        other.fetch("origin", "main").unwrap();
        other.switch_branch("main").unwrap();
        other.reset(&base.id, ResetMode::Hard).unwrap();
        let remote_tip = other.commit("remote work").unwrap();
        other.push("origin", "main").unwrap();

        let local_tip = source.commit("local rewrite").unwrap();
        assert!(matches!(
            source.push("origin", "main"),
            Err(RepoErr::NonFastForward {
                remote,
                local_branch,
                remote_branch,
            }) if remote == "origin" && local_branch == "main" && remote_branch == "main"
        ));

        let push = source
            .push_branch_with_force("origin", "main", "main", true)
            .unwrap();

        assert!(push.forced);
        assert_eq!(push.head, local_tip.id);
        assert_eq!(
            source.remote_tracking_ref("origin", "main").unwrap(),
            Some(local_tip.id.clone())
        );
        assert_eq!(
            fs::read_to_string(remote_dir.path().join("refs/heads/main"))
                .unwrap()
                .trim(),
            local_tip.id
        );
        assert_ne!(remote_tip.id, local_tip.id);
    }

    #[test]
    fn push_rejects_when_remote_ref_lock_is_held() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote).unwrap();
        source.commit("initial database").unwrap();

        let lock_path = remote_dir.path().join("locks/refs/heads/main.lock");
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        fs::write(&lock_path, "held\n").unwrap();

        let err = source.push("origin", "main").unwrap_err();

        assert!(matches!(
            err,
            RepoErr::RemoteRefChanged { remote, branch }
                if remote == "origin" && branch == "main"
        ));
        assert_eq!(source.remote_tracking_ref("origin", "main").unwrap(), None);
        assert!(!remote_dir.path().join("refs/heads/main").exists());
    }

    #[test]
    fn push_delete_refspec_deletes_remote_branch_and_tracking_ref() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote).unwrap();
        let main = source.commit("initial database").unwrap();
        source.push("origin", "main").unwrap();
        assert_eq!(
            source.remote_tracking_ref("origin", "main").unwrap(),
            Some(main.id.clone())
        );
        assert!(remote_dir.path().join("refs/heads/main").is_file());

        let deleted = source
            .push_refspec_with_force("origin", ":main", false)
            .unwrap();
        assert_eq!(deleted.branches.len(), 1);
        let outcome = &deleted.branches[0];
        assert!(outcome.deleted);
        assert_eq!(outcome.remote_branch, "main");
        assert_eq!(outcome.head, main.id);
        assert_eq!(outcome.commits, 0);
        assert_eq!(source.remote_tracking_ref("origin", "main").unwrap(), None);
        assert!(!remote_dir.path().join("refs/heads/main").exists());
        assert!(matches!(
            source.push_refspec_with_force("origin", ":main", false),
            Err(RepoErr::RemoteBranchNotFound { remote, branch })
                if remote == "origin" && branch == "main"
        ));
    }

    #[test]
    fn push_all_and_fetch_all_sync_default_branch_refspecs() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote.clone()).unwrap();
        let base = source.commit("base").unwrap();
        source.switch_new_branch("feature/search", None).unwrap();
        let feature = source.commit("feature").unwrap();
        source.switch_branch("main").unwrap();
        let main = source.commit("main").unwrap();

        let push = source.push_all("origin").unwrap();
        assert_eq!(
            push.branches
                .iter()
                .map(|outcome| outcome.remote_branch.as_str())
                .collect::<Vec<_>>(),
            vec!["feature/search", "main"]
        );
        assert_eq!(
            source
                .remote_branch_refs("origin")
                .unwrap()
                .iter()
                .map(|reference| (reference.branch.as_str(), reference.head.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("feature/search", feature.id.as_str()),
                ("main", main.id.as_str())
            ]
        );

        let clone_dir = tempfile::tempdir().unwrap();
        let clone = Repository::init(clone_dir.path()).unwrap();
        clone.remote_add("origin", remote).unwrap();
        let fetch = clone.fetch_all("origin").unwrap();

        assert_eq!(
            fetch
                .branches
                .iter()
                .map(|outcome| outcome.branch.as_str())
                .collect::<Vec<_>>(),
            vec!["feature/search", "main"]
        );
        assert_eq!(
            clone
                .remote_tracking_ref("origin", "feature/search")
                .unwrap(),
            Some(feature.id.clone())
        );
        assert_eq!(
            clone.remote_tracking_ref("origin", "main").unwrap(),
            Some(main.id.clone())
        );
        assert_eq!(
            clone
                .remote_tracking_branches()
                .unwrap()
                .into_iter()
                .map(|reference| (reference.remote, reference.branch, reference.head))
                .collect::<Vec<_>>(),
            vec![
                (
                    "origin".to_string(),
                    "feature/search".to_string(),
                    feature.id.clone()
                ),
                ("origin".to_string(), "main".to_string(), main.id)
            ]
        );
        assert_eq!(clone.read_commit(&base.id).unwrap().message, "base");
        assert_eq!(
            clone.read_commit(&feature.id).unwrap().parent,
            Some(base.id)
        );
    }

    #[test]
    fn explicit_refspecs_map_push_and_fetch_branch_names() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote.clone()).unwrap();
        source.commit("base").unwrap();
        source.switch_new_branch("feature/search", None).unwrap();
        let feature = source.commit("feature").unwrap();

        let push = source
            .push_refspec_with_force(
                "origin",
                "refs/heads/feature/search:refs/heads/review/search",
                false,
            )
            .unwrap();

        assert_eq!(push.branches.len(), 1);
        assert_eq!(push.branches[0].local_branch, "feature/search");
        assert_eq!(push.branches[0].remote_branch, "review/search");
        assert_eq!(
            fs::read_to_string(remote_dir.path().join("refs/heads/review/search"))
                .unwrap()
                .trim(),
            feature.id
        );

        let clone_dir = tempfile::tempdir().unwrap();
        let clone = Repository::init(clone_dir.path()).unwrap();
        clone.remote_add("origin", remote).unwrap();
        let fetch = clone
            .fetch_refspec(
                "origin",
                "refs/heads/review/search:refs/remotes/origin/local/search",
            )
            .unwrap();

        assert_eq!(fetch.branches.len(), 1);
        assert_eq!(fetch.branches[0].branch, "local/search");
        assert_eq!(
            clone.remote_tracking_ref("origin", "local/search").unwrap(),
            Some(feature.id)
        );
        assert_eq!(
            clone
                .remote_tracking_ref("origin", "review/search")
                .unwrap(),
            None
        );
    }

    #[test]
    fn remote_prune_deletes_stale_tracking_refs() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote.clone()).unwrap();
        source.commit("base").unwrap();
        source.switch_new_branch("feature/prune", None).unwrap();
        let feature = source.commit("feature").unwrap();
        source.switch_branch("main").unwrap();
        let main = source.commit("main").unwrap();
        source.push_all("origin").unwrap();

        let clone_dir = tempfile::tempdir().unwrap();
        let clone = Repository::init(clone_dir.path()).unwrap();
        clone.remote_add("origin", remote).unwrap();
        clone.fetch_all("origin").unwrap();
        assert_eq!(
            clone
                .remote_tracking_ref("origin", "feature/prune")
                .unwrap(),
            Some(feature.id.clone())
        );
        assert_eq!(
            clone.remote_tracking_ref("origin", "main").unwrap(),
            Some(main.id.clone())
        );

        let deleted = source
            .push_refspec_with_force("origin", ":feature/prune", false)
            .unwrap();
        assert_eq!(deleted.branches[0].remote_branch, "feature/prune");
        assert!(deleted.branches[0].deleted);
        assert_eq!(
            clone
                .remote_tracking_ref("origin", "feature/prune")
                .unwrap(),
            Some(feature.id)
        );

        let pruned = clone.remote_prune("origin").unwrap();
        assert_eq!(pruned.remote, "origin");
        assert_eq!(pruned.branches, vec!["feature/prune"]);
        assert_eq!(
            clone
                .remote_tracking_ref("origin", "feature/prune")
                .unwrap(),
            None
        );
        assert_eq!(
            clone.remote_tracking_ref("origin", "main").unwrap(),
            Some(main.id)
        );

        let pruned_again = clone.remote_prune("origin").unwrap();
        assert_eq!(pruned_again.branches, Vec::<String>::new());
    }

    #[test]
    fn wildcard_refspecs_map_branch_captures() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote.clone()).unwrap();
        source.commit("base").unwrap();
        source.switch_new_branch("feature/search", None).unwrap();
        let search = source.commit("search").unwrap();
        source
            .switch_new_branch("feature/reporting", Some("main"))
            .unwrap();
        let reporting = source.commit("reporting").unwrap();
        source.switch_branch("main").unwrap();
        let main = source.commit("main").unwrap();

        let push = source
            .push_refspec_with_force("origin", "refs/heads/feature/*:refs/heads/review/*", false)
            .unwrap();

        assert_eq!(
            push.branches
                .iter()
                .map(|outcome| outcome.remote_branch.as_str())
                .collect::<Vec<_>>(),
            vec!["review/reporting", "review/search"]
        );
        assert!(!remote_dir.path().join("refs/heads/main").exists());

        let clone_dir = tempfile::tempdir().unwrap();
        let clone = Repository::init(clone_dir.path()).unwrap();
        clone.remote_add("origin", remote).unwrap();
        let fetch = clone
            .fetch_refspec(
                "origin",
                "refs/heads/review/*:refs/remotes/origin/reviewed/*",
            )
            .unwrap();

        assert_eq!(
            fetch
                .branches
                .iter()
                .map(|outcome| outcome.branch.as_str())
                .collect::<Vec<_>>(),
            vec!["reviewed/reporting", "reviewed/search"]
        );
        assert_eq!(
            clone
                .remote_tracking_ref("origin", "reviewed/search")
                .unwrap(),
            Some(search.id)
        );
        assert_eq!(
            clone
                .remote_tracking_ref("origin", "reviewed/reporting")
                .unwrap(),
            Some(reporting.id)
        );
        assert_eq!(clone.remote_tracking_ref("origin", "main").unwrap(), None);
        assert_eq!(source.branch_target("main").unwrap(), Some(main.id));
    }

    #[test]
    fn pull_merges_diverged_remote_branch_and_push_sees_second_parent() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };
        let volume = VolumeId::random();
        let log = LogId::random();

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote.clone()).unwrap();
        let source_app = source_dir.path().join("app.db");
        let source_notes = source_dir.path().join("notes.db");
        let app_base = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let notes_base = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(3), PageCount::new(4));
        let notes_remote = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(4), PageCount::new(5));
        let app_local = Snapshot::new(log, LSN::FIRST..=LSN::new(5), PageCount::new(6));

        source
            .stage_file(&source_app, volume.clone(), &app_base)
            .unwrap();
        source
            .stage_file(&source_notes, volume.clone(), &notes_base)
            .unwrap();
        let base = source.commit_staged("base").unwrap();
        source.push("origin", "main").unwrap();

        let clone_dir = tempfile::tempdir().unwrap();
        let clone = Repository::init(clone_dir.path()).unwrap();
        clone.remote_add("origin", remote).unwrap();
        let initial_pull = clone.pull("origin", "main", "main").unwrap();
        assert_eq!(
            initial_pull.merge,
            MergeOutcome::FastForward { from: None, to: base.id.clone() }
        );
        assert_eq!(clone.head_target().unwrap(), Some(base.id.clone()));

        let remote_commit = source
            .commit_file(&source_notes, "remote notes", volume.clone(), &notes_remote)
            .unwrap();
        source.push("origin", "main").unwrap();

        let local_commit = clone
            .commit_file(
                clone_dir.path().join("app.db"),
                "local app",
                volume,
                &app_local,
            )
            .unwrap();

        let pull = clone.pull("origin", "main", "main").unwrap();

        assert_eq!(
            pull.merge,
            MergeOutcome::Merged {
                head: local_commit.id.clone(),
                target: remote_commit.id.clone(),
                merge_base: Some(base.id),
                staged: vec!["notes.db".to_string()],
                conflicted: vec![],
            }
        );
        assert_eq!(clone.head_target().unwrap(), Some(local_commit.id.clone()));
        assert_eq!(clone.merge_head().unwrap(), Some(remote_commit.id.clone()));
        assert_eq!(
            clone.read_index().unwrap().staged_paths(),
            vec!["notes.db".to_string()]
        );

        let merge_commit = clone.commit_staged("merge origin/main").unwrap();
        assert_eq!(
            merge_commit.parents,
            vec![local_commit.id.clone(), remote_commit.id.clone()]
        );
        assert!(
            clone
                .is_ancestor(&local_commit.id, &merge_commit.id)
                .unwrap()
        );
        assert!(
            clone
                .is_ancestor(&remote_commit.id, &merge_commit.id)
                .unwrap()
        );

        let push = clone.push("origin", "main").unwrap();
        assert_eq!(push.head, merge_commit.id);
        assert_eq!(push.commits, 2);
    }

    #[test]
    fn pull_plan_freezes_fetched_target_before_tracking_ref_moves() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = RemoteConfig::Fs {
            root: remote_dir.path().to_string_lossy().into_owned(),
        };
        let volume = VolumeId::random();
        let log = LogId::random();

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repository::init(source_dir.path()).unwrap();
        source.remote_add("origin", remote.clone()).unwrap();
        let source_app = source_dir.path().join("app.db");
        let base_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
        let next_snapshot = Snapshot::new(log, LSN::FIRST..=LSN::new(4), PageCount::new(4));
        let base = source
            .commit_file(&source_app, "base", volume.clone(), &base_snapshot)
            .unwrap();
        source.push("origin", "main").unwrap();

        let clone_dir = tempfile::tempdir().unwrap();
        let clone = Repository::init(clone_dir.path()).unwrap();
        clone.remote_add("origin", remote).unwrap();
        let plan = clone.plan_pull("origin", "main", "main").unwrap();
        assert_eq!(plan.merge.checkout.target, Some(base.id.clone()));

        let next = source
            .commit_file(&source_app, "next", volume, &next_snapshot)
            .unwrap();
        source.push("origin", "main").unwrap();
        clone.fetch("origin", "main").unwrap();
        assert_eq!(
            clone.remote_tracking_ref("origin", "main").unwrap(),
            Some(next.id)
        );

        let outcome = clone.apply_pull_plan(&plan).unwrap();
        assert_eq!(
            outcome.merge,
            MergeOutcome::FastForward { from: None, to: base.id.clone() }
        );
        assert_eq!(clone.branch_target("main").unwrap(), Some(base.id.clone()));
        assert_eq!(
            clone
                .read_commit(&base.id)
                .unwrap()
                .files
                .get("app.db")
                .expect("planned app.db")
                .snapshot
                .to_snapshot()
                .head(),
            base_snapshot.head()
        );
    }

    #[test]
    fn discover_from_nested_path() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        let nested = tmp.path().join("a/b/c");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(Repository::discover(&nested).unwrap(), repo);
        assert_eq!(
            Repository::discover_for_file(nested.join("app.db")).unwrap(),
            repo
        );
    }

    fn write_sqlite_magic(path: impl AsRef<Path>) {
        fs::write(path, SQLITE_DATABASE_MAGIC).unwrap();
    }
}
