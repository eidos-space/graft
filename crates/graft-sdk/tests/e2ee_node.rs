#![cfg(all(feature = "e2ee", unix))]

use graft_sdk::e2ee::{EncryptedConfig, RepositoryId, RootKey};
use std::{fs, path::Path, process::Command};
use zeroize::Zeroizing;

#[test]
#[ignore = "requires built native Node E2EE addon and TypeScript remote; run test:e2ee-node"]
fn native_node_secret_sessions_sync_sqlite_and_files() {
    let addon =
        std::env::var_os("GRAFT_E2EE_NODE_TEST_BINDING").expect("native addon path required");
    assert!(Path::new(&addon).is_file());
    let directory = tempfile::tempdir().unwrap();
    let repository =
        RepositoryId::from_bytes([0, 1, 2, 3, 4, 5, 0x46, 7, 0x88, 9, 10, 11, 12, 13, 14, 15])
            .unwrap();
    let root = RootKey::import(repository, Zeroizing::new([42; 32]));
    let config = EncryptedConfig::create(&root, b"node synthetic sync password").unwrap();
    let fixture = directory.path().join("fixture.json");
    fs::write(&fixture, serde_json::to_vec(&serde_json::json!({
        "encryption": { "profile":"encrypted-remote-v1", "version":1, "suite":1, "uuid":"00010203-0405-4607-8809-0a0b0c0d0e0f" },
        "objects": [["e2ee/config", config.as_bytes()]],
        "failIntegrationHeadCreates": true,
        "dropIntegrationConfigMutationAcknowledgements": true,
        "dropInitialConfigAcknowledgements": true,
        "failInitialHeadCreates": true,
        "serviceLifetimeMs": 240000,
    })).unwrap()).unwrap();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let status = Command::new("node")
        .arg(workspace.join("tests/e2ee/node-session.mjs"))
        .arg(&fixture)
        .arg(directory.path())
        .env("GRAFT_SDK_NATIVE_PATH", addon)
        .current_dir(&workspace)
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
#[ignore = "requires built native Node E2EE addon and TypeScript remote; run test:e2ee-node"]
fn native_node_bounded_privacy_evidence_covers_remote_bytes_and_diagnostics() {
    let addon =
        std::env::var_os("GRAFT_E2EE_NODE_TEST_BINDING").expect("native addon path required");
    assert!(Path::new(&addon).is_file());
    let directory = tempfile::tempdir().unwrap();
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let password = format!("privacy-password-{nonce}");
    let repository =
        RepositoryId::from_bytes([0, 1, 2, 3, 4, 5, 0x46, 7, 0x88, 9, 10, 11, 12, 13, 14, 15])
            .unwrap();
    let root = RootKey::import(repository, Zeroizing::new([42; 32]));
    let config = EncryptedConfig::create(&root, password.as_bytes()).unwrap();
    let fixture = directory.path().join("fixture.json");
    fs::write(&fixture, serde_json::to_vec(&serde_json::json!({
        "encryption": { "profile":"encrypted-remote-v1", "version":1, "suite":1, "uuid":"00010203-0405-4607-8809-0a0b0c0d0e0f" },
        "objects": [["e2ee/config", config.as_bytes()]],
        "privacyEvidence": true,
        "privacyCase": { "nonce": nonce, "password": password },
        "enableMultipart": true, "maxRequestBytes": 32768, "multipartPartBytes": 16384,
        "serviceLifetimeMs": 120000,
    })).unwrap()).unwrap();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let status = Command::new("node")
        .arg(workspace.join("tests/e2ee/privacy-runner.mjs"))
        .arg(&fixture)
        .arg(directory.path())
        .env("GRAFT_SDK_NATIVE_PATH", addon)
        .current_dir(workspace)
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
#[ignore = "requires built native Node E2EE addon and TypeScript remote; run test:e2ee-node"]
fn native_node_migration_reopens_and_cuts_over_with_real_writer_fence() {
    use graft::repo::Repository;
    use graft_sdk::RepositorySession;
    let addon =
        std::env::var_os("GRAFT_E2EE_NODE_TEST_BINDING").expect("native addon path required");
    let directory = tempfile::tempdir().unwrap();
    let worktree = directory.path().join("source");
    let source = RepositorySession::new(&worktree);
    source.open().unwrap();
    source.init().unwrap();
    let db = rusqlite::Connection::open(worktree.join("state.db")).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL;CREATE TABLE data(value TEXT);INSERT INTO data VALUES('Node migration first WAL row');").unwrap();
    fs::write(
        worktree.join("attachment.txt"),
        "Node migration private file",
    )
    .unwrap();
    let mut seed = 17_u64;
    let payload: Vec<u8> = (0..5 * 1024 * 1024)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as u8
        })
        .collect();
    fs::write(worktree.join("large-private.bin"), payload).unwrap();
    source.add_all().unwrap();
    source.commit("first private migration commit").unwrap();
    let first = source.repository_metadata().unwrap().current_head.unwrap();
    db.execute("INSERT INTO data VALUES('second WAL row')", [])
        .unwrap();
    source.add_all().unwrap();
    source.commit("second private migration commit").unwrap();
    source.close().unwrap();
    let repository = Repository::discover(&worktree).unwrap();
    repository.branch_create("older", Some(&first)).unwrap();
    repository.branch_create_unborn("empty").unwrap();
    repository
        .tag_create_annotated("release", Some(&first), "private tag")
        .unwrap();
    let fixture = directory.path().join("fixture.json");
    let encryption = serde_json::json!({"profile":"encrypted-remote-v1","version":1,"suite":1,"uuid":"40010203-0405-4607-8809-0a0b0c0d0e0f"});
    fs::write(
        &fixture,
        serde_json::to_vec(&serde_json::json!({
            "encryption":encryption,"migrationEncryption":encryption,"objects":[],
            "privacyObjectIds":[first],
            "dropMigrationConfigAcknowledgements":true,
            "enableMultipart":true,"maxRequestBytes":1024*1024,"multipartPartBytes":512*1024,
            "multipartFault":"part-and-complete-loss",
        "serviceLifetimeMs":240000,
        }))
        .unwrap(),
    )
    .unwrap();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let status = Command::new("node")
        .arg(workspace.join("tests/e2ee/node-migration.mjs"))
        .arg(&fixture)
        .arg(directory.path())
        .env("GRAFT_SDK_NATIVE_PATH", addon)
        .current_dir(workspace)
        .status()
        .unwrap();
    assert!(status.success());
}
