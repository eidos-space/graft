use graft::core::{LogId, PageCount};
use graft_test::GraftTestRuntime;
use rusqlite::{Connection, OpenFlags, ToSql, functions::FunctionFlags};
use serde_json::Value;

#[test]
fn test_sync_and_reset() {
    graft_test::ensure_test_env();

    // create two nodes connected to the same remote
    let remote = LogId::random();
    let mut runtime1 = GraftTestRuntime::with_memory_remote();
    let sqlite1 = runtime1.open_sqlite("main", Some(remote.clone()));
    let mut runtime2 = runtime1.spawn_peer();
    let sqlite2 = runtime2.open_sqlite("main", Some(remote));

    // create two counter tables
    sqlite1
        .execute_batch(
            r#"
            CREATE TABLE t1 (counter INTEGER);
            INSERT INTO t1 VALUES (0);
            CREATE TABLE t2 (counter INTEGER);
            INSERT INTO t2 VALUES (0);
            "#,
        )
        .unwrap();

    // sync the changes from node 1 to node 2
    sqlite1.graft_pragma("debug_volume_push").unwrap();
    sqlite2.graft_pragma("debug_volume_pull").unwrap();

    // write to both nodes, creating a conflict
    sqlite1.execute("update t1 set counter = 1", []).unwrap();
    sqlite2.execute("update t2 set counter = 1", []).unwrap();

    // sync the changes from node 1
    sqlite1.graft_pragma("debug_volume_push").unwrap();

    // attempt to push from node 2, which should detect the conflict
    let result = sqlite2.pragma_query(None, "graft_debug_volume_push", |_| Ok(()));
    assert!(result.is_err(), "push should fail due to divergence");

    // force reset node 2 to the latest remote
    sqlite2.graft_pragma("debug_volume_fetch").unwrap();
    sqlite2.graft_pragma("debug_volume_clone").unwrap();

    // verify both nodes are now pointing at the same remote LSN
    // and they have no outstanding local changes
    let graft1 = runtime1.tag_get("main").unwrap().unwrap();
    let status1 = runtime1.volume_status(&graft1).unwrap();
    let graft2 = runtime2.tag_get("main").unwrap().unwrap();
    let status2 = runtime2.volume_status(&graft2).unwrap();
    assert_eq!(status1.remote, status2.remote);
    assert_eq!(status1.remote_status.base, status2.remote_status.base);
    assert_eq!(status1.local_status.changes(), None);
    assert_eq!(status2.local_status.changes(), None);

    // verify that node2 sees that the t1 counter is 1 and the t2 counter is 0
    let t1_counter: u32 = sqlite2
        .query_row("select counter from t1", [], |row| row.get(0))
        .unwrap();
    let t2_counter: u32 = sqlite2
        .query_row("select counter from t2", [], |row| row.get(0))
        .unwrap();
    assert_eq!(t1_counter, 1);
    assert_eq!(t2_counter, 0);

    // shutdown everything
    runtime1.shutdown().unwrap();
    runtime2.shutdown().unwrap();
}

#[test]
fn test_export() {
    graft_test::ensure_test_env();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite("main", None);

    // Create a table with some data
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE test_data (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                value INTEGER NOT NULL
            );
            INSERT INTO test_data (id, name, value) VALUES
                (1, 'Alice', 100),
                (2, 'Bob', 200),
                (3, 'Charlie', 300);
            "#,
        )
        .unwrap();

    // Verify the data
    let count: i64 = sqlite
        .query_row("SELECT COUNT(*) FROM test_data", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 3);

    // Create a temporary directory for export
    let temp_dir = tempfile::tempdir().unwrap();
    let export_path = temp_dir.path().join("exported.db");
    let export_path_str = export_path.to_str().unwrap();

    // Export the database
    sqlite
        .graft_pragma_arg("debug_volume_export", export_path_str)
        .unwrap();

    // Verify the exported file exists
    assert!(export_path.exists());

    // Open the exported SQLite file directly to verify it's valid
    let exported_conn = Connection::open(&export_path).unwrap();

    // Verify we can query the exported database
    let count: i64 = exported_conn
        .query_row("SELECT COUNT(*) FROM test_data", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 3);

    // Verify the data is correct
    let name: String = exported_conn
        .query_row("SELECT name FROM test_data WHERE id = 2", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(name, "Bob");

    // Cleanup
    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_history_pragmas_require_repository() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_path.to_str().unwrap(), None);

    for err in [
        pragma_query_error(&sqlite, "graft_log"),
        pragma_query_error(&sqlite, "graft_json_log"),
        pragma_query_error(&sqlite, "graft_status"),
        pragma_query_error(&sqlite, "graft_tags"),
        pragma_query_error(&sqlite, "graft_diff"),
        pragma_query_error(&sqlite, "graft_json_diff"),
        pragma_arg_error(&sqlite, "graft_diff", "1,3"),
        pragma_arg_error(&sqlite, "graft_json_diff", "1,3"),
        pragma_arg_error(&sqlite, "graft_show", "HEAD"),
        pragma_arg_error(&sqlite, "graft_json_show", "HEAD"),
        pragma_arg_error(&sqlite, "graft_fetch", "origin main"),
        pragma_arg_error(&sqlite, "graft_pull", "origin main"),
        pragma_arg_error(&sqlite, "graft_push", "origin main"),
    ] {
        assert!(
            err.contains("no .graft repository"),
            "expected repo-not-found error, got: {err}"
        );
    }

    runtime.shutdown().unwrap();
}

#[test]
fn test_debug_lsn_pragmas_expose_storage_coordinates_without_repo() {
    graft_test::ensure_test_env();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite("main", None);

    sqlite
        .execute_batch(
            r#"
            CREATE TABLE debug_lsn (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO debug_lsn (name) VALUES ('Alice');
            UPDATE debug_lsn SET name = 'Alicia' WHERE id = 1;
            "#,
        )
        .unwrap();

    let vid = runtime.tag_get("main").unwrap().unwrap();
    let volume = runtime.volume_get(&vid).unwrap();
    let log = pragma_query_string(&sqlite, "graft_debug_log_lsn");
    assert!(log.contains(&format!("log {}", volume.local)));
    assert!(log.contains(&format!("commit {}:2", volume.local)));
    assert!(log.contains(&format!("commit {}:3", volume.local)));

    let show = pragma_arg_string(
        &sqlite,
        "graft_debug_show_lsn",
        format!("{}:3", volume.local),
    );
    assert!(show.contains(&format!("Commit @ {}:3", volume.local)));
    assert!(show.contains("commit_hash:"));

    let diff = pragma_arg_string(
        &sqlite,
        "graft_debug_diff_lsn",
        format!("{}:2 {}:3", volume.local, volume.local),
    );
    assert!(diff.contains("Diff between LSN 2 and LSN 3"));
    assert!(diff.contains("Changed pages:"));

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_pragmas_on_physical_database_path() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();
    let remote_dir = temp_dir.path().join("remote");

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    let init = pragma_query_string(&sqlite, "graft_init");
    assert!(init.contains(".graft"));
    let source_repo = graft::repo::Repository::discover_for_file(&db_path).unwrap();
    assert!(
        source_repo.store_dir().read_dir().unwrap().next().is_some(),
        "graft_init should switch the connection to repo-local storage"
    );

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["head"]["type"], "branch");
    assert_eq!(status["head"]["name"], "main");
    assert_eq!(status["head_target"], Value::Null);
    assert_eq!(status["dirty"], false);

    sqlite
        .execute_batch(
            r#"
            CREATE TABLE repo_test (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_test (name) VALUES ('Alice');
            "#,
        )
        .unwrap();

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], true);
    assert_eq!(status["unstaged"][0], "app.db");
    assert_eq!(status["unstaged_changes"][0]["path"], "app.db");
    assert_eq!(status["unstaged_changes"][0]["change"], "untracked");
    let text_status = pragma_query_string(&sqlite, "graft_status");
    assert!(text_status.contains("Changes not staged for commit."));
    assert!(text_status.contains("untracked: app.db"));

    let add = pragma_query_string(&sqlite, "graft_add");
    assert!(add.contains("app.db"));
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], false);
    assert_eq!(status["unstaged"].as_array().unwrap().len(), 0);
    assert_eq!(status["staged"][0], "app.db");

    let commit = pragma_arg_string(&sqlite, "graft_commit", "initial schema");
    assert!(commit.contains("initial schema"));

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], false);
    assert_eq!(status["staged"].as_array().unwrap().len(), 0);
    assert!(status["head_target"].as_str().is_some());

    let vfs_id = runtime.ensure_vfs().to_string();
    let readonly_uri = format!("file:{db_name}?vfs={vfs_id}");
    let readonly = Connection::open_with_flags(
        readonly_uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("repo-local database should reopen read-only through the same VFS");
    let readonly_count: i64 = readonly
        .query_row("SELECT COUNT(*) FROM repo_test", [], |row| row.get(0))
        .unwrap();
    assert_eq!(readonly_count, 1);
    drop(readonly);

    let branch = pragma_arg_string(&sqlite, "graft_branch_create", "feature/search");
    assert!(branch.contains("feature/search"));
    let branch_conflict = pragma_arg_error(&sqlite, "graft_branch_create", "feature");
    assert!(branch_conflict.contains("cannot create ref `refs/heads/feature`"));
    let switched = pragma_arg_string(&sqlite, "graft_switch_branch", "feature/search");
    assert!(switched.contains("feature/search"));

    sqlite
        .execute("INSERT INTO repo_test (name) VALUES ('Bob')", [])
        .unwrap();
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], true);
    assert_eq!(status["unstaged_changes"][0]["path"], "app.db");
    assert_eq!(status["unstaged_changes"][0]["change"], "modified");
    let text_status = pragma_query_string(&sqlite, "graft_status");
    assert!(text_status.contains("modified: app.db"));

    let add = pragma_query_string(&sqlite, "graft_add");
    assert!(add.contains("app.db"));
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], false);
    assert_eq!(status["staged"][0], "app.db");

    let commit = pragma_arg_string(&sqlite, "graft_commit", "feature row");
    assert!(commit.contains("feature row"));

    let diff = pragma_arg_string(&sqlite, "graft_diff", "HEAD~1 HEAD -- app.db");
    assert!(diff.contains("modified: app.db"));

    let json_diff: Value = serde_json::from_str(&pragma_arg_string(
        &sqlite,
        "graft_json_diff",
        "HEAD~1 HEAD -- app.db",
    ))
    .expect("graft_json_diff should return repo diff JSON");
    assert_eq!(json_diff["files"][0]["path"], "app.db");
    assert_eq!(json_diff["files"][0]["change"], "modified");

    let show = pragma_arg_string(&sqlite, "graft_show", "HEAD");
    assert!(show.contains("feature row"));
    assert!(show.contains("app.db"));

    let json_show: Value =
        serde_json::from_str(&pragma_arg_string(&sqlite, "graft_json_show", "HEAD"))
            .expect("graft_json_show should return repo commit JSON");
    assert_eq!(json_show["message"], "feature row");
    assert!(json_show["files"]["app.db"].is_object());
    let app_ranges = json_show["files"]["app.db"]["snapshot"]["ranges"]
        .as_array()
        .expect("app.db should expose snapshot ranges");
    let app_commits = app_ranges[0]["commits"]
        .as_array()
        .expect("repo snapshot ranges should record storage commit hashes");
    assert!(!app_commits.is_empty());
    let app_commit_hash = app_commits[0]["commit_hash"]
        .as_str()
        .expect("repo snapshot commit entries should include commit hashes");
    assert_eq!(app_commit_hash.len(), 44);

    let json_log: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_log"))
        .expect("graft_json_log should return repo commit JSON");
    assert_eq!(json_log[0]["message"], "feature row");
    assert_eq!(json_log[0]["changed_tables"], 1);
    assert_eq!(json_log[0]["tables"][0]["name"], "repo_test");
    assert_eq!(json_log[0]["tables"][0]["inserts"], 1);
    assert_eq!(json_log[0]["tables"][0]["deletes"], 0);
    assert_eq!(json_log[0]["tables"][0]["updates"], 0);

    let tag = pragma_arg_string(&sqlite, "graft_tag_create", "v-feature HEAD");
    assert!(tag.contains("Created tag 'v-feature'"));
    let namespaced_tag = pragma_arg_string(&sqlite, "graft_tag_create", "v-test/feature HEAD");
    assert!(namespaced_tag.contains("Created tag 'v-test/feature'"));
    let tag_conflict = pragma_arg_error(&sqlite, "graft_tag_create", "v-test HEAD");
    assert!(tag_conflict.contains("cannot create ref `refs/tags/v-test`"));
    let deleted_tag = pragma_arg_string(&sqlite, "graft_tag_delete", "v-test/feature");
    assert!(deleted_tag.contains("Deleted tag 'v-test/feature'"));
    let tags = pragma_query_string(&sqlite, "graft_tags");
    assert!(tags.contains("v-feature"));
    let show_tag = pragma_arg_string(&sqlite, "graft_show", "v-feature");
    assert!(show_tag.contains("feature row"));
    let annotated_tag = pragma_arg_string(
        &sqlite,
        "graft_tag_create",
        "--annotated v-annotated HEAD -- release feature row",
    );
    assert!(annotated_tag.contains("Created annotated tag 'v-annotated'"));
    let tags = pragma_query_string(&sqlite, "graft_tags");
    assert!(tags.contains("v-annotated"));
    assert!(tags.contains("annotated"));
    let show_annotated_tag = pragma_arg_string(&sqlite, "graft_show", "v-annotated");
    assert!(show_annotated_tag.contains("feature row"));
    let deleted_tag = pragma_arg_string(&sqlite, "graft_tag_delete", "v-annotated");
    assert!(deleted_tag.contains("Deleted annotated tag 'v-annotated'"));
    let deleted_tag = pragma_arg_string(&sqlite, "graft_tag_delete", "v-feature");
    assert!(deleted_tag.contains("Deleted tag 'v-feature'"));
    let tags = pragma_query_string(&sqlite, "graft_tags");
    assert!(!tags.contains("v-feature"));

    let release_branch =
        pragma_arg_string(&sqlite, "graft_branch_create", "release/initial HEAD~1");
    assert!(release_branch.contains("release/initial"));
    let switched = pragma_arg_string(&sqlite, "graft_switch_branch", "release/initial");
    assert!(switched.contains("release/initial"));
    let release_count: i64 = sqlite
        .query_row("SELECT COUNT(*) FROM repo_test", [], |row| row.get(0))
        .unwrap();
    assert_eq!(release_count, 1);

    let switched = pragma_arg_string(&sqlite, "graft_switch_branch", "feature/search");
    assert!(switched.contains("feature/search"));
    let deleted = pragma_arg_string(&sqlite, "graft_branch_delete", "release/initial");
    assert!(deleted.contains("Deleted branch 'release/initial'"));

    let feature_count: i64 = sqlite
        .query_row("SELECT COUNT(*) FROM repo_test", [], |row| row.get(0))
        .unwrap();
    assert_eq!(feature_count, 2);

    let reset_branch = pragma_arg_string(&sqlite, "graft_branch_create", "tmp/reset");
    assert!(reset_branch.contains("tmp/reset"));
    let switched = pragma_arg_string(&sqlite, "graft_switch_branch", "tmp/reset");
    assert!(switched.contains("tmp/reset"));
    let reset = pragma_arg_string(&sqlite, "graft_reset", "--hard HEAD~1");
    assert!(reset.contains("hard"));
    let reset_status: Value =
        serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
            .expect("graft_json_status should return repo status JSON");
    assert_eq!(reset_status["head"]["type"], "branch");
    assert_eq!(reset_status["head"]["name"], "tmp/reset");
    assert_eq!(reset_status["dirty"], false);
    assert_eq!(reset_status["staged"].as_array().unwrap().len(), 0);
    let reset_count: i64 = sqlite
        .query_row("SELECT COUNT(*) FROM repo_test", [], |row| row.get(0))
        .unwrap();
    assert_eq!(reset_count, 1);

    let switched = pragma_arg_string(&sqlite, "graft_switch_branch", "feature/search");
    assert!(switched.contains("feature/search"));
    let feature_count: i64 = sqlite
        .query_row("SELECT COUNT(*) FROM repo_test", [], |row| row.get(0))
        .unwrap();
    assert_eq!(feature_count, 2);

    let checkout = pragma_arg_string(&sqlite, "graft_checkout", "HEAD~1");
    assert!(checkout.contains("HEAD detached"));
    let detached_status: Value =
        serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
            .expect("graft_json_status should return repo status JSON");
    assert_eq!(detached_status["head"]["type"], "detached");
    let detached_count: i64 = sqlite
        .query_row("SELECT COUNT(*) FROM repo_test", [], |row| row.get(0))
        .unwrap();
    assert_eq!(detached_count, 1);

    let switched = pragma_arg_string(&sqlite, "graft_switch_branch", "main");
    assert!(switched.contains("main"));
    let main_count: i64 = sqlite
        .query_row("SELECT COUNT(*) FROM repo_test", [], |row| row.get(0))
        .unwrap();
    assert_eq!(main_count, 1);

    let switched = pragma_arg_string(&sqlite, "graft_switch_branch", "feature/search");
    assert!(switched.contains("feature/search"));
    let feature_count: i64 = sqlite
        .query_row("SELECT COUNT(*) FROM repo_test", [], |row| row.get(0))
        .unwrap();
    assert_eq!(feature_count, 2);

    let branches = pragma_query_string(&sqlite, "graft_branch");
    assert!(branches.contains("* feature/search"));
    assert!(branches.contains("main"));
    assert!(branches.contains("tmp/reset"));

    let deleted = pragma_arg_string(&sqlite, "graft_branch_delete", "tmp/reset");
    assert!(deleted.contains("Deleted branch 'tmp/reset'"));
    let branches = pragma_query_string(&sqlite, "graft_branch");
    assert!(!branches.contains("tmp/reset"));

    let remote = pragma_arg_string(
        &sqlite,
        "graft_remote_add",
        format!("origin fs://{}", remote_dir.display()),
    );
    assert!(remote.contains("origin"));
    let upstream = pragma_arg_string(&sqlite, "graft_branch_upstream", "origin/feature/search");
    assert!(upstream.contains("feature/search"));
    assert!(upstream.contains("origin/feature/search"));
    let status = pragma_query_string(&sqlite, "graft_status");
    assert!(status.contains("Tracking: origin/feature/search"));
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["upstream"]["remote"], "origin");
    assert_eq!(status["upstream"]["branch"], "feature/search");
    let branches = pragma_query_string(&sqlite, "graft_branch");
    assert!(branches.contains("[origin/feature/search]"));

    let renamed = pragma_arg_string(&sqlite, "graft_branch_rename", "feature/query");
    assert!(renamed.contains("Renamed branch 'feature/search' to 'feature/query'"));
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["head"]["type"], "branch");
    assert_eq!(status["head"]["name"], "feature/query");
    assert_eq!(status["upstream"]["remote"], "origin");
    assert_eq!(status["upstream"]["branch"], "feature/search");
    let branches = pragma_query_string(&sqlite, "graft_branch");
    assert!(branches.contains("* feature/query"));
    assert!(branches.contains("[origin/feature/search]"));

    let remotes = pragma_query_string(&sqlite, "graft_remotes");
    assert!(remotes.contains("origin"));
    assert!(remotes.contains("fs://"));

    let backup = pragma_arg_string(&sqlite, "graft_remote_add", "backup memory");
    assert!(backup.contains("backup"));
    let remotes = pragma_query_string(&sqlite, "graft_remotes");
    assert!(remotes.contains("backup"));
    let removed = pragma_arg_string(&sqlite, "graft_remote_remove", "backup");
    assert!(removed.contains("Removed remote 'backup'"));
    let minio = pragma_arg_string(
        &sqlite,
        "graft_remote_add",
        "minio s3_compatible://repo-bucket/prod?endpoint=http://localhost:9000",
    );
    assert!(minio.contains("minio"));
    let remotes = pragma_query_string(&sqlite, "graft_remotes");
    assert!(remotes.contains("origin"));
    assert!(remotes.contains("minio"));
    assert!(remotes.contains("endpoint=http://localhost:9000"));
    assert!(!remotes.contains("backup"));
    let removed = pragma_arg_string(&sqlite, "graft_remote_remove", "minio");
    assert!(removed.contains("Removed remote 'minio'"));

    let pushed = pragma_query_string(&sqlite, "graft_push");
    assert!(pushed.contains("origin/feature/search"));

    let clone_dir = tempfile::tempdir().unwrap();
    let clone_db = clone_dir.path().join("app.db");
    let clone_db_name = clone_db.to_str().unwrap();
    let mut clone_runtime = GraftTestRuntime::with_memory_remote();
    let clone = clone_runtime.open_sqlite(clone_db_name, None);
    let init = pragma_query_string(&clone, "graft_init");
    assert!(init.contains(".graft"));
    let clone_repo = graft::repo::Repository::discover_for_file(&clone_db).unwrap();
    assert!(
        clone_repo.store_dir().read_dir().unwrap().next().is_some(),
        "clone repo should use its own repo-local storage"
    );
    let remote = pragma_arg_string(
        &clone,
        "graft_remote_add",
        format!("origin fs://{}", remote_dir.display()),
    );
    assert!(remote.contains("origin"));
    let upstream = pragma_arg_string(&clone, "graft_branch_upstream", "origin/feature/search");
    assert!(upstream.contains("main"));
    assert!(upstream.contains("origin/feature/search"));
    let fetched = pragma_query_string(&clone, "graft_fetch");
    assert!(fetched.contains("origin/feature/search"));

    assert!(
        clone_repo
            .remote_tracking_ref("origin", "feature/search")
            .unwrap()
            .is_some()
    );
    let pulled = pragma_query_string(&clone, "graft_pull");
    assert!(pulled.contains("origin/feature/search"));
    let clone_count: i64 = clone
        .query_row("SELECT COUNT(*) FROM repo_test", [], |row| row.get(0))
        .unwrap();
    assert_eq!(clone_count, 2);

    let log = pragma_query_string(&sqlite, "graft_log");
    assert!(log.contains("initial schema"));

    runtime.shutdown().unwrap();
    clone_runtime.shutdown().unwrap();
}

#[test]
fn test_repo_init_preserves_existing_vfs_database_contents() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    sqlite
        .execute_batch(
            r#"
            CREATE TABLE preinit_data (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO preinit_data (name) VALUES ('Alice'), ('Bob');
            "#,
        )
        .unwrap();

    let init = pragma_query_string(&sqlite, "graft_init");
    assert!(init.contains(".graft"));
    let source_repo = graft::repo::Repository::discover_for_file(&db_path).unwrap();
    assert!(
        source_repo.store_dir().read_dir().unwrap().next().is_some(),
        "graft_init should move pre-existing VFS contents into repo-local storage"
    );

    let count: u32 = sqlite
        .query_row("SELECT COUNT(*) FROM preinit_data", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
    let name: String = sqlite
        .query_row("SELECT name FROM preinit_data WHERE id = 2", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(name, "Bob");

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], true);
    assert_eq!(status["unstaged"][0], "app.db");

    pragma_query_string(&sqlite, "graft_add");
    let commit = pragma_arg_string(&sqlite, "graft_commit", "import existing database");
    assert!(commit.contains("import existing database"));

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_json_status_ignores_untracked_auxiliary_sqlite_files() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let eidos_dir = temp_dir.path().join(".eidos");
    std::fs::create_dir_all(&eidos_dir).unwrap();

    let raw_path = eidos_dir.join("raw.sqlite3");
    {
        let raw = Connection::open(&raw_path).unwrap();
        raw.execute_batch(
            r#"
            CREATE TABLE raw_data (id INTEGER PRIMARY KEY);
            INSERT INTO raw_data DEFAULT VALUES;
            "#,
        )
        .unwrap();
    }

    let db_path = eidos_dir.join("db.sqlite3");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE main_data (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO main_data (name) VALUES ('Alice');
            "#,
        )
        .unwrap();

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], true);
    assert_eq!(status["unstaged"], serde_json::json!(["db.sqlite3"]));

    pragma_query_string(&sqlite, "graft_add");
    let commit = pragma_arg_string(&sqlite, "graft_commit", "commit main database");
    assert!(commit.contains("commit main database"));

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], false);
    assert_eq!(status["unstaged"].as_array().unwrap().len(), 0);

    runtime.shutdown().unwrap();
}

