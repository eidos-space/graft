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
fn config_get_set_manages_track_roots() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();

    assert_eq!(
        repo.config_get(CONFIG_KEY_TRACK_DEFAULT_ROOTS).unwrap(),
        RepoConfigEntry {
            key: CONFIG_KEY_TRACK_DEFAULT_ROOTS.to_string(),
            value: String::new()
        }
    );

    assert_eq!(
        repo.config_set(
            CONFIG_KEY_TRACK_DEFAULT_ROOTS,
            ".eidos/db.sqlite3, ./.eidos/files/**"
        )
        .unwrap(),
        RepoConfigEntry {
            key: CONFIG_KEY_TRACK_DEFAULT_ROOTS.to_string(),
            value: ".eidos/db.sqlite3, .eidos/files/**".to_string()
        }
    );
    assert_eq!(
        repo.config_set(CONFIG_KEY_TRACK_USER_ROOTS, "notes.md docs/")
            .unwrap(),
        RepoConfigEntry {
            key: CONFIG_KEY_TRACK_USER_ROOTS.to_string(),
            value: "notes.md, docs".to_string()
        }
    );
    assert_eq!(
        repo.config().unwrap().track.default_roots,
        vec![
            ".eidos/db.sqlite3".to_string(),
            ".eidos/files/**".to_string()
        ]
    );
    assert_eq!(
        repo.config().unwrap().track.user_roots,
        vec!["notes.md".to_string(), "docs".to_string()]
    );

    let raw_config = fs::read_to_string(repo.graft_dir().join(CONFIG_FILE)).unwrap();
    assert!(raw_config.contains("[track]"));
    assert!(raw_config.contains("default_roots = ["));
    assert!(raw_config.contains(r#"".eidos/db.sqlite3""#));
    assert!(raw_config.contains(r#"".eidos/files/**""#));
    assert!(raw_config.contains("user_roots = ["));
    assert!(raw_config.contains(r#""notes.md""#));
    assert!(raw_config.contains(r#""docs""#));

    assert_eq!(
        repo.config_unset(CONFIG_KEY_TRACK_DEFAULT_ROOTS).unwrap(),
        RepoConfigEntry {
            key: CONFIG_KEY_TRACK_DEFAULT_ROOTS.to_string(),
            value: String::new()
        }
    );
    assert!(repo.config().unwrap().track.default_roots.is_empty());
}

#[test]
fn config_get_set_manages_worktree_materialize_sqlite() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();

    assert_eq!(
        repo.config_get(CONFIG_KEY_WORKTREE_MATERIALIZE_SQLITE)
            .unwrap(),
        RepoConfigEntry {
            key: CONFIG_KEY_WORKTREE_MATERIALIZE_SQLITE.to_string(),
            value: "true".to_string()
        }
    );
    assert!(repo.config().unwrap().worktree.materialize_sqlite);

    assert_eq!(
        repo.config_set(CONFIG_KEY_WORKTREE_MATERIALIZE_SQLITE, "false")
            .unwrap(),
        RepoConfigEntry {
            key: CONFIG_KEY_WORKTREE_MATERIALIZE_SQLITE.to_string(),
            value: "false".to_string()
        }
    );
    assert!(!repo.config().unwrap().worktree.materialize_sqlite);

    assert_eq!(
        repo.config_unset(CONFIG_KEY_WORKTREE_MATERIALIZE_SQLITE)
            .unwrap(),
        RepoConfigEntry {
            key: CONFIG_KEY_WORKTREE_MATERIALIZE_SQLITE.to_string(),
            value: "true".to_string()
        }
    );
    assert!(repo.config().unwrap().worktree.materialize_sqlite);
    assert!(matches!(
        repo.config_set(CONFIG_KEY_WORKTREE_MATERIALIZE_SQLITE, "sometimes"),
        Err(RepoErr::InvalidConfigValue { key, value, .. })
            if key == CONFIG_KEY_WORKTREE_MATERIALIZE_SQLITE && value == "sometimes"
    ));
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
fn versioned_merge_policy_is_normalized_validated_and_tokenized() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    let initial_raw = fs::read(repo.graft_dir().join(CONFIG_FILE)).unwrap();
    let initial = repo.config().unwrap().merge.effective();
    assert_eq!(
        initial.internal_resolvers["sqlite_sequence"],
        "sequence_max"
    );
    assert_eq!(
        initial.schema_resolvers["add_column"],
        "alter_table_add_column"
    );
    let initial_token = initial.policy_token();

    let mut config = repo.config().unwrap();
    config.merge.same_row_merge = true;
    config
        .merge
        .semantic_keys
        .insert("records".into(), vec!["external_id".into()]);
    config.merge.semantic_key_collations.insert(
        "records".into(),
        BTreeMap::from([("external_id".into(), SemanticKeyCollation::NoCase)]),
    );
    config
        .merge
        .generated_columns
        .insert("records".into(), vec!["search_text".into()]);
    config.merge.column_resolvers.insert(
        "records".into(),
        BTreeMap::from([("updated_at".into(), ManagedColumnResolver::MaxTimestamp)]),
    );
    repo.write_config(&config).unwrap();
    let effective = repo.config().unwrap().merge.effective();
    assert_eq!(
        effective.column_resolvers["records"]["search_text"],
        ManagedColumnResolver::Recompute
    );
    assert_ne!(effective.policy_token(), initial_token);

    let before_invalid = fs::read(repo.graft_dir().join(CONFIG_FILE)).unwrap();
    let mut invalid = repo.config().unwrap();
    invalid
        .merge
        .semantic_key_collations
        .get_mut("records")
        .unwrap()
        .insert("missing".into(), SemanticKeyCollation::Binary);
    assert!(matches!(
        repo.write_config(&invalid),
        Err(RepoErr::InvalidConfigValue { key, .. })
            if key == "merge.semantic_key_collations.records.missing"
    ));
    assert_eq!(
        fs::read(repo.graft_dir().join(CONFIG_FILE)).unwrap(),
        before_invalid
    );
    assert_ne!(before_invalid, initial_raw);
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
                key: CONFIG_KEY_TRACK_DEFAULT_ROOTS.to_string(),
                value: String::new()
            },
            RepoConfigEntry {
                key: CONFIG_KEY_TRACK_USER_ROOTS.to_string(),
                value: String::new()
            },
            RepoConfigEntry {
                key: CONFIG_KEY_WORKTREE_MATERIALIZE_SQLITE.to_string(),
                value: "true".to_string()
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
                key: CONFIG_KEY_TRACK_DEFAULT_ROOTS.to_string(),
                value: String::new()
            },
            RepoConfigEntry {
                key: CONFIG_KEY_TRACK_USER_ROOTS.to_string(),
                value: String::new()
            },
            RepoConfigEntry {
                key: CONFIG_KEY_WORKTREE_MATERIALIZE_SQLITE.to_string(),
                value: "true".to_string()
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
fn log_page_stops_after_a_bounded_window_and_resumes_after_cursor() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    let app = tmp.path().join("app.db");

    repo.mark_dirty_path(&app).unwrap();
    let first = repo.commit("first").unwrap();
    repo.mark_dirty_path(&app).unwrap();
    let second = repo.commit("second").unwrap();
    repo.mark_dirty_path(&app).unwrap();
    let third = repo.commit("third").unwrap();

    let (first_page, has_more) = repo.log_page(2, None).unwrap();
    assert_eq!(
        first_page
            .iter()
            .map(|commit| &commit.id)
            .collect::<Vec<_>>(),
        vec![&third.id, &second.id]
    );
    assert!(has_more);

    let (second_page, has_more) = repo.log_page(2, Some(&second.id)).unwrap();
    assert_eq!(second_page, vec![first]);
    assert!(!has_more);

    assert!(matches!(
        repo.log_page(1, Some("missing-commit")),
        Err(RepoErr::InvalidRevision(rev)) if rev == "missing-commit"
    ));
}

#[test]
fn history_summary_page_is_lightweight_paginated_and_carries_path_counts() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    let note = tmp.path().join("note.txt");

    fs::write(&note, "first\n").unwrap();
    repo.stage_artifact_path(&note).unwrap();
    let first = repo.commit_staged("first").unwrap();
    fs::write(&note, "second\n").unwrap();
    repo.stage_artifact_path(&note).unwrap();
    let second = repo.commit_staged("second").unwrap();

    let first_page = repo.history_summary_page(1, None).unwrap();
    assert!(first_page.has_more);
    assert_eq!(first_page.next_cursor.as_deref(), Some(second.id.as_str()));
    assert_eq!(first_page.commits[0].message, "second");
    assert!(first_page.commits[0].path_counts_complete);
    assert_eq!(
        first_page.commits[0].path_changes,
        Some(CommitPathChangeCounts { added: 0, modified: 1, deleted: 0 })
    );

    let second_page = repo
        .history_summary_page(1, first_page.next_cursor.as_deref())
        .unwrap();
    assert!(!second_page.has_more);
    assert_eq!(second_page.commits[0].id, first.id);
    assert_eq!(second_page.commits[0].path_changes.unwrap().added, 1);
}

#[test]
fn commit_changed_paths_pages_root_and_first_parent_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        let path = tmp.path().join(name);
        fs::write(&path, "first\n").unwrap();
        repo.stage_artifact_path(&path).unwrap();
    }
    let root = repo.commit_staged("root").unwrap();

    let first_page = repo.commit_changed_paths_page(&root.id, 2, None).unwrap();
    assert_eq!(first_page.revision, root.id);
    assert_eq!(first_page.parent, None);
    assert_eq!(first_page.total_changed_paths, 3);
    assert_eq!(
        first_page
            .paths
            .iter()
            .map(|change| change.path.as_str())
            .collect::<Vec<_>>(),
        vec!["a.txt", "b.txt"]
    );
    assert!(first_page.has_more);

    let second_page = repo
        .commit_changed_paths_page(&root.id, 2, first_page.next_cursor.as_deref())
        .unwrap();
    assert_eq!(
        second_page
            .paths
            .iter()
            .map(|change| change.path.as_str())
            .collect::<Vec<_>>(),
        vec!["c.txt"]
    );
    assert!(!second_page.has_more);

    let changed = tmp.path().join("a.txt");
    fs::write(&changed, "second\n").unwrap();
    repo.stage_artifact_path(&changed).unwrap();
    let child = repo.commit_staged("child").unwrap();
    let child_page = repo
        .commit_changed_paths_page(&child.id, 100, None)
        .unwrap();
    assert_eq!(child_page.parent.as_deref(), Some(root.id.as_str()));
    assert_eq!(child_page.paths.len(), 1);
    assert_eq!(child_page.paths[0].path, "a.txt");
    assert_eq!(child_page.paths[0].change, RepoFileChange::Modified);

    let token = CancellationToken::new();
    token.cancel();
    assert!(matches!(
        with_cancellation(&token, || repo
            .commit_changed_paths_page(&child.id, 100, None)),
        Err(RepoErr::Cancelled)
    ));
    assert_eq!(
        repo.commit_changed_paths_page(&child.id, 100, None)
            .unwrap()
            .paths
            .len(),
        1
    );
}

#[test]
fn recorded_artifact_move_is_one_rename_across_status_diff_and_history() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    let previous = tmp.path().join("old.txt");
    let current = tmp.path().join("folder/new.txt");
    fs::write(&previous, "same payload\n").unwrap();
    repo.stage_artifact_path(&previous).unwrap();
    let base = repo.commit_staged("base").unwrap();

    fs::create_dir_all(current.parent().unwrap()).unwrap();
    fs::rename(&previous, &current).unwrap();
    repo.stage_path_move_keys("old.txt", "folder/new.txt")
        .unwrap();

    let status = repo.status().unwrap();
    assert_eq!(status.staged_changes.len(), 1);
    assert_eq!(status.staged_changes[0].path, "folder/new.txt");
    assert_eq!(
        status.staged_changes[0].previous_path.as_deref(),
        Some("old.txt")
    );
    assert_eq!(status.staged_changes[0].change, RepoFileChange::Renamed);

    let staged = repo.diff_staged(Some("folder/new.txt")).unwrap();
    assert_eq!(staged.paths.len(), 1);
    assert_eq!(staged.paths[0].change, RepoFileChange::Renamed);
    assert_eq!(staged.paths[0].previous_path.as_deref(), Some("old.txt"));
    assert_eq!(staged.artifacts[0].from, staged.artifacts[0].to);
    let by_old_path = repo.diff_staged(Some("old.txt")).unwrap();
    assert_eq!(by_old_path.paths, staged.paths);

    let moved = repo.commit_staged("move").unwrap();
    let changed = repo
        .commit_changed_paths_page(&moved.id, 100, None)
        .unwrap();
    assert_eq!(changed.parent.as_deref(), Some(base.id.as_str()));
    assert_eq!(changed.total_changed_paths, 1);
    assert_eq!(changed.paths[0].change, RepoFileChange::Renamed);
    assert_eq!(changed.paths[0].previous_path.as_deref(), Some("old.txt"));
    for path in ["old.txt", "folder/new.txt"] {
        let detail = repo
            .diff_revisions(&base.id, &moved.id, Some(path))
            .unwrap();
        assert_eq!(detail.paths.len(), 1);
        assert_eq!(detail.paths[0].change, RepoFileChange::Renamed);
        assert_eq!(detail.paths[0].previous_path.as_deref(), Some("old.txt"));
        assert_eq!(detail.artifacts[0].from, detail.artifacts[0].to);
    }
    assert_eq!(
        repo.history_summary_page(1, None).unwrap().commits[0]
            .path_changes
            .unwrap()
            .modified,
        1
    );
}

#[test]
fn recorded_sqlite_move_preserves_identity_across_later_content_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    let previous = tmp.path().join("old.eidos");
    let current = tmp.path().join("folder/new.eidos");
    let volume = VolumeId::random();
    let snapshot = Snapshot::new(LogId::random(), LSN::FIRST..=LSN::new(3), PageCount::new(7));
    fs::write(&previous, b"physical placeholder").unwrap();
    repo.stage_file(&previous, volume.clone(), &snapshot)
        .unwrap();
    let base = repo.commit_staged("base").unwrap();
    let expected = repo.head_file(&previous).unwrap().unwrap();

    fs::create_dir_all(current.parent().unwrap()).unwrap();
    fs::rename(&previous, &current).unwrap();
    repo.stage_path_move_keys("old.eidos", "folder/new.eidos")
        .unwrap();

    assert_eq!(repo.index_file(&current).unwrap(), Some(expected.clone()));
    let staged = repo.diff_staged(Some("folder/new.eidos")).unwrap();
    assert_eq!(staged.paths.len(), 1);
    assert_eq!(staged.paths[0].change, RepoFileChange::Renamed);
    assert_eq!(staged.files[0].from, Some(expected.clone()));
    assert_eq!(staged.files[0].to, Some(expected.clone()));

    let updated_snapshot =
        Snapshot::new(LogId::random(), LSN::FIRST..=LSN::new(5), PageCount::new(9));
    repo.stage_file(&current, volume, &updated_snapshot)
        .unwrap();
    let updated = repo.index_file(&current).unwrap().unwrap();
    assert_ne!(updated, expected);
    assert_eq!(updated.volume, expected.volume);
    let staged = repo.diff_staged(Some("folder/new.eidos")).unwrap();
    assert_eq!(staged.paths.len(), 1);
    assert_eq!(staged.paths[0].change, RepoFileChange::Renamed);
    assert_eq!(staged.files[0].from, Some(expected.clone()));
    assert_eq!(staged.files[0].to, Some(updated.clone()));

    let moved = repo.commit_staged("move sqlite").unwrap();
    let changed = repo
        .commit_changed_paths_page(&moved.id, 100, None)
        .unwrap();
    assert_eq!(changed.total_changed_paths, 1);
    assert_eq!(changed.paths[0].change, RepoFileChange::Renamed);
    assert_eq!(changed.paths[0].previous_path.as_deref(), Some("old.eidos"));
    for path in ["old.eidos", "folder/new.eidos"] {
        let detail = repo
            .diff_revisions(&base.id, &moved.id, Some(path))
            .unwrap();
        assert_eq!(detail.paths.len(), 1);
        assert_eq!(detail.paths[0].change, RepoFileChange::Renamed);
        assert_eq!(detail.paths[0].previous_path.as_deref(), Some("old.eidos"));
        assert_eq!(detail.files[0].from, Some(expected.clone()));
        assert_eq!(detail.files[0].to, Some(updated.clone()));
    }
}

#[test]
fn recorded_directory_move_expands_tracked_descendants_atomically() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    let previous = tmp.path().join("old");
    fs::create_dir_all(previous.join("nested")).unwrap();
    fs::write(previous.join("one.txt"), "one\n").unwrap();
    fs::write(previous.join("nested/two.txt"), "two\n").unwrap();
    repo.stage_artifact_path(previous.join("one.txt")).unwrap();
    repo.stage_artifact_path(previous.join("nested/two.txt"))
        .unwrap();
    repo.commit_staged("base").unwrap();

    fs::rename(&previous, tmp.path().join("new")).unwrap();
    repo.stage_path_move_keys("old", "new").unwrap();

    let status = repo.status().unwrap();
    assert_eq!(status.staged_changes.len(), 2);
    assert!(status.staged_changes.iter().all(|change| {
        change.change == RepoFileChange::Renamed && change.previous_path.is_some()
    }));
    let moved = repo.commit_staged("move folder").unwrap();
    let changed = repo
        .commit_changed_paths_page(&moved.id, 100, None)
        .unwrap();
    assert_eq!(changed.total_changed_paths, 2);
    assert_eq!(
        changed
            .paths
            .iter()
            .map(|change| (change.previous_path.as_deref(), change.path.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (Some("old/nested/two.txt"), "new/nested/two.txt"),
            (Some("old/one.txt"), "new/one.txt")
        ]
    );
}

#[test]
fn cancellation_scope_returns_cancelled_without_poisoning_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    fs::write(tmp.path().join("note.txt"), "content\n").unwrap();

    let token = CancellationToken::new();
    token.cancel();
    let cancelled = with_cancellation(&token, || repo.status());
    assert!(matches!(cancelled, Err(RepoErr::Cancelled)));
    assert!(repo.status().is_ok());
}

