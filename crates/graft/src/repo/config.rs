use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{core::byte_unit::ByteUnit, remote::RemoteConfig};

use super::{
    BranchConfig, DEFAULT_LARGE_FILE_THRESHOLD, OBJECT_FORMAT, REPOSITORY_FORMAT_VERSION, RepoErr,
    Result,
};

pub const CONFIG_KEY_FILES_INLINE_TEXT_THRESHOLD: &str = "files.inline_text_threshold";
pub const CONFIG_KEY_FILES_EXTERNAL_PATHS: &str = "files.external_paths";
pub const CONFIG_KEY_TRACK_DEFAULT_ROOTS: &str = "track.default_roots";
pub const CONFIG_KEY_TRACK_USER_ROOTS: &str = "track.user_roots";
pub const CONFIG_KEY_WORKTREE_MATERIALIZE_SQLITE: &str = "worktree.materialize_sqlite";
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

    #[serde(default, skip_serializing_if = "TrackConfig::is_default")]
    pub track: TrackConfig,

    #[serde(default, skip_serializing_if = "WorktreeConfig::is_default")]
    pub worktree: WorktreeConfig,

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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_roots: Vec<String>,
}

impl TrackConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }

    pub fn roots(&self) -> Vec<String> {
        self.default_roots
            .iter()
            .chain(self.user_roots.iter())
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn has_roots(&self) -> bool {
        !self.default_roots.is_empty() || !self.user_roots.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeConfig {
    pub materialize_sqlite: bool,
}

impl WorktreeConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            // Browser OPFS access is already mediated by Graft's persistent VFS bindings;
            // maintaining a second full SQLite projection only adds copy amplification.
            materialize_sqlite: !cfg!(target_arch = "wasm32"),
        }
    }
}

pub(super) fn normalize_config_key(key: &str) -> Result<&str> {
    let key = key.trim();
    if key.is_empty() {
        return Err(RepoErr::UnknownConfigKey(key.to_string()));
    }
    Ok(key)
}

pub(super) fn config_entry(config: &RepoConfig, key: &str) -> Result<RepoConfigEntry> {
    let value = if key == CONFIG_KEY_FILES_INLINE_TEXT_THRESHOLD {
        config.files.inline_text_threshold.to_string()
    } else if key == CONFIG_KEY_FILES_EXTERNAL_PATHS {
        format_config_string_list(&config.files.external_paths)
    } else if key == CONFIG_KEY_TRACK_DEFAULT_ROOTS {
        format_config_string_list(&config.track.default_roots)
    } else if key == CONFIG_KEY_TRACK_USER_ROOTS {
        format_config_string_list(&config.track.user_roots)
    } else if key == CONFIG_KEY_WORKTREE_MATERIALIZE_SQLITE {
        config.worktree.materialize_sqlite.to_string()
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

pub(super) fn config_entries(config: &RepoConfig) -> Vec<RepoConfigEntry> {
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
            key: CONFIG_KEY_TRACK_DEFAULT_ROOTS.to_string(),
            value: format_config_string_list(&config.track.default_roots),
        },
        RepoConfigEntry {
            key: CONFIG_KEY_TRACK_USER_ROOTS.to_string(),
            value: format_config_string_list(&config.track.user_roots),
        },
        RepoConfigEntry {
            key: CONFIG_KEY_WORKTREE_MATERIALIZE_SQLITE.to_string(),
            value: config.worktree.materialize_sqlite.to_string(),
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

pub(super) fn config_semantic_keys_table(key: &str) -> Result<Option<&str>> {
    config_key_suffix(key, CONFIG_KEY_MERGE_SEMANTIC_KEYS_PREFIX)
}

pub(super) fn config_generated_columns_table(key: &str) -> Result<Option<&str>> {
    config_key_suffix(key, CONFIG_KEY_MERGE_GENERATED_COLUMNS_PREFIX)
}

pub(super) fn config_internal_resolver_subject<'a>(
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

pub(super) fn config_schema_resolver_operation<'a>(
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

pub(super) fn config_key_suffix<'a>(key: &'a str, prefix: &str) -> Result<Option<&'a str>> {
    let Some(suffix) = key.strip_prefix(prefix) else {
        return Ok(None);
    };
    if suffix.trim().is_empty() {
        return Err(RepoErr::UnknownConfigKey(key.to_string()));
    }
    Ok(Some(suffix))
}

pub(super) fn format_config_string_list(values: &[String]) -> String {
    values.join(", ")
}

pub(super) fn default_internal_resolver(subject: &str) -> Option<&'static str> {
    DEFAULT_INTERNAL_RESOLVERS
        .iter()
        .find_map(|(candidate, resolver)| (*candidate == subject).then_some(*resolver))
}

pub(super) fn default_schema_resolver(operation: &str) -> Option<&'static str> {
    DEFAULT_SCHEMA_RESOLVERS
        .iter()
        .find_map(|(candidate, resolver)| (*candidate == operation).then_some(*resolver))
}

pub(super) fn parse_config_byte_unit_value(key: &str, value: &str) -> Result<ByteUnit> {
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

pub(super) fn parse_config_string_list_value(key: &str, value: &str) -> Result<Vec<String>> {
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

pub(super) fn parse_config_bool_value(key: &str, value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => Err(RepoErr::InvalidConfigValue {
            key: key.to_string(),
            value: value.to_string(),
            message: "expected true or false".to_string(),
        }),
    }
}

pub(super) fn parse_config_internal_resolver_value(
    key: &str,
    subject: &str,
    value: &str,
) -> Result<String> {
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

pub(super) fn parse_config_schema_resolver_value(
    key: &str,
    operation: &str,
    value: &str,
) -> Result<String> {
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