#[test]
fn test_vfs_open_imports_existing_physical_sqlite_database() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();
    {
        let physical = Connection::open(&db_path).unwrap();
        physical
            .execute_batch(
                r#"
                PRAGMA page_size=4096;
                CREATE TABLE physical_data (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                INSERT INTO physical_data (name) VALUES ('Alice'), ('Bob');
                "#,
            )
            .unwrap();
    }

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    let count: u32 = sqlite
        .query_row("SELECT COUNT(*) FROM physical_data", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    let name: String = sqlite
        .query_row("SELECT name FROM physical_data WHERE id = 2", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(name, "Bob");

    pragma_query_string(&sqlite, "graft_add");
    assert!(
        pragma_arg_string(&sqlite, "graft_commit", "import physical database")
            .contains("import physical database")
    );
    let show: Value = serde_json::from_str(&pragma_arg_string(&sqlite, "graft_json_show", "HEAD"))
        .expect("graft_json_show should return repo commit JSON");
    assert!(
        show["files"]["app.db"]["snapshot"]["page_count"]
            .as_u64()
            .is_some_and(|page_count| page_count > 0)
    );
    assert!(
        !show["files"]["app.db"]["snapshot"]["ranges"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    runtime.shutdown().unwrap();
}

#[test]
fn test_vfs_readonly_open_imports_existing_physical_sqlite_database() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();
    {
        let physical = Connection::open(&db_path).unwrap();
        physical
            .execute_batch(
                r#"
                PRAGMA page_size=4096;
                CREATE TABLE readonly_data (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                INSERT INTO readonly_data (name) VALUES ('Alice'), ('Bob');
                "#,
            )
            .unwrap();
    }

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let vfs_id = runtime.ensure_vfs().to_string();
    let uri = format!("file:{db_name}?vfs={vfs_id}");
    let sqlite = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("existing physical SQLite database should open read-only through Graft VFS");

    let count: u32 = sqlite
        .query_row("SELECT COUNT(*) FROM readonly_data", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
    let err = sqlite
        .execute("INSERT INTO readonly_data (name) VALUES ('Carol')", [])
        .expect_err("read-only VFS handle should reject writes");
    assert!(
        err.to_string().contains("readonly") || err.to_string().contains("locked"),
        "unexpected read-only write error: {err}"
    );

    drop(sqlite);
    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_mode_isolates_same_database_name_by_project_directory() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let project_a = temp_dir.path().join("project-a");
    let project_b = temp_dir.path().join("project-b");
    std::fs::create_dir_all(&project_a).unwrap();
    std::fs::create_dir_all(&project_b).unwrap();
    let db_a = project_a.join("app.db");
    let db_b = project_b.join("app.db");

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite_a = runtime.open_sqlite(db_a.to_str().unwrap(), None);
    let sqlite_b = runtime.open_sqlite(db_b.to_str().unwrap(), None);

    assert!(pragma_query_string(&sqlite_a, "graft_init").contains(".graft"));
    assert!(pragma_query_string(&sqlite_b, "graft_init").contains(".graft"));

    sqlite_a
        .execute_batch(
            r#"
            CREATE TABLE project_data (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO project_data (name) VALUES ('alpha');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite_a, "graft_add");
    assert!(pragma_arg_string(&sqlite_a, "graft_commit", "commit alpha").contains("commit alpha"));

    sqlite_b
        .execute_batch(
            r#"
            CREATE TABLE project_data (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO project_data (name) VALUES ('beta');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite_b, "graft_add");
    assert!(pragma_arg_string(&sqlite_b, "graft_commit", "commit beta").contains("commit beta"));

    let repo_a = graft::repo::Repository::discover_for_file(&db_a).unwrap();
    let repo_b = graft::repo::Repository::discover_for_file(&db_b).unwrap();
    assert_ne!(repo_a.graft_dir(), repo_b.graft_dir());
    assert_ne!(repo_a.store_dir(), repo_b.store_dir());

    let value_a: String = sqlite_a
        .query_row("SELECT name FROM project_data WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    let value_b: String = sqlite_b
        .query_row("SELECT name FROM project_data WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(value_a, "alpha");
    assert_eq!(value_b, "beta");

    let show_a: Value =
        serde_json::from_str(&pragma_arg_string(&sqlite_a, "graft_json_show", "HEAD"))
            .expect("project A should expose its own HEAD commit");
    let show_b: Value =
        serde_json::from_str(&pragma_arg_string(&sqlite_b, "graft_json_show", "HEAD"))
            .expect("project B should expose its own HEAD commit");
    assert_eq!(show_a["message"], "commit alpha");
    assert_eq!(show_b["message"], "commit beta");

    drop(sqlite_a);
    drop(sqlite_b);

    let sqlite_a = runtime.open_sqlite(db_a.to_str().unwrap(), None);
    let sqlite_b = runtime.open_sqlite(db_b.to_str().unwrap(), None);
    let reopened_a: String = sqlite_a
        .query_row("SELECT name FROM project_data WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    let reopened_b: String = sqlite_b
        .query_row("SELECT name FROM project_data WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(reopened_a, "alpha");
    assert_eq!(reopened_b, "beta");

    runtime.shutdown().unwrap();
}

#[test]
#[cfg(unix)]
fn test_repo_mode_canonicalizes_symlinked_database_path_tags() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let project = temp_dir.path().join("project");
    let alias = temp_dir.path().join("alias");
    std::fs::create_dir_all(&project).unwrap();
    std::os::unix::fs::symlink(&project, &alias).unwrap();

    let canonical_db = std::fs::canonicalize(&project).unwrap().join("app.db");
    let alias_db = alias.join("app.db");

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let vfs_id = runtime.ensure_vfs().to_string();
    let alias_uri = format!("file:{}?vfs={vfs_id}", alias_db.display());
    let canonical_uri = format!("file:{}?vfs={vfs_id}", canonical_db.display());

    {
        let alias_sqlite = Connection::open(&alias_uri).unwrap();
        assert!(pragma_query_string(&alias_sqlite, "graft_init").contains(".graft"));
        alias_sqlite
            .execute_batch(
                r#"
                CREATE TABLE path_alias_data (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                INSERT INTO path_alias_data (name) VALUES ('Alice');
                "#,
            )
            .unwrap();
    }

    {
        let canonical_sqlite = Connection::open(&canonical_uri).unwrap();
        let status = pragma_query_string(&canonical_sqlite, "graft_status");
        assert!(
            status.contains("untracked: app.db"),
            "canonical path should see writes made through symlink path: {status}"
        );
        assert_eq!(
            pragma_query_string(&canonical_sqlite, "graft_add"),
            "Added app.db"
        );
        assert!(
            pragma_arg_string(&canonical_sqlite, "graft_commit", "initial alias row")
                .contains("initial alias row")
        );
        let show: Value = serde_json::from_str(&pragma_arg_string(
            &canonical_sqlite,
            "graft_json_show",
            "HEAD",
        ))
        .unwrap();
        assert_eq!(show["files"]["app.db"]["snapshot"]["page_count"], 2);
    }

    {
        let alias_sqlite = Connection::open(&alias_uri).unwrap();
        alias_sqlite
            .execute("INSERT INTO path_alias_data (name) VALUES ('Bob')", [])
            .unwrap();
    }

    {
        let canonical_sqlite = Connection::open(&canonical_uri).unwrap();
        let status = pragma_query_string(&canonical_sqlite, "graft_status");
        assert!(
            status.contains("modified: app.db"),
            "canonical path should see dirty state from symlink path: {status}"
        );
        assert_eq!(
            pragma_query_string(&canonical_sqlite, "graft_add"),
            "Added app.db"
        );
        assert!(
            pragma_arg_string(&canonical_sqlite, "graft_commit", "add bob through alias")
                .contains("add bob through alias")
        );
        let diff = pragma_arg_string(&canonical_sqlite, "graft_diff", "HEAD~1 HEAD -- app.db");
        assert!(
            diff.contains("modified: app.db"),
            "diff should compare snapshots written through symlink and canonical paths: {diff}"
        );
    }

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_clone_pragma_fetches_branch_and_materializes_worktree() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let remote_dir = temp_dir.path().join("remote");
    let source_db = temp_dir.path().join("source/app.db");
    let clone_db = temp_dir.path().join("clone/app.db");
    std::fs::create_dir_all(source_db.parent().unwrap()).unwrap();
    std::fs::create_dir_all(clone_db.parent().unwrap()).unwrap();

    let mut source_runtime = GraftTestRuntime::with_memory_remote();
    let source = source_runtime.open_sqlite(source_db.to_str().unwrap(), None);

    assert!(pragma_query_string(&source, "graft_init").contains(".graft"));
    assert!(
        pragma_arg_string(
            &source,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );
    source
        .execute_batch(
            r#"
            CREATE TABLE repo_clone (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_clone (name) VALUES ('Alice');
            "#,
        )
        .unwrap();
    assert_eq!(pragma_query_string(&source, "graft_add"), "Added app.db");
    assert!(
        pragma_arg_string(&source, "graft_commit", "base clone data").contains("base clone data")
    );

    let branch = pragma_arg_string(&source, "graft_branch_create", "feature/search");
    assert!(branch.contains("feature/search"));
    let switched = pragma_arg_string(&source, "graft_switch_branch", "feature/search");
    assert!(switched.contains("feature/search"));
    source
        .execute("INSERT INTO repo_clone (name) VALUES ('Bob')", [])
        .unwrap();
    assert_eq!(pragma_query_string(&source, "graft_add"), "Added app.db");
    assert!(
        pragma_arg_string(&source, "graft_commit", "feature clone data")
            .contains("feature clone data")
    );
    let pushed = pragma_arg_string(&source, "graft_push", "origin feature/search");
    assert!(pushed.contains("origin/feature/search"));

    let mut clone_runtime = GraftTestRuntime::with_memory_remote();
    let clone = clone_runtime.open_sqlite(clone_db.to_str().unwrap(), None);
    let cloned = pragma_arg_string(
        &clone,
        "graft_clone",
        format!("fs://{} feature/search", remote_dir.display()),
    );
    assert!(cloned.contains("Cloned origin/feature/search"));

    let names: Vec<String> = clone
        .prepare("SELECT name FROM repo_clone ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(names, vec!["Alice".to_string(), "Bob".to_string()]);

    let clone_repo = graft::repo::Repository::discover_for_file(&clone_db).unwrap();
    assert_eq!(
        clone_repo.current_branch().unwrap().as_deref(),
        Some("feature/search")
    );
    assert!(
        clone_repo
            .branch_target("feature/search")
            .unwrap()
            .is_some()
    );
    assert!(
        clone_repo
            .remote_tracking_ref("origin", "feature/search")
            .unwrap()
            .is_some()
    );
    let upstream = clone_repo
        .branch_upstream("feature/search")
        .unwrap()
        .expect("clone should configure branch upstream");
    assert_eq!(upstream.remote, "origin");
    assert_eq!(upstream.branch, "feature/search");
    assert!(
        clone_repo.store_dir().read_dir().unwrap().next().is_some(),
        "clone repo should use its own repo-local storage"
    );

    source_runtime.shutdown().unwrap();
    clone_runtime.shutdown().unwrap();
}

#[test]
fn test_repo_remote_rename_pragma_updates_upstreams_and_tracking_refs() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let remote_dir = temp_dir.path().join("remote");
    let db_path = temp_dir.path().join("project/app.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_path.to_str().unwrap(), None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE remote_rename (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO remote_rename (name) VALUES ('Alice');
            "#,
        )
        .unwrap();
    assert_eq!(pragma_query_string(&sqlite, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base").contains("base"));
    assert!(
        pragma_arg_string(
            &sqlite,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_branch_upstream", "origin/main").contains("origin/main")
    );
    assert!(pragma_query_string(&sqlite, "graft_push").contains("origin/main"));

    let renamed = pragma_arg_string(&sqlite, "graft_remote_rename", "origin upstream");
    assert!(renamed.contains("Renamed remote 'origin' to 'upstream'"));
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["upstream"]["remote"], "upstream");
    assert_eq!(status["upstream"]["branch"], "main");
    let remotes = pragma_query_string(&sqlite, "graft_remotes");
    assert!(remotes.contains("upstream"));
    assert!(!remotes.contains("origin"));

    let repo = graft::repo::Repository::discover_for_file(&db_path).unwrap();
    assert!(
        repo.remote_tracking_ref("upstream", "main")
            .unwrap()
            .is_some()
    );
    assert_eq!(repo.remote_tracking_ref("origin", "main").unwrap(), None);

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_remote_set_url_and_get_url_pragmas_update_config_only() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let remote_a = temp_dir.path().join("remote-a");
    let remote_b = temp_dir.path().join("remote-b");
    let db_path = temp_dir.path().join("project/app.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_path.to_str().unwrap(), None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE remote_set_url (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO remote_set_url (name) VALUES ('Alice');
            "#,
        )
        .unwrap();
    assert_eq!(pragma_query_string(&sqlite, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base").contains("base"));
    assert!(
        pragma_arg_string(
            &sqlite,
            "graft_remote_add",
            format!("origin fs://{}", remote_a.display()),
        )
        .contains("origin")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_branch_upstream", "origin/main").contains("origin/main")
    );
    assert!(pragma_query_string(&sqlite, "graft_push").contains("origin/main"));

    let initial_url = pragma_arg_string(&sqlite, "graft_remote_get_url", "origin");
    assert_eq!(initial_url, format!("fs://{}", remote_a.display()));
    let updated = pragma_arg_string(
        &sqlite,
        "graft_remote_set_url",
        format!("origin fs://{}", remote_b.display()),
    );
    assert!(updated.contains("Updated remote 'origin'"));
    let updated_url = pragma_arg_string(&sqlite, "graft_remote_get_url", "origin");
    assert_eq!(updated_url, format!("fs://{}", remote_b.display()));

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["upstream"]["remote"], "origin");
    assert_eq!(status["upstream"]["branch"], "main");
    let repo = graft::repo::Repository::discover_for_file(&db_path).unwrap();
    assert!(
        repo.remote_tracking_ref("origin", "main")
            .unwrap()
            .is_some()
    );

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_json_status_reports_local_commit_ahead_of_upstream() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let remote_dir = temp_dir.path().join("remote");
    let db_path = temp_dir.path().join("project/app.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_path.to_str().unwrap(), None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE upstream_status (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO upstream_status (name) VALUES ('Alice');
            "#,
        )
        .unwrap();
    assert_eq!(pragma_query_string(&sqlite, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base").contains("base"));
    assert!(
        pragma_arg_string(
            &sqlite,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_branch_upstream", "origin/main").contains("origin/main")
    );
    assert!(pragma_query_string(&sqlite, "graft_push").contains("origin/main"));

    sqlite
        .execute("INSERT INTO upstream_status (name) VALUES ('Bob')", [])
        .unwrap();
    assert_eq!(pragma_query_string(&sqlite, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "local row").contains("local row"));

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["upstream"]["remote"], "origin");
    assert_eq!(status["upstream"]["branch"], "main");
    assert_eq!(status["ahead"], 1);
    assert_eq!(status["behind"], 0);
    assert_eq!(status["upstream_status"]["state"], "ahead");
    assert_eq!(status["upstream_status"]["ahead"], 1);
    assert_eq!(status["upstream_status"]["behind"], 0);

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_push_skips_remote_ancestor_snapshot_uploads() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let remote_dir = temp_dir.path().join("remote");
    let db_path = temp_dir.path().join("project/app.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_path.to_str().unwrap(), None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE push_perf (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO push_perf (name) VALUES ('Alice');
            "#,
        )
        .unwrap();
    assert_eq!(pragma_query_string(&sqlite, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base").contains("base"));
    assert!(
        pragma_arg_string(
            &sqlite,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_branch_upstream", "origin/main").contains("origin/main")
    );
    assert!(pragma_arg_string(&sqlite, "graft_push", "origin main").contains("origin/main"));

    let first_segments = collect_files(&remote_dir.join("segments"));
    assert!(
        !first_segments.is_empty(),
        "initial push should upload SQLite snapshot segments"
    );
    let before = first_segments
        .iter()
        .map(|path| {
            (
                path.strip_prefix(&remote_dir).unwrap().to_path_buf(),
                std::fs::metadata(path).unwrap().modified().unwrap(),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();

    std::thread::sleep(std::time::Duration::from_millis(50));
    sqlite
        .execute("INSERT INTO push_perf (name) VALUES ('Bob')", [])
        .unwrap();
    assert_eq!(pragma_query_string(&sqlite, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "second").contains("second"));
    assert!(pragma_arg_string(&sqlite, "graft_push", "origin main").contains("origin/main"));

    for (relative, modified) in before {
        let path = remote_dir.join(&relative);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified,
            "second push should not rewrite ancestor snapshot segment {}",
            relative.display()
        );
    }

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_push_new_branch_skips_snapshots_reachable_from_remote_refs() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let remote_dir = temp_dir.path().join("remote");
    let db_path = temp_dir.path().join("project/app.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_path.to_str().unwrap(), None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE branch_push_perf (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO branch_push_perf (name) VALUES ('base');
            "#,
        )
        .unwrap();
    assert_eq!(pragma_query_string(&sqlite, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base").contains("base"));
    assert!(
        pragma_arg_string(
            &sqlite,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );
    assert!(pragma_arg_string(&sqlite, "graft_push", "origin main").contains("origin/main"));

    let first_segments = collect_files(&remote_dir.join("segments"));
    assert!(
        !first_segments.is_empty(),
        "main push should upload SQLite snapshot segments"
    );
    let before = first_segments
        .iter()
        .map(|path| {
            (
                path.strip_prefix(&remote_dir).unwrap().to_path_buf(),
                std::fs::metadata(path).unwrap().modified().unwrap(),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();

    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(
        pragma_arg_string(&sqlite, "graft_switch_create", "feature/shared")
            .contains("feature/shared")
    );
    sqlite
        .execute("INSERT INTO branch_push_perf (name) VALUES ('feature')", [])
        .unwrap();
    assert_eq!(pragma_query_string(&sqlite, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "feature").contains("feature"));
    assert!(
        pragma_arg_string(&sqlite, "graft_push", "origin feature/shared")
            .contains("origin/feature/shared")
    );

    for (relative, modified) in before {
        let path = remote_dir.join(&relative);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified,
            "new branch push should not rewrite segment already reachable from origin/main: {}",
            relative.display()
        );
    }

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_clone_pragma_cleans_new_repo_after_fetch_failure() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let remote_dir = temp_dir.path().join("remote");
    let source_db = temp_dir.path().join("source/app.db");
    let clone_db = temp_dir.path().join("clone/app.db");
    std::fs::create_dir_all(source_db.parent().unwrap()).unwrap();
    std::fs::create_dir_all(clone_db.parent().unwrap()).unwrap();

    let mut source_runtime = GraftTestRuntime::with_memory_remote();
    let source = source_runtime.open_sqlite(source_db.to_str().unwrap(), None);

    assert!(pragma_query_string(&source, "graft_init").contains(".graft"));
    assert!(
        pragma_arg_string(
            &source,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );
    source
        .execute_batch(
            r#"
            CREATE TABLE repo_clone_retry (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_clone_retry (name) VALUES ('Alice');
            "#,
        )
        .unwrap();
    assert_eq!(pragma_query_string(&source, "graft_add"), "Added app.db");
    assert!(
        pragma_arg_string(&source, "graft_commit", "clone retry data").contains("clone retry data")
    );
    let pushed = pragma_arg_string(&source, "graft_push", "origin main");
    assert!(pushed.contains("origin/main"));

    let mut clone_runtime = GraftTestRuntime::with_memory_remote();
    let clone = clone_runtime.open_sqlite(clone_db.to_str().unwrap(), None);
    let err = pragma_arg_error(
        &clone,
        "graft_clone",
        format!("fs://{} missing/branch", remote_dir.display()),
    );
    assert!(
        err.contains("missing/branch"),
        "unexpected clone error: {err}"
    );
    assert!(
        !clone_db.parent().unwrap().join(".graft").exists(),
        "failed clone should remove the repo it created"
    );

    let cloned = pragma_arg_string(
        &clone,
        "graft_clone",
        format!("fs://{} main", remote_dir.display()),
    );
    assert!(cloned.contains("Cloned origin/main"));
    let count: i64 = clone
        .query_row("SELECT COUNT(*) FROM repo_clone_retry", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 1);

    source_runtime.shutdown().unwrap();
    clone_runtime.shutdown().unwrap();
}

#[test]
fn test_repo_clone_pragma_without_branch_uses_remote_head() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let remote_dir = temp_dir.path().join("remote");
    let source_db = temp_dir.path().join("source/app.db");
    let clone_db = temp_dir.path().join("clone/app.db");
    std::fs::create_dir_all(source_db.parent().unwrap()).unwrap();
    std::fs::create_dir_all(clone_db.parent().unwrap()).unwrap();

    let mut source_runtime = GraftTestRuntime::with_memory_remote();
    let source = source_runtime.open_sqlite(source_db.to_str().unwrap(), None);

    assert!(pragma_query_string(&source, "graft_init").contains(".graft"));
    let source_repo = graft::repo::Repository::discover_for_file(&source_db).unwrap();
    let mut config = source_repo.config().unwrap();
    config.core.default_branch = "trunk".to_string();
    source_repo.write_config(&config).unwrap();

    assert!(
        pragma_arg_string(
            &source,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );
    source
        .execute_batch(
            r#"
            CREATE TABLE repo_clone_head (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_clone_head (name) VALUES ('Alice');
            "#,
        )
        .unwrap();
    assert_eq!(pragma_query_string(&source, "graft_add"), "Added app.db");
    assert!(
        pragma_arg_string(&source, "graft_commit", "base clone head").contains("base clone head")
    );
    assert!(pragma_arg_string(&source, "graft_branch_create", "trunk").contains("trunk"));
    assert!(pragma_arg_string(&source, "graft_switch_branch", "trunk").contains("trunk"));
    source
        .execute("INSERT INTO repo_clone_head (name) VALUES ('Bob')", [])
        .unwrap();
    assert_eq!(pragma_query_string(&source, "graft_add"), "Added app.db");
    assert!(
        pragma_arg_string(&source, "graft_commit", "trunk clone head").contains("trunk clone head")
    );
    assert!(pragma_arg_string(&source, "graft_push", "origin trunk").contains("origin/trunk"));
    assert_eq!(
        std::fs::read_to_string(remote_dir.join("HEAD")).unwrap(),
        "ref: refs/heads/trunk\n"
    );

    let mut clone_runtime = GraftTestRuntime::with_memory_remote();
    let clone = clone_runtime.open_sqlite(clone_db.to_str().unwrap(), None);
    let cloned = pragma_arg_string(
        &clone,
        "graft_clone",
        format!("fs://{}", remote_dir.display()),
    );
    assert!(cloned.contains("Cloned origin/trunk"));

    let clone_repo = graft::repo::Repository::discover_for_file(&clone_db).unwrap();
    assert_eq!(
        clone_repo.current_branch().unwrap().as_deref(),
        Some("trunk")
    );
    let count: i64 = clone
        .query_row("SELECT COUNT(*) FROM repo_clone_head", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);

    source_runtime.shutdown().unwrap();
    clone_runtime.shutdown().unwrap();
}

#[test]
fn test_repo_force_checkout_and_switch_discard_worktree_changes() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE force_test (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO force_test (name) VALUES ('base');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base").contains("base"));

    assert!(
        pragma_arg_string(&sqlite, "graft_switch_create", "feature/search")
            .contains("feature/search")
    );
    sqlite
        .execute("INSERT INTO force_test (name) VALUES ('feature')", [])
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "feature").contains("feature"));

    sqlite
        .execute("INSERT INTO force_test (name) VALUES ('dirty feature')", [])
        .unwrap();
    let err = pragma_arg_error(&sqlite, "graft_switch_branch", "--force missing/branch");
    assert!(err.contains("branch `missing/branch` does not exist"));
    let err = pragma_arg_error(&sqlite, "graft_switch_create", "--force feature HEAD");
    assert!(err.contains("cannot create ref `refs/heads/feature`"));
    let count: i64 = sqlite
        .query_row("SELECT COUNT(*) FROM force_test", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 3);
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], true);

    let err = pragma_arg_error(&sqlite, "graft_switch_branch", "main");
    assert!(err.contains("staged or unstaged"));
    let switched = pragma_arg_string(&sqlite, "graft_switch_branch", "--force main");
    assert!(switched.contains("main"));
    let count: i64 = sqlite
        .query_row("SELECT COUNT(*) FROM force_test", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], false);
    assert_eq!(status["staged"].as_array().unwrap().len(), 0);

    sqlite
        .execute("INSERT INTO force_test (name) VALUES ('dirty main')", [])
        .unwrap();
    let err = pragma_arg_error(&sqlite, "graft_checkout", "feature/search");
    assert!(err.contains("staged or unstaged"));
    let checkout = pragma_arg_string(&sqlite, "graft_checkout", "--force feature/search");
    assert!(checkout.contains("HEAD detached"));
    let count: i64 = sqlite
        .query_row("SELECT COUNT(*) FROM force_test", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["head"]["type"], "detached");
    assert_eq!(status["dirty"], false);
    assert_eq!(status["staged"].as_array().unwrap().len(), 0);

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_status_scans_physical_untracked_sqlite_files() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);
    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));

    let external_db = temp_dir.path().join("external.db");
    {
        let external = Connection::open(&external_db).unwrap();
        external
            .execute_batch("CREATE TABLE external_data (id INTEGER PRIMARY KEY);")
            .unwrap();
    }

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], true);
    assert_eq!(status["unstaged_changes"][0]["path"], "external.db");
    assert_eq!(status["unstaged_changes"][0]["change"], "untracked");

    let text_status = pragma_query_string(&sqlite, "graft_status");
    assert!(text_status.contains("untracked: external.db"));

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_status_classifies_tracked_physical_sqlite_files() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);
    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));

    let external_db = temp_dir.path().join("external.db");
    {
        let external = Connection::open(&external_db).unwrap();
        external
            .execute_batch(
                r#"
                PRAGMA page_size=4096;
                CREATE TABLE external_data (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                INSERT INTO external_data (name) VALUES ('v1');
                "#,
            )
            .unwrap();
    }

    assert_eq!(
        pragma_arg_string(&sqlite, "graft_add", "external.db"),
        "Added external.db"
    );
    assert!(pragma_arg_string(&sqlite, "graft_commit", "external v1").contains("external v1"));

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], false);
    assert!(
        status["unstaged_changes"].as_array().is_none_or(|changes| {
            !changes.iter().any(|change| change["path"] == "external.db")
        })
    );
    let clean_diff = pragma_arg_string(&sqlite, "graft_diff", "-- external.db");
    assert!(clean_diff.contains("No changes."));

    {
        let external = Connection::open(&external_db).unwrap();
        external
            .execute("UPDATE external_data SET name = 'v2' WHERE id = 1", [])
            .unwrap();
    }

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], true);
    assert!(
        status["unstaged_changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| { change["path"] == "external.db" && change["change"] == "modified" })
    );
    let text_status = pragma_query_string(&sqlite, "graft_status");
    assert!(text_status.contains("modified: external.db"));

    std::fs::remove_file(&external_db).unwrap();
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert!(
        status["unstaged_changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| { change["path"] == "external.db" && change["change"] == "deleted" })
    );
    let text_status = pragma_query_string(&sqlite, "graft_status");
    assert!(text_status.contains("deleted: external.db"));

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_add_stages_physical_untracked_sqlite_file() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);
    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));

    let external_db = temp_dir.path().join("external.db");
    {
        let external = Connection::open(&external_db).unwrap();
        external
            .execute_batch(
                r#"
                PRAGMA page_size=4096;
                CREATE TABLE external_data (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                INSERT INTO external_data (name) VALUES ('physical file');
                "#,
            )
            .unwrap();
    }

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["unstaged_changes"][0]["path"], "external.db");
    assert_eq!(status["unstaged_changes"][0]["change"], "untracked");

    let added = pragma_arg_string(&sqlite, "graft_add", "external.db");
    assert_eq!(added, "Added external.db");

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["staged"][0], "external.db");
    assert!(
        status["unstaged_changes"].as_array().is_none_or(|changes| {
            !changes.iter().any(|change| change["path"] == "external.db")
        })
    );

    let commit = pragma_arg_string(&sqlite, "graft_commit", "add external database");
    assert!(commit.contains("add external database"));
    let show = pragma_arg_string(&sqlite, "graft_show", "HEAD");
    assert!(show.contains("external.db"));

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_add_physical_sqlite_file_rejects_non_graft_page_size() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);
    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));

    let external_db = temp_dir.path().join("external.db");
    {
        let external = Connection::open(&external_db).unwrap();
        external
            .execute_batch(
                r#"
                PRAGMA page_size=8192;
                CREATE TABLE external_data (id INTEGER PRIMARY KEY);
                "#,
            )
            .unwrap();
    }

    let err = pragma_arg_error(&sqlite, "graft_add", "external.db");
    assert!(err.contains("4096-byte pages"));
    assert!(err.contains("8192-byte pages"));
    assert!(err.contains("VACUUM INTO"));

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_diff_physical_sqlite_worktree_path() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);
    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));

    let external_db = temp_dir.path().join("external.db");
    {
        let external = Connection::open(&external_db).unwrap();
        external
            .execute_batch(
                r#"
                PRAGMA page_size=4096;
                CREATE TABLE external_data (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                INSERT INTO external_data (name) VALUES ('v1');
                "#,
            )
            .unwrap();
    }

    assert_eq!(
        pragma_arg_string(&sqlite, "graft_add", "external.db"),
        "Added external.db"
    );
    assert!(pragma_arg_string(&sqlite, "graft_commit", "external v1").contains("external v1"));

    {
        let external = Connection::open(&external_db).unwrap();
        external
            .execute("UPDATE external_data SET name = 'v2' WHERE id = 1", [])
            .unwrap();
    }

    let worktree_diff = pragma_arg_string(&sqlite, "graft_diff", "-- external.db");
    assert!(worktree_diff.contains("modified: external.db"));

    let rev_worktree_diff = pragma_arg_string(&sqlite, "graft_diff", "HEAD -- external.db");
    assert!(rev_worktree_diff.contains("modified: external.db"));

    let json_diff: Value = serde_json::from_str(&pragma_arg_string(
        &sqlite,
        "graft_json_diff",
        "HEAD -- external.db",
    ))
    .expect("graft_json_diff should return repo diff JSON");
    assert_eq!(json_diff["files"][0]["path"], "external.db");
    assert_eq!(json_diff["files"][0]["change"], "modified");

    std::fs::remove_file(&external_db).unwrap();
    let deleted_diff = pragma_arg_string(&sqlite, "graft_diff", "-- external.db");
    assert!(deleted_diff.contains("deleted: external.db"));

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_rm_removes_and_stages_physical_sqlite_file() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);
    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));

    let external_db = temp_dir.path().join("external.db");
    {
        let external = Connection::open(&external_db).unwrap();
        external
            .execute_batch(
                r#"
                PRAGMA page_size=4096;
                CREATE TABLE external_data (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                INSERT INTO external_data (name) VALUES ('physical file');
                "#,
            )
            .unwrap();
    }

    assert_eq!(
        pragma_arg_string(&sqlite, "graft_add", "external.db"),
        "Added external.db"
    );
    assert!(pragma_arg_string(&sqlite, "graft_commit", "add external").contains("add external"));

    let removed = pragma_arg_string(&sqlite, "graft_rm", "external.db");
    assert_eq!(removed, "Removed external.db");
    assert!(!external_db.exists());

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["staged"][0], "external.db");
    assert!(
        status["unstaged_changes"].as_array().is_none_or(|changes| {
            !changes.iter().any(|change| change["path"] == "external.db")
        })
    );

    let diff = pragma_arg_string(&sqlite, "graft_diff", "--staged");
    assert!(diff.contains("deleted: external.db"));

    assert!(
        pragma_arg_string(&sqlite, "graft_commit", "remove external").contains("remove external")
    );
    let repo = graft::repo::Repository::discover_for_file(&db_path).unwrap();
    assert!(repo.head_file(&external_db).unwrap().is_none());

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_checkout_path_materializes_physical_sqlite_file() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);
    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));

    let external_db = temp_dir.path().join("external.db");
    {
        let external = Connection::open(&external_db).unwrap();
        external
            .execute_batch(
                r#"
                PRAGMA page_size=4096;
                CREATE TABLE external_data (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                INSERT INTO external_data (name) VALUES ('v1');
                "#,
            )
            .unwrap();
    }

    assert_eq!(
        pragma_arg_string(&sqlite, "graft_add", "external.db"),
        "Added external.db"
    );
    assert!(pragma_arg_string(&sqlite, "graft_commit", "external v1").contains("external v1"));

    {
        let external = Connection::open(&external_db).unwrap();
        external
            .execute("UPDATE external_data SET name = 'v2' WHERE id = 1", [])
            .unwrap();
    }
    assert_eq!(
        pragma_arg_string(&sqlite, "graft_add", "external.db"),
        "Added external.db"
    );
    assert!(pragma_arg_string(&sqlite, "graft_commit", "external v2").contains("external v2"));

    std::fs::remove_file(&external_db).unwrap();
    assert!(!external_db.exists());

    let checkout = pragma_arg_string(&sqlite, "graft_checkout", "HEAD~1 -- external.db");
    assert!(checkout.contains("Checked out external.db"));
    assert!(external_db.exists());

    let restored = Connection::open(&external_db).unwrap();
    let name: String = restored
        .query_row("SELECT name FROM external_data WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(name, "v1");

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["staged"][0], "external.db");

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_checkout_rejects_snapshot_commit_hash_mismatch() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);
    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));

    sqlite
        .execute_batch(
            r#"
            CREATE TABLE repo_hash_test (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_hash_test (name) VALUES ('Alice');
            "#,
        )
        .unwrap();
    assert_eq!(pragma_query_string(&sqlite, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "hash checked").contains("hash checked"));

    let repo = graft::repo::Repository::discover_for_file(&db_path).unwrap();
    let head = repo.resolve_revision("HEAD").unwrap();
    let tampered = tamper_sqlite_snapshot_range_hash(&repo, &head, "app.db");
    std::fs::write(
        repo.graft_dir().join("refs/heads/main"),
        format!("{tampered}\n"),
    )
    .unwrap();

    let err = pragma_arg_error(&sqlite, "graft_checkout", "HEAD -- app.db");
    assert!(
        err.contains("snapshot storage commit hash mismatch"),
        "expected snapshot hash mismatch, got: {err}"
    );
    assert!(
        !repo.has_staged_changes().unwrap(),
        "hash mismatch should fail before staging checkout results"
    );

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_checkout_rejects_snapshot_missing_commit_hash() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);
    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));

    sqlite
        .execute_batch(
            r#"
            CREATE TABLE repo_missing_hash_test (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_missing_hash_test (name) VALUES ('Alice');
            "#,
        )
        .unwrap();
    assert_eq!(pragma_query_string(&sqlite, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "hash required").contains("hash required"));

    let repo = graft::repo::Repository::discover_for_file(&db_path).unwrap();
    let head = repo.resolve_revision("HEAD").unwrap();
    let tampered = remove_sqlite_snapshot_range_hash(&repo, &head, "app.db");
    std::fs::write(
        repo.graft_dir().join("refs/heads/main"),
        format!("{tampered}\n"),
    )
    .unwrap();

    let err = pragma_arg_error(&sqlite, "graft_checkout", "HEAD -- app.db");
    assert!(
        err.contains("invalid blob object")
            && err.contains("storage commit hashes")
            && err.contains("expected"),
        "expected invalid snapshot hash object error, got: {err}"
    );
    assert!(
        !repo.has_staged_changes().unwrap(),
        "missing hash should fail before staging checkout results"
    );

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_push_rejects_snapshot_missing_commit_hash_before_remote_ref_update() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let remote_dir = temp_dir.path().join("remote");
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);
    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    assert!(
        pragma_arg_string(
            &sqlite,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );

    sqlite
        .execute_batch(
            r#"
            CREATE TABLE repo_push_hash_test (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_push_hash_test (name) VALUES ('Alice');
            "#,
        )
        .unwrap();
    assert_eq!(pragma_query_string(&sqlite, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "hash checked").contains("hash checked"));

    let repo = graft::repo::Repository::discover_for_file(&db_path).unwrap();
    let head = repo.resolve_revision("HEAD").unwrap();
    let tampered = remove_sqlite_snapshot_range_hash(&repo, &head, "app.db");
    std::fs::write(
        repo.graft_dir().join("refs/heads/main"),
        format!("{tampered}\n"),
    )
    .unwrap();

    let err = pragma_arg_error(&sqlite, "graft_push", "origin main");
    assert!(
        err.contains("invalid blob object")
            && err.contains("storage commit hashes")
            && err.contains("expected"),
        "expected invalid snapshot hash object error, got: {err}"
    );
    assert!(
        !remote_dir.join("refs/heads/main").exists(),
        "hash mismatch should fail before updating the remote branch ref"
    );

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_push_accepts_hydrated_snapshot_commit_hash_mismatch() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let remote_dir = temp_dir.path().join("remote");
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);
    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    assert!(
        pragma_arg_string(
            &sqlite,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );

    sqlite
        .execute_batch(
            r#"
            CREATE TABLE repo_push_hash_mismatch (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_push_hash_mismatch (name) VALUES ('Alice');
            "#,
        )
        .unwrap();
    assert_eq!(pragma_query_string(&sqlite, "graft_add"), "Added app.db");
    assert!(
        pragma_arg_string(&sqlite, "graft_commit", "hash normalized").contains("hash normalized")
    );

    let repo = graft::repo::Repository::discover_for_file(&db_path).unwrap();
    let head = repo.resolve_revision("HEAD").unwrap();
    let tampered = tamper_sqlite_snapshot_range_hash(&repo, &head, "app.db");
    std::fs::write(
        repo.graft_dir().join("refs/heads/main"),
        format!("{tampered}\n"),
    )
    .unwrap();

    let push = pragma_arg_string(&sqlite, "graft_push", "origin main");
    assert!(
        push.contains("origin/main"),
        "snapshot hash mismatch should be normalized from hydrated storage during push: {push}"
    );
    let remote_head = std::fs::read_to_string(remote_dir.join("refs/heads/main"))
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(remote_head, tampered);

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_pull_accepts_hydrated_remote_snapshot_commit_hash_mismatch() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let remote_dir = temp_dir.path().join("remote");
    let source_db = temp_dir.path().join("source/app.db");
    let clone_db = temp_dir.path().join("clone/app.db");
    std::fs::create_dir_all(source_db.parent().unwrap()).unwrap();
    std::fs::create_dir_all(clone_db.parent().unwrap()).unwrap();

    let mut source_runtime = GraftTestRuntime::with_memory_remote();
    let source = source_runtime.open_sqlite(source_db.to_str().unwrap(), None);
    let mut clone_runtime = GraftTestRuntime::with_memory_remote();
    let clone = clone_runtime.open_sqlite(clone_db.to_str().unwrap(), None);

    assert!(pragma_query_string(&source, "graft_init").contains(".graft"));
    assert!(pragma_query_string(&clone, "graft_init").contains(".graft"));
    assert!(
        pragma_arg_string(
            &source,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );
    assert!(
        pragma_arg_string(
            &clone,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );

    source
        .execute_batch(
            r#"
            CREATE TABLE repo_bad_remote_hash (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_bad_remote_hash (name) VALUES ('Alice');
            "#,
        )
        .unwrap();
    assert_eq!(pragma_query_string(&source, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&source, "graft_commit", "base row").contains("base row"));
    assert!(pragma_arg_string(&source, "graft_push", "origin main").contains("origin/main"));

    let first_pull = pragma_arg_string(&clone, "graft_pull", "origin main");
    assert!(first_pull.contains("Fast-forwarded main"));

    source
        .execute("INSERT INTO repo_bad_remote_hash (name) VALUES ('Bob')", [])
        .unwrap();
    assert_eq!(pragma_query_string(&source, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&source, "graft_commit", "remote row").contains("remote row"));
    assert!(pragma_arg_string(&source, "graft_push", "origin main").contains("origin/main"));

    let source_repo = graft::repo::Repository::discover_for_file(&source_db).unwrap();
    let remote_head = std::fs::read_to_string(remote_dir.join("refs/heads/main"))
        .unwrap()
        .trim()
        .to_string();
    let tampered = tamper_sqlite_snapshot_range_hash(&source_repo, &remote_head, "app.db");
    write_remote_object_pack_for_commit(&remote_dir, &source_repo, &tampered);
    std::fs::write(remote_dir.join("refs/heads/main"), format!("{tampered}\n")).unwrap();

    let pull = pragma_arg_string(&clone, "graft_pull", "origin main");
    assert!(
        pull.contains("Fast-forwarded main"),
        "unexpected pull output: {pull}"
    );
    let names: Vec<String> = clone
        .prepare("SELECT name FROM repo_bad_remote_hash ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(names, vec!["Alice".to_string(), "Bob".to_string()]);
    let status: Value = serde_json::from_str(&pragma_query_string(&clone, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], false);
    assert_eq!(status["staged"].as_array().unwrap().len(), 0);
    assert_eq!(status["unstaged"].as_array().unwrap().len(), 0);

    let row_diff: Value = serde_json::from_str(&pragma_arg_string(
        &clone,
        "graft_json_diff",
        "--rows HEAD~1 HEAD -- app.db",
    ))
    .expect("pulled commit row diff should return JSON despite remote snapshot hash mismatch");
    assert_eq!(row_diff["files"][0]["path"], "app.db");
    assert_eq!(row_diff["files"][0]["row_diff_available"], true);
    assert!(
        row_diff["files"][0]["tables"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|table| table["changes"].as_array().into_iter().flatten())
            .any(|change| row_values_contain(&change["values"], "Bob")),
        "expected pulled commit row diff to include Bob insert: {row_diff}"
    );

    clone
        .execute(
            "INSERT INTO repo_bad_remote_hash (name) VALUES ('Charlie')",
            [],
        )
        .unwrap();
    assert_eq!(pragma_query_string(&clone, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&clone, "graft_commit", "local row").contains("local row"));
    let reset: Value = serde_json::from_str(&pragma_arg_string(
        &clone,
        "graft_json_reset",
        "--hard HEAD~1",
    ))
    .expect("hard reset to pulled commit should return JSON despite snapshot mismatch");
    assert_eq!(reset["target"], tampered);
    let reset_names: Vec<String> = clone
        .prepare("SELECT name FROM repo_bad_remote_hash ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(reset_names, vec!["Alice".to_string(), "Bob".to_string()]);

    clone_runtime.shutdown().unwrap();
    source_runtime.shutdown().unwrap();
}

#[test]
fn test_repo_push_after_resolving_pulled_row_conflict_with_hydrated_hash_mismatch() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let remote_dir = temp_dir.path().join("remote");
    let source_db = temp_dir.path().join("source/app.db");
    let clone_db = temp_dir.path().join("clone/app.db");
    std::fs::create_dir_all(source_db.parent().unwrap()).unwrap();
    std::fs::create_dir_all(clone_db.parent().unwrap()).unwrap();

    let mut source_runtime = GraftTestRuntime::with_memory_remote();
    let source = source_runtime.open_sqlite(source_db.to_str().unwrap(), None);
    let mut clone_runtime = GraftTestRuntime::with_memory_remote();
    let clone = clone_runtime.open_sqlite(clone_db.to_str().unwrap(), None);

    assert!(pragma_query_string(&source, "graft_init").contains(".graft"));
    assert!(pragma_query_string(&clone, "graft_init").contains(".graft"));
    assert!(
        pragma_arg_string(
            &source,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );
    assert!(
        pragma_arg_string(
            &clone,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );

    source
        .execute_batch(
            r#"
            CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT NOT NULL);
            INSERT INTO docs (id, body) VALUES (1, 'base');
            "#,
        )
        .unwrap();
    assert_eq!(pragma_query_string(&source, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&source, "graft_commit", "base doc").contains("base doc"));
    assert!(pragma_arg_string(&source, "graft_push", "origin main").contains("origin/main"));

    let first_pull = pragma_arg_string(&clone, "graft_pull", "origin main");
    assert!(first_pull.contains("Fast-forwarded main"));

    source
        .execute("UPDATE docs SET body = 'theirs' WHERE id = 1", [])
        .unwrap();
    assert_eq!(pragma_query_string(&source, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&source, "graft_commit", "remote edit").contains("remote edit"));
    assert!(pragma_arg_string(&source, "graft_push", "origin main").contains("origin/main"));

    let source_repo = graft::repo::Repository::discover_for_file(&source_db).unwrap();
    let remote_head = std::fs::read_to_string(remote_dir.join("refs/heads/main"))
        .unwrap()
        .trim()
        .to_string();
    let tampered = tamper_sqlite_snapshot_range_hash(&source_repo, &remote_head, "app.db");
    write_remote_object_pack_for_commit(&remote_dir, &source_repo, &tampered);
    std::fs::write(remote_dir.join("refs/heads/main"), format!("{tampered}\n")).unwrap();

    clone
        .execute("UPDATE docs SET body = 'ours' WHERE id = 1", [])
        .unwrap();
    assert_eq!(pragma_query_string(&clone, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&clone, "graft_commit", "local edit").contains("local edit"));

    let pull = pragma_arg_string(&clone, "graft_pull", "origin main");
    assert!(pull.contains("Fetched origin/main"));
    assert!(pull.contains("Unmerged paths:"));

    let conflicts: Value =
        serde_json::from_str(&pragma_query_string(&clone, "graft_json_conflicts"))
            .expect("graft_json_conflicts should return conflict artifact JSON");
    assert_eq!(conflicts["conflicts"].as_array().unwrap().len(), 1);
    assert_eq!(conflicts["conflicts"][0]["kind"], "row");
    assert_eq!(conflicts["conflicts"][0]["table"], "docs");

    let resolved: Value = serde_json::from_str(&pragma_arg_string(
        &clone,
        "graft_json_resolve_conflict",
        "--theirs --row docs 1",
    ))
    .expect("graft_json_resolve_conflict should return resolve JSON");
    assert_eq!(resolved["remaining_conflicts"], 0);

    let continued = pragma_arg_string(&clone, "graft_merge_continue", "merge remote edit");
    assert!(continued.contains("Merge commit"));

    clone
        .execute("UPDATE docs SET body = 'after merge' WHERE id = 1", [])
        .unwrap();
    assert_eq!(pragma_query_string(&clone, "graft_add"), "Added app.db");
    assert!(
        pragma_arg_string(&clone, "graft_commit", "after merge edit").contains("after merge edit")
    );

    let push = pragma_arg_string(&clone, "graft_push", "origin main");
    assert!(
        push.contains("origin/main"),
        "push after row conflict resolution should succeed, got: {push}"
    );

    let clone_repo = graft::repo::Repository::discover_for_file(&clone_db).unwrap();
    let clone_head = clone_repo.resolve_revision("HEAD").unwrap();
    let remote_head = std::fs::read_to_string(remote_dir.join("refs/heads/main"))
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(remote_head, clone_head);

    clone_runtime.shutdown().unwrap();
    source_runtime.shutdown().unwrap();
}

#[test]
fn test_repo_row_diff_hydrates_pulled_historical_snapshots_on_demand() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let remote_dir = temp_dir.path().join("remote");
    let source_db = temp_dir.path().join("source/app.db");
    let clone_db = temp_dir.path().join("clone/app.db");
    std::fs::create_dir_all(source_db.parent().unwrap()).unwrap();
    std::fs::create_dir_all(clone_db.parent().unwrap()).unwrap();

    let mut source_runtime = GraftTestRuntime::with_memory_remote();
    let source = source_runtime.open_sqlite(source_db.to_str().unwrap(), None);

    assert!(pragma_query_string(&source, "graft_init").contains(".graft"));
    assert!(
        pragma_arg_string(
            &source,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );
    assert!(pragma_arg_string(&source, "graft_branch_upstream", "origin/main").contains("main"));

    source
        .execute_batch(
            r#"
            CREATE TABLE repo_pulled_history (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_pulled_history (name) VALUES ('Alice');
            "#,
        )
        .unwrap();
    assert_eq!(pragma_query_string(&source, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&source, "graft_commit", "base").contains("base"));

    source
        .execute("INSERT INTO repo_pulled_history (name) VALUES ('Bob')", [])
        .unwrap();
    assert_eq!(pragma_query_string(&source, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&source, "graft_commit", "add Bob").contains("add Bob"));

    source
        .execute(
            "UPDATE repo_pulled_history SET name = 'Bobby' WHERE name = 'Bob'",
            [],
        )
        .unwrap();
    assert_eq!(pragma_query_string(&source, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&source, "graft_commit", "rename Bob").contains("rename Bob"));
    assert!(pragma_arg_string(&source, "graft_push", "origin main").contains("origin/main"));

    let mut clone_runtime = GraftTestRuntime::with_memory_remote();
    let clone = clone_runtime.open_sqlite(clone_db.to_str().unwrap(), None);
    assert!(pragma_query_string(&clone, "graft_init").contains(".graft"));
    assert!(
        pragma_arg_string(
            &clone,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );
    assert!(pragma_arg_string(&clone, "graft_branch_upstream", "origin/main").contains("main"));

    let pull = pragma_arg_string(&clone, "graft_pull", "origin main");
    assert!(
        pull.contains("Fast-forwarded main"),
        "unexpected pull output: {pull}"
    );
    let names: Vec<String> = clone
        .prepare("SELECT name FROM repo_pulled_history ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(names, vec!["Alice".to_string(), "Bobby".to_string()]);

    let historical_diff: Value = serde_json::from_str(&pragma_arg_string(
        &clone,
        "graft_json_diff",
        "--rows HEAD~2 HEAD~1 -- app.db",
    ))
    .expect("pulled historical commit row diff should return JSON");
    assert_eq!(historical_diff["files"][0]["path"], "app.db");
    assert_eq!(historical_diff["files"][0]["row_diff_available"], true);
    assert!(
        historical_diff["files"][0]["tables"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|table| table["changes"].as_array().into_iter().flatten())
            .any(|change| row_values_contain(&change["values"], "Bob")),
        "expected pulled historical commit row diff to include Bob insert: {historical_diff}"
    );

    let latest_diff: Value = serde_json::from_str(&pragma_arg_string(
        &clone,
        "graft_json_diff",
        "--rows HEAD~1 HEAD -- app.db",
    ))
    .expect("pulled latest commit row diff should return JSON");
    assert_eq!(latest_diff["files"][0]["row_diff_available"], true);
    assert!(
        latest_diff["files"][0]["tables"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|table| table["changes"].as_array().into_iter().flatten())
            .any(|change| {
                row_values_contain(&change["old_values"], "Bob")
                    && row_values_contain(&change["values"], "Bobby")
            }),
        "expected pulled latest commit row diff to include Bob update: {latest_diff}"
    );

    clone_runtime.shutdown().unwrap();
    source_runtime.shutdown().unwrap();
}

#[test]
fn test_repo_switch_materializes_physical_sqlite_worktree_files() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);
    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));

    sqlite
        .execute_batch(
            r#"
            CREATE TABLE base_app (id INTEGER PRIMARY KEY);
            INSERT INTO base_app DEFAULT VALUES;
            "#,
        )
        .unwrap();
    assert_eq!(pragma_query_string(&sqlite, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base").contains("base"));
    assert!(
        pragma_arg_string(&sqlite, "graft_branch_create", "without-external")
            .contains("without-external")
    );

    let external_db = temp_dir.path().join("external.db");
    {
        let external = Connection::open(&external_db).unwrap();
        external
            .execute_batch(
                r#"
                PRAGMA page_size=4096;
                CREATE TABLE external_data (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                INSERT INTO external_data (name) VALUES ('v1');
                "#,
            )
            .unwrap();
    }
    assert_eq!(
        pragma_arg_string(&sqlite, "graft_add", "external.db"),
        "Added external.db"
    );
    assert!(pragma_arg_string(&sqlite, "graft_commit", "external v1").contains("external v1"));

    assert!(
        pragma_arg_string(&sqlite, "graft_switch_create", "feature/external")
            .contains("feature/external")
    );
    {
        let external = Connection::open(&external_db).unwrap();
        external
            .execute("UPDATE external_data SET name = 'v2' WHERE id = 1", [])
            .unwrap();
    }
    assert_eq!(
        pragma_arg_string(&sqlite, "graft_add", "external.db"),
        "Added external.db"
    );
    assert!(pragma_arg_string(&sqlite, "graft_commit", "external v2").contains("external v2"));

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    assert_eq!(external_value(&external_db), "v1");

    assert!(
        pragma_arg_string(&sqlite, "graft_switch_branch", "without-external")
            .contains("without-external")
    );
    assert!(!external_db.exists());

    assert!(
        pragma_arg_string(&sqlite, "graft_switch_branch", "feature/external")
            .contains("feature/external")
    );
    assert_eq!(external_value(&external_db), "v2");

    {
        let external = Connection::open(&external_db).unwrap();
        external
            .execute("UPDATE external_data SET name = 'dirty' WHERE id = 1", [])
            .unwrap();
    }
    let err = pragma_arg_error(&sqlite, "graft_switch_branch", "without-external");
    assert!(err.contains("staged or unstaged"));
    assert_eq!(external_value(&external_db), "dirty");

    assert!(
        pragma_arg_string(&sqlite, "graft_switch_branch", "--force without-external")
            .contains("without-external")
    );
    assert!(!external_db.exists());

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_restore_physical_sqlite_worktree_file_from_index_and_revision() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);
    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));

    sqlite
        .execute_batch(
            r#"
            CREATE TABLE base_app (id INTEGER PRIMARY KEY);
            INSERT INTO base_app DEFAULT VALUES;
            "#,
        )
        .unwrap();
    assert_eq!(pragma_query_string(&sqlite, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base").contains("base"));

    let external_db = temp_dir.path().join("external.db");
    {
        let external = Connection::open(&external_db).unwrap();
        external
            .execute_batch(
                r#"
                PRAGMA page_size=4096;
                CREATE TABLE external_data (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                INSERT INTO external_data (name) VALUES ('v1');
                "#,
            )
            .unwrap();
    }
    assert_eq!(
        pragma_arg_string(&sqlite, "graft_add", "external.db"),
        "Added external.db"
    );
    assert!(pragma_arg_string(&sqlite, "graft_commit", "external v1").contains("external v1"));

    assert!(
        pragma_arg_string(&sqlite, "graft_switch_create", "feature/external")
            .contains("feature/external")
    );
    {
        let external = Connection::open(&external_db).unwrap();
        external
            .execute("UPDATE external_data SET name = 'v2' WHERE id = 1", [])
            .unwrap();
    }
    assert_eq!(
        pragma_arg_string(&sqlite, "graft_add", "external.db"),
        "Added external.db"
    );
    assert!(pragma_arg_string(&sqlite, "graft_commit", "external v2").contains("external v2"));

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    assert_eq!(external_value(&external_db), "v1");

    let restored = pragma_arg_string(
        &sqlite,
        "graft_restore",
        "--staged --source feature/external -- external.db",
    );
    assert_eq!(restored, "Restored external.db");
    assert_eq!(external_value(&external_db), "v1");
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], true);
    assert_eq!(status["unstaged"][0], "external.db");
    assert_eq!(status["staged"][0], "external.db");

    let restored = pragma_arg_string(&sqlite, "graft_restore", "--staged external.db");
    assert_eq!(restored, "Restored external.db");
    assert_eq!(external_value(&external_db), "v1");
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], false);
    assert_eq!(status["unstaged"].as_array().unwrap().len(), 0);
    assert_eq!(status["staged"].as_array().unwrap().len(), 0);

    let restored = pragma_arg_string(
        &sqlite,
        "graft_restore",
        "--source feature/external -- external.db",
    );
    assert_eq!(restored, "Restored external.db");
    assert_eq!(external_value(&external_db), "v2");
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], true);
    assert_eq!(status["unstaged"][0], "external.db");
    assert_eq!(status["staged"].as_array().unwrap().len(), 0);

    let restored = pragma_arg_string(&sqlite, "graft_restore", "external.db");
    assert_eq!(restored, "Restored external.db");
    assert_eq!(external_value(&external_db), "v1");
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], false);
    assert_eq!(status["unstaged"].as_array().unwrap().len(), 0);
    assert_eq!(status["staged"].as_array().unwrap().len(), 0);

    {
        let external = Connection::open(&external_db).unwrap();
        external
            .execute("UPDATE external_data SET name = 'staged' WHERE id = 1", [])
            .unwrap();
    }
    assert_eq!(
        pragma_arg_string(&sqlite, "graft_add", "external.db"),
        "Added external.db"
    );
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], false);
    assert_eq!(status["staged"][0], "external.db");

    let restored = pragma_arg_string(&sqlite, "graft_restore", "--staged external.db");
    assert_eq!(restored, "Restored external.db");
    assert_eq!(external_value(&external_db), "staged");
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], true);
    assert_eq!(status["unstaged"][0], "external.db");
    assert_eq!(status["staged"].as_array().unwrap().len(), 0);

    let restored = pragma_arg_string(&sqlite, "graft_restore", "external.db");
    assert_eq!(restored, "Restored external.db");
    assert_eq!(external_value(&external_db), "v1");
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], false);
    assert_eq!(status["unstaged"].as_array().unwrap().len(), 0);
    assert_eq!(status["staged"].as_array().unwrap().len(), 0);

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_merge_materializes_clean_physical_sqlite_worktree_files() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);
    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));

    sqlite
        .execute_batch(
            r#"
            CREATE TABLE base_app (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO base_app (name) VALUES ('base');
            "#,
        )
        .unwrap();
    let external_db = temp_dir.path().join("external.db");
    {
        let external = Connection::open(&external_db).unwrap();
        external
            .execute_batch(
                r#"
                PRAGMA page_size=4096;
                CREATE TABLE external_data (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                INSERT INTO external_data (name) VALUES ('v1');
                "#,
            )
            .unwrap();
    }
    assert_eq!(pragma_query_string(&sqlite, "graft_add"), "Added app.db");
    assert_eq!(
        pragma_arg_string(&sqlite, "graft_add", "external.db"),
        "Added external.db"
    );
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base").contains("base"));

    assert!(
        pragma_arg_string(&sqlite, "graft_switch_create", "feature/external")
            .contains("feature/external")
    );
    {
        let external = Connection::open(&external_db).unwrap();
        external
            .execute("UPDATE external_data SET name = 'v2' WHERE id = 1", [])
            .unwrap();
    }
    assert_eq!(
        pragma_arg_string(&sqlite, "graft_add", "external.db"),
        "Added external.db"
    );
    assert!(pragma_arg_string(&sqlite, "graft_commit", "external v2").contains("external v2"));

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    assert_eq!(external_value(&external_db), "v1");
    sqlite
        .execute("UPDATE base_app SET name = 'main' WHERE id = 1", [])
        .unwrap();
    assert_eq!(pragma_query_string(&sqlite, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "main app").contains("main app"));

    let merge = pragma_arg_string(&sqlite, "graft_merge", "feature/external");
    assert!(merge.contains("Merged"));
    assert_eq!(external_value(&external_db), "v2");
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["staged"][0], "external.db");
    assert!(status["merge_head"].as_str().is_some());

    let abort = pragma_query_string(&sqlite, "graft_merge_abort");
    assert!(abort.contains("Aborted merge"));
    assert_eq!(external_value(&external_db), "v1");

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_pull_non_fast_forward_enters_merge_state() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let remote_dir = temp_dir.path().join("remote");
    let source_db = temp_dir.path().join("source/app.db");
    let clone_db = temp_dir.path().join("clone/app.db");
    std::fs::create_dir_all(source_db.parent().unwrap()).unwrap();
    std::fs::create_dir_all(clone_db.parent().unwrap()).unwrap();

    let mut source_runtime = GraftTestRuntime::with_memory_remote();
    let source = source_runtime.open_sqlite(source_db.to_str().unwrap(), None);
    let mut clone_runtime = GraftTestRuntime::with_memory_remote();
    let clone = clone_runtime.open_sqlite(clone_db.to_str().unwrap(), None);

    assert!(pragma_query_string(&source, "graft_init").contains(".graft"));
    assert!(pragma_query_string(&clone, "graft_init").contains(".graft"));
    assert!(
        pragma_arg_string(
            &source,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );
    assert!(
        pragma_arg_string(
            &clone,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );

    source
        .execute_batch(
            r#"
            CREATE TABLE repo_pull (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_pull (name) VALUES ('Alice');
            "#,
        )
        .unwrap();
    pragma_query_string(&source, "graft_add");
    assert!(pragma_arg_string(&source, "graft_commit", "base row").contains("base row"));
    assert!(pragma_arg_string(&source, "graft_push", "origin main").contains("origin/main"));

    let first_pull = pragma_arg_string(&clone, "graft_pull", "origin main");
    assert!(first_pull.contains("Fast-forwarded main"));
    let clone_count: i64 = clone
        .query_row("SELECT COUNT(*) FROM repo_pull", [], |row| row.get(0))
        .unwrap();
    assert_eq!(clone_count, 1);

    source
        .execute("INSERT INTO repo_pull (name) VALUES ('Bob')", [])
        .unwrap();
    pragma_query_string(&source, "graft_add");
    assert!(pragma_arg_string(&source, "graft_commit", "remote row").contains("remote row"));
    assert!(pragma_arg_string(&source, "graft_push", "origin main").contains("origin/main"));

    clone
        .execute("INSERT INTO repo_pull (name) VALUES ('Carol')", [])
        .unwrap();
    pragma_query_string(&clone, "graft_add");
    assert!(pragma_arg_string(&clone, "graft_commit", "local row").contains("local row"));

    let pull = pragma_arg_string(&clone, "graft_pull", "origin main");
    assert!(pull.contains("Fetched origin/main"));
    assert!(pull.contains("Unmerged paths:"));
    assert!(pull.contains("app.db"));

    let status: Value = serde_json::from_str(&pragma_query_string(&clone, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert!(status["merge_head"].as_str().is_some());
    assert_eq!(status["conflicted"][0], "app.db");
    let clone_count: i64 = clone
        .query_row("SELECT COUNT(*) FROM repo_pull", [], |row| row.get(0))
        .unwrap();
    assert_eq!(clone_count, 2);

    let abort = pragma_query_string(&clone, "graft_merge_abort");
    assert!(abort.contains("Aborted merge"));
    let status: Value = serde_json::from_str(&pragma_query_string(&clone, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert!(status["merge_head"].is_null());
    assert_eq!(status["conflicted"].as_array().unwrap().len(), 0);

    source_runtime.shutdown().unwrap();
    clone_runtime.shutdown().unwrap();
}

#[test]
fn test_repo_fetch_and_push_all_pragmas_sync_remote_branches() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let remote_dir = temp_dir.path().join("remote");
    let source_db = temp_dir.path().join("source/app.db");
    let clone_db = temp_dir.path().join("clone/app.db");
    std::fs::create_dir_all(source_db.parent().unwrap()).unwrap();
    std::fs::create_dir_all(clone_db.parent().unwrap()).unwrap();

    let mut source_runtime = GraftTestRuntime::with_memory_remote();
    let source = source_runtime.open_sqlite(source_db.to_str().unwrap(), None);
    let mut clone_runtime = GraftTestRuntime::with_memory_remote();
    let clone = clone_runtime.open_sqlite(clone_db.to_str().unwrap(), None);

    assert!(pragma_query_string(&source, "graft_init").contains(".graft"));
    assert!(
        pragma_arg_string(
            &source,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );

    source
        .execute_batch(
            r#"
            CREATE TABLE repo_all (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_all (name) VALUES ('base');
            "#,
        )
        .unwrap();
    pragma_query_string(&source, "graft_add");
    assert!(pragma_arg_string(&source, "graft_commit", "base").contains("base"));

    assert!(
        pragma_arg_string(&source, "graft_switch_create", "feature/search")
            .contains("feature/search")
    );
    source
        .execute("INSERT INTO repo_all (name) VALUES ('feature')", [])
        .unwrap();
    pragma_query_string(&source, "graft_add");
    assert!(pragma_arg_string(&source, "graft_commit", "feature").contains("feature"));

    assert!(pragma_arg_string(&source, "graft_switch_branch", "main").contains("main"));
    source
        .execute("INSERT INTO repo_all (name) VALUES ('main')", [])
        .unwrap();
    pragma_query_string(&source, "graft_add");
    assert!(pragma_arg_string(&source, "graft_commit", "main").contains("main"));

    let pushed = pragma_arg_string(&source, "graft_push", "--all origin");
    assert!(pushed.contains("Pushed origin"));
    assert!(pushed.contains("origin/feature/search"));
    assert!(pushed.contains("origin/main"));

    assert!(pragma_query_string(&clone, "graft_init").contains(".graft"));
    assert!(
        pragma_arg_string(
            &clone,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );
    let fetched = pragma_arg_string(&clone, "graft_fetch", "--all origin");
    assert!(fetched.contains("Fetched origin"));
    assert!(fetched.contains("origin/feature/search"));
    assert!(fetched.contains("origin/main"));

    let clone_repo = graft::repo::Repository::discover_for_file(&clone_db).unwrap();
    assert!(
        clone_repo
            .remote_tracking_ref("origin", "feature/search")
            .unwrap()
            .is_some()
    );
    assert!(
        clone_repo
            .remote_tracking_ref("origin", "main")
            .unwrap()
            .is_some()
    );
    let remote_branches = pragma_arg_string(&clone, "graft_branch", "--remote");
    assert!(remote_branches.contains("origin/feature/search"));
    assert!(remote_branches.contains("origin/main"));
    assert!(!remote_branches.contains("remotes/origin/main"));

    let all_branches = pragma_arg_string(&clone, "graft_branch", "--all");
    assert!(all_branches.contains("* main"));
    assert!(all_branches.contains("remotes/origin/feature/search"));
    assert!(all_branches.contains("remotes/origin/main"));

    source_runtime.shutdown().unwrap();
    clone_runtime.shutdown().unwrap();
}

#[test]
fn test_repo_fetch_async_job_updates_remote_tracking_refs() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let remote_dir = temp_dir.path().join("remote");
    let source_db = temp_dir.path().join("source/app.db");
    let clone_db = temp_dir.path().join("clone/app.db");
    std::fs::create_dir_all(source_db.parent().unwrap()).unwrap();
    std::fs::create_dir_all(clone_db.parent().unwrap()).unwrap();

    let mut source_runtime = GraftTestRuntime::with_memory_remote();
    let source = source_runtime.open_sqlite(source_db.to_str().unwrap(), None);
    let mut clone_runtime = GraftTestRuntime::with_memory_remote();
    let clone = clone_runtime.open_sqlite(clone_db.to_str().unwrap(), None);

    assert!(pragma_query_string(&source, "graft_init").contains(".graft"));
    assert!(
        pragma_arg_string(
            &source,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );

    source
        .execute_batch(
            r#"
            CREATE TABLE repo_async (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_async (name) VALUES ('base');
            "#,
        )
        .unwrap();
    pragma_query_string(&source, "graft_add");
    assert!(pragma_arg_string(&source, "graft_commit", "base").contains("base"));
    assert!(pragma_arg_string(&source, "graft_push", "origin main").contains("origin/main"));

    assert!(pragma_query_string(&clone, "graft_init").contains(".graft"));
    assert!(
        pragma_arg_string(
            &clone,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );

    let job_id = pragma_arg_string(&clone, "graft_fetch_async", "origin main");
    assert!(job_id.starts_with("graft-job-"));

    let status = wait_for_job_done(&clone, &job_id);
    assert_eq!(status["kind"], "fetch");
    assert_eq!(status["state"], "done");

    let result = pragma_arg_string(&clone, "graft_job_result", &job_id);
    assert!(result.contains("Fetched origin/main"));

    let clone_repo = graft::repo::Repository::discover_for_file(&clone_db).unwrap();
    assert!(
        clone_repo
            .remote_tracking_ref("origin", "main")
            .unwrap()
            .is_some()
    );

    source_runtime.shutdown().unwrap();
    clone_runtime.shutdown().unwrap();
}

#[test]
fn test_repo_json_pragmas_cover_eidos_sync_commands() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let source_db = temp_dir.path().join("app.db");
    let remote_dir = temp_dir.path().join("remote");
    let clone_dir = tempfile::tempdir().unwrap();
    let clone_db = clone_dir.path().join("app.db");

    let mut source_runtime = GraftTestRuntime::with_memory_remote();
    let source = source_runtime.open_sqlite(source_db.to_str().unwrap(), None);
    assert!(pragma_query_string(&source, "graft_init").contains(".graft"));
    source
        .execute_batch(
            r#"
            CREATE TABLE repo_json (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_json (name) VALUES ('base');
            "#,
        )
        .unwrap();
    assert_eq!(pragma_query_string(&source, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&source, "graft_commit", "base").contains("base"));

    let branches: Value =
        serde_json::from_str(&pragma_arg_string(&source, "graft_json_branch", "--all"))
            .expect("graft_json_branch should return JSON");
    assert_eq!(branches["branches"][0]["name"], "main");
    assert_eq!(branches["branches"][0]["current"], true);

    assert!(pragma_arg_string(&source, "graft_tag_create", "v-json HEAD").contains("v-json"));
    let tags: Value = serde_json::from_str(&pragma_query_string(&source, "graft_json_tags"))
        .expect("graft_json_tags should return JSON");
    assert_eq!(tags[0]["name"], "v-json");
    assert_eq!(tags[0]["annotated"], false);

    let volumes: Value = serde_json::from_str(&pragma_query_string(
        &source,
        "graft_debug_volume_json_list",
    ))
    .expect("graft_debug_volume_json_list should return JSON");
    assert_eq!(volumes[0]["current"], true);
    assert!(volumes[0]["id"].as_str().is_some());

    let info: Value = serde_json::from_str(&pragma_query_string(
        &source,
        "graft_debug_volume_json_info",
    ))
    .expect("graft_debug_volume_json_info should return JSON");
    assert!(info["vid"].as_str().is_some());
    assert!(info["snapshot_pages"].as_u64().is_some());

    let audit: Value = serde_json::from_str(&pragma_query_string(
        &source,
        "graft_debug_volume_json_audit",
    ))
    .expect("graft_debug_volume_json_audit should return JSON");
    assert_eq!(audit["needs_hydrate"], false);
    assert!(audit["checksum"].as_str().is_some());

    assert!(
        pragma_arg_string(
            &source,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );
    assert!(pragma_arg_string(&source, "graft_branch_upstream", "origin/main").contains("main"));
    let push: Value = serde_json::from_str(&pragma_arg_string(
        &source,
        "graft_json_push",
        "origin main",
    ))
    .expect("graft_json_push should return JSON");
    assert_eq!(push["operation"], "push");
    assert_eq!(push["branches"][0]["remote"], "origin");
    assert_eq!(push["branches"][0]["remote_branch"], "main");

    let mut clone_runtime = GraftTestRuntime::with_memory_remote();
    let clone = clone_runtime.open_sqlite(clone_db.to_str().unwrap(), None);
    assert!(pragma_query_string(&clone, "graft_init").contains(".graft"));
    assert!(
        pragma_arg_string(
            &clone,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );
    assert!(pragma_arg_string(&clone, "graft_branch_upstream", "origin/main").contains("main"));

    let job_id = pragma_arg_string(&clone, "graft_json_fetch_async", "origin main");
    let status = wait_for_job_done(&clone, &job_id);
    assert_eq!(status["state"], "done");
    let fetch: Value =
        serde_json::from_str(&pragma_arg_string(&clone, "graft_json_job_result", &job_id))
            .expect("graft_json_job_result should return fetch JSON");
    assert_eq!(fetch["operation"], "fetch");
    assert_eq!(fetch["branches"][0]["branch"], "main");

    let pull: Value =
        serde_json::from_str(&pragma_arg_string(&clone, "graft_json_pull", "origin main"))
            .expect("graft_json_pull should return JSON");
    assert_eq!(pull["operation"], "pull");
    assert_eq!(pull["remote"], "origin");
    assert_eq!(pull["remote_branch"], "main");
    let clone_count: i64 = clone
        .query_row("SELECT COUNT(*) FROM repo_json", [], |row| row.get(0))
        .unwrap();
    assert_eq!(clone_count, 1);

    clone
        .execute("INSERT INTO repo_json (name) VALUES ('local')", [])
        .unwrap();
    assert_eq!(pragma_query_string(&clone, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&clone, "graft_commit", "local").contains("local"));
    let reset: Value = serde_json::from_str(&pragma_arg_string(
        &clone,
        "graft_json_reset",
        "--hard HEAD~1",
    ))
    .expect("graft_json_reset should return JSON");
    assert_eq!(reset["operation"], "reset");
    assert_eq!(reset["mode"], "hard");
    let reset_count: i64 = clone
        .query_row("SELECT COUNT(*) FROM repo_json", [], |row| row.get(0))
        .unwrap();
    assert_eq!(reset_count, 1);

    let checkout: Value =
        serde_json::from_str(&pragma_arg_string(&clone, "graft_json_checkout", "HEAD"))
            .expect("graft_json_checkout should return JSON");
    assert_eq!(checkout["operation"], "checkout");
    assert!(checkout["target"].as_str().is_some());

    source_runtime.shutdown().unwrap();
    clone_runtime.shutdown().unwrap();
}

#[test]
fn test_repo_fetch_and_push_refspec_pragmas_map_branches() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let remote_dir = temp_dir.path().join("remote");
    let source_db = temp_dir.path().join("source/app.db");
    let clone_db = temp_dir.path().join("clone/app.db");
    std::fs::create_dir_all(source_db.parent().unwrap()).unwrap();
    std::fs::create_dir_all(clone_db.parent().unwrap()).unwrap();

    let mut source_runtime = GraftTestRuntime::with_memory_remote();
    let source = source_runtime.open_sqlite(source_db.to_str().unwrap(), None);
    let mut clone_runtime = GraftTestRuntime::with_memory_remote();
    let clone = clone_runtime.open_sqlite(clone_db.to_str().unwrap(), None);

    assert!(pragma_query_string(&source, "graft_init").contains(".graft"));
    assert!(
        pragma_arg_string(
            &source,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );

    source
        .execute_batch(
            r#"
            CREATE TABLE repo_refspec (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_refspec (name) VALUES ('base');
            "#,
        )
        .unwrap();
    pragma_query_string(&source, "graft_add");
    assert!(pragma_arg_string(&source, "graft_commit", "base").contains("base"));
    assert!(
        pragma_arg_string(&source, "graft_switch_create", "feature/search")
            .contains("feature/search")
    );
    source
        .execute("INSERT INTO repo_refspec (name) VALUES ('feature')", [])
        .unwrap();
    pragma_query_string(&source, "graft_add");
    assert!(pragma_arg_string(&source, "graft_commit", "feature").contains("feature"));
    assert!(
        pragma_arg_string(&source, "graft_switch_create", "unused/refspec")
            .contains("unused/refspec")
    );
    source
        .execute("INSERT INTO repo_refspec (name) VALUES ('unused')", [])
        .unwrap();
    pragma_query_string(&source, "graft_add");
    assert!(pragma_arg_string(&source, "graft_commit", "unused").contains("unused"));

    let pushed = pragma_arg_string(
        &source,
        "graft_push",
        "origin refs/heads/feature/search:refs/heads/review/search",
    );
    assert!(pushed.contains("Pushed origin"));
    assert!(pushed.contains("origin/review/search"));
    let segments_after_refspec = collect_files(&remote_dir.join("segments")).len();

    let pushed_unused = pragma_arg_string(&source, "graft_push", "origin unused/refspec");
    assert!(
        collect_files(&remote_dir.join("segments")).len() > segments_after_refspec,
        "push refspec should not publish snapshots for unmatched local branches before they are pushed explicitly"
    );
    assert!(pushed_unused.contains("origin/unused/refspec"));

    assert!(pragma_query_string(&clone, "graft_init").contains(".graft"));
    assert!(
        pragma_arg_string(
            &clone,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );
    let fetched = pragma_arg_string(
        &clone,
        "graft_fetch",
        "origin refs/heads/review/search:refs/remotes/origin/local/search",
    );
    assert!(fetched.contains("Fetched origin"));
    assert!(fetched.contains("origin/local/search"));

    let clone_repo = graft::repo::Repository::discover_for_file(&clone_db).unwrap();
    assert!(
        clone_repo
            .remote_tracking_ref("origin", "local/search")
            .unwrap()
            .is_some()
    );
    assert_eq!(
        clone_repo
            .remote_tracking_ref("origin", "review/search")
            .unwrap(),
        None
    );

    let deleted = pragma_arg_string(&source, "graft_push", "origin :review/search");
    assert!(deleted.contains("Deleted origin/review/search"));
    let fetched = pragma_arg_error(&clone, "graft_fetch", "origin review/search");
    assert!(fetched.contains("remote `origin` has no branch `review/search`"));

    source_runtime.shutdown().unwrap();
    clone_runtime.shutdown().unwrap();
}

#[test]
fn test_repo_remote_prune_pragma_removes_stale_tracking_refs() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let remote_dir = temp_dir.path().join("remote");
    let source_db = temp_dir.path().join("source/app.db");
    let clone_db = temp_dir.path().join("clone/app.db");
    std::fs::create_dir_all(source_db.parent().unwrap()).unwrap();
    std::fs::create_dir_all(clone_db.parent().unwrap()).unwrap();

    let mut source_runtime = GraftTestRuntime::with_memory_remote();
    let source = source_runtime.open_sqlite(source_db.to_str().unwrap(), None);
    let mut clone_runtime = GraftTestRuntime::with_memory_remote();
    let clone = clone_runtime.open_sqlite(clone_db.to_str().unwrap(), None);

    assert!(pragma_query_string(&source, "graft_init").contains(".graft"));
    assert!(
        pragma_arg_string(
            &source,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );

    source
        .execute_batch(
            r#"
            CREATE TABLE repo_prune (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_prune (name) VALUES ('base');
            "#,
        )
        .unwrap();
    pragma_query_string(&source, "graft_add");
    assert!(pragma_arg_string(&source, "graft_commit", "base").contains("base"));
    assert!(
        pragma_arg_string(&source, "graft_switch_create", "feature/prune")
            .contains("feature/prune")
    );
    source
        .execute("INSERT INTO repo_prune (name) VALUES ('feature')", [])
        .unwrap();
    pragma_query_string(&source, "graft_add");
    assert!(pragma_arg_string(&source, "graft_commit", "feature").contains("feature"));
    assert!(pragma_arg_string(&source, "graft_switch_branch", "main").contains("main"));
    source
        .execute("INSERT INTO repo_prune (name) VALUES ('main')", [])
        .unwrap();
    pragma_query_string(&source, "graft_add");
    assert!(pragma_arg_string(&source, "graft_commit", "main").contains("main"));
    assert!(pragma_arg_string(&source, "graft_push", "--all origin").contains("origin/main"));
    let remote_refs = pragma_arg_string(&source, "graft_ls_remote", "origin");
    assert!(remote_refs.contains("\tHEAD"));
    assert!(remote_refs.contains("refs/heads/feature/prune"));
    assert!(remote_refs.contains("refs/heads/main"));

    assert!(pragma_query_string(&clone, "graft_init").contains(".graft"));
    assert!(
        pragma_arg_string(
            &clone,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );
    assert!(
        pragma_arg_string(&clone, "graft_fetch", "--all origin").contains("origin/feature/prune")
    );

    let clone_repo = graft::repo::Repository::discover_for_file(&clone_db).unwrap();
    assert!(
        clone_repo
            .remote_tracking_ref("origin", "feature/prune")
            .unwrap()
            .is_some()
    );
    assert!(
        clone_repo
            .remote_tracking_ref("origin", "main")
            .unwrap()
            .is_some()
    );

    let deleted = pragma_arg_string(&source, "graft_push", "origin :feature/prune");
    assert!(deleted.contains("Deleted origin/feature/prune"));
    let remote_refs = pragma_arg_string(&source, "graft_ls_remote", "origin");
    assert!(!remote_refs.contains("refs/heads/feature/prune"));
    assert!(remote_refs.contains("refs/heads/main"));
    assert!(
        clone_repo
            .remote_tracking_ref("origin", "feature/prune")
            .unwrap()
            .is_some()
    );

    let pruned = pragma_arg_string(&clone, "graft_remote_prune", "origin");
    assert!(pruned.contains("Pruned origin"));
    assert!(pruned.contains("origin/feature/prune"));
    assert_eq!(
        clone_repo
            .remote_tracking_ref("origin", "feature/prune")
            .unwrap(),
        None
    );
    assert!(
        clone_repo
            .remote_tracking_ref("origin", "main")
            .unwrap()
            .is_some()
    );
    assert!(
        pragma_arg_string(&clone, "graft_remote_prune", "origin")
            .contains("no stale remote-tracking branches")
    );

    source_runtime.shutdown().unwrap();
    clone_runtime.shutdown().unwrap();
}

#[test]
fn test_repo_force_push_pragma_overwrites_non_fast_forward_remote() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let remote_dir = temp_dir.path().join("remote");
    let source_db = temp_dir.path().join("source/app.db");
    let other_repo_dir = temp_dir.path().join("other");
    std::fs::create_dir_all(source_db.parent().unwrap()).unwrap();

    let mut source_runtime = GraftTestRuntime::with_memory_remote();
    let source = source_runtime.open_sqlite(source_db.to_str().unwrap(), None);

    assert!(pragma_query_string(&source, "graft_init").contains(".graft"));
    assert!(
        pragma_arg_string(
            &source,
            "graft_remote_add",
            format!("origin fs://{}", remote_dir.display()),
        )
        .contains("origin")
    );

    source
        .execute_batch(
            r#"
            CREATE TABLE repo_force_push (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_force_push (name) VALUES ('base');
            "#,
        )
        .unwrap();
    pragma_query_string(&source, "graft_add");
    assert!(pragma_arg_string(&source, "graft_commit", "base").contains("base"));
    assert!(pragma_arg_string(&source, "graft_push", "origin main").contains("origin/main"));

    let other = graft::repo::Repository::init(&other_repo_dir).unwrap();
    other
        .remote_add(
            "origin",
            graft::remote::RemoteConfig::Fs {
                root: remote_dir.to_string_lossy().into_owned(),
            },
        )
        .unwrap();
    other.fetch("origin", "main").unwrap();
    other
        .reset("origin/main", graft::repo::ResetMode::Hard)
        .unwrap();
    let remote_tip = other.commit("remote rewrite").unwrap();
    other.push("origin", "main").unwrap();

    source
        .execute("INSERT INTO repo_force_push (name) VALUES ('local')", [])
        .unwrap();
    pragma_query_string(&source, "graft_add");
    assert!(pragma_arg_string(&source, "graft_commit", "local rewrite").contains("local rewrite"));

    let rejected = pragma_arg_error(&source, "graft_push", "origin main");
    assert!(rejected.contains("not an ancestor"));

    let forced = pragma_arg_string(&source, "graft_push", "--force origin main");
    assert!(forced.contains("Force-pushed origin/main"));

    let source_repo = graft::repo::Repository::discover_for_file(&source_db).unwrap();
    let local_tip = source_repo
        .branch_target("main")
        .unwrap()
        .expect("main should point at forced commit");
    assert_ne!(remote_tip.id, local_tip);
    assert_eq!(
        std::fs::read_to_string(remote_dir.join("refs/heads/main"))
            .unwrap()
            .trim(),
        local_tip
    );

    source_runtime.shutdown().unwrap();
}

#[test]
fn test_repo_diff_matrix_on_physical_database_path() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    let init = pragma_query_string(&sqlite, "graft_init");
    assert!(init.contains(".graft"));

    sqlite
        .execute_batch(
            r#"
            CREATE TABLE repo_diff (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_diff (name) VALUES ('Alice');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    let commit = pragma_arg_string(&sqlite, "graft_commit", "initial row");
    assert!(commit.contains("initial row"));

    sqlite
        .execute("INSERT INTO repo_diff (name) VALUES ('Bob')", [])
        .unwrap();
    sqlite
        .execute("UPDATE repo_diff SET name = 'Alicia' WHERE id = 1", [])
        .unwrap();

    let worktree_diff = pragma_query_string(&sqlite, "graft_diff");
    assert!(worktree_diff.contains("Diff index..worktree"));
    assert!(worktree_diff.contains("modified: app.db"));

    let worktree_row_diff = pragma_arg_string(&sqlite, "graft_diff", "--rows");
    assert_repo_row_diff_text(&worktree_row_diff);

    let rev_worktree_diff = pragma_arg_string(&sqlite, "graft_diff", "HEAD");
    assert!(rev_worktree_diff.contains("Diff "));
    assert!(rev_worktree_diff.contains("..worktree"));
    assert!(rev_worktree_diff.contains("modified: app.db"));

    let json_worktree_diff: Value =
        serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_diff"))
            .expect("graft_json_diff should return repo worktree diff JSON");
    assert_eq!(json_worktree_diff["from"], "index");
    assert_eq!(json_worktree_diff["to"], "worktree");
    assert_eq!(json_worktree_diff["files"][0]["path"], "app.db");
    assert_eq!(json_worktree_diff["files"][0]["change"], "modified");

    let json_worktree_row_diff: Value =
        serde_json::from_str(&pragma_arg_string(&sqlite, "graft_json_diff", "--rows"))
            .expect("graft_json_diff --rows should return repo row diff JSON");
    assert_eq!(json_worktree_row_diff["from"], "index");
    assert_eq!(json_worktree_row_diff["to"], "worktree");
    assert_repo_row_diff_json(&json_worktree_row_diff);

    pragma_query_string(&sqlite, "graft_add");

    let unstaged_diff = pragma_query_string(&sqlite, "graft_diff");
    assert!(unstaged_diff.contains("No changes."));

    let unstaged_row_diff = pragma_arg_string(&sqlite, "graft_diff", "--rows");
    assert!(unstaged_row_diff.contains("No changes."));

    let staged_diff = pragma_arg_string(&sqlite, "graft_diff", "--staged");
    assert!(staged_diff.contains("..index"));
    assert!(staged_diff.contains("modified: app.db"));

    let staged_row_diff = pragma_arg_string(&sqlite, "graft_diff", "--rows --staged");
    assert_repo_row_diff_text(&staged_row_diff);

    let json_staged_diff: Value =
        serde_json::from_str(&pragma_arg_string(&sqlite, "graft_json_diff", "--staged"))
            .expect("graft_json_diff should return repo staged diff JSON");
    assert_eq!(json_staged_diff["to"], "index");
    assert_eq!(json_staged_diff["files"][0]["path"], "app.db");
    assert_eq!(json_staged_diff["files"][0]["change"], "modified");

    let json_staged_row_diff: Value = serde_json::from_str(&pragma_arg_string(
        &sqlite,
        "graft_json_diff",
        "--rows --staged",
    ))
    .expect("graft_json_diff --rows should return repo staged row diff JSON");
    assert_eq!(json_staged_row_diff["to"], "index");
    assert_repo_row_diff_json(&json_staged_row_diff);

    let commit = pragma_arg_string(&sqlite, "graft_commit", "add Bob and rename Alice");
    assert!(commit.contains("add Bob and rename Alice"));

    let revision_row_diff =
        pragma_arg_string(&sqlite, "graft_diff", "--rows HEAD~1 HEAD -- app.db");
    assert_repo_row_diff_text(&revision_row_diff);

    let json_revision_row_diff: Value = serde_json::from_str(&pragma_arg_string(
        &sqlite,
        "graft_json_diff",
        "--rows HEAD~1 HEAD -- app.db",
    ))
    .expect("graft_json_diff --rows should return repo revision row diff JSON");
    assert_repo_row_diff_json(&json_revision_row_diff);

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_rm_stages_and_commits_deleted_database_path() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE repo_rm (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_rm (name) VALUES ('Alice');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "initial row").contains("initial row"));

    let removed = pragma_query_string(&sqlite, "graft_rm");
    assert!(removed.contains("Removed app.db"));
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["staged"][0], "app.db");

    let diff = pragma_arg_string(&sqlite, "graft_diff", "--staged");
    assert!(diff.contains("deleted: app.db"));

    let commit = pragma_arg_string(&sqlite, "graft_commit", "remove database");
    assert!(commit.contains("remove database"));
    let repo = graft::repo::Repository::discover_for_file(&db_path).unwrap();
    assert!(repo.head_file(&db_path).unwrap().is_none());

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_checkout_path_restores_database_and_stages_it() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    let init = pragma_query_string(&sqlite, "graft_init");
    assert!(init.contains(".graft"));

    sqlite
        .execute_batch(
            r#"
            CREATE TABLE repo_checkout (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_checkout (name) VALUES ('Alice');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    let commit = pragma_arg_string(&sqlite, "graft_commit", "initial row");
    assert!(commit.contains("initial row"));

    sqlite
        .execute("INSERT INTO repo_checkout (name) VALUES ('Bob')", [])
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    let commit = pragma_arg_string(&sqlite, "graft_commit", "feature row");
    assert!(commit.contains("feature row"));

    let checkout = pragma_arg_string(&sqlite, "graft_checkout", "HEAD~1 -- app.db");
    assert!(checkout.contains("Checked out app.db from"));

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["head"]["type"], "branch");
    assert_eq!(status["head"]["name"], "main");
    assert_eq!(status["dirty"], false);
    assert_eq!(status["staged"][0], "app.db");

    let count: i64 = sqlite
        .query_row("SELECT COUNT(*) FROM repo_checkout", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);

    let staged_diff = pragma_arg_string(&sqlite, "graft_diff", "--staged");
    assert!(staged_diff.contains("modified: app.db"));

    let restored = pragma_arg_string(&sqlite, "graft_restore", "--staged --source HEAD app.db");
    assert_eq!(restored, "Restored app.db");
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], true);
    assert_eq!(status["unstaged"][0], "app.db");
    assert_eq!(status["staged"].as_array().unwrap().len(), 0);
    let count: i64 = sqlite
        .query_row("SELECT COUNT(*) FROM repo_checkout", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);

    let restored = pragma_arg_string(&sqlite, "graft_restore", "app.db");
    assert_eq!(restored, "Restored app.db");
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], false);
    assert_eq!(status["staged"].as_array().unwrap().len(), 0);
    let count: i64 = sqlite
        .query_row("SELECT COUNT(*) FROM repo_checkout", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);

    let checkout = pragma_arg_string(&sqlite, "graft_checkout", "HEAD~1 -- app.db");
    assert!(checkout.contains("Checked out app.db from"));

    let restored = pragma_arg_string(&sqlite, "graft_restore", "--staged app.db");
    assert_eq!(restored, "Restored app.db");
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], true);
    assert_eq!(status["unstaged"][0], "app.db");
    assert_eq!(status["staged"].as_array().unwrap().len(), 0);

    let restored = pragma_arg_string(&sqlite, "graft_restore", "app.db");
    assert_eq!(restored, "Restored app.db");
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["dirty"], false);
    assert_eq!(status["staged"].as_array().unwrap().len(), 0);
    let count: i64 = sqlite
        .query_row("SELECT COUNT(*) FROM repo_checkout", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);

    let checkout = pragma_arg_string(&sqlite, "graft_checkout", "HEAD~1 -- app.db");
    assert!(checkout.contains("Checked out app.db from"));

    let commit = pragma_arg_string(&sqlite, "graft_commit", "restore initial row");
    assert!(commit.contains("restore initial row"));
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["staged"].as_array().unwrap().len(), 0);

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_merge_conflict_records_index_stages() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    let init = pragma_query_string(&sqlite, "graft_init");
    assert!(init.contains(".graft"));

    sqlite
        .execute_batch(
            r#"
            CREATE TABLE repo_merge (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_merge (name) VALUES ('Alice');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    let commit = pragma_arg_string(&sqlite, "graft_commit", "base row");
    assert!(commit.contains("base row"));

    let branch = pragma_arg_string(&sqlite, "graft_branch_create", "feature/search");
    assert!(branch.contains("feature/search"));
    let switched = pragma_arg_string(&sqlite, "graft_switch_branch", "feature/search");
    assert!(switched.contains("feature/search"));
    sqlite
        .execute("INSERT INTO repo_merge (name) VALUES ('Bob')", [])
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    let commit = pragma_arg_string(&sqlite, "graft_commit", "feature row");
    assert!(commit.contains("feature row"));

    let switched = pragma_arg_string(&sqlite, "graft_switch_branch", "main");
    assert!(switched.contains("main"));
    sqlite
        .execute("INSERT INTO repo_merge (name) VALUES ('Carol')", [])
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    let commit = pragma_arg_string(&sqlite, "graft_commit", "main row");
    assert!(commit.contains("main row"));

    let merge = pragma_arg_string(&sqlite, "graft_merge", "feature/search");
    assert!(merge.contains("Unmerged paths:"));
    assert!(merge.contains("app.db"));

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["conflicted"][0], "app.db");
    assert_eq!(status["conflict_analysis"]["path"], "app.db");
    assert_eq!(status["conflict_analysis"]["available"], true);
    assert_eq!(status["conflict_analysis"]["can_auto_merge"], false);
    assert_eq!(
        status["conflict_analysis"]["blocked_reasons"][0],
        "row_conflicts"
    );
    assert_eq!(
        status["conflict_analysis"]["row_conflicts"][0]["table"],
        "repo_merge"
    );
    assert_eq!(status["conflict_analysis"]["row_conflicts"][0]["rowid"], 2);
    assert_eq!(
        status["conflict_analysis"]["row_conflicts"][0]["ours"],
        "insert"
    );
    assert_eq!(
        status["conflict_analysis"]["row_conflicts"][0]["theirs"],
        "insert"
    );
    assert!(
        status["conflict_analysis"]["row_conflicts"][0]["ours_row"]
            .as_array()
            .is_some()
    );
    assert!(
        status["conflict_analysis"]["row_conflicts"][0]["theirs_row"]
            .as_array()
            .is_some()
    );

    let conflicts: Value =
        serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_conflicts"))
            .expect("graft_json_conflicts should return conflict artifact JSON");
    assert_eq!(conflicts["merge_head"], status["merge_head"]);
    assert_eq!(conflicts["conflicts"][0]["kind"], "row");
    assert_eq!(conflicts["conflicts"][0]["reason"], "row_conflict");
    assert_eq!(conflicts["conflicts"][0]["status"], "unresolved");
    assert_eq!(conflicts["conflicts"][0]["path"], "app.db");
    assert_eq!(conflicts["conflicts"][0]["table"], "repo_merge");
    assert_eq!(conflicts["conflicts"][0]["rowid"], 2);
    assert_eq!(conflicts["conflicts"][0]["ours_op"], "insert");
    assert_eq!(conflicts["conflicts"][0]["theirs_op"], "insert");
    let ours_row = conflicts["conflicts"][0]["ours_row"].as_array().unwrap();
    let theirs_row = conflicts["conflicts"][0]["theirs_row"].as_array().unwrap();
    assert!(ours_row.iter().any(|value| value == "Carol"));
    assert!(theirs_row.iter().any(|value| value == "Bob"));

    let mut output = None;
    let result = sqlite.pragma(None, "graft_commit", "merge feature", |row| {
        output = Some(row.get::<_, String>(0)?);
        Ok(())
    });
    assert!(
        result.is_err(),
        "commit should fail with unresolved conflicts"
    );
    assert!(output.is_none());

    let abort = pragma_query_string(&sqlite, "graft_merge_abort");
    assert!(abort.contains("Aborted merge"));
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["conflicted"].as_array().unwrap().len(), 0);
    assert_eq!(status["staged"].as_array().unwrap().len(), 0);
    let count: i64 = sqlite
        .query_row("SELECT COUNT(*) FROM repo_merge", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_row_conflict_resolution_preserves_non_conflicting_changes() {
    graft_test::ensure_test_env();

    for (resolve_arg, expected_body) in [("--ours", "ours"), ("--theirs", "theirs")] {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("app.db");
        let db_name = db_path.to_str().unwrap();

        let mut runtime = GraftTestRuntime::with_memory_remote();
        let sqlite = runtime.open_sqlite(db_name, None);

        assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
        sqlite
            .execute_batch(
                r#"
                CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT NOT NULL);
                INSERT INTO docs (id, body) VALUES (1, 'base');
                "#,
            )
            .unwrap();
        pragma_query_string(&sqlite, "graft_add");
        assert!(pragma_arg_string(&sqlite, "graft_commit", "base doc").contains("base"));

        assert!(
            pragma_arg_string(&sqlite, "graft_branch_create", "feature/theirs")
                .contains("feature/theirs")
        );
        assert!(
            pragma_arg_string(&sqlite, "graft_switch_branch", "feature/theirs")
                .contains("feature/theirs")
        );
        sqlite
            .execute_batch(
                r#"
                CREATE TABLE theirs_table (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                INSERT INTO theirs_table (id, name) VALUES (1, 'theirs table');
                UPDATE docs SET body = 'theirs' WHERE id = 1;
                "#,
            )
            .unwrap();
        pragma_query_string(&sqlite, "graft_add");
        assert!(
            pragma_arg_string(&sqlite, "graft_commit", "theirs table and doc").contains("theirs")
        );

        assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
        sqlite
            .execute_batch(
                r#"
                CREATE TABLE ours_table (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                INSERT INTO ours_table (id, name) VALUES (1, 'ours table');
                UPDATE docs SET body = 'ours' WHERE id = 1;
                "#,
            )
            .unwrap();
        pragma_query_string(&sqlite, "graft_add");
        assert!(pragma_arg_string(&sqlite, "graft_commit", "ours table and doc").contains("ours"));

        let merge = pragma_arg_string(&sqlite, "graft_merge", "feature/theirs");
        assert!(merge.contains("Unmerged paths:"));
        assert!(merge.contains("app.db"));

        let tables_before_resolve: Vec<String> = {
            let mut stmt = sqlite
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE '%_table' ORDER BY name",
                )
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(
            tables_before_resolve,
            vec!["ours_table".to_string(), "theirs_table".to_string()]
        );

        let diff: Value = serde_json::from_str(&pragma_arg_string(
            &sqlite,
            "graft_json_diff",
            "--rows HEAD",
        ))
        .expect("graft_json_diff should return row-level worktree diff JSON");
        let diff_tables = diff["files"][0]["tables"]
            .as_array()
            .unwrap_or_else(|| panic!("worktree diff should include file tables: {diff}"));
        assert!(
            diff_tables
                .iter()
                .any(|table| table["name"].as_str() == Some("theirs_table")),
            "worktree diff should include the auto-merged incoming table: {diff}"
        );

        let conflicts: Value =
            serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_conflicts"))
                .expect("graft_json_conflicts should return conflict artifact JSON");
        assert_eq!(conflicts["conflicts"].as_array().unwrap().len(), 1);
        assert_eq!(conflicts["conflicts"][0]["kind"], "row");
        assert_eq!(conflicts["conflicts"][0]["table"], "docs");

        let resolved: Value = serde_json::from_str(&pragma_arg_string(
            &sqlite,
            "graft_json_resolve_conflict",
            resolve_arg,
        ))
        .expect("graft_json_resolve_conflict should return resolve JSON");
        assert_eq!(resolved["remaining_conflicts"], 0);

        let continued = pragma_arg_string(
            &sqlite,
            "graft_merge_continue",
            &format!("merge with {resolve_arg}"),
        );
        assert!(continued.contains("Merge commit"));

        let tables_after_resolve: Vec<String> = {
            let mut stmt = sqlite
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE '%_table' ORDER BY name",
                )
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(
            tables_after_resolve,
            vec!["ours_table".to_string(), "theirs_table".to_string()]
        );

        let body: String = sqlite
            .query_row("SELECT body FROM docs WHERE id = 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(body, expected_body);

        runtime.shutdown().unwrap();
    }
}

#[test]
fn test_repo_row_conflict_resolution_can_choose_individual_rows() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT NOT NULL);
            INSERT INTO docs (id, body) VALUES
                (1, 'base-1'),
                (2, 'base-2'),
                (3, 'base-3');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base docs").contains("base"));

    assert!(
        pragma_arg_string(&sqlite, "graft_branch_create", "feature/theirs")
            .contains("feature/theirs")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_switch_branch", "feature/theirs")
            .contains("feature/theirs")
    );
    sqlite
        .execute_batch(
            r#"
            UPDATE docs SET body = 'theirs-only-1' WHERE id = 1;
            UPDATE docs SET body = 'theirs-2' WHERE id = 2;
            UPDATE docs SET body = 'theirs-3' WHERE id = 3;
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "theirs docs").contains("theirs"));

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    sqlite
        .execute_batch(
            r#"
            UPDATE docs SET body = 'ours-2' WHERE id = 2;
            UPDATE docs SET body = 'ours-3' WHERE id = 3;
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "ours docs").contains("ours"));

    let merge = pragma_arg_string(&sqlite, "graft_merge", "feature/theirs");
    assert!(merge.contains("Unmerged paths:"));
    assert!(merge.contains("app.db"));

    let rows_after_partial_merge = docs_rows(&sqlite);
    assert_eq!(
        rows_after_partial_merge,
        vec![
            (1, "theirs-only-1".to_string()),
            (2, "ours-2".to_string()),
            (3, "ours-3".to_string())
        ],
        "non-conflicting incoming row should be applied before manual row picks"
    );

    let conflicts: Value =
        serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_conflicts"))
            .expect("graft_json_conflicts should return conflict artifact JSON");
    assert_eq!(conflicts["conflicts"].as_array().unwrap().len(), 2);
    assert_eq!(conflicts["conflicts"][0]["status"], "unresolved");
    assert_eq!(conflicts["conflicts"][1]["status"], "unresolved");

    let resolved_row_2: Value = serde_json::from_str(&pragma_arg_string(
        &sqlite,
        "graft_json_resolve_conflict",
        "--theirs --row docs 2",
    ))
    .expect("graft_json_resolve_conflict should resolve one row");
    assert_eq!(resolved_row_2["remaining_conflicts"], 1);
    assert_eq!(
        docs_rows(&sqlite),
        vec![
            (1, "theirs-only-1".to_string()),
            (2, "theirs-2".to_string()),
            (3, "ours-3".to_string())
        ]
    );

    let conflicts: Value =
        serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_conflicts"))
            .expect("graft_json_conflicts should return conflict artifact JSON");
    let conflicts = conflicts["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 2);
    assert_eq!(conflicts[0]["rowid"], 2);
    assert_eq!(conflicts[0]["status"], "resolved");
    assert_eq!(conflicts[0]["resolution"], "theirs");
    assert_eq!(conflicts[1]["rowid"], 3);
    assert_eq!(conflicts[1]["status"], "unresolved");

    let resolved_row_3: Value = serde_json::from_str(&pragma_arg_string(
        &sqlite,
        "graft_json_resolve_conflict",
        "--ours --row docs 3",
    ))
    .expect("graft_json_resolve_conflict should resolve final row");
    assert_eq!(resolved_row_3["remaining_conflicts"], 0);

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["conflicted"].as_array().unwrap().len(), 0);

    let continued = pragma_arg_string(&sqlite, "graft_merge_continue", "row-picked merge");
    assert!(continued.contains("Merge commit"));
    assert_eq!(
        docs_rows(&sqlite),
        vec![
            (1, "theirs-only-1".to_string()),
            (2, "theirs-2".to_string()),
            (3, "ours-3".to_string())
        ]
    );

    runtime.shutdown().unwrap();
}

fn docs_rows(sqlite: &rusqlite::Connection) -> Vec<(i64, String)> {
    let mut stmt = sqlite
        .prepare("SELECT id, body FROM docs ORDER BY id")
        .unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

#[test]
fn test_repo_merge_continue_creates_merge_commit_after_resolution() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE repo_merge_continue (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_merge_continue (name) VALUES ('Alice');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base row").contains("base row"));

    assert!(
        pragma_arg_string(&sqlite, "graft_branch_create", "feature/continue")
            .contains("feature/continue")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_switch_branch", "feature/continue")
            .contains("feature/continue")
    );
    sqlite
        .execute("INSERT INTO repo_merge_continue (name) VALUES ('Bob')", [])
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "feature row").contains("feature row"));

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    sqlite
        .execute(
            "INSERT INTO repo_merge_continue (name) VALUES ('Carol')",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "main row").contains("main row"));

    let merge = pragma_arg_string(&sqlite, "graft_merge", "feature/continue");
    assert!(merge.contains("Unmerged paths:"));

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert!(status["merge_head"].as_str().is_some());
    assert_eq!(status["conflicted"][0], "app.db");

    let conflicts = pragma_query_string(&sqlite, "graft_conflicts");
    assert!(conflicts.contains("Unmerged paths:"));
    assert!(conflicts.contains("app.db"));

    sqlite
        .execute(
            "UPDATE repo_merge_continue SET name = 'Manual' WHERE id = 2",
            [],
        )
        .unwrap();
    let resolved: Value = serde_json::from_str(&pragma_arg_string(
        &sqlite,
        "graft_json_resolve_conflict",
        "--manual",
    ))
    .expect("graft_json_resolve_conflict should return resolve JSON");
    assert_eq!(resolved["operation"], "resolve_conflict");
    assert_eq!(resolved["path"], "app.db");
    assert_eq!(resolved["resolution"], "manual");
    assert_eq!(resolved["remaining_conflicts"], 0);
    let continued = pragma_arg_string(&sqlite, "graft_merge_continue", "merge feature");
    assert!(continued.contains("Merge commit"));
    assert!(continued.contains("merge feature"));

    let show: Value = serde_json::from_str(&pragma_arg_string(&sqlite, "graft_json_show", "HEAD"))
        .expect("graft_json_show should return repo commit JSON");
    assert_eq!(show["message"], "merge feature");
    assert_eq!(show["parents"].as_array().unwrap().len(), 2);

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["merge_head"], Value::Null);
    assert_eq!(status["conflicted"].as_array().unwrap().len(), 0);
    assert_eq!(status["staged"].as_array().unwrap().len(), 0);

    let names: Vec<String> = {
        let mut stmt = sqlite
            .prepare("SELECT name FROM repo_merge_continue ORDER BY id")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(names, vec!["Alice".to_string(), "Manual".to_string()]);

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_resolve_materializes_physical_sqlite_conflict_side() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);
    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));

    sqlite
        .execute_batch(
            r#"
            CREATE TABLE base_app (id INTEGER PRIMARY KEY);
            INSERT INTO base_app DEFAULT VALUES;
            "#,
        )
        .unwrap();
    let external_db = temp_dir.path().join("external.db");
    {
        let external = Connection::open(&external_db).unwrap();
        external
            .execute_batch(
                r#"
                PRAGMA page_size=4096;
                CREATE TABLE external_data (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                INSERT INTO external_data (name) VALUES ('base');
                "#,
            )
            .unwrap();
    }
    assert_eq!(pragma_query_string(&sqlite, "graft_add"), "Added app.db");
    assert_eq!(
        pragma_arg_string(&sqlite, "graft_add", "external.db"),
        "Added external.db"
    );
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base").contains("base"));

    assert!(
        pragma_arg_string(&sqlite, "graft_switch_create", "feature/external")
            .contains("feature/external")
    );
    {
        let external = Connection::open(&external_db).unwrap();
        external
            .execute("UPDATE external_data SET name = 'theirs' WHERE id = 1", [])
            .unwrap();
    }
    assert_eq!(
        pragma_arg_string(&sqlite, "graft_add", "external.db"),
        "Added external.db"
    );
    assert!(pragma_arg_string(&sqlite, "graft_commit", "theirs external").contains("theirs"));

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    assert_eq!(external_value(&external_db), "base");
    {
        let external = Connection::open(&external_db).unwrap();
        external
            .execute("UPDATE external_data SET name = 'ours' WHERE id = 1", [])
            .unwrap();
    }
    assert_eq!(
        pragma_arg_string(&sqlite, "graft_add", "external.db"),
        "Added external.db"
    );
    assert!(pragma_arg_string(&sqlite, "graft_commit", "ours external").contains("ours"));

    let merge = pragma_arg_string(&sqlite, "graft_merge", "feature/external");
    assert!(merge.contains("Unmerged paths:"));
    assert!(merge.contains("external.db"));
    assert_eq!(external_value(&external_db), "ours");

    let conflicts = pragma_query_string(&sqlite, "graft_conflicts");
    assert!(conflicts.contains("external.db"));
    assert!(conflicts.contains("--theirs [path]"));

    let resolved = pragma_arg_string(&sqlite, "graft_resolve", "--theirs external.db");
    assert_eq!(resolved, "Resolved external.db using theirs");
    assert_eq!(external_value(&external_db), "theirs");
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["conflicted"].as_array().unwrap().len(), 0);
    assert_eq!(status["staged"][0], "external.db");

    let continued = pragma_arg_string(&sqlite, "graft_merge_continue", "merge external");
    assert!(continued.contains("Merge commit"));
    assert_eq!(external_value(&external_db), "theirs");
    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["merge_head"], Value::Null);
    assert_eq!(status["staged"].as_array().unwrap().len(), 0);

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_merge_auto_merges_disjoint_row_changes() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE repo_row_merge (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_row_merge (id, name) VALUES (1, 'Alice');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base row").contains("base row"));

    assert!(pragma_arg_string(&sqlite, "graft_branch_create", "feature/rows").contains("feature"));
    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "feature/rows").contains("feature"));
    sqlite
        .execute("UPDATE repo_row_merge SET name = 'Alicia' WHERE id = 1", [])
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "feature update").contains("feature"));

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    sqlite
        .execute(
            "INSERT INTO repo_row_merge (id, name) VALUES (2, 'Bob')",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "main insert").contains("main"));

    let merge = pragma_arg_string(&sqlite, "graft_merge", "feature/rows");

    assert!(merge.contains("Staged paths:"));
    assert!(merge.contains("app.db"));
    assert!(merge.contains("Row-level auto-merged app.db:"));
    assert!(merge.contains("applied 1 row change(s) from theirs"));
    assert!(merge.contains("ours: 1 row change(s)"));
    assert!(merge.contains("theirs: 1 row change(s)"));
    assert!(!merge.contains("Unmerged paths:"));

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert!(status["merge_head"].as_str().is_some());
    assert_eq!(status["conflicted"].as_array().unwrap().len(), 0);
    assert_eq!(status["staged"][0], "app.db");

    let names: Vec<String> = {
        let mut stmt = sqlite
            .prepare("SELECT name FROM repo_row_merge ORDER BY id")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(names, vec!["Alicia".to_string(), "Bob".to_string()]);

    let continued = pragma_arg_string(&sqlite, "graft_merge_continue", "merge row changes");
    assert!(continued.contains("Merge commit"));

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_merge_continue_auto_resolves_row_merge_candidate_conflict() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE repo_continue_auto_merge (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO repo_continue_auto_merge (id, name) VALUES (1, 'Alice');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base row").contains("base"));

    assert!(
        pragma_arg_string(&sqlite, "graft_branch_create", "feature/continue-auto")
            .contains("feature")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_switch_branch", "feature/continue-auto")
            .contains("feature")
    );
    sqlite
        .execute(
            "UPDATE repo_continue_auto_merge SET name = 'Alicia' WHERE id = 1",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "feature update").contains("feature"));

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    sqlite
        .execute(
            "INSERT INTO repo_continue_auto_merge (id, name) VALUES (2, 'Bob')",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "main insert").contains("main"));

    let repo = graft::repo::Repository::discover_for_file(&db_path).unwrap();
    let plan = repo.plan_merge_revision("feature/continue-auto").unwrap();
    let outcome = repo.apply_merge_plan(&plan).unwrap();
    let graft::repo::MergeOutcome::Merged { conflicted, .. } = outcome else {
        panic!("expected a merge conflict outcome");
    };
    assert_eq!(conflicted, vec!["app.db".to_string()]);

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["conflicted"][0], "app.db");
    assert_eq!(status["conflict_analysis"]["available"], true);
    assert_eq!(status["conflict_analysis"]["can_auto_merge"], true);
    assert_eq!(status["conflict_analysis"]["apply_changes"], 1);

    let continued = pragma_arg_string(&sqlite, "graft_merge_continue", "continue auto row merge");
    assert!(continued.contains("Merge commit"));
    assert!(continued.contains("continue auto row merge"));

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["merge_head"], Value::Null);
    assert_eq!(status["conflicted"].as_array().unwrap().len(), 0);
    assert_eq!(status["staged"].as_array().unwrap().len(), 0);

    let names: Vec<String> = {
        let mut stmt = sqlite
            .prepare("SELECT name FROM repo_continue_auto_merge ORDER BY id")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(names, vec!["Alicia".to_string(), "Bob".to_string()]);

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_merge_continue_auto_merge_ignores_application_triggers() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);
    sqlite
        .create_scalar_function(
            "eidos_column_event_insert",
            1,
            FunctionFlags::SQLITE_UTF8,
            |_| Ok(0_i64),
        )
        .unwrap();

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE repo_triggered_merge (
              id INTEGER PRIMARY KEY,
              name TEXT NOT NULL
            );
            CREATE TRIGGER repo_triggered_merge_insert
            AFTER INSERT ON repo_triggered_merge
            BEGIN
              SELECT eidos_column_event_insert(NEW.id);
            END;
            INSERT INTO repo_triggered_merge (id, name) VALUES (1, 'Alice');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base row").contains("base"));

    assert!(
        pragma_arg_string(&sqlite, "graft_branch_create", "feature/trigger-auto")
            .contains("feature")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_switch_branch", "feature/trigger-auto")
            .contains("feature")
    );
    sqlite
        .execute(
            "INSERT INTO repo_triggered_merge (id, name) VALUES (2, 'Bob')",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "feature insert").contains("feature"));

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    sqlite
        .execute(
            "UPDATE repo_triggered_merge SET name = 'Alicia' WHERE id = 1",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "main update").contains("main"));

    let repo = graft::repo::Repository::discover_for_file(&db_path).unwrap();
    let plan = repo.plan_merge_revision("feature/trigger-auto").unwrap();
    let outcome = repo.apply_merge_plan(&plan).unwrap();
    let graft::repo::MergeOutcome::Merged { conflicted, .. } = outcome else {
        panic!("expected a merge conflict outcome");
    };
    assert_eq!(conflicted, vec!["app.db".to_string()]);

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return repo status JSON");
    assert_eq!(status["conflict_analysis"]["can_auto_merge"], true);

    let continued = pragma_arg_string(&sqlite, "graft_merge_continue", "merge trigger rows");
    assert!(continued.contains("Merge commit"));

    let names: Vec<String> = {
        let mut stmt = sqlite
            .prepare("SELECT name FROM repo_triggered_merge ORDER BY id")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(names, vec!["Alicia".to_string(), "Bob".to_string()]);

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_row_auto_merge_skips_generated_columns_in_apply_sql() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE repo_generated_merge (
              id INTEGER PRIMARY KEY,
              body TEXT NOT NULL,
              body_upper TEXT GENERATED ALWAYS AS (upper(body)) STORED
            );
            INSERT INTO repo_generated_merge (id, body) VALUES (1, 'alpha');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base generated row").contains("base"));

    assert!(
        pragma_arg_string(&sqlite, "graft_branch_create", "feature/generated-apply")
            .contains("feature")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_switch_branch", "feature/generated-apply")
            .contains("feature")
    );
    sqlite
        .execute(
            "INSERT INTO repo_generated_merge (id, body) VALUES (2, 'beta')",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(
        pragma_arg_string(&sqlite, "graft_commit", "feature generated insert").contains("feature")
    );

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    sqlite
        .execute(
            "UPDATE repo_generated_merge SET body = 'alpha main' WHERE id = 1",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "main generated update").contains("main"));

    let merge = pragma_arg_string(&sqlite, "graft_merge", "feature/generated-apply");
    assert!(merge.contains("Row-level auto-merged app.db:"), "{merge}");
    assert!(
        !merge.contains("Unmerged paths:"),
        "generated columns should be omitted from apply SQL: {merge}"
    );

    let rows: Vec<(i64, String, String)> = {
        let mut stmt = sqlite
            .prepare("SELECT id, body, body_upper FROM repo_generated_merge ORDER BY id")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(
        rows,
        vec![
            (1, "alpha main".to_string(), "ALPHA MAIN".to_string()),
            (2, "beta".to_string(), "BETA".to_string()),
        ]
    );

    let continued = pragma_arg_string(&sqlite, "graft_merge_continue", "merge generated rows");
    assert!(continued.contains("Merge commit"));

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_row_auto_merge_resolves_sqlite_sequence_internal_state() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE repo_auto_docs (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              body TEXT NOT NULL
            );
            INSERT INTO repo_auto_docs (body) VALUES ('alpha');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base auto docs").contains("base"));

    let repo = graft::repo::Repository::discover_for_file(&db_path).unwrap();
    let mut config = repo.config().unwrap();
    config
        .merge
        .internal_resolvers
        .insert("sqlite_sequence".to_string(), "sequence_max".to_string());
    repo.write_config(&config).unwrap();
    assert_eq!(
        repo.config().unwrap().merge.internal_resolvers["sqlite_sequence"],
        "sequence_max"
    );

    assert!(
        pragma_arg_string(&sqlite, "graft_branch_create", "feature/auto-sequence")
            .contains("feature")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_switch_branch", "feature/auto-sequence")
            .contains("feature")
    );
    sqlite
        .execute("INSERT INTO repo_auto_docs (body) VALUES ('beta')", [])
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "feature insert").contains("feature"));

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    sqlite
        .execute(
            "UPDATE repo_auto_docs SET body = 'alpha main' WHERE id = 1",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "main update").contains("main"));

    let plan = repo.plan_merge_revision("feature/auto-sequence").unwrap();
    let outcome = repo.apply_merge_plan(&plan).unwrap();
    let graft::repo::MergeOutcome::Merged { conflicted, .. } = outcome else {
        panic!("expected a merge conflict outcome");
    };
    assert_eq!(conflicted, vec!["app.db".to_string()]);

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return JSON");
    assert_eq!(status["conflict_analysis"]["can_auto_merge"], true);
    assert_eq!(status["conflict_analysis"]["opaque_changes"], 0);
    assert_eq!(status["conflict_analysis"]["resolved_opaque_changes"], 1);
    assert!(
        status["conflict_analysis"]["resolved_opaque_change_details"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| {
                change["name"] == "sqlite_sequence"
                    && change["reason"] == "sqlite_internal_table"
                    && change["resolver"] == "sequence_max"
            }),
        "resolved sqlite_sequence details should be exposed: {status}"
    );
    assert!(
        status["conflict_analysis"]["apply_policy"]["internal_resolvers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|resolver| {
                resolver["table"] == "sqlite_sequence" && resolver["resolver"] == "sequence_max"
            }),
        "sqlite_sequence resolver should be exposed in apply policy: {status}"
    );

    let continued = pragma_arg_string(&sqlite, "graft_merge_continue", "merge auto sequence");
    assert!(continued.contains("Merge commit"));

    let rows: Vec<(i64, String)> = {
        let mut stmt = sqlite
            .prepare("SELECT id, body FROM repo_auto_docs ORDER BY id")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(
        rows,
        vec![(1, "alpha main".to_string()), (2, "beta".to_string())]
    );
    let sequence: i64 = sqlite
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'repo_auto_docs'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sequence, 2);

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_row_auto_merge_rebuilds_sqlite_statistics_internal_state() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE repo_stat_docs (
              id INTEGER PRIMARY KEY,
              category TEXT NOT NULL,
              body TEXT NOT NULL
            );
            CREATE INDEX repo_stat_docs_category ON repo_stat_docs(category);
            INSERT INTO repo_stat_docs (id, category, body)
              VALUES (1, 'base', 'alpha');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base stat docs").contains("base"));

    assert!(
        pragma_arg_string(&sqlite, "graft_branch_create", "feature/stat-rebuild")
            .contains("feature")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_switch_branch", "feature/stat-rebuild")
            .contains("feature")
    );
    sqlite
        .execute_batch(
            r#"
            INSERT INTO repo_stat_docs (id, category, body)
              VALUES (2, 'feature', 'beta');
            ANALYZE;
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "feature stats").contains("feature"));

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    sqlite
        .execute(
            "UPDATE repo_stat_docs SET body = 'alpha main' WHERE id = 1",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "main update").contains("main"));

    let repo = graft::repo::Repository::discover_for_file(&db_path).unwrap();
    let plan = repo.plan_merge_revision("feature/stat-rebuild").unwrap();
    let outcome = repo.apply_merge_plan(&plan).unwrap();
    let graft::repo::MergeOutcome::Merged { conflicted, .. } = outcome else {
        panic!("expected a merge conflict outcome");
    };
    assert_eq!(conflicted, vec!["app.db".to_string()]);

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return JSON");
    assert_eq!(status["conflict_analysis"]["can_auto_merge"], true);
    assert_eq!(status["conflict_analysis"]["opaque_changes"], 0);
    assert!(
        status["conflict_analysis"]["resolved_opaque_changes"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert!(
        status["conflict_analysis"]["apply_policy"]["internal_resolvers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|resolver| {
                resolver["table"] == "sqlite_stat1" && resolver["resolver"] == "rebuild"
            }),
        "sqlite_stat1 resolver should be exposed in apply policy: {status}"
    );
    assert!(
        status["conflict_analysis"]["apply_policy"]["internal_resolvers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|resolver| {
                resolver["table"] == "index_btree" && resolver["resolver"] == "reindex"
            }),
        "index btree resolver should be exposed in apply policy: {status}"
    );

    let continued = pragma_arg_string(&sqlite, "graft_merge_continue", "merge stat rebuild");
    assert!(continued.contains("Merge commit"));

    let rows: Vec<(i64, String, String)> = {
        let mut stmt = sqlite
            .prepare("SELECT id, category, body FROM repo_stat_docs ORDER BY id")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(
        rows,
        vec![
            (1, "base".to_string(), "alpha main".to_string()),
            (2, "feature".to_string(), "beta".to_string())
        ]
    );
    let stat_rows: i64 = sqlite
        .query_row(
            "SELECT count(*) FROM sqlite_stat1 WHERE tbl = 'repo_stat_docs'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(stat_rows > 0, "ANALYZE should rebuild sqlite_stat1 rows");

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_row_auto_merge_applies_compatible_add_column_schema_change() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    let repo = graft::repo::Repository::discover_for_file(&db_path).unwrap();
    let mut config = repo.config().unwrap();
    config.merge.schema_resolvers.insert(
        "add_column".to_string(),
        "alter_table_add_column".to_string(),
    );
    repo.write_config(&config).unwrap();
    assert_eq!(
        repo.config().unwrap().merge.schema_resolvers["add_column"],
        "alter_table_add_column"
    );

    sqlite
        .execute_batch(
            r#"
            CREATE TABLE repo_schema_docs (
              id INTEGER PRIMARY KEY,
              title TEXT NOT NULL
            );
            INSERT INTO repo_schema_docs (id, title) VALUES (1, 'alpha');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base schema docs").contains("base"));

    assert!(
        pragma_arg_string(&sqlite, "graft_branch_create", "feature/add-column").contains("feature")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_switch_branch", "feature/add-column").contains("feature")
    );
    sqlite
        .execute("ALTER TABLE repo_schema_docs ADD COLUMN note TEXT", [])
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "feature add note").contains("feature"));

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    sqlite
        .execute(
            "UPDATE repo_schema_docs SET title = 'alpha main' WHERE id = 1",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "main update").contains("main"));

    let merge = pragma_arg_string(&sqlite, "graft_merge", "feature/add-column");
    assert!(merge.contains("Row-level auto-merged app.db:"), "{merge}");
    assert!(
        !merge.contains("Unmerged paths:"),
        "compatible ADD COLUMN should not remain conflicted: {merge}"
    );

    let table_info: Vec<String> = {
        let mut stmt = sqlite
            .prepare("PRAGMA table_info(repo_schema_docs)")
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(table_info, vec!["id", "title", "note"]);
    let title: String = sqlite
        .query_row(
            "SELECT title FROM repo_schema_docs WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(title, "alpha main");

    let continued = pragma_arg_string(&sqlite, "graft_merge_continue", "merge add column");
    assert!(continued.contains("Merge commit"));

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_row_merge_reports_schema_modify_conflict_reason() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE repo_schema_reason_docs (
              id INTEGER PRIMARY KEY,
              body TEXT NOT NULL
            );
            INSERT INTO repo_schema_reason_docs (id, body) VALUES (1, 'alpha');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base schema reason").contains("base"));

    assert!(
        pragma_arg_string(&sqlite, "graft_branch_create", "feature/rename-column")
            .contains("feature")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_switch_branch", "feature/rename-column")
            .contains("feature")
    );
    sqlite
        .execute(
            "ALTER TABLE repo_schema_reason_docs RENAME COLUMN body TO text_body",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(
        pragma_arg_string(&sqlite, "graft_commit", "feature rename column").contains("feature")
    );

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    sqlite
        .execute(
            "UPDATE repo_schema_reason_docs SET body = 'alpha main' WHERE id = 1",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "main body update").contains("main"));

    let merge = pragma_arg_string(&sqlite, "graft_merge", "feature/rename-column");
    assert!(merge.contains("Unmerged paths:"), "{merge}");

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return JSON");
    let schema_conflict = &status["conflict_analysis"]["schema_conflicts"][0];
    assert_eq!(schema_conflict["reason"], "schema_modify_conflict");
    assert_eq!(schema_conflict["name"], "repo_schema_reason_docs");
    assert_eq!(schema_conflict["entry_type"], "table");
    assert!(
        schema_conflict["column_changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| {
                change["side"] == "theirs"
                    && change["operation"] == "rename_column"
                    && change["from"] == "body"
                    && change["to"] == "text_body"
            }),
        "schema conflict should expose column rename details: {status}"
    );
    assert!(
        schema_conflict["message"]
            .as_str()
            .unwrap()
            .contains("compatible schema resolver"),
        "schema conflict message should explain why resolver did not apply: {status}"
    );

    let conflicts: Value =
        serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_conflicts"))
            .expect("graft_json_conflicts should return JSON");
    let artifact = conflicts["conflicts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["kind"] == "schema")
        .expect("schema conflict artifact should be present");
    assert_eq!(artifact["reason"], "schema_modify_conflict");
    assert!(
        artifact["column_changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| {
                change["side"] == "theirs"
                    && change["operation"] == "rename_column"
                    && change["from"] == "body"
                    && change["to"] == "text_body"
            }),
        "schema conflict artifact should expose column rename details: {conflicts}"
    );
    assert!(
        artifact["message"]
            .as_str()
            .unwrap()
            .contains("compatible schema resolver")
    );

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_row_merge_reports_opaque_conflict_artifact_details() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE repo_opaque_marker (
              id INTEGER PRIMARY KEY,
              body TEXT NOT NULL
            );
            CREATE TABLE repo_opaque_docs (
              id TEXT PRIMARY KEY,
              body TEXT NOT NULL
            ) WITHOUT ROWID;
            INSERT INTO repo_opaque_marker (id, body) VALUES (1, 'base');
            INSERT INTO repo_opaque_docs (id, body) VALUES ('doc-1', 'base');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base opaque docs").contains("base"));

    assert!(
        pragma_arg_string(
            &sqlite,
            "graft_branch_create",
            "feature/opaque-without-rowid"
        )
        .contains("feature")
    );
    assert!(
        pragma_arg_string(
            &sqlite,
            "graft_switch_branch",
            "feature/opaque-without-rowid"
        )
        .contains("feature")
    );
    sqlite
        .execute(
            "UPDATE repo_opaque_docs SET body = 'feature' WHERE id = 'doc-1'",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(
        pragma_arg_string(&sqlite, "graft_commit", "feature opaque update").contains("feature")
    );

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    sqlite
        .execute(
            "UPDATE repo_opaque_marker SET body = 'main' WHERE id = 1",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "main marker update").contains("main"));

    let merge = pragma_arg_string(&sqlite, "graft_merge", "feature/opaque-without-rowid");
    assert!(merge.contains("Unmerged paths:"), "{merge}");

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return JSON");
    assert_eq!(status["conflict_analysis"]["can_auto_merge"], false);
    assert_eq!(status["conflict_analysis"]["opaque_changes"], 1);
    assert!(
        status["conflict_analysis"]["blocked_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason == "opaque_changes"),
        "opaque changes should block auto merge: {status}"
    );
    assert!(
        status["conflict_analysis"]["limitations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|limitation| {
                limitation["kind"] == "without_rowid_table"
                    && limitation["subject"] == "repo_opaque_docs"
            }),
        "merge analysis should expose unsupported opaque surface: {status}"
    );

    let conflicts: Value =
        serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_conflicts"))
            .expect("graft_json_conflicts should return JSON");
    let artifact = conflicts["conflicts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["kind"] == "opaque")
        .expect("opaque conflict artifact should be present");
    assert_eq!(artifact["reason"], "without_rowid_table");
    assert_eq!(artifact["name"], "repo_opaque_docs");
    assert_eq!(artifact["change"], "modified");
    assert_eq!(artifact["status"], "unresolved");
    assert!(
        artifact["message"]
            .as_str()
            .unwrap()
            .contains("WITHOUT ROWID"),
        "opaque artifact message should explain resolver boundary: {conflicts}"
    );

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_row_auto_merge_combines_independent_add_column_schema_changes() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE repo_dual_schema_docs (
              id INTEGER PRIMARY KEY,
              title TEXT NOT NULL
            );
            INSERT INTO repo_dual_schema_docs (id, title) VALUES (1, 'alpha');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base dual schema").contains("base"));

    assert!(
        pragma_arg_string(&sqlite, "graft_branch_create", "feature/add-note").contains("feature")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_switch_branch", "feature/add-note").contains("feature")
    );
    sqlite
        .execute("ALTER TABLE repo_dual_schema_docs ADD COLUMN note TEXT", [])
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "feature note").contains("feature"));

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    sqlite
        .execute(
            "ALTER TABLE repo_dual_schema_docs ADD COLUMN status TEXT DEFAULT 'open'",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "main status").contains("main"));

    let merge = pragma_arg_string(&sqlite, "graft_merge", "feature/add-note");
    assert!(merge.contains("Row-level auto-merged app.db:"), "{merge}");
    assert!(
        !merge.contains("Unmerged paths:"),
        "independent ADD COLUMN changes should be combined: {merge}"
    );

    let table_info: Vec<String> = {
        let mut stmt = sqlite
            .prepare("PRAGMA table_info(repo_dual_schema_docs)")
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(table_info, vec!["id", "title", "status", "note"]);

    let continued = pragma_arg_string(&sqlite, "graft_merge_continue", "merge dual add column");
    assert!(continued.contains("Merge commit"));

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_row_auto_merge_preserves_hidden_rowid_insert() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE repo_hidden_rowid_merge (name TEXT NOT NULL);
            INSERT INTO repo_hidden_rowid_merge (rowid, name) VALUES (1, 'Alice');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base hidden rowid").contains("base"));

    assert!(
        pragma_arg_string(&sqlite, "graft_branch_create", "feature/hidden-rowid")
            .contains("feature")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_switch_branch", "feature/hidden-rowid")
            .contains("feature")
    );
    sqlite
        .execute(
            "INSERT INTO repo_hidden_rowid_merge (rowid, name) VALUES (5, 'Bob')",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(
        pragma_arg_string(&sqlite, "graft_commit", "feature hidden insert").contains("feature")
    );

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    sqlite
        .execute(
            "UPDATE repo_hidden_rowid_merge SET name = 'Alicia' WHERE rowid = 1",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "main hidden update").contains("main"));

    let merge = pragma_arg_string(&sqlite, "graft_merge", "feature/hidden-rowid");
    assert!(merge.contains("Row-level auto-merged app.db:"));
    assert!(merge.contains("applied 1 row change(s) from theirs"));

    let rows: Vec<(i64, String)> = {
        let mut stmt = sqlite
            .prepare("SELECT rowid, name FROM repo_hidden_rowid_merge ORDER BY rowid")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(
        rows,
        vec![(1, "Alicia".to_string()), (5, "Bob".to_string())]
    );

    let continued = pragma_arg_string(&sqlite, "graft_merge_continue", "merge hidden rowid");
    assert!(continued.contains("Merge commit"));

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_row_auto_merge_remaps_schema_derived_rowid_conflicts() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE app_nodes (
              id TEXT PRIMARY KEY,
              name TEXT,
              type TEXT,
              position REAL
            );
            CREATE TABLE app_columns (
              name TEXT,
              type TEXT,
              table_name TEXT,
              table_column_name TEXT,
              property TEXT,
              UNIQUE(table_name, table_column_name)
            );
            CREATE TABLE app_views (
              id TEXT PRIMARY KEY,
              name TEXT NOT NULL,
              type TEXT NOT NULL,
              table_id TEXT NOT NULL,
              query TEXT NOT NULL
            );
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base app meta").contains("base"));

    assert!(
        pragma_arg_string(&sqlite, "graft_branch_create", "feature/table-a").contains("feature")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_switch_branch", "feature/table-a").contains("feature")
    );
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE app_table_a (
              _id TEXT PRIMARY KEY NOT NULL,
              title TEXT NULL
            );
            INSERT INTO app_nodes (rowid, id, name, type, position)
              VALUES (1, 'table_a', 'Table A', 'table', 1);
            INSERT INTO app_columns (rowid, name, type, table_name, table_column_name)
              VALUES (1, '_id', 'row-id', 'app_table_a', '_id');
            INSERT INTO app_columns (rowid, name, type, table_name, table_column_name)
              VALUES (2, 'title', 'title', 'app_table_a', 'title');
            INSERT INTO app_views (rowid, id, name, type, table_id, query)
              VALUES (1, 'view_a', 'Grid', 'grid', 'table_a', 'SELECT * FROM app_table_a');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "create table a").contains("table"));

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE app_table_b (
              _id TEXT PRIMARY KEY NOT NULL,
              title TEXT NULL
            );
            INSERT INTO app_nodes (rowid, id, name, type, position)
              VALUES (1, 'table_b', 'Table B', 'table', 1);
            INSERT INTO app_columns (rowid, name, type, table_name, table_column_name)
              VALUES (1, '_id', 'row-id', 'app_table_b', '_id');
            INSERT INTO app_columns (rowid, name, type, table_name, table_column_name)
              VALUES (2, 'title', 'title', 'app_table_b', 'title');
            INSERT INTO app_views (rowid, id, name, type, table_id, query)
              VALUES (1, 'view_b', 'Grid', 'grid', 'table_b', 'SELECT * FROM app_table_b');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "create table b").contains("table"));

    let merge = pragma_arg_string(&sqlite, "graft_merge", "feature/table-a");
    assert!(merge.contains("Row-level auto-merged app.db:"));
    assert!(!merge.contains("Unmerged paths:"));

    let table_names: Vec<String> = {
        let mut stmt = sqlite
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'app_table_%' ORDER BY name",
            )
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(
        table_names,
        vec!["app_table_a".to_string(), "app_table_b".to_string()]
    );

    let tree_ids: Vec<String> = {
        let mut stmt = sqlite
            .prepare("SELECT id FROM app_nodes ORDER BY id")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(tree_ids, vec!["table_a".to_string(), "table_b".to_string()]);

    let columns: Vec<(String, String)> = {
        let mut stmt = sqlite
            .prepare(
                "SELECT table_name, table_column_name FROM app_columns ORDER BY table_name, table_column_name",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(
        columns,
        vec![
            ("app_table_a".to_string(), "_id".to_string()),
            ("app_table_a".to_string(), "title".to_string()),
            ("app_table_b".to_string(), "_id".to_string()),
            ("app_table_b".to_string(), "title".to_string()),
        ]
    );

    let continued = pragma_arg_string(&sqlite, "graft_merge_continue", "merge app tables");
    assert!(continued.contains("Merge commit"));

    runtime.shutdown().unwrap();
}

#[test]
fn test_json_row_diff_handles_overflow_pages() {
    graft_test::ensure_test_env();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite("main", None);

    sqlite
        .execute(
            "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT NOT NULL)",
            [],
        )
        .unwrap();

    let old_body = "a".repeat(10_000);
    let new_body = format!("{}b", "a".repeat(9_999));

    sqlite
        .execute("INSERT INTO docs (id, body) VALUES (1, ?1)", [&old_body])
        .unwrap();
    sqlite
        .execute("UPDATE docs SET body = ?1 WHERE id = 1", [&new_body])
        .unwrap();

    let from_lsn = 2;
    let to_lsn = 3;

    let diff: Value = serde_json::from_str(&pragma_arg_string(
        &sqlite,
        "graft_debug_volume_json_diff",
        format!("{from_lsn},{to_lsn},rows"),
    ))
    .expect("graft_debug_volume_json_diff should return valid JSON");
    let docs_diff = diff["tables"]
        .as_array()
        .expect("diff tables should be an array")
        .iter()
        .find(|table| table["name"] == "docs")
        .expect("docs table should be present in diff output");
    let changes = docs_diff["changes"]
        .as_array()
        .expect("docs changes should be an array");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["op"], "update");
    assert_eq!(changes[0]["rowid"].as_i64(), Some(1));

    assert!(row_values_contain(&changes[0]["old_values"], &old_body));
    assert!(row_values_contain(&changes[0]["values"], &new_body));

    runtime.shutdown().unwrap();
}

#[test]
fn test_json_row_diff_skips_fts_virtual_tables() {
    graft_test::ensure_test_env();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite("main", None);

    let has_fts5: i64 = sqlite
        .query_row(
            "SELECT sqlite_compileoption_used('ENABLE_FTS5')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    if has_fts5 == 0 {
        runtime.shutdown().unwrap();
        return;
    }

    sqlite
        .execute_batch(
            r#"
            CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT NOT NULL);
            INSERT INTO docs (id, body) VALUES (1, 'alpha');
            "#,
        )
        .unwrap();
    let vid = runtime.tag_get("main").unwrap().unwrap();
    let from_lsn = runtime
        .volume_status(&vid)
        .unwrap()
        .local_status
        .head
        .unwrap();

    sqlite
        .execute_batch(
            r#"
            CREATE VIRTUAL TABLE fts_docs USING fts5(body);
            UPDATE docs SET body = 'beta' WHERE id = 1;
            "#,
        )
        .unwrap();
    let to_lsn = runtime
        .volume_status(&vid)
        .unwrap()
        .local_status
        .head
        .unwrap();

    let diff: Value = serde_json::from_str(&pragma_arg_string(
        &sqlite,
        "graft_debug_volume_json_diff",
        format!("{from_lsn},{to_lsn},rows"),
    ))
    .expect("graft_debug_volume_json_diff should skip FTS internals and return valid JSON");
    let tables = diff["tables"]
        .as_array()
        .expect("diff tables should be an array");
    let table_names: Vec<&str> = tables
        .iter()
        .filter_map(|table| table["name"].as_str())
        .collect();
    let opaque_changes = diff["opaque_changes"]
        .as_array()
        .expect("opaque changes should be an array");

    assert_eq!(table_names, vec!["docs"]);
    assert!(
        !opaque_changes.is_empty(),
        "creating the FTS virtual table should be reported as opaque changes"
    );
    assert!(
        !table_names.iter().any(|name| name.starts_with("fts_docs")),
        "FTS virtual and shadow tables should not be exposed as row-level changes: {table_names:?}"
    );
    assert_eq!(tables[0]["changes"][0]["op"], "update");
    assert!(row_values_contain(
        &tables[0]["changes"][0]["values"],
        "beta"
    ));

    runtime.shutdown().unwrap();
}