#[test]
fn transfer_progress_reports_known_and_unknown_body_lengths() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    let reporter = TransferProgressReporter::new(move |progress| {
        captured.lock().unwrap().push(progress);
    });

    with_transfer_progress(&reporter, || {
        let mut upload = begin_transfer_progress(TransferDirection::Upload, Some(5)).unwrap();
        upload.advance(2);
        upload.advance(3);

        let mut download = begin_transfer_progress(TransferDirection::Download, None).unwrap();
        download.advance(7);
        download.finish();
    });

    let events = events.lock().unwrap();
    assert!(events.contains(&TransferProgress {
        direction: TransferDirection::Upload,
        transferred_bytes: 5,
        total_bytes: Some(5),
    }));
    assert_eq!(
        events.last(),
        Some(&TransferProgress {
            direction: TransferDirection::Download,
            transferred_bytes: 7,
            total_bytes: None,
        })
    );
    assert!(begin_transfer_progress(TransferDirection::Upload, Some(1)).is_none());
}

#[test]
fn transfer_progress_coalesces_short_transfers_and_flushes_the_aggregate_total() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    let reporter = TransferProgressReporter::new(move |progress| {
        captured.lock().unwrap().push(progress);
    });

    with_transfer_progress(&reporter, || {
        declare_transfer_progress_total(TransferDirection::Upload, 100_000);
        for _ in 0..10_000 {
            let mut upload = begin_transfer_progress(TransferDirection::Upload, Some(10)).unwrap();
            upload.advance(10);
            upload.finish();
        }
    });

    let events = events.lock().unwrap();
    assert!(
        events.len() <= 8,
        "short transfers emitted {} progress callbacks",
        events.len()
    );
    assert_eq!(
        events.first(),
        Some(&TransferProgress {
            direction: TransferDirection::Upload,
            transferred_bytes: 0,
            total_bytes: Some(100_000),
        })
    );
    assert_eq!(
        events.last(),
        Some(&TransferProgress {
            direction: TransferDirection::Upload,
            transferred_bytes: 100_000,
            total_bytes: Some(100_000),
        })
    );
}

