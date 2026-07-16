use super::*;

impl Repository {
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
            CONFIG_KEY_TRACK_DEFAULT_ROOTS => {
                config.track.default_roots = parse_config_string_list_value(key, value)?
                    .into_iter()
                    .map(|path| normalize_repo_path(&path))
                    .collect();
            }
            CONFIG_KEY_TRACK_USER_ROOTS => {
                config.track.user_roots = parse_config_string_list_value(key, value)?
                    .into_iter()
                    .map(|path| normalize_repo_path(&path))
                    .collect();
            }
            CONFIG_KEY_WORKTREE_MATERIALIZE_SQLITE => {
                config.worktree.materialize_sqlite = parse_config_bool_value(key, value)?;
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
            CONFIG_KEY_TRACK_DEFAULT_ROOTS => {
                config.track.default_roots.clear();
            }
            CONFIG_KEY_TRACK_USER_ROOTS => {
                config.track.user_roots.clear();
            }
            CONFIG_KEY_WORKTREE_MATERIALIZE_SQLITE => {
                config.worktree.materialize_sqlite = WorktreeConfig::default().materialize_sqlite;
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

    pub(super) fn file_config(&self) -> Result<FileConfig> {
        Ok(self.config()?.files)
    }

    pub fn has_configured_track_roots(&self) -> Result<bool> {
        Ok(self.config()?.track.has_roots())
    }

    pub fn path_matches_track_roots(&self, path: &str) -> Result<bool> {
        let roots = self.track_roots()?;
        Ok(roots.is_empty() || config_path_patterns_match(&roots, path))
    }

    pub(super) fn track_roots(&self) -> Result<Vec<String>> {
        Ok(self.config()?.track.roots())
    }
}