#[test]
fn test_json_row_diff_reports_opaque_fts_only_changes() {
    graft_test::ensure_test_env();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite("main", None);

    let has_fts5: i64 = sqlite
        .query_row(
            "SELECT sqlite_compileoption_used('ENABLE_FTS5')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    if has_fts5 == 0 {
        runtime.shutdown().unwrap();
        return;
    }

    sqlite
        .execute_batch(
            r#"
            CREATE VIRTUAL TABLE fts_docs USING fts5(body);
            INSERT INTO fts_docs (body) VALUES ('alpha');
            "#,
        )
        .unwrap();
    let vid = runtime.tag_get("main").unwrap().unwrap();
    let from_lsn = runtime
        .volume_status(&vid)
        .unwrap()
        .local_status
        .head
        .unwrap();

    sqlite
        .execute("INSERT INTO fts_docs (body) VALUES ('beta')", [])
        .unwrap();
    let to_lsn = runtime
        .volume_status(&vid)
        .unwrap()
        .local_status
        .head
        .unwrap();

    let diff: Value = serde_json::from_str(&pragma_arg_string(
        &sqlite,
        "graft_debug_volume_json_diff",
        format!("{from_lsn},{to_lsn},rows"),
    ))
    .expect("graft_debug_volume_json_diff should return valid JSON for FTS-only changes");
    assert_eq!(
        diff["tables"]
            .as_array()
            .expect("tables should be an array")
            .len(),
        0
    );

    let opaque_changes = diff["opaque_changes"]
        .as_array()
        .expect("opaque changes should be an array");
    assert!(
        opaque_changes.iter().any(|change| {
            change["reason"] == "fts_shadow_table"
                && change["owner"] == "fts_docs"
                && change["change"] == "modified"
        }),
        "FTS-only changes should be reported as opaque shadow table changes: {opaque_changes:?}"
    );

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_row_auto_merge_uses_configured_semantic_key_policy() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE policy_entities (
              name TEXT NOT NULL
            );
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base policy table").contains("base"));

    let repo = graft::repo::Repository::discover_for_file(&db_path).unwrap();
    let mut config = repo.config().unwrap();
    config
        .merge
        .semantic_keys
        .insert("policy_entities".to_string(), vec!["name".to_string()]);
    repo.write_config(&config).unwrap();

    assert!(
        pragma_arg_string(&sqlite, "graft_branch_create", "feature/policy-key").contains("feature")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_switch_branch", "feature/policy-key").contains("feature")
    );
    sqlite
        .execute(
            "INSERT INTO policy_entities (rowid, name) VALUES (1, 'feature')",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "feature insert").contains("feature"));

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    sqlite
        .execute(
            "INSERT INTO policy_entities (rowid, name) VALUES (1, 'main')",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "main insert").contains("main"));

    let merge = pragma_arg_string(&sqlite, "graft_merge", "feature/policy-key");
    assert!(merge.contains("Row-level auto-merged app.db:"), "{merge}");
    assert!(
        !merge.contains("Unmerged paths:"),
        "semantic key policy should avoid rowid-only conflicts: {merge}"
    );

    let rows: Vec<(i64, String)> = {
        let mut stmt = sqlite
            .prepare("SELECT rowid, name FROM policy_entities ORDER BY name")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(
        rows,
        vec![(2, "feature".to_string()), (1, "main".to_string())]
    );

    let continued = pragma_arg_string(&sqlite, "graft_merge_continue", "merge policy key");
    assert!(continued.contains("Merge commit"));

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_row_merge_detects_configured_semantic_key_insert_conflict() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE policy_conflict_entities (
              entity_id TEXT NOT NULL,
              body TEXT NOT NULL
            );
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base semantic conflict").contains("base"));

    let repo = graft::repo::Repository::discover_for_file(&db_path).unwrap();
    let mut config = repo.config().unwrap();
    config.merge.semantic_keys.insert(
        "policy_conflict_entities".to_string(),
        vec!["entity_id".to_string()],
    );
    repo.write_config(&config).unwrap();

    assert!(
        pragma_arg_string(&sqlite, "graft_branch_create", "feature/semantic-conflict")
            .contains("feature")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_switch_branch", "feature/semantic-conflict")
            .contains("feature")
    );
    sqlite
        .execute(
            "INSERT INTO policy_conflict_entities (rowid, entity_id, body)
             VALUES (2, 'entity-1', 'feature')",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "feature entity").contains("feature"));

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    sqlite
        .execute(
            "INSERT INTO policy_conflict_entities (rowid, entity_id, body)
             VALUES (1, 'entity-1', 'main')",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "main entity").contains("main"));

    let merge = pragma_arg_string(&sqlite, "graft_merge", "feature/semantic-conflict");
    assert!(merge.contains("Unmerged paths:"), "{merge}");
    assert!(
        !merge.contains("Row-level auto-merged"),
        "same semantic key inserts should block auto-merge: {merge}"
    );

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return JSON");
    assert_eq!(status["conflict_analysis"]["can_auto_merge"], false);
    let conflict = &status["conflict_analysis"]["row_conflicts"][0];
    assert_eq!(conflict["reason"], "semantic_key_conflict");
    assert_eq!(conflict["rowid"], 1);
    assert_eq!(conflict["ours_rowid"], Value::Null);
    assert_eq!(conflict["theirs_rowid"], 2);
    assert_eq!(conflict["semantic_key"][0], "t:entity-1");

    let conflicts: Value =
        serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_conflicts"))
            .expect("graft_json_conflicts should return JSON");
    let artifact = &conflicts["conflicts"][0];
    assert_eq!(artifact["reason"], "semantic_key_conflict");
    assert_eq!(artifact["semantic_key"][0], "t:entity-1");

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_row_merge_detects_default_semantic_key_insert_conflict() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE app_default_key_entities (
              _id TEXT NOT NULL,
              title TEXT NOT NULL
            );
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base default key").contains("base"));

    let repo = graft::repo::Repository::discover_for_file(&db_path).unwrap();
    let mut config = repo.config().unwrap();
    config.merge.default_semantic_keys = vec!["_id".to_string()];
    repo.write_config(&config).unwrap();

    assert!(
        pragma_arg_string(&sqlite, "graft_branch_create", "feature/default-semantic")
            .contains("feature")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_switch_branch", "feature/default-semantic")
            .contains("feature")
    );
    sqlite
        .execute(
            "INSERT INTO app_default_key_entities (rowid, _id, title)
             VALUES (5, 'row-1', 'feature')",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "feature default key").contains("feature"));

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    sqlite
        .execute(
            "INSERT INTO app_default_key_entities (rowid, _id, title)
             VALUES (3, 'row-1', 'main')",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "main default key").contains("main"));

    let merge = pragma_arg_string(&sqlite, "graft_merge", "feature/default-semantic");
    assert!(merge.contains("Unmerged paths:"), "{merge}");

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return JSON");
    assert_eq!(
        status["conflict_analysis"]["apply_policy"]["default_semantic_keys"][0],
        "_id"
    );
    let conflict = &status["conflict_analysis"]["row_conflicts"][0];
    assert_eq!(conflict["reason"], "semantic_key_conflict");
    assert_eq!(conflict["rowid"], 3);
    assert_eq!(conflict["theirs_rowid"], 5);
    assert_eq!(conflict["semantic_key"][0], "t:row-1");

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_row_merge_reports_semantic_key_on_rowid_update_conflict() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE app_update_key_entities (
              _id TEXT NOT NULL,
              title TEXT NOT NULL
            );
            INSERT INTO app_update_key_entities (rowid, _id, title)
            VALUES (1, 'row-1', 'base');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base update key").contains("base"));

    let repo = graft::repo::Repository::discover_for_file(&db_path).unwrap();
    let mut config = repo.config().unwrap();
    config.merge.default_semantic_keys = vec!["_id".to_string()];
    repo.write_config(&config).unwrap();

    assert!(
        pragma_arg_string(&sqlite, "graft_branch_create", "feature/update-semantic")
            .contains("feature")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_switch_branch", "feature/update-semantic")
            .contains("feature")
    );
    sqlite
        .execute(
            "UPDATE app_update_key_entities SET title = 'feature' WHERE rowid = 1",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "feature update key").contains("feature"));

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    sqlite
        .execute(
            "UPDATE app_update_key_entities SET title = 'main' WHERE rowid = 1",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "main update key").contains("main"));

    let merge = pragma_arg_string(&sqlite, "graft_merge", "feature/update-semantic");
    assert!(merge.contains("Unmerged paths:"), "{merge}");

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return JSON");
    let conflict = &status["conflict_analysis"]["row_conflicts"][0];
    assert_eq!(conflict["reason"], "row_conflict");
    assert_eq!(conflict["rowid"], 1);
    assert_eq!(conflict["semantic_key"][0], "t:row-1");

    let conflicts: Value =
        serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_conflicts"))
            .expect("graft_json_conflicts should return JSON");
    let artifact = &conflicts["conflicts"][0];
    assert_eq!(artifact["reason"], "row_conflict");
    assert_eq!(artifact["semantic_key"][0], "t:row-1");

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_row_merge_analysis_reports_parser_limitations() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE limited_merge_rows (
              id INTEGER PRIMARY KEY,
              body TEXT NOT NULL
            );
            CREATE TABLE generated_merge_surface (
              id INTEGER PRIMARY KEY,
              body TEXT NOT NULL,
              body_len INTEGER GENERATED ALWAYS AS (length(body)) VIRTUAL
            );
            INSERT INTO limited_merge_rows (id, body) VALUES (1, 'base');
            INSERT INTO generated_merge_surface (id, body) VALUES (1, 'unchanged');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base limited merge").contains("base"));

    let repo = graft::repo::Repository::discover_for_file(&db_path).unwrap();
    let mut config = repo.config().unwrap();
    config.merge.generated_columns.insert(
        "generated_merge_surface".to_string(),
        vec!["body_len".to_string()],
    );
    repo.write_config(&config).unwrap();

    assert!(
        pragma_arg_string(&sqlite, "graft_branch_create", "feature/limited-analysis")
            .contains("feature")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_switch_branch", "feature/limited-analysis")
            .contains("feature")
    );
    sqlite
        .execute(
            "UPDATE limited_merge_rows SET body = 'feature' WHERE id = 1",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "feature limited row").contains("feature"));

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    sqlite
        .execute(
            "UPDATE limited_merge_rows SET body = 'main' WHERE id = 1",
            [],
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "main limited row").contains("main"));

    let merge = pragma_arg_string(&sqlite, "graft_merge", "feature/limited-analysis");
    assert!(merge.contains("Unmerged paths:"), "{merge}");

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return JSON");
    assert_eq!(status["conflict_analysis"]["can_auto_merge"], false);
    assert!(
        status["conflict_analysis"]["limitations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|limitation| {
                limitation["kind"] == "generated_columns"
                    && limitation["subject"] == "generated_merge_surface"
            }),
        "merge analysis should expose parser limitations: {status}"
    );
    assert!(
        status["conflict_analysis"]["apply_policy"]["generated_columns"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| {
                entry["table"] == "generated_merge_surface" && entry["columns"][0] == "body_len"
            }),
        "merge analysis should expose configured generated column policy: {status}"
    );

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_row_diff_reports_file_changed_without_supported_logical_changes() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE logical_noop (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            INSERT INTO logical_noop (id, name) VALUES (1, 'Alice');
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base logical row").contains("base"));

    sqlite
        .execute(
            "INSERT INTO logical_noop (id, name) VALUES (2, 'temporary')",
            [],
        )
        .unwrap();
    sqlite
        .execute("DELETE FROM logical_noop WHERE id = 2", [])
        .unwrap();

    let diff: Value =
        serde_json::from_str(&pragma_arg_string(&sqlite, "graft_json_diff", "--rows"))
            .expect("graft_json_diff --rows should return JSON for logical no-op file changes");
    assert_eq!(diff["files"][0]["path"], "app.db");
    assert_eq!(diff["files"][0]["change"], "modified");
    assert_eq!(diff["files"][0]["row_diff_available"], true);
    assert_eq!(
        diff["files"][0]["logical_status"],
        "file_changed_no_supported_logical_changes"
    );
    assert_eq!(diff["files"][0]["tables"].as_array().map_or(0, Vec::len), 0);
    assert_eq!(
        diff["files"][0]["opaque_changes"]
            .as_array()
            .map_or(0, Vec::len),
        0
    );

    runtime.shutdown().unwrap();
}