#[test]
fn transfer_progress_retains_periodic_partial_updates() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    let reporter = TransferProgressReporter::new(move |progress| {
        captured.lock().unwrap().push(progress);
    });

    with_transfer_progress(&reporter, || {
        let mut upload = begin_transfer_progress(TransferDirection::Upload, Some(100)).unwrap();
        std::thread::sleep(TRANSFER_PROGRESS_EMIT_INTERVAL + Duration::from_millis(10));
        upload.advance(40);
        upload.advance(60);
    });

    let events = events.lock().unwrap();
    assert!(events.contains(&TransferProgress {
        direction: TransferDirection::Upload,
        transferred_bytes: 40,
        total_bytes: Some(100),
    }));
    assert_eq!(
        events.last(),
        Some(&TransferProgress {
            direction: TransferDirection::Upload,
            transferred_bytes: 100,
            total_bytes: Some(100),
        })
    );
}

#[test]
fn remote_wait_is_cancelled_without_a_wall_clock_request_timeout() {
    let token = CancellationToken::new();
    let canceller = token.clone();
    let task = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        canceller.cancel();
    });
    let started = std::time::Instant::now();

    let result = with_cancellation(&token, || {
        block_on_remote(std::future::pending::<std::result::Result<(), RemoteErr>>())
    });

    task.join().unwrap();
    assert!(matches!(result, Err(RepoErr::Cancelled)));
    assert!(started.elapsed() < Duration::from_secs(1));
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
fn gitignore_rules_follow_git_syntax_and_nested_precedence() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    for directory in [
        "blocked",
        "docs",
        "generated/nested",
        "logs",
        "nested/deeper",
        "open",
    ] {
        fs::create_dir_all(tmp.path().join(directory)).unwrap();
    }

    fs::write(
        tmp.path().join(GIT_IGNORE_FILE),
        concat!(
            "# Git-style ignore rules\n",
            ".gitignore\n",
            "*.tmp\n",
            "!important.tmp\n",
            "/root-only.txt\n",
            "blocked/\n",
            "!blocked/keep.txt\n",
            "open/*\n",
            "!open/keep.txt\n",
            "generated/**\n",
            "graft-priority.txt\n",
            "logs/?.txt\n",
            "docs/[ab].md\n",
            "\\#literal\n",
        ),
    )
    .unwrap();
    fs::write(
        tmp.path().join(GRAFT_IGNORE_FILE),
        "!graft-priority.txt\n.graftignore\n",
    )
    .unwrap();
    fs::write(
        tmp.path().join("nested").join(GIT_IGNORE_FILE),
        "*.json\n!keep.tmp\n/local.txt\n",
    )
    .unwrap();

    for (path, content) in [
        ("#literal", "ignored"),
        ("blocked/keep.txt", "still ignored"),
        ("docs/a.md", "ignored"),
        ("docs/c.md", "visible"),
        ("generated/nested/data.bin", "ignored"),
        ("graft-priority.txt", "visible"),
        ("ignored.tmp", "ignored"),
        ("important.tmp", "visible"),
        ("logs/a.txt", "ignored"),
        ("logs/ab.txt", "visible"),
        ("nested/deeper/local.txt", "visible"),
        ("nested/drop.json", "ignored"),
        ("nested/keep.tmp", "visible"),
        ("nested/local.txt", "ignored"),
        ("nested/root-only.txt", "visible"),
        ("open/drop.txt", "ignored"),
        ("open/keep.txt", "visible"),
        ("root-only.txt", "ignored"),
    ] {
        fs::write(tmp.path().join(path), content).unwrap();
    }

    let paths = repo
        .untracked_paths()
        .unwrap()
        .into_iter()
        .map(|path| path.path)
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            "docs/c.md",
            "graft-priority.txt",
            "important.tmp",
            "logs/ab.txt",
            "nested/deeper/local.txt",
            "nested/keep.tmp",
            "nested/root-only.txt",
            "open/keep.txt",
        ]
    );
    assert!(
        repo.is_ignored_worktree_path(repo.worktree().join("blocked/keep.txt"))
            .unwrap()
    );
    assert!(
        !repo
            .is_ignored_worktree_path(repo.worktree().join("nested/keep.tmp"))
            .unwrap()
    );

    let mut matcher = repo.ignore_matcher().unwrap();
    assert!(matcher.is_ignored("nested/drop.json", false).unwrap());
    assert!(!matcher.is_ignored("nested/keep.tmp", false).unwrap());
    assert!(matcher.rules_unchanged().unwrap());
    fs::write(tmp.path().join("nested").join(GIT_IGNORE_FILE), "other/\n").unwrap();
    assert!(!matcher.rules_unchanged().unwrap());
    let mut refreshed_matcher = repo.ignore_matcher().unwrap();
    assert!(
        !refreshed_matcher
            .is_ignored("nested/drop.json", false)
            .unwrap()
    );
}

