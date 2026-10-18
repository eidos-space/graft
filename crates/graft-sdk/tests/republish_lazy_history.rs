use graft_sdk::{RemoteConfigureOptions, RepositorySession};
use rusqlite::Connection;
use std::path::Path;

fn open(root: &Path) -> RepositorySession {
    std::fs::create_dir_all(root).unwrap();
    let session = RepositorySession::new(root);
    session.open().unwrap();
    session
}

fn remote(session: &RepositorySession, name: &str, directory: &Path) {
    std::fs::create_dir_all(directory).unwrap();
    session
        .configure_remote(&RemoteConfigureOptions {
            name: name.into(),
            url: format!("fs://{}", directory.display()),
            bearer_token: None,
            overwrite: false,
            upstream_branch: None,
        })
        .unwrap();
}

#[test]
fn cloned_history_can_be_published_to_a_new_remote_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let author_root = directory.path().join("author");
    let source_remote = directory.path().join("source");
    let destination = directory.path().join("destination");
    let clone_root = directory.path().join("clone");
    let phone_root = directory.path().join("phone");
    let author = open(&author_root);
    author.init().unwrap();
    let database = Connection::open(author_root.join("history.eidos")).unwrap();
    database
        .execute_batch("CREATE TABLE notes(body TEXT); INSERT INTO notes VALUES ('first');")
        .unwrap();
    author.add_all().unwrap();
    author.commit("first").unwrap();
    // Replace both SQLite pages so the old segment is absent from the current
    // snapshot, while the first commit still needs it when history is exported.
    database
        .execute_batch("PRAGMA user_version=2; UPDATE notes SET body='second';")
        .unwrap();
    drop(database);
    author.add_all().unwrap();
    author.commit("second").unwrap();
    remote(&author, "origin", &source_remote);
    author.push(Some("origin"), Some("main")).unwrap();
    author.close().unwrap();

    let clone = open(&clone_root);
    clone
        .clone_repository(
            &format!("fs://{}", source_remote.display()),
            Some("main"),
            None,
        )
        .unwrap();
    clone.close().unwrap();
    let clone = open(&clone_root);
    let before = clone.status().unwrap();
    remote(&clone, "device", &destination);
    clone.push(Some("device"), Some("main")).unwrap();
    assert_eq!(
        clone.status().unwrap()["current_head"],
        before["current_head"]
    );
    assert_eq!(clone.status().unwrap()["dirty"], false);

    let phone = open(&phone_root);
    phone
        .clone_repository(
            &format!("fs://{}", destination.display()),
            Some("main"),
            None,
        )
        .unwrap();
    assert_eq!(
        phone.history(50, None).unwrap()["commits"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let database = Connection::open(phone_root.join("history.eidos")).unwrap();
    let body: String = database
        .query_row("SELECT body FROM notes", [], |row| row.get(0))
        .unwrap();
    assert_eq!(body, "second");
    let integrity: String = database
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    phone.close().unwrap();
    clone.close().unwrap();
}