#[test]
fn test_json_row_diff_reports_without_rowid_as_unsupported_surface() {
    graft_test::ensure_test_env();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite("main", None);

    sqlite
        .execute_batch(
            r#"
            CREATE TABLE without_rowid_docs (
              id TEXT PRIMARY KEY,
              body TEXT NOT NULL
            ) WITHOUT ROWID;
            INSERT INTO without_rowid_docs (id, body) VALUES ('doc-1', 'alpha');
            "#,
        )
        .unwrap();
    let vid = runtime.tag_get("main").unwrap().unwrap();
    let from_lsn = runtime
        .volume_status(&vid)
        .unwrap()
        .local_status
        .head
        .unwrap();

    sqlite
        .execute(
            "UPDATE without_rowid_docs SET body = 'beta' WHERE id = 'doc-1'",
            [],
        )
        .unwrap();
    let to_lsn = runtime
        .volume_status(&vid)
        .unwrap()
        .local_status
        .head
        .unwrap();

    let diff: Value = serde_json::from_str(&pragma_arg_string(
        &sqlite,
        "graft_debug_volume_json_diff",
        format!("{from_lsn},{to_lsn},rows"),
    ))
    .expect("row diff should return JSON for WITHOUT ROWID changes");
    assert_eq!(diff["logical_status"], "unsupported_logical_surface");
    assert_eq!(diff["tables"].as_array().unwrap().len(), 0);
    assert!(
        diff["opaque_changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| {
                change["name"] == "without_rowid_docs"
                    && change["reason"] == "without_rowid_table"
                    && change["change"] == "modified"
            }),
        "WITHOUT ROWID table changes should be opaque: {diff}"
    );
    assert!(
        diff["limitations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|limitation| {
                limitation["kind"] == "without_rowid_table"
                    && limitation["subject"] == "without_rowid_docs"
            }),
        "WITHOUT ROWID limitation should be explicit: {diff}"
    );

    runtime.shutdown().unwrap();
}