#[test]
fn status_limits_untracked_paths_to_configured_track_roots() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    repo.config_set(
        CONFIG_KEY_TRACK_DEFAULT_ROOTS,
        ".eidos/db.sqlite3 .eidos/files/** .eidos/agent/sessions/**",
    )
    .unwrap();

    fs::create_dir_all(tmp.path().join(".eidos/files")).unwrap();
    fs::create_dir_all(tmp.path().join(".eidos/agent/sessions")).unwrap();
    write_sqlite_magic(tmp.path().join(".eidos/db.sqlite3"));
    fs::write(tmp.path().join(".eidos/files/image.png"), b"png").unwrap();
    fs::write(
        tmp.path().join(".eidos/agent/sessions/history.jsonl"),
        br#"{"event":"created"}"#,
    )
    .unwrap();
    fs::write(tmp.path().join("notes.md"), b"user-owned note").unwrap();

    let status = repo.status().unwrap();
    assert_eq!(
        status.unstaged,
        vec![
            ".eidos/agent/sessions/history.jsonl".to_string(),
            ".eidos/db.sqlite3".to_string(),
            ".eidos/files/image.png".to_string(),
        ]
    );

    let all_candidates = repo.untracked_paths().unwrap();
    assert_eq!(
        all_candidates
            .iter()
            .map(|path| path.path.as_str())
            .collect::<Vec<_>>(),
        vec![
            ".eidos/agent/sessions/history.jsonl",
            ".eidos/db.sqlite3",
            ".eidos/files/image.png",
            "notes.md",
        ]
    );
}

