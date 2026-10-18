use super::*;

#[test]
fn split_assets_are_embedded_and_only_hashed_assets_are_cached() {
    assert!(
        embedded_asset("/")
            .unwrap()
            .content_type
            .starts_with("text/html")
    );
    let entry = embedded_asset("/app.js").unwrap();
    assert!(
        entry.bytes.len() < 1024 * 1024,
        "initial JavaScript exceeds the 1 MB budget"
    );
    assert_eq!(asset_cache_control("/"), "no-store");
    assert_eq!(asset_cache_control("/app.js"), "no-store");
    let mut paths = BTreeSet::new();
    for asset in ASSETS {
        assert!(paths.insert(asset.path), "duplicate embedded asset");
        assert!(!asset.bytes.is_empty());
        if asset.path.starts_with("/assets/") {
            assert!(asset_cache_control(asset.path).contains("immutable"));
            assert!(embedded_asset(asset.path).is_some());
        }
    }
    for path in [
        "/assets.rs",
        "/assets/../app.js",
        "/.graft/config",
        "/src/main.tsx",
    ] {
        assert!(embedded_asset(path).is_none());
    }
    assert!(
        ASSETS
            .iter()
            .any(|asset| asset.path.starts_with("/assets/Code-"))
    );
    assert!(ASSETS.iter().any(|asset| asset.path.contains("pdf.worker")
        && asset.content_type.starts_with("text/javascript")));
}

fn fixture() -> (tempfile::TempDir, Viewer, String) {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("src")).unwrap();
    fs::write(
        directory.path().join("src/main.ts"),
        "export const count = 1;\n",
    )
    .unwrap();
    let database = Connection::open(directory.path().join("data.sqlite")).unwrap();
    database.execute_batch("CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT); INSERT INTO notes VALUES(1, 'before');").unwrap();
    drop(database);
    let session = RepositorySession::new(directory.path());
    session.open().unwrap();
    session.init().unwrap();
    session.add_all().unwrap();
    let commit = session.commit("initial files").unwrap()["commit"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    drop(session);
    let viewer = Viewer::open(directory.path()).unwrap();
    (directory, viewer, commit)
}

fn input(path: &str) -> Input {
    Input {
        path: Some(path.into()),
        ..Input::default()
    }
}

#[test]
fn database_cell_reads_complete_text_and_rejects_stale_or_invalid_reads() {
    let (directory, viewer, _) = fixture();
    let connection = Connection::open(directory.path().join("data.sqlite")).unwrap();
    let text = format!(
        "{}<p>literal HTML, not rendered</p>END",
        "长文本\n".repeat(2000)
    );
    connection
        .execute("UPDATE notes SET body=?1 WHERE id=1", [&text])
        .unwrap();
    let page = viewer.database(&input("data.sqlite")).unwrap();
    assert_eq!(page["truncated_cells"][0], json!([1]));
    assert_ne!(page["rows"][0][1].as_str().unwrap(), text);
    let request = Input {
        table: Some("notes".into()),
        column: Some("body".into()),
        offset: Some(0),
        expected: Some(page["version"].to_string()),
        ..input("data.sqlite")
    };
    assert_eq!(viewer.database_cell(&request).unwrap()["value"], text);
    assert_eq!(
        viewer
            .database(&Input {
                expected: request.expected.clone(),
                ..input("data.sqlite")
            })
            .unwrap()["unchanged"],
        true
    );
    assert!(
        viewer
            .database_cell(&Input {
                column: Some("missing\"; DROP TABLE notes;--".into()),
                ..Input {
                    table: request.table.clone(),
                    offset: request.offset,
                    expected: request.expected.clone(),
                    ..input("data.sqlite")
                }
            })
            .is_err()
    );
    assert!(
        viewer
            .database_cell(&Input {
                column: Some("body".into()),
                table: Some("notes".into()),
                offset: Some(0),
                ..input("data.sqlite")
            })
            .is_err()
    );
    connection
        .execute(
            "UPDATE notes SET body='changed after opening' WHERE id=1",
            [],
        )
        .unwrap();
    assert!(
        viewer
            .database_cell(&request)
            .unwrap_err()
            .to_string()
            .contains("changed")
    );
    let next = viewer.database(&input("data.sqlite")).unwrap();
    let huge = "x".repeat(MAX_TEXT as usize + 1);
    connection
        .execute("UPDATE notes SET body=?1 WHERE id=1", [&huge])
        .unwrap();
    let fresh = viewer.database(&input("data.sqlite")).unwrap();
    assert_ne!(next["version"], fresh["version"]);
    assert!(
        viewer
            .database_cell(&Input {
                table: Some("notes".into()),
                column: Some("body".into()),
                offset: Some(0),
                expected: Some(fresh["version"].to_string()),
                ..input("data.sqlite")
            })
            .unwrap_err()
            .to_string()
            .contains("2 MB")
    );
}