#[test]
fn test_json_row_diff_reports_sqlite_sequence_internal_change() {
    graft_test::ensure_test_env();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite("main", None);

    sqlite
        .execute_batch(
            r#"
            CREATE TABLE auto_docs (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              body TEXT NOT NULL
            );
            INSERT INTO auto_docs (body) VALUES ('alpha');
            "#,
        )
        .unwrap();
    let vid = runtime.tag_get("main").unwrap().unwrap();
    let from_lsn = runtime
        .volume_status(&vid)
        .unwrap()
        .local_status
        .head
        .unwrap();

    sqlite
        .execute("INSERT INTO auto_docs (body) VALUES ('beta')", [])
        .unwrap();
    let to_lsn = runtime
        .volume_status(&vid)
        .unwrap()
        .local_status
        .head
        .unwrap();

    let diff: Value = serde_json::from_str(&pragma_arg_string(
        &sqlite,
        "graft_debug_volume_json_diff",
        format!("{from_lsn},{to_lsn},rows"),
    ))
    .expect("row diff should return JSON for sqlite_sequence changes");
    assert_eq!(diff["logical_status"], "logical_changes");
    assert!(
        diff["opaque_changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| {
                change["name"] == "sqlite_sequence"
                    && change["reason"] == "sqlite_internal_table"
                    && change["change"] == "modified"
            }),
        "sqlite_sequence changes should be exposed as internal opaque changes: {diff}"
    );
    assert!(
        diff["limitations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|limitation| {
                limitation["kind"] == "sqlite_internal_table"
                    && limitation["subject"] == "sqlite_sequence"
            }),
        "sqlite_sequence limitation should be explicit: {diff}"
    );

    runtime.shutdown().unwrap();
}