#[test]
fn status_rejects_paths_with_ambiguous_whitespace_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    fs::write(tmp.path().join("note.md"), b"plain").unwrap();
    fs::write(tmp.path().join(" note.md "), b"spaced").unwrap();

    let err = repo.status().unwrap_err();

    assert!(matches!(
        err,
        RepoErr::UnsupportedPathIdentity { path, reason }
            if path == " note.md "
                && reason == "path components must not start or end with whitespace"
    ));
    assert!(!repo.has_staged_changes().unwrap());
}

#[cfg(not(windows))]
#[test]
fn explicit_posix_backslash_path_cannot_alias_slash_path() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    let slash_dir = tmp.path().join("foo");
    fs::create_dir_all(&slash_dir).unwrap();
    let slash_path = slash_dir.join("bar.md");
    fs::write(&slash_path, b"committed slash content").unwrap();
    repo.stage_artifact_path(&slash_path).unwrap();
    repo.commit_staged("track slash path").unwrap();
    fs::write(&slash_path, b"local slash draft").unwrap();

    let backslash_path = tmp.path().join("foo\\bar.md");
    fs::write(&backslash_path, b"backslash content").unwrap();
    let err = repo
        .plan_checkout_artifact_from_revision("HEAD", &backslash_path)
        .unwrap_err();

    assert!(matches!(
        err,
        RepoErr::UnsupportedPathIdentity { path, reason }
            if path == "foo\\bar.md"
                && reason == "backslashes are not supported in POSIX repository paths"
    ));
    assert_eq!(fs::read(&slash_path).unwrap(), b"local slash draft");
    assert_eq!(fs::read(&backslash_path).unwrap(), b"backslash content");
    assert!(!repo.has_staged_changes().unwrap());
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
        fs::read_to_string(repo.graft_dir().join(DIR_LOGS_REFS).join("refs/heads/main")).unwrap();
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

    let object::Object::Tree(tree) = repo.read_object(commit_object.tree.as_str()).unwrap() else {
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
    let state = entry.artifact.expect("artifact staged");
    assert_eq!(state.size(), 15);
    assert_eq!(
        *state.content_hash(),
        object::ObjectId::for_bytes(b"hello app state")
    );

    let commit = repo.commit_staged("track notes").unwrap();
    assert!(commit.files.is_empty());
    assert_eq!(commit.artifacts.get("notes.txt"), Some(&state));
    assert_eq!(repo.head_artifact(&notes).unwrap(), Some(state));
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
fn artifact_status_cache_detects_same_size_rewrites() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    let notes = tmp.path().join("notes.txt");
    fs::write(&notes, b"first").unwrap();

    repo.stage_artifact_path(&notes).unwrap();
    repo.commit_staged("track notes").unwrap();
    assert!(repo.status().unwrap().unstaged_changes.is_empty());

    fs::write(&notes, b"other").unwrap();
    assert_eq!(
        repo.status().unwrap().unstaged_changes,
        vec![RepoWorktreeChange {
            path: "notes.txt".to_string(),
            change: RepoWorktreeChangeKind::Modified,
            kind: RepoTrackedPathKind::TextFile,
            storage: RepoPathStorage::Inline,
        }]
    );

    fs::write(&notes, b"first").unwrap();
    assert!(repo.status().unwrap().unstaged_changes.is_empty());
}

#[test]
fn stage_artifact_path_handles_large_inline_text_with_compact_encoding() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    let notes = tmp.path().join("long.md");
    let bytes = "## Section\n\nParagraph 中文 content.\n"
        .repeat(4_096)
        .into_bytes();
    assert!(bytes.len() < DEFAULT_LARGE_FILE_THRESHOLD.as_u64() as usize);
    fs::write(&notes, &bytes).unwrap();

    let state = repo
        .stage_artifact_path(&notes)
        .unwrap()
        .artifact
        .expect("large text artifact staged inline");
    assert!(!state.is_large());

    let object::Object::Blob(object::BlobObject::File(blob)) =
        repo.read_object(state.oid().as_str()).unwrap()
    else {
        panic!("large inline text should point at a file blob");
    };
    assert_eq!(blob.kind, object::FileContentKind::TextFile);
    assert_eq!(blob.bytes, bytes);
}

#[test]
fn status_treats_a_tracked_file_replaced_by_a_directory_as_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    let shape = tmp.path().join("shape");
    fs::write(&shape, b"file topology").unwrap();
    repo.stage_artifact_path(&shape).unwrap();
    repo.commit_staged("track shape as a file").unwrap();

    fs::remove_file(&shape).unwrap();
    fs::create_dir(&shape).unwrap();
    fs::write(shape.join("child.md"), b"directory topology").unwrap();

    let status = repo.status().unwrap();
    assert_eq!(
        status.unstaged_changes,
        vec![
            RepoWorktreeChange {
                path: "shape".to_string(),
                change: RepoWorktreeChangeKind::Deleted,
                kind: RepoTrackedPathKind::TextFile,
                storage: RepoPathStorage::Inline,
            },
            RepoWorktreeChange {
                path: "shape/child.md".to_string(),
                change: RepoWorktreeChangeKind::Untracked,
                kind: RepoTrackedPathKind::TextFile,
                storage: RepoPathStorage::Inline,
            },
        ]
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
    let state = entry.artifact.expect("artifact staged");
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
fn stage_artifact_path_classifies_utf8_codepoints_across_sniff_boundary_as_text() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();

    for (width, character) in [(2, "¢"), (3, "中"), (4, "😀")] {
        for bytes_before_boundary in 1..width {
            let name = format!("utf8-{width}-{bytes_before_boundary}.txt");
            let path = tmp.path().join(&name);
            let mut bytes = vec![b'a'; CONTENT_CLASS_SAMPLE_BYTES - bytes_before_boundary];
            bytes.extend_from_slice(character.as_bytes());
            bytes.push(b'\n');
            fs::write(&path, bytes).unwrap();

            assert_eq!(
                classify_artifact_path(&path).unwrap(),
                RepoTrackedPathKind::TextFile,
                "{name} should be text during bounded worktree classification"
            );

            let state = repo
                .stage_artifact_path(&path)
                .unwrap()
                .artifact
                .expect("artifact staged");
            assert_eq!(
                artifact_tracked_path_kind(&state),
                RepoTrackedPathKind::TextFile,
                "{name} should remain text when its UTF-8 codepoint crosses the sniff boundary"
            );
        }
    }
}

