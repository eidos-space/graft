use std::{path::Path, process::Command};

use rusqlite::Connection;

fn create_tasks_database(path: &Path, status: &str) {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE tasks(id INTEGER PRIMARY KEY, title TEXT NOT NULL, status TEXT NOT NULL);",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO tasks(title, status) VALUES ('ship direct diff', ?1)",
            [status],
        )
        .unwrap();
}

#[test]
fn no_index_diff_compares_two_sqlite_files_without_a_repository() {
    let temp_dir = tempfile::tempdir().unwrap();
    let before = temp_dir.path().join("before.sqlite");
    let after = temp_dir.path().join("after.sqlite");
    create_tasks_database(&before, "open");
    create_tasks_database(&after, "open");

    let writer = Connection::open(&after).unwrap();
    writer.pragma_update(None, "journal_mode", "WAL").unwrap();
    writer.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    writer
        .execute("UPDATE tasks SET status = 'done' WHERE id = 1", [])
        .unwrap();
    writer
        .execute(
            "INSERT INTO tasks(title, status) VALUES ('document it', 'open')",
            [],
        )
        .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_graft"))
        .current_dir(temp_dir.path())
        .args([
            "diff",
            "--no-index",
            "--rows",
            "--json",
            before.to_str().unwrap(),
            after.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["from"]["path"], before.to_str().unwrap());
    assert_eq!(json["to"]["path"], after.to_str().unwrap());
    assert_eq!(json["changed"], true);
    assert_eq!(json["kind"], "sqlite_database");
    assert_eq!(json["rows"], true);
    assert_eq!(json["row_diff"]["logical_status"], "logical_changes");
    let tasks = json["row_diff"]["tables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|table| table["name"] == "tasks")
        .unwrap();
    assert_eq!(tasks["changes"].as_array().unwrap().len(), 2);
    assert!(
        tasks["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change["op"] == "update")
    );
    assert!(
        tasks["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change["op"] == "insert")
    );
    assert!(!temp_dir.path().join(".graft").exists());

    let unchanged = Command::new(env!("CARGO_BIN_EXE_graft"))
        .current_dir(temp_dir.path())
        .args([
            "diff",
            "--no-index",
            before.to_str().unwrap(),
            before.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(unchanged.status.success());
    assert!(String::from_utf8_lossy(&unchanged.stdout).contains("No changes."));
}

#[test]
fn no_index_diff_rejects_non_sqlite_inputs() {
    let temp_dir = tempfile::tempdir().unwrap();
    let text = temp_dir.path().join("note.txt");
    let database = temp_dir.path().join("data.sqlite");
    std::fs::write(&text, "not sqlite").unwrap();
    create_tasks_database(&database, "open");

    let output = Command::new(env!("CARGO_BIN_EXE_graft"))
        .current_dir(temp_dir.path())
        .args([
            "diff",
            "--no-index",
            text.to_str().unwrap(),
            database.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not a SQLite database"));
}