#[test]
fn test_json_row_diff_reports_index_btree_internal_change() {
    graft_test::ensure_test_env();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite("main", None);

    sqlite
        .execute_batch(
            r#"
            CREATE TABLE indexed_docs (
              id INTEGER PRIMARY KEY,
              category TEXT NOT NULL,
              body TEXT NOT NULL
            );
            CREATE INDEX indexed_docs_category ON indexed_docs(category);
            INSERT INTO indexed_docs (id, category, body) VALUES (1, 'alpha', 'first');
            "#,
        )
        .unwrap();
    let vid = runtime.tag_get("main").unwrap().unwrap();
    let from_lsn = runtime
        .volume_status(&vid)
        .unwrap()
        .local_status
        .head
        .unwrap();

    sqlite
        .execute("UPDATE indexed_docs SET category = 'beta' WHERE id = 1", [])
        .unwrap();
    let to_lsn = runtime
        .volume_status(&vid)
        .unwrap()
        .local_status
        .head
        .unwrap();

    let diff: Value = serde_json::from_str(&pragma_arg_string(
        &sqlite,
        "graft_debug_volume_json_diff",
        format!("{from_lsn},{to_lsn},rows"),
    ))
    .expect("row diff should return JSON for index btree changes");
    assert_eq!(diff["logical_status"], "logical_changes");
    assert!(
        diff["opaque_changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| {
                change["name"] == "indexed_docs_category"
                    && change["reason"] == "index_btree"
                    && change["change"] == "modified"
            }),
        "index btree changes should be exposed as resolved internal state: {diff}"
    );
    assert!(
        diff["limitations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|limitation| {
                limitation["kind"] == "index_btree"
                    && limitation["subject"] == "indexed_docs_category"
            }),
        "index btree limitation should be explicit: {diff}"
    );

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_row_auto_merge_validates_foreign_keys_after_apply() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let db_name = db_path.to_str().unwrap();

    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_name, None);

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE fk_parent (id INTEGER PRIMARY KEY);
            CREATE TABLE fk_child (
              id INTEGER PRIMARY KEY,
              parent_id INTEGER NOT NULL REFERENCES fk_parent(id)
            );
            INSERT INTO fk_parent (id) VALUES (1);
            "#,
        )
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "base fk").contains("base"));

    assert!(
        pragma_arg_string(&sqlite, "graft_branch_create", "feature/fk-child").contains("feature")
    );
    assert!(
        pragma_arg_string(&sqlite, "graft_switch_branch", "feature/fk-child").contains("feature")
    );
    sqlite
        .execute("INSERT INTO fk_child (id, parent_id) VALUES (1, 1)", [])
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "child insert").contains("child"));

    assert!(pragma_arg_string(&sqlite, "graft_switch_branch", "main").contains("main"));
    sqlite
        .execute("DELETE FROM fk_parent WHERE id = 1", [])
        .unwrap();
    pragma_query_string(&sqlite, "graft_add");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "parent delete").contains("parent"));

    let merge = pragma_arg_string(&sqlite, "graft_merge", "feature/fk-child");
    assert!(merge.contains("Unmerged paths:"), "{merge}");
    assert!(
        !merge.contains("Row-level auto-merged"),
        "validation failure should keep the database conflicted: {merge}"
    );

    let status: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_status"))
        .expect("graft_json_status should return JSON");
    assert_eq!(
        status["conflict_analysis"]["apply_policy"]["foreign_keys"],
        "disabled_during_apply_checked_after"
    );
    assert_eq!(
        status["conflict_analysis"]["apply_policy"]["triggers"],
        "disabled_during_apply"
    );
    assert!(
        status["conflict_analysis"]["apply_policy"]["validation"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "foreign_key_check")
    );
    assert!(
        status["conflict_analysis"]["apply_policy"]["schema_resolvers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|resolver| {
                resolver["operation"] == "add_column"
                    && resolver["resolver"] == "alter_table_add_column"
            }),
        "ADD COLUMN schema resolver should be exposed in apply policy: {status}"
    );

    let err = pragma_arg_error(&sqlite, "graft_merge_continue", "invalid fk merge");
    assert!(
        err.contains("foreign_key_check"),
        "merge_continue should fail with foreign_key_check details: {err}"
    );

    runtime.shutdown().unwrap();
}