#[test]
fn artifact_classification_rejects_invalid_utf8_at_sniff_boundary() {
    let mut incomplete = vec![b'a'; CONTENT_CLASS_SAMPLE_BYTES - 1];
    incomplete.push(0xe4);
    assert_eq!(
        classify_artifact_bytes(&incomplete),
        RepoTrackedPathKind::BinaryFile
    );

    let mut invalid_continuation = vec![b'a'; CONTENT_CLASS_SAMPLE_BYTES - 1];
    invalid_continuation.extend_from_slice(&[0xe4, 0xff, 0xad]);
    assert_eq!(
        classify_artifact_bytes(&invalid_continuation),
        RepoTrackedPathKind::BinaryFile
    );
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
    let first_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
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
    let first_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
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
    let first_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
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
    let first_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
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
fn diff_root_reports_first_commit_paths_as_added() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    let note = tmp.path().join("note.md");
    let binary = tmp.path().join("image.bin");
    fs::write(&note, "# First\n").unwrap();
    fs::write(&binary, [0, 159, 146, 150]).unwrap();
    repo.stage_artifact_path(&note).unwrap();
    repo.stage_artifact_path(&binary).unwrap();
    let first = repo.commit_staged("first").unwrap();

    let diff = repo.diff_root(&first.id, None).unwrap();
    assert_eq!(diff.from, "root");
    assert_eq!(diff.to, first.id);
    assert_eq!(diff.artifacts.len(), 2);
    assert!(diff.artifacts.iter().all(|path| {
        path.change == RepoFileChange::Added && path.from.is_none() && path.to.is_some()
    }));
    assert_eq!(
        diff.paths
            .iter()
            .map(|path| (path.path.as_str(), path.kind))
            .collect::<Vec<_>>(),
        vec![
            ("image.bin", RepoTrackedPathKind::BinaryFile),
            ("note.md", RepoTrackedPathKind::TextFile),
        ]
    );

    let note_only = repo.diff_root("HEAD", Some("note.md")).unwrap();
    assert_eq!(note_only.artifacts.len(), 1);
    let content = repo
        .diff_text_content(&note_only.artifacts[0], ByteUnit::new(128))
        .unwrap();
    assert_eq!(content.before, RepoTextContentState::Absent);
    assert!(matches!(
        content.after,
        RepoTextContentState::Utf8 { content, .. } if content == "# First\n"
    ));

    let binary_only = repo.diff_root("HEAD", Some("image.bin")).unwrap();
    let binary_content = repo
        .diff_artifact_content(&binary_only.artifacts[0], ByteUnit::new(128))
        .unwrap();
    assert_eq!(binary_content.before, RepoTextContentState::Absent);
    assert!(matches!(
        binary_content.after,
        RepoTextContentState::Base64 { content, size: 4, .. } if content == "AJ+Slg=="
    ));
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
    let first_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
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
fn path_filtered_worktree_diff_does_not_hydrate_unrelated_tree_blobs() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    let target = tmp.path().join("target.txt");
    let unrelated = tmp.path().join("unrelated.txt");
    fs::write(&target, "target v1").unwrap();
    fs::write(&unrelated, "unrelated").unwrap();
    repo.stage_artifact_path(&target).unwrap();
    repo.stage_artifact_path(&unrelated).unwrap();
    let commit = repo.commit_staged("baseline").unwrap();
    let unrelated_state = commit.artifacts.get("unrelated.txt").unwrap();
    fs::remove_file(repo.object_store().path_for(unrelated_state.oid())).unwrap();

    fs::write(&target, "target v2").unwrap();
    let diff = repo
        .diff_worktree_artifact(&target, Some("target.txt"))
        .unwrap();
    assert_eq!(diff.artifacts.len(), 1);
    assert_eq!(diff.artifacts[0].path, "target.txt");
    assert_eq!(diff.artifacts[0].change, RepoFileChange::Modified);
}

#[test]
fn diff_text_content_reports_utf8_and_absent_states_without_mutation() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    let anchor = tmp.path().join("anchor.txt");
    let note = tmp.path().join("note.md");

    fs::write(&anchor, "anchor").unwrap();
    repo.stage_artifact_path(&anchor).unwrap();
    let first = repo.commit_staged("anchor").unwrap();
    fs::write(&note, "# Hello\n").unwrap();
    repo.stage_artifact_path(&note).unwrap();
    let second = repo.commit_staged("add note").unwrap();

    let diff = repo
        .diff_revisions(&first.id, &second.id, Some("note.md"))
        .unwrap();
    let before_status = repo.status().unwrap();
    let content = repo
        .diff_text_content(&diff.artifacts[0], ByteUnit::new(128))
        .unwrap();

    assert_eq!(content.before, RepoTextContentState::Absent);
    assert!(matches!(
        content.after,
        RepoTextContentState::Utf8 { content, size: 8, .. } if content == "# Hello\n"
    ));
    let read = repo
        .read_path_content(&second.id, "note.md", ByteUnit::new(128))
        .unwrap();
    assert_eq!(read.revision, second.id);
    assert_eq!(read.kind, Some(RepoTrackedPathKind::TextFile));
    assert!(matches!(
        read.content,
        RepoPathContentState::Utf8 { content, size: 8, .. } if content == "# Hello\n"
    ));
    let absent = repo
        .read_path_content(&second.id, "missing.md", ByteUnit::new(128))
        .unwrap();
    assert_eq!(absent.kind, None);
    assert_eq!(absent.storage, None);
    assert_eq!(absent.content, RepoPathContentState::Absent);
    let after_status = repo.status().unwrap();
    assert_eq!(after_status.head_target, before_status.head_target);
    assert_eq!(after_status.staged, before_status.staged);
    assert_eq!(after_status.unstaged, before_status.unstaged);
    assert_eq!(after_status.conflicted, before_status.conflicted);
    assert_eq!(fs::read_to_string(&note).unwrap(), "# Hello\n");

    fs::remove_file(&note).unwrap();
    repo.stage_file_removal(&note).unwrap();
    let third = repo.commit_staged("remove note").unwrap();
    let deleted = repo
        .diff_revisions(&second.id, &third.id, Some("note.md"))
        .unwrap();
    let deleted_content = repo
        .diff_text_content(&deleted.artifacts[0], ByteUnit::new(128))
        .unwrap();
    assert!(matches!(
        deleted_content.before,
        RepoTextContentState::Utf8 { .. }
    ));
    assert_eq!(deleted_content.after, RepoTextContentState::Absent);
}