#[test]
fn database_pages_reject_changes_between_viewports() {
    let (directory, viewer, _) = fixture();
    let page = viewer.database(&input("data.sqlite")).unwrap();
    let request = Input {
        table: Some("notes".into()),
        offset: Some(1),
        expected: Some(page["version"].to_string()),
        ..input("data.sqlite")
    };
    let next = viewer.database(&request).unwrap();
    assert_eq!(next["version"], page["version"]);
    assert!(next.get("unchanged").is_none());
    let database = Connection::open(directory.path().join("data.sqlite")).unwrap();
    database
        .execute("INSERT INTO notes VALUES(2, 'new')", [])
        .unwrap();
    assert!(
        viewer
            .database(&request)
            .unwrap_err()
            .to_string()
            .contains("changed")
    );
    assert!(
        viewer
            .database(&Input { offset: Some(1), ..input("data.sqlite") })
            .is_err()
    );
}

#[test]
fn history_filter_searches_beyond_first_page_and_keeps_matching_pagination() {
    let (directory, viewer, initial) = fixture();
    for index in 0..42 {
        fs::write(
            directory.path().join("src/main.ts"),
            format!("export const count = {};\n", index + 2),
        )
        .unwrap();
        viewer.session.add_all().unwrap();
        viewer
            .session
            .commit(&format!("checkpoint {index}"))
            .unwrap();
    }
    let result = viewer
        .history(&Input {
            filter: Some("INITIAL FILES".into()),
            ..Input::default()
        })
        .unwrap();
    assert_eq!(result["commits"].as_array().unwrap().len(), 1);
    assert_eq!(result["commits"][0]["id"], initial);
    assert_eq!(result["has_more"], false);
    let hash = viewer
        .history(&Input {
            filter: Some(initial[..12].into()),
            ..Input::default()
        })
        .unwrap();
    assert_eq!(hash["commits"][0]["id"], initial);
    let first = viewer
        .history(&Input {
            filter: Some("checkpoint".into()),
            ..Input::default()
        })
        .unwrap();
    assert_eq!(first["commits"].as_array().unwrap().len(), 40);
    assert_eq!(first["has_more"], true);
    let next = viewer
        .history(&Input {
            filter: Some("checkpoint".into()),
            after: first["next_cursor"].as_str().map(str::to_string),
            ..Input::default()
        })
        .unwrap();
    assert_eq!(next["commits"].as_array().unwrap().len(), 2);
    assert_eq!(next["has_more"], false);
    assert_ne!(first["commits"][39]["id"], next["commits"][0]["id"]);
    let missing = viewer
        .history(&Input {
            filter: Some("missing commit query".into()),
            ..Input::default()
        })
        .unwrap();
    assert!(missing["commits"].as_array().unwrap().is_empty());
}