#[test]
fn test_repo_commit_summary_skips_fts_virtual_tables() {
    graft_test::ensure_test_env();

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("app.db");
    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite(db_path.to_str().unwrap(), None);

    let has_fts5: i64 = sqlite
        .query_row(
            "SELECT sqlite_compileoption_used('ENABLE_FTS5')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    if has_fts5 == 0 {
        runtime.shutdown().unwrap();
        return;
    }

    assert!(pragma_query_string(&sqlite, "graft_init").contains(".graft"));
    sqlite
        .execute_batch(
            r#"
            CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT NOT NULL);
            CREATE VIRTUAL TABLE fts_docs USING fts5(body);
            INSERT INTO docs (id, body) VALUES (1, 'alpha');
            "#,
        )
        .unwrap();

    assert_eq!(pragma_query_string(&sqlite, "graft_add"), "Added app.db");
    assert!(pragma_arg_string(&sqlite, "graft_commit", "fts initial").contains("fts initial"));

    let json_log: Value = serde_json::from_str(&pragma_query_string(&sqlite, "graft_json_log"))
        .expect("graft_json_log should return repo commit JSON");
    let tables = json_log[0]["tables"]
        .as_array()
        .expect("commit tables should be an array");
    let table_names: Vec<&str> = tables
        .iter()
        .filter_map(|table| table["name"].as_str())
        .collect();

    assert_eq!(table_names, vec!["docs"]);
    assert_eq!(json_log[0]["changed_tables"], 1);

    runtime.shutdown().unwrap();
}