#[test]
fn diff_text_content_bounds_and_reports_missing_external_payloads() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    let note = tmp.path().join("note.md");

    fs::write(&note, "old external text").unwrap();
    repo.stage_artifact_path_with_inline_text_threshold(&note, 1)
        .unwrap();
    let first = repo.commit_staged("old note").unwrap();
    fs::write(&note, "new external text").unwrap();
    repo.stage_artifact_path_with_inline_text_threshold(&note, 1)
        .unwrap();
    let second = repo.commit_staged("new note").unwrap();
    let diff = repo
        .diff_revisions(&first.id, &second.id, Some("note.md"))
        .unwrap();

    let bounded = repo
        .diff_text_content(&diff.artifacts[0], ByteUnit::new(4))
        .unwrap();
    assert!(matches!(
        bounded.before,
        RepoTextContentState::TooLarge { size: 17, .. }
    ));
    assert!(matches!(
        bounded.after,
        RepoTextContentState::TooLarge { size: 17, .. }
    ));
    let bounded_read = repo
        .read_path_content(&first.id, "note.md", ByteUnit::new(4))
        .unwrap();
    assert!(matches!(
        bounded_read.content,
        RepoPathContentState::TooLarge { size: 17, .. }
    ));

    let before = diff.artifacts[0].from.as_ref().unwrap();
    fs::remove_file(repo.large_file_content_path(before.content_hash())).unwrap();
    let missing = repo
        .diff_text_content(&diff.artifacts[0], ByteUnit::new(128))
        .unwrap();
    assert!(matches!(
        missing.before,
        RepoTextContentState::MissingPayload { .. }
    ));
    assert!(matches!(missing.after, RepoTextContentState::Utf8 { .. }));
    let missing_read = repo
        .read_path_content(&first.id, "note.md", ByteUnit::new(128))
        .unwrap();
    assert!(matches!(
        missing_read.content,
        RepoPathContentState::MissingPayload { .. }
    ));
}

#[test]
fn diff_text_content_reports_invalid_full_utf8() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    let anchor = tmp.path().join("anchor.txt");
    let note = tmp.path().join("note.md");

    fs::write(&anchor, "anchor").unwrap();
    repo.stage_artifact_path(&anchor).unwrap();
    let first = repo.commit_staged("anchor").unwrap();
    let mut invalid = vec![b'a'; CONTENT_CLASS_SAMPLE_BYTES];
    invalid.push(0xff);
    fs::write(&note, invalid).unwrap();
    repo.stage_artifact_path(&note).unwrap();
    let second = repo.commit_staged("invalid note").unwrap();

    let diff = repo
        .diff_revisions(&first.id, &second.id, Some("note.md"))
        .unwrap();
    assert_eq!(diff.artifacts[0].kind, RepoTrackedPathKind::TextFile);
    let content = repo
        .diff_text_content(&diff.artifacts[0], ByteUnit::new(16 * 1024))
        .unwrap();
    assert!(matches!(
        content.after,
        RepoTextContentState::InvalidUtf8 { size: 8193, .. }
    ));
    let read = repo
        .read_path_content(&second.id, "note.md", ByteUnit::new(16 * 1024))
        .unwrap();
    assert!(matches!(
        read.content,
        RepoPathContentState::InvalidUtf8 { size: 8193, .. }
    ));
}

#[test]
fn read_path_content_rejects_inline_blob_metadata_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    let note = tmp.path().join("note.md");
    fs::write(&note, "hello").unwrap();
    repo.stage_artifact_path(&note).unwrap();
    let commit = repo.commit_staged("note").unwrap();
    let state = commit.artifacts.get("note.md").unwrap();
    let object_path = repo.object_store().path_for(state.oid());
    let encoded = fs::read_to_string(&object_path).unwrap();
    assert!(encoded.contains("aGVsbG8="));
    fs::write(&object_path, encoded.replace("aGVsbG8=", "d29ybGQ=")).unwrap();

    let error = repo
        .read_path_content(&commit.id, "note.md", ByteUnit::new(128))
        .unwrap_err();
    assert!(matches!(error, RepoErr::Object(_)));
}