#[test]
fn historical_database_summary_and_rows_reuse_metadata() {
    let (directory, viewer, _) = fixture();
    let database = Connection::open(directory.path().join("data.sqlite")).unwrap();
    database.execute_batch("WITH RECURSIVE ids(id) AS (SELECT 2 UNION ALL SELECT id + 1 FROM ids WHERE id < 20000) INSERT INTO notes SELECT id, 'added' FROM ids;").unwrap();
    drop(database);
    viewer.session.add_all().unwrap();
    let revision = viewer.session.commit("many rows").unwrap()["commit"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let request = Input {
        revision: Some(revision),
        ..input("data.sqlite")
    };
    let started = std::time::Instant::now();
    let catalog = viewer.database_tables(&request).unwrap();
    let catalog_elapsed = started.elapsed();
    let started = std::time::Instant::now();
    let rows = viewer.sqlite_diff(&request).unwrap();
    eprintln!(
        "historical catalog: {catalog_elapsed:?}; first page after catalog: {:?}",
        started.elapsed()
    );
    assert_eq!(catalog["tables"][0]["inserts"], 19999);
    assert_eq!(
        rows["tables"][0]["changes"].as_array().unwrap().len(),
        PAGE_SIZE
    );
    assert_eq!(rows["summaries"][0]["inserts"], 19999);
    assert_eq!(viewer.historical_summaries.borrow().len(), 1);
    // Mutable worktree comparisons must never reuse a historical summary.
    let database = Connection::open(directory.path().join("data.sqlite")).unwrap();
    database
        .execute_batch("UPDATE notes SET body = 'working' WHERE id = 1;")
        .unwrap();
    let working = viewer.database_tables(&input("data.sqlite")).unwrap();
    assert_eq!(working["tables"][0]["updates"], 1);
    assert_eq!(viewer.historical_summaries.borrow().len(), 1);
}

#[test]
fn branches_keep_the_session_usable_and_reject_dirty_switches() {
    let (directory, viewer, _) = fixture();
    viewer
        .branch_command(&Input {
            action: Some("create".into()),
            name: Some("review".into()),
            ..Input::default()
        })
        .unwrap();
    let branches = viewer.branch_command(&Input::default()).unwrap();
    assert!(
        branches["branches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|branch| branch["name"] == "review")
    );
    viewer
        .branch_command(&Input {
            action: Some("switch".into()),
            name: Some("review".into()),
            ..Input::default()
        })
        .unwrap();
    assert_eq!(viewer.session.status().unwrap()["current_branch"], "review");
    assert!(
        viewer
            .branch_command(&Input {
                action: Some("switch".into()),
                name: Some("missing".into()),
                ..Input::default()
            })
            .is_err()
    );
    assert!(viewer.snapshot().is_ok());
    assert!(
        viewer
            .branch_command(&Input {
                action: Some("create".into()),
                name: Some("--force main".into()),
                ..Input::default()
            })
            .is_err()
    );
    fs::write(directory.path().join("src/main.ts"), "uncommitted edit\n").unwrap();
    // A switch that would overwrite an edit must use the service's normal rejection.
    viewer.session.add_all().unwrap();
    viewer.session.commit("review change").unwrap();
    fs::write(directory.path().join("src/main.ts"), "unsaved work\n").unwrap();
    assert!(
        viewer
            .branch_command(&Input {
                action: Some("switch".into()),
                name: Some("main".into()),
                ..Input::default()
            })
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("src/main.ts")).unwrap(),
        "unsaved work\n"
    );
    assert!(viewer.snapshot().is_ok());
}

#[test]
fn conflict_endpoints_preview_and_resolve_with_state_tokens() {
    let (directory, viewer, _) = fixture();
    let policy = viewer.session.get_merge_policy().unwrap();
    let mut document = policy.policy;
    document.config.same_row_merge = true;
    viewer
        .session
        .set_merge_policy(&graft_sdk::SetMergePolicyOptions {
            policy: document,
            expected_policy_token: policy.policy_token,
        })
        .unwrap();
    Connection::open(directory.path().join("data.sqlite")).unwrap().execute_batch(
        "ALTER TABLE notes ADD COLUMN title TEXT DEFAULT 'base'; UPDATE notes SET body='before one', title='base'; INSERT INTO notes VALUES(2, 'before two', 'base');"
    ).unwrap();
    viewer.session.add_all().unwrap();
    viewer.session.commit("additional conflict fields").unwrap();
    viewer
        .branch_command(&Input {
            action: Some("create".into()),
            name: Some("incoming".into()),
            ..Input::default()
        })
        .unwrap();
    viewer
        .branch_command(&Input {
            action: Some("switch".into()),
            name: Some("incoming".into()),
            ..Input::default()
        })
        .unwrap();
    fs::write(
        directory.path().join("src/main.ts"),
        "export const count = 3;\n",
    )
    .unwrap();
    Connection::open(directory.path().join("data.sqlite"))
        .unwrap()
        .execute(
            "UPDATE notes SET body='incoming', title='incoming title'",
            [],
        )
        .unwrap();
    viewer.session.add_all().unwrap();
    viewer.session.commit("incoming edit").unwrap();
    viewer
        .branch_command(&Input {
            action: Some("switch".into()),
            name: Some("main".into()),
            ..Input::default()
        })
        .unwrap();
    fs::write(
        directory.path().join("src/main.ts"),
        "export const count = 2;\n",
    )
    .unwrap();
    Connection::open(directory.path().join("data.sqlite"))
        .unwrap()
        .execute("UPDATE notes SET body='current', title='current title'", [])
        .unwrap();
    viewer.session.add_all().unwrap();
    viewer.session.commit("current edit").unwrap();
    let head = viewer.session.repository_metadata().unwrap().current_head;
    let plan = viewer
        .session
        .plan_merge(&graft_sdk::PlanMergeOptions {
            revision: "incoming".into(),
            expected_head: head.clone(),
        })
        .unwrap();
    viewer
        .session
        .apply_merge(&graft_sdk::ApplyMergeOptions {
            revision: "incoming".into(),
            expected_head: head,
            plan_token: plan.plan_token,
        })
        .unwrap();
    let page = viewer.merge_paths(&Input::default()).unwrap();
    assert_eq!(page["status"]["unmerged_count"], 2);
    let token = page["status"]["state_token"].as_str().unwrap().to_string();
    let database = viewer
        .conflict(&Input {
            path: Some("data.sqlite".into()),
            state_token: Some(token.clone()),
            ..Input::default()
        })
        .unwrap();
    let snapshot = viewer.snapshot().unwrap();
    assert!(
        snapshot["paths"]
            .as_array()
            .unwrap()
            .contains(&json!("data.sqlite"))
    );
    assert!(
        snapshot["paths"]
            .as_array()
            .unwrap()
            .contains(&json!("src/main.ts"))
    );
    assert_eq!(database["ours"]["content"]["state"], "sqlite");
    assert_eq!(
        database["tables"][0]["columns"],
        json!(["id", "body", "title"])
    );
    assert_eq!(database["tables"][0]["rowid_alias"], "id");
    assert!(!database["conflicts"].as_array().unwrap().is_empty());
    let preview = viewer
        .conflict(&Input {
            path: Some("src/main.ts".into()),
            state_token: Some(token.clone()),
            ..Input::default()
        })
        .unwrap();
    assert_eq!(
        preview["ours"]["content"]["content"],
        "export const count = 2;\n"
    );
    assert_eq!(
        preview["theirs"]["content"]["content"],
        "export const count = 3;\n"
    );
    assert!(preview["base"]["content"]["content"].is_string());
    assert!(
        viewer
            .dispatch(
                "/api/resolve",
                Input {
                    path: Some("src/main.ts".into()),
                    action: Some("edited".into()),
                    content: Some("export const count = 5;\n".into()),
                    expected: preview["result_etag"].as_str().map(str::to_string),
                    state_token: Some("stale".into()),
                    ..Input::default()
                }
            )
            .is_err()
    );
    for (content, expected) in [
        ("export const count = 5;\n", "stale".to_string()),
        (
            "<<<<<<< Current\na\n=======\nb\n>>>>>>> Incoming\n",
            preview["result_etag"].as_str().unwrap().to_string(),
        ),
    ] {
        assert!(
            viewer
                .dispatch(
                    "/api/resolve",
                    Input {
                        path: Some("src/main.ts".into()),
                        action: Some("edited".into()),
                        content: Some(content.into()),
                        expected: Some(expected),
                        state_token: Some(token.clone()),
                        ..Input::default()
                    }
                )
                .is_err()
        );
    }
    viewer
        .dispatch(
            "/api/resolve",
            Input {
                path: Some("src/main.ts".into()),
                action: Some("edited".into()),
                content: Some("export const count = 5;\n".into()),
                expected: preview["result_etag"].as_str().map(str::to_string),
                state_token: Some(token),
                ..Input::default()
            },
        )
        .unwrap();
    assert_eq!(
        fs::read_to_string(directory.path().join("src/main.ts")).unwrap(),
        "export const count = 5;\n"
    );
    let page = viewer.merge_paths(&Input::default()).unwrap();
    assert_eq!(page["status"]["unmerged_count"], 1);
    viewer
        .dispatch(
            "/api/resolve-cell",
            Input {
                path: Some("data.sqlite".into()),
                action: Some("ours".into()),
                table: Some("notes".into()),
                identity: Some(json!(1)),
                column: Some("body".into()),
                state_token: page["status"]["state_token"].as_str().map(str::to_string),
                ..Input::default()
            },
        )
        .unwrap();
    let page = viewer.merge_paths(&Input::default()).unwrap();
    assert_eq!(page["status"]["unmerged_count"], 1);
    assert!(
        viewer
            .dispatch(
                "/api/resolve-cell",
                Input {
                    path: Some("data.sqlite".into()),
                    table: Some("notes".into()),
                    identity: Some(json!(1)),
                    column: Some("title".into()),
                    action: Some("theirs".into()),
                    state_token: Some("stale".into()),
                    ..Input::default()
                }
            )
            .is_err()
    );
    viewer
        .dispatch(
            "/api/resolve-cell",
            Input {
                path: Some("data.sqlite".into()),
                table: Some("notes".into()),
                identity: Some(json!(1)),
                column: Some("title".into()),
                action: Some("theirs".into()),
                state_token: page["status"]["state_token"].as_str().map(str::to_string),
                ..Input::default()
            },
        )
        .unwrap();
    let page = viewer.merge_paths(&Input::default()).unwrap();
    assert_eq!(page["status"]["unmerged_count"], 1);
    viewer
        .dispatch(
            "/api/resolve-row",
            Input {
                path: Some("data.sqlite".into()),
                table: Some("notes".into()),
                identity: Some(json!(2)),
                action: Some("theirs".into()),
                state_token: page["status"]["state_token"].as_str().map(str::to_string),
                ..Input::default()
            },
        )
        .unwrap();
    let connection = Connection::open(directory.path().join("data.sqlite")).unwrap();
    let values: (String, String) = connection
        .query_row("SELECT body,title FROM notes WHERE id=1", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(values, ("current".into(), "incoming title".into()));
    assert_eq!(
        connection
            .query_row("SELECT body FROM notes WHERE id=2", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "incoming"
    );
    drop(connection);
    let page = viewer.merge_paths(&Input::default()).unwrap();
    assert_eq!(page["status"]["unmerged_count"], 0);
    viewer
        .dispatch(
            "/api/merge-continue",
            Input {
                content: Some("Reviewed merge".into()),
                state_token: page["status"]["state_token"].as_str().map(str::to_string),
                ..Input::default()
            },
        )
        .unwrap();
    assert_eq!(
        viewer.merge_paths(&Input::default()).unwrap()["status"]["state"],
        "none"
    );
}

#[test]
fn history_endpoint_returns_the_selected_commits_changed_paths() {
    let (_directory, viewer, revision) = fixture();
    let history = viewer.dispatch("/api/history", Input::default()).unwrap();
    assert_eq!(history["commits"][0]["id"], revision);
    let page = viewer
        .dispatch(
            "/api/commit",
            Input {
                revision: Some(revision),
                ..Input::default()
            },
        )
        .unwrap();
    assert_eq!(page["paths"].as_array().unwrap().len(), 2);
    assert_eq!(page["paths"][0]["path"], "data.sqlite");
}

#[test]
fn database_tree_lists_tables_without_rows_and_keeps_revision_scope() {
    let (directory, viewer, _) = fixture();
    let database = Connection::open(directory.path().join("data.sqlite")).unwrap();
    database.execute_batch("CREATE TABLE untouched(id); CREATE TABLE empty_deleted(id); CREATE TABLE \"a/b\"(id); CREATE TABLE \"a%2Fb\"(id);").unwrap();
    viewer.session.add_all().unwrap();
    viewer.session.commit("table baseline").unwrap();
    database.execute_batch("CREATE TABLE empty_added(id); DROP TABLE empty_deleted; UPDATE notes SET body='updated' WHERE id=1;").unwrap();

    let current = viewer
        .database_tables(&Input {
            view: Some("file".into()),
            ..input("data.sqlite")
        })
        .unwrap();
    let names = current["tables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|table| table["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["a%2Fb", "a/b", "empty_added", "notes", "untouched"]);
    assert!(current.get("rows").is_none());
    assert!(current["tables"][0].get("rows").is_none());

    let changes = viewer.database_tables(&input("data.sqlite")).unwrap();
    let names = changes["tables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|table| table["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["empty_added", "empty_deleted", "notes"]);
    assert_eq!(changes["tables"][0]["schema_changed"], true);
    assert_eq!(changes["tables"][1]["schema_changed"], true);
    assert_eq!(changes["tables"][2]["updates"], 1);

    viewer.session.add_all().unwrap();
    let revision = viewer.session.commit("schema and row changes").unwrap()["commit"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    database
        .execute_batch("DROP TABLE empty_added; CREATE TABLE later_table(id);")
        .unwrap();
    let historic = viewer
        .database_tables(&Input {
            revision: Some(revision),
            ..input("data.sqlite")
        })
        .unwrap();
    assert_eq!(historic, changes);
    assert_eq!(historic["default_table"], "notes");

    let literal = viewer
        .database(&Input {
            table: Some("a/b".into()),
            ..input("data.sqlite")
        })
        .unwrap();
    assert_eq!(literal["table"], "a/b");
    assert_eq!(literal["columns"], json!(["id"]));
}

#[test]
fn snapshot_database_candidates_are_verified_and_deleted_databases_keep_history_tables() {
    let (directory, viewer, root) = fixture();
    fs::write(directory.path().join("text.db"), "not a database").unwrap();
    assert_eq!(
        viewer.snapshot().unwrap()["databases"],
        json!(["data.sqlite"])
    );
    fs::remove_file(directory.path().join("data.sqlite")).unwrap();
    let deleted = viewer.database_tables(&input("data.sqlite")).unwrap();
    assert_eq!(deleted["tables"][0]["name"], "notes");
    assert_eq!(deleted["tables"][0]["deletes"], 1);
    let historic = viewer
        .database_tables(&Input {
            revision: Some(root),
            ..input("data.sqlite")
        })
        .unwrap();
    assert_eq!(historic["tables"][0]["name"], "notes");
    assert_eq!(historic["tables"][0]["inserts"], 1);
}

#[test]
fn sqlite_before_first_commit_can_preview_staged_rows() {
    let directory = tempfile::tempdir().unwrap();
    let database = Connection::open(directory.path().join("new.sqlite")).unwrap();
    database.execute_batch("CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT); INSERT INTO notes VALUES(1, 'first');").unwrap();
    drop(database);
    let session = RepositorySession::new(directory.path());
    session.open().unwrap();
    session.init().unwrap();
    session.add_all().unwrap();
    drop(session);
    let viewer = Viewer::open(directory.path()).unwrap();
    let diff = viewer.sqlite_diff(&input("new.sqlite")).unwrap();
    assert_eq!(diff["summaries"][0]["inserts"], 1);
    assert_eq!(diff["tables"][0]["changes"][0]["values"][1], "first");
    assert!(diff["message"].as_str().unwrap().contains("first commit"));
}

#[test]
fn text_review_and_history_include_added_and_deleted_files() {
    let (directory, viewer, root) = fixture();
    fs::write(
        directory.path().join("src/main.ts"),
        "export const count = 2;\n",
    )
    .unwrap();
    fs::write(directory.path().join("new.txt"), "new file\n").unwrap();
    let snapshot = viewer.snapshot().unwrap();
    assert!(
        snapshot["paths"]
            .as_array()
            .unwrap()
            .contains(&json!("new.txt"))
    );
    assert_eq!(
        viewer.file(&input("src/main.ts")).unwrap()["before"]["content"],
        "export const count = 1;\n"
    );
    assert_eq!(
        viewer.file(&input("new.txt")).unwrap()["before"]["state"],
        "absent"
    );
    let historic = viewer
        .file(&Input {
            revision: Some(root),
            ..input("src/main.ts")
        })
        .unwrap();
    assert_eq!(historic["before"]["state"], "absent");
    assert_eq!(historic["after"]["content"], "export const count = 1;\n");
    fs::remove_file(directory.path().join("src/main.ts")).unwrap();
    let deleted = viewer.file(&input("src/main.ts")).unwrap();
    assert_eq!(deleted["after"]["state"], "absent");
    assert_eq!(deleted["before"]["state"], "utf8");
}

#[test]
fn save_rejects_stale_drafts_and_never_stages_files() {
    let (directory, viewer, _) = fixture();
    let path = "src/main.ts";
    let pair = viewer.file(&input(path)).unwrap();
    let expected = pair["after"]["etag"].as_str().unwrap().to_string();
    let save = Input {
        expected: Some(expected),
        content: Some("saved\n".into()),
        ..input(path)
    };
    let result = viewer.save(&save).unwrap();
    assert_eq!(result["etag"], fingerprint(b"saved\n"));
    assert_eq!(
        fs::read_to_string(directory.path().join(path)).unwrap(),
        "saved\n"
    );
    assert_eq!(
        viewer.session.status().unwrap()["has_staged_changes"],
        false
    );
    fs::write(directory.path().join(path), "agent update\n").unwrap();
    let error = viewer.save(&save).unwrap_err();
    assert!(error.downcast_ref::<SaveConflict>().is_some());
    assert_eq!(
        fs::read_to_string(directory.path().join(path)).unwrap(),
        "agent update\n"
    );
    assert!(
        viewer
            .save(&Input {
                expected: Some("unused".into()),
                content: Some("text".into()),
                ..input("data.sqlite")
            })
            .is_err()
    );
}

#[test]
fn sqlite_inspection_reads_committed_wal_and_pages_row_diffs() {
    let (directory, viewer, root) = fixture();
    let database = Connection::open(directory.path().join("data.sqlite")).unwrap();
    database
        .execute_batch("PRAGMA journal_mode=WAL; UPDATE notes SET body = 'after' WHERE id=1;")
        .unwrap();
    for id in 2..=130 {
        database
            .execute("INSERT INTO notes VALUES(?1, 'added')", [id])
            .unwrap();
    }
    let page = viewer.database(&input("data.sqlite")).unwrap();
    assert_eq!(page["rows"][0][1], "after");
    assert_eq!(page["rows"].as_array().unwrap().len(), 100);
    assert_eq!(page["has_more"], true);
    assert_eq!(page["total_rows"], 130);
    let last = viewer
        .database(&Input {
            offset: Some(120),
            expected: Some(page["version"].to_string()),
            ..input("data.sqlite")
        })
        .unwrap();
    assert_eq!(last["total_rows"], 130);
    assert_eq!(last["rows"][0][0], 121);
    assert_eq!(last["rows"].as_array().unwrap().len(), 10);
    assert_eq!(last["has_more"], false);
    let diff = viewer.sqlite_diff(&input("data.sqlite")).unwrap();
    assert_eq!(diff["summaries"][0]["inserts"], 129);
    assert_eq!(diff["summaries"][0]["updates"], 1);
    assert_eq!(diff["tables"][0]["rowid_alias"], "id");
    assert_eq!(diff["tables"][0]["primary_key_columns"], json!(["id"]));
    assert_eq!(diff["tables"][0]["changes"].as_array().unwrap().len(), 100);
    let next = viewer
        .sqlite_diff(&Input {
            table: Some("notes".into()),
            after: Some(diff["next_cursor"].as_str().unwrap().into()),
            ..input("data.sqlite")
        })
        .unwrap();
    assert_eq!(next["tables"][0]["changes"].as_array().unwrap().len(), 30);
    let history = viewer
        .sqlite_diff(&Input {
            revision: Some(root),
            ..input("data.sqlite")
        })
        .unwrap();
    assert_eq!(history["summaries"][0]["inserts"], 1);
    assert_eq!(history["tables"][0]["changes"][0]["values"][1], "before");
    let initial = viewer.database(&input("data.sqlite")).unwrap();
    database
        .execute(
            "UPDATE notes SET body='outside first page' WHERE id=130",
            [],
        )
        .unwrap();
    let refreshed = viewer.database(&input("data.sqlite")).unwrap();
    assert_eq!(initial["rows"], refreshed["rows"]);
    assert_ne!(initial["version"], refreshed["version"]);
}

#[test]
fn paths_and_session_credentials_reject_escape_attempts() {
    let (_directory, viewer, _) = fixture();
    for path in [
        "../outside",
        "/etc/passwd",
        ".graft/config.toml",
        "src/../main.ts",
        "",
    ] {
        assert!(viewer.safe_path(path).is_err(), "accepted {path}");
    }
    assert!(authorized(
        Some("127.0.0.1:1234"),
        Some("http://127.0.0.1:1234"),
        Some("Bearer secret"),
        "http://127.0.0.1:1234",
        "secret"
    ));
    assert!(!authorized(
        Some("evil.test:1234"),
        None,
        Some("Bearer secret"),
        "http://127.0.0.1:1234",
        "secret"
    ));
    assert!(!authorized(
        Some("127.0.0.1:1234"),
        Some("https://evil.test"),
        Some("Bearer secret"),
        "http://127.0.0.1:1234",
        "secret"
    ));
    assert!(!authorized(
        Some("127.0.0.1:1234"),
        None,
        None,
        "http://127.0.0.1:1234",
        "secret"
    ));
}

#[cfg(unix)]
#[test]
fn symbolic_links_cannot_expose_repository_metadata_or_outside_files() {
    use std::os::unix::fs::symlink;
    let (directory, viewer, _) = fixture();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    symlink(outside.path(), directory.path().join("outside")).unwrap();
    symlink(
        directory.path().join(".graft"),
        directory.path().join("metadata"),
    )
    .unwrap();
    assert!(viewer.safe_path("outside/secret.txt").is_err());
    assert!(viewer.safe_path("metadata/config.toml").is_err());
}

#[test]
fn existing_directory_shorthand_preserves_regular_commands() {
    let directory = tempfile::tempdir().unwrap();
    let args = directory_arguments(vec![
        "graft".into(),
        directory.path().as_os_str().into(),
        "--no-open".into(),
    ]);
    assert_eq!(args[1], "ui");
    assert_eq!(args[2], directory.path().as_os_str());
    assert_eq!(
        directory_arguments(vec!["graft".into(), "--version".into()])[1],
        "--version"
    );
    assert_eq!(
        directory_arguments(vec!["graft".into(), "status".into()])[1],
        "status"
    );
}