/// Test that VACUUM INTO can be used to import a non-graft `SQLite` database into Graft.
/// This is the recommended way to import existing databases as documented at:
/// <https://graft.rs/r/graft_import>
#[test]
fn test_vacuum_into_import() {
    graft_test::ensure_test_env();

    // Create a regular SQLite database with some data
    let temp_dir = tempfile::tempdir().unwrap();
    let source_path = temp_dir.path().join("source.db");

    let source_conn = Connection::open(&source_path).unwrap();

    source_conn
        .execute_batch(
            r#"
            -- set WAL mode on the source db
            PRAGMA journal_mode=WAL;
            -- set a too-large page size
            PRAGMA page_size=8192;

            CREATE TABLE test_data (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                value INTEGER NOT NULL
            );
            INSERT INTO test_data (id, name, value) VALUES
                (1, 'Alice', 100),
                (2, 'Bob', 200),
                (3, 'Charlie', 300);
            "#,
        )
        .unwrap();

    // Verify source data
    let count: i64 = source_conn
        .query_row("SELECT COUNT(*) FROM test_data", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 3);

    // Create a Graft runtime and ensure the VFS is registered
    let mut runtime = GraftTestRuntime::with_memory_remote();
    let vfs_name = runtime.ensure_vfs();

    // Use VACUUM INTO to import the source database into a new Graft volume
    // Also use the page size pragma right before the vacuum to change the page size
    let vacuum_uri = format!("file:imported?vfs={vfs_name}");
    source_conn
        .execute_batch(&format!(
            r#"
            PRAGMA page_size=4096;
            VACUUM INTO '{vacuum_uri}';
            "#
        ))
        .unwrap();

    drop(source_conn);

    // Open the imported Graft volume and verify the data
    let sqlite = runtime.open_sqlite("imported", None);

    // Verify the imported data
    let count: i64 = sqlite
        .query_row("SELECT COUNT(*) FROM test_data", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 3);

    let name: String = sqlite
        .query_row("SELECT name FROM test_data WHERE id = 2", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(name, "Bob");

    // Verify we can query all the data
    let sum: i64 = sqlite
        .query_row("SELECT SUM(value) FROM test_data", [], |row| row.get(0))
        .unwrap();
    assert_eq!(sum, 600);

    // Cleanup
    runtime.shutdown().unwrap();
}

#[test]
fn test_sqlite_query_only_fetches_needed_pages() {
    graft_test::ensure_test_env();

    let log = LogId::random();

    // create a writer
    let mut writer = GraftTestRuntime::with_memory_remote();
    let writer_sql = writer.open_sqlite("main", Some(log.clone()));
    let writer_vid = writer.tag_get("main").unwrap().unwrap();

    // create a reader
    let mut reader = writer.spawn_peer();
    let reader_sql = reader.open_sqlite("main", Some(log));
    let reader_vid = reader.tag_get("main").unwrap().unwrap();

    // create a table and then insert 10 rows, which each consume just over a page. then push each segment to the remote
    // note: we use separate txns for each row to ensure they end up in separate segments
    writer_sql.execute("CREATE TABLE t (d)", []).unwrap();
    for _ in 0..10 {
        writer_sql
            .execute("insert into t values (printf('%0*d', 4096, 0))", [])
            .unwrap();
        writer_sql.graft_pragma("debug_volume_push").unwrap();
    }

    let snapshot = writer.volume_snapshot(&writer_vid).unwrap();
    assert_eq!(
        writer.snapshot_pages(&snapshot).unwrap(),
        PageCount::new(14)
    );

    // pull changes into the reader
    reader_sql.graft_pragma("debug_volume_pull").unwrap();

    // all pages missing
    let snapshot = reader.volume_snapshot(&reader_vid).unwrap();
    assert_eq!(
        reader
            .snapshot_missing_pages(&snapshot)
            .unwrap()
            .cardinality()
            .to_usize(),
        14
    );

    // perform a single row lookup by ID
    let value: i32 = reader_sql
        .query_row("SELECT length(d) FROM t LIMIT 1", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, 4096);

    // only 5 pages retrieved
    assert_eq!(
        reader
            .snapshot_missing_pages(&snapshot)
            .unwrap()
            .cardinality()
            .to_usize(),
        9
    );

    // perform a query that reads all rows
    let value: i32 = reader_sql
        .query_row("SELECT sum(length(d)) FROM t", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, 40960);

    // no pages missing
    assert_eq!(
        reader
            .snapshot_missing_pages(&snapshot)
            .unwrap()
            .cardinality()
            .to_usize(),
        0
    );
}

fn assert_repo_row_diff_text(diff: &str) {
    assert!(diff.contains("Row Diff"), "{diff}");
    assert!(diff.contains("modified: app.db"), "{diff}");
    assert!(diff.contains("Table 'repo_diff'"), "{diff}");
    assert!(diff.contains("+1 inserts"), "{diff}");
    assert!(diff.contains("~1 updates"), "{diff}");
    assert!(diff.contains("Alicia"), "{diff}");
    assert!(diff.contains("Bob"), "{diff}");
}

fn assert_repo_row_diff_json(diff: &Value) {
    assert_eq!(diff["files"][0]["path"], "app.db");
    assert_eq!(diff["files"][0]["change"], "modified");
    assert_eq!(diff["files"][0]["row_diff_available"], true);
    assert_eq!(diff["files"][0]["logical_status"], "logical_changes");
    assert!(
        diff["files"][0]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "rowid_table_rows"),
        "{diff}"
    );

    let tables = diff["files"][0]["tables"]
        .as_array()
        .expect("repo row diff should include tables");
    let table = tables
        .iter()
        .find(|table| table["name"] == "repo_diff")
        .expect("repo_diff table should be present");
    let changes = table["changes"]
        .as_array()
        .expect("repo_diff table should include row changes");
    assert!(
        changes.iter().any(|change| {
            change["op"] == "insert" && json_values_contain(&change["values"], "Bob")
        }),
        "{diff}"
    );
    assert!(
        changes.iter().any(|change| {
            change["op"] == "update"
                && json_values_contain(&change["old_values"], "Alice")
                && json_values_contain(&change["values"], "Alicia")
        }),
        "{diff}"
    );
}

fn json_values_contain(values: &Value, needle: &str) -> bool {
    values
        .as_array()
        .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(needle)))
}

fn pragma_query_string(conn: &Connection, name: &str) -> String {
    let mut output = None;
    conn.pragma_query(None, name, |row| {
        output = Some(row.get::<_, String>(0)?);
        Ok(())
    })
    .unwrap();
    output.unwrap()
}

fn pragma_query_error(conn: &Connection, name: &str) -> String {
    let mut output = None;
    let err = conn
        .pragma_query(None, name, |row| {
            output = Some(row.get::<_, String>(0)?);
            Ok(())
        })
        .expect_err("pragma should fail");
    assert!(output.is_none());
    err.to_string()
}

fn pragma_arg_string<T: ToSql>(conn: &Connection, name: &str, arg: T) -> String {
    let mut output = None;
    conn.pragma(None, name, arg, |row| {
        output = Some(row.get::<_, String>(0)?);
        Ok(())
    })
    .unwrap();
    output.unwrap()
}

fn pragma_arg_error<T: ToSql>(conn: &Connection, name: &str, arg: T) -> String {
    let mut output = None;
    let err = conn
        .pragma(None, name, arg, |row| {
            output = Some(row.get::<_, String>(0)?);
            Ok(())
        })
        .expect_err("pragma should fail");
    assert!(output.is_none());
    err.to_string()
}

fn wait_for_job_done(conn: &Connection, job_id: &str) -> Value {
    for _ in 0..100 {
        let status: Value =
            serde_json::from_str(&pragma_arg_string(conn, "graft_job_status", job_id)).unwrap();
        match status["state"].as_str() {
            Some("done") => return status,
            Some("failed") => panic!("job failed: {status}"),
            Some("running") => std::thread::sleep(std::time::Duration::from_millis(10)),
            other => panic!("unexpected job state {other:?}: {status}"),
        }
    }
    panic!("job `{job_id}` did not finish in time")
}

fn collect_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    let mut files = Vec::new();
    collect_files_into(root, &mut files);
    files.sort();
    files
}

fn collect_files_into(root: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            collect_files_into(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn external_value(path: &std::path::Path) -> String {
    let conn = Connection::open(path).unwrap();
    conn.query_row("SELECT name FROM external_data WHERE id = 1", [], |row| {
        row.get(0)
    })
    .unwrap()
}

fn tamper_sqlite_snapshot_range_hash(
    repo: &graft::repo::Repository,
    commit_id: &str,
    path: &str,
) -> String {
    rewrite_sqlite_snapshot_range_hash(
        repo,
        commit_id,
        path,
        Some(graft::core::commit_hash::CommitHash::ZERO),
    )
}

fn remove_sqlite_snapshot_range_hash(
    repo: &graft::repo::Repository,
    commit_id: &str,
    path: &str,
) -> String {
    rewrite_sqlite_snapshot_range_hash(repo, commit_id, path, None)
}

fn rewrite_sqlite_snapshot_range_hash(
    repo: &graft::repo::Repository,
    commit_id: &str,
    path: &str,
    commit_hash: Option<graft::core::commit_hash::CommitHash>,
) -> String {
    use graft::repo::object::{BlobObject, Object, TreeObject};

    let Object::Commit(mut commit) = repo.read_object(commit_id).unwrap() else {
        panic!("HEAD should point at a commit object");
    };
    let Object::Tree(tree) = repo.read_object(commit.tree.as_str()).unwrap() else {
        panic!("commit should point at a tree object");
    };
    let mut entries = tree.entries;
    let entry = entries
        .iter_mut()
        .find(|entry| entry.path == path)
        .expect("tree should contain sqlite database path");
    let Object::Blob(BlobObject::SqliteSnapshot(mut blob)) =
        repo.read_object(entry.oid.as_str()).unwrap()
    else {
        panic!("tree entry should point at a sqlite snapshot blob");
    };
    assert!(
        !blob.ranges[0].commits.is_empty(),
        "snapshot range should contain storage commit hashes"
    );
    assert_ne!(
        blob.ranges[0].commits[0].commit_hash,
        graft::core::commit_hash::CommitHash::ZERO
    );
    let missing_commit_hash = commit_hash.is_none();
    match commit_hash {
        Some(commit_hash) => blob.ranges[0].commits[0].commit_hash = commit_hash,
        None => {
            blob.ranges[0].commits.remove(0);
        }
    }

    let object_store = repo.object_store();
    let blob_object = Object::Blob(BlobObject::SqliteSnapshot(blob));
    entry.oid = if missing_commit_hash {
        write_raw_test_object_unchecked(&object_store, &blob_object)
    } else {
        object_store.write(&blob_object).unwrap()
    };
    commit.tree = object_store
        .write(&Object::Tree(TreeObject::new(entries).unwrap()))
        .unwrap();
    object_store
        .write(&Object::Commit(commit))
        .unwrap()
        .to_string()
}

fn write_raw_test_object_unchecked(
    object_store: &graft::repo::object::LooseObjectStore,
    object: &graft::repo::object::Object,
) -> graft::repo::object::ObjectId {
    let bytes = object.canonical_bytes();
    let oid = graft::repo::object::ObjectId::for_bytes(&bytes);
    let path = object_store.path_for(&oid);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
    oid
}

fn write_remote_object_pack_for_commit(
    remote_dir: &std::path::Path,
    repo: &graft::repo::Repository,
    commit_id: &str,
) {
    let mut objects = std::collections::BTreeMap::<graft::repo::object::ObjectId, Vec<u8>>::new();
    let id = graft::repo::object::ObjectId::new(commit_id.to_string()).unwrap();
    collect_object_graph_for_test_pack(repo, &id, &mut objects);

    let pack_name = format!("test-{}.pack", &commit_id[..12]);
    let idx_name = format!("test-{}.idx", &commit_id[..12]);
    let pack_path = format!("objects/pack/{pack_name}");
    let mut pack = b"graft-object-pack-v1\n".to_vec();
    let mut entries = Vec::new();
    for (id, bytes) in objects {
        let offset = pack.len() as u64;
        let len = bytes.len() as u64;
        pack.extend_from_slice(&bytes);
        entries.push(serde_json::json!({
            "id": id.to_string(),
            "offset": offset,
            "len": len,
        }));
    }

    let pack_dir = remote_dir.join("objects/pack");
    std::fs::create_dir_all(&pack_dir).unwrap();
    std::fs::write(pack_dir.join(pack_name), pack).unwrap();
    std::fs::write(
        pack_dir.join(idx_name),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "pack": pack_path,
            "objects": entries,
        }))
        .unwrap(),
    )
    .unwrap();
}

fn collect_object_graph_for_test_pack(
    repo: &graft::repo::Repository,
    id: &graft::repo::object::ObjectId,
    objects: &mut std::collections::BTreeMap<graft::repo::object::ObjectId, Vec<u8>>,
) {
    if objects.contains_key(id) {
        return;
    }
    let object = repo.read_object(id.as_str()).unwrap();
    let actual = object.id();
    assert_eq!(&actual, id);
    let bytes = object.canonical_bytes();
    match object {
        graft::repo::object::Object::Commit(commit) => {
            collect_object_graph_for_test_pack(repo, &commit.tree, objects);
        }
        graft::repo::object::Object::Tree(tree) => {
            for entry in tree.entries {
                collect_object_graph_for_test_pack(repo, &entry.oid, objects);
            }
        }
        graft::repo::object::Object::Blob(_) | graft::repo::object::Object::Tag(_) => {}
    }
    objects.insert(actual, bytes);
}

fn row_values_contain(values: &Value, expected: &str) -> bool {
    values.as_array().is_some_and(|values| {
        values
            .iter()
            .any(|value| value.as_str().is_some_and(|value| value == expected))
    })
}