#[test]
fn diff_staged_and_worktree_file_reports_git_like_states() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    let volume = VolumeId::random();
    let log = LogId::random();
    let first_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
    let staged_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(4), PageCount::new(4));
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
    let first_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
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
    let first_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
    let second_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(4), PageCount::new(4));
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
    let first_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(2), PageCount::new(3));
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
    let feature_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(4), PageCount::new(4));
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
    let feature_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(4), PageCount::new(4));
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
    let feature_snapshot = Snapshot::new(log.clone(), LSN::FIRST..=LSN::new(4), PageCount::new(4));
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
fn merge_plan_rejects_a_moved_local_head() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path()).unwrap();
    let base = repo.commit("base").unwrap();
    repo.switch_new_branch("feature", None).unwrap();
    let feature = repo.commit("feature").unwrap();
    repo.switch_branch("main").unwrap();

    let plan = repo.plan_merge_revision("feature").unwrap();
    assert_eq!(
        plan.outcome,
        MergeOutcome::FastForward {
            from: Some(base.id.clone()),
            to: feature.id,
        }
    );
    let moved = repo.commit("main moved after plan").unwrap();

    assert!(matches!(
        repo.apply_merge_plan(&plan),
        Err(RepoErr::MergePlanStale { expected, actual })
            if expected == Some(base.id) && actual == Some(moved.id.clone())
    ));
    assert_eq!(repo.head_target().unwrap(), Some(moved.id));
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
    let object::Object::Commit(commit_object) = repo.read_object(&merge_commit.id).unwrap() else {
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
    let object::Object::Tree(_) = clone.read_object(commit_object.tree.as_str()).unwrap() else {
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
fn incremental_push_does_not_prepare_unchanged_large_artifact_payloads() {
    let remote_dir = tempfile::tempdir().unwrap();
    let remote = RemoteConfig::Fs {
        root: remote_dir.path().to_string_lossy().into_owned(),
    };

    let source_dir = tempfile::tempdir().unwrap();
    let source = Repository::init(source_dir.path()).unwrap();
    source.remote_add("origin", remote).unwrap();

    let asset = source_dir.path().join("assets/model.bin");
    fs::create_dir_all(asset.parent().unwrap()).unwrap();
    fs::write(&asset, vec![7_u8; 2 * 1024 * 1024]).unwrap();
    source
        .stage_artifact_path_with_inline_text_threshold(&asset, 4)
        .unwrap();
    let initial = source.commit_staged("track model").unwrap();
    source.push("origin", "main").unwrap();

    let note = source_dir.path().join("checkpoint.txt");
    fs::write(&note, b"small checkpoint").unwrap();
    source.stage_artifact_path(&note).unwrap();
    let checkpoint = source.commit_staged("small checkpoint").unwrap();
    let remote_store = source.remote_store("origin").unwrap();
    let prepared = source
        .push_commit_chain(&remote_store, &checkpoint.id, Some(&initial.id))
        .unwrap();

    assert_eq!(prepared.commits, 1);
    assert!(
        prepared.bundle_objects.is_empty(),
        "the unchanged 2 MiB payload must not be prepared for the incremental push"
    );

    source.push("origin", "main").unwrap();

    let second_asset = source_dir.path().join("assets/new-model.bin");
    fs::write(&second_asset, vec![9_u8; 1024 * 1024]).unwrap();
    source
        .stage_artifact_path_with_inline_text_threshold(&second_asset, 4)
        .unwrap();
    let with_new_payload = source.commit_staged("add another model").unwrap();
    let prepared = source
        .push_commit_chain(&remote_store, &with_new_payload.id, Some(&checkpoint.id))
        .unwrap();

    assert_eq!(prepared.bundle_objects.len(), 1);
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
        issue.path == "assets/model.bin" && issue.kind == RepoArtifactAuditIssueKind::MissingObject
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
fn upstream_status_reports_heads_and_common_ancestor_for_divergence() {
    let remote_dir = tempfile::tempdir().unwrap();
    let remote = RemoteConfig::Fs {
        root: remote_dir.path().to_string_lossy().into_owned(),
    };

    let source_dir = tempfile::tempdir().unwrap();
    let source = Repository::init(source_dir.path()).unwrap();
    source.remote_add("origin", remote.clone()).unwrap();
    let base = source.commit("base").unwrap();
    source.push("origin", "main").unwrap();

    let clone_dir = tempfile::tempdir().unwrap();
    let clone = Repository::init(clone_dir.path()).unwrap();
    clone.remote_add("origin", remote).unwrap();
    clone.set_branch_upstream("main", "origin", "main").unwrap();
    clone.pull("origin", "main", "main").unwrap();

    let remote_commit = source.commit("remote").unwrap();
    source.push("origin", "main").unwrap();
    let local_commit = clone.commit("local").unwrap();
    clone.fetch("origin", "main").unwrap();

    assert_eq!(
        clone.status().unwrap().upstream_status,
        Some(RepoUpstreamStatus {
            remote: "origin".to_string(),
            branch: "main".to_string(),
            local: local_commit.id,
            remote_target: remote_commit.id,
            common_ancestor: Some(base.id),
            ahead: 1,
            behind: 1,
            state: RepoUpstreamState::Diverged,
        })
    );
}

#[test]
fn upstream_status_prefers_the_nearest_common_ancestor_after_a_merge() {
    let remote_dir = tempfile::tempdir().unwrap();
    let remote = RemoteConfig::Fs {
        root: remote_dir.path().to_string_lossy().into_owned(),
    };

    let source_dir = tempfile::tempdir().unwrap();
    let source = Repository::init(source_dir.path()).unwrap();
    source.remote_add("origin", remote.clone()).unwrap();
    source.commit("root").unwrap();
    source.commit("branch point").unwrap();
    source.branch_create("side", None).unwrap();
    let shared_first_parent = source.commit("main before merge").unwrap();
    source.push("origin", "main").unwrap();

    let clone_dir = tempfile::tempdir().unwrap();
    let clone = Repository::init(clone_dir.path()).unwrap();
    clone.remote_add("origin", remote).unwrap();
    clone.set_branch_upstream("main", "origin", "main").unwrap();
    clone.pull("origin", "main", "main").unwrap();

    source.switch_branch("side").unwrap();
    source.commit("side before merge").unwrap();
    source.switch_branch("main").unwrap();
    source.merge_revision("side").unwrap();
    source.commit("merge").unwrap();
    source.push("origin", "main").unwrap();

    clone.commit("local after shared first parent").unwrap();
    clone.fetch("origin", "main").unwrap();

    assert_eq!(
        clone
            .status()
            .unwrap()
            .upstream_status
            .unwrap()
            .common_ancestor,
        Some(shared_first_parent.id)
    );
}

#[test]
fn pull_plan_starts_snapshot_checkout_with_fresh_http_pool() {
    use std::io::{ErrorKind, Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::{Duration, Instant};

    fn read_headers(stream: &mut std::net::TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        request
    }

    fn write_response(stream: &mut std::net::TcpStream, body: &[u8]) {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nGraft-Protocol: 1\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();
    }

    let repo_dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(repo_dir.path()).unwrap();
    let commit = repo.commit("base").unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let head = format!("{}\n", commit.id);
    let server = thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        assert!(
            String::from_utf8_lossy(&read_headers(&mut first))
                .starts_with("GET /org/repo/raw/refs/heads/main ")
        );
        write_response(&mut first, head.as_bytes());

        first.set_nonblocking(true).unwrap();
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut reused_request = Vec::new();
        loop {
            let mut buffer = [0_u8; 1024];
            match first.read(&mut buffer) {
                Ok(0) => {}
                Ok(read) => {
                    reused_request.extend_from_slice(&buffer[..read]);
                    if reused_request
                        .windows(4)
                        .any(|window| window == b"\r\n\r\n")
                    {
                        write_response(&mut first, b"reused");
                        return;
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {}
                Err(err) => panic!("failed reading reused connection: {err}"),
            }

            match listener.accept() {
                Ok((mut fresh, _)) => {
                    fresh.set_nonblocking(false).unwrap();
                    read_headers(&mut fresh);
                    write_response(&mut fresh, b"fresh");
                    return;
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {}
                Err(err) => panic!("failed accepting fresh connection: {err}"),
            }

            assert!(
                Instant::now() < deadline,
                "client sent neither a reused nor a fresh request"
            );
            thread::sleep(Duration::from_millis(1));
        }
    });

    repo.remote_add(
        "origin",
        RemoteConfig::Http {
            url: format!("http://{address}/org/repo"),
            token_env: None,
        },
    )
    .unwrap();

    repo.plan_pull("origin", "main", "main").unwrap();
    let checkout_remote = repo.remote_store("origin").unwrap();
    let connection = block_on_remote(checkout_remote.get_raw("snapshot-phase-probe"))
        .unwrap()
        .unwrap();
    server.join().unwrap();

    assert_eq!(connection.as_ref(), b"fresh");
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
