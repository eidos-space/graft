#![cfg(all(feature = "e2ee", unix))]
use graft::{remote::RemoteConfig, repo::Repository};
use graft_sdk::RepositorySession;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Output, Stdio},
    sync::Arc,
};

struct Service(Child);
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
const UUID: &str = "40010203-0405-4607-8809-0a0b0c0d0e0f";
const PASSWORD: &[u8] = "CLI migration 密码 raw \r\n".as_bytes();

fn spawn(source: &Path, state: &Path, url: &str, enroll: bool, arguments: &[&str]) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graft"));
    command
        .args(["e2ee", "--worktree"])
        .arg(source)
        .arg("--state-dir")
        .arg(state)
        .args([
            "--url",
            url,
            "--uuid",
            UUID,
            "--account",
            "test-account",
            "--token-env",
            "GRAFT_E2EE_TEST_TOKEN",
            "--password-stdin",
        ])
        .env("GRAFT_E2EE_TEST_TOKEN", "account-token");
    if enroll {
        command.arg("--enroll");
    }
    command
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(PASSWORD).unwrap();
    child
}
fn launch(source: &Path, state: &Path, url: &str, enroll: bool, arguments: &[&str]) -> Output {
    spawn(source, state, url, enroll, arguments)
        .wait_with_output()
        .unwrap()
}
fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
#[ignore = "requires built TypeScript remote service and loopback networking"]
fn encrypted_cli_migration_prepares_offline_resumes_exact_journals_and_independently_verifies_all_refs()
 {
    let fixture = tempfile::tempdir().unwrap();
    let input = fixture.path().join("fixture.json");
    let captures = fixture.path().join("captures.json");
    fs::write(&input, serde_json::to_vec(&json!({
        "encryption":{"profile":"encrypted-remote-v1","version":1,"suite":1,"uuid":UUID},
        "migrationEncryption":{"profile":"encrypted-remote-v1","version":1,"suite":1,"uuid":UUID},
        "objects":[],"dropMigrationConfigAcknowledgements":true,
    })).unwrap()).unwrap();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut server = Service(
        Command::new("node")
            .arg(workspace.join("tests/e2ee/remote-service.mjs"))
            .arg(input)
            .arg(&captures)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut ready = String::new();
    BufReader::new(server.0.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    let ready: Value = serde_json::from_str(&ready).unwrap();
    let url = ready["migrationUrl"].as_str().unwrap();
    let worktree = fixture.path().join("source");
    let source = RepositorySession::new(&worktree);
    source.open().unwrap();
    source.init().unwrap();
    let db = Connection::open(worktree.join("migration.db")).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL;PRAGMA wal_autocheckpoint=0;CREATE TABLE data(value TEXT);INSERT INTO data VALUES('CLI migration private WAL row');").unwrap();
    fs::write(
        worktree.join("private.bin"),
        b"CLI migration private attachment",
    )
    .unwrap();
    fs::write(
        worktree.join("large-private.bin"),
        vec![57_u8; 5 * 1024 * 1024],
    )
    .unwrap();
    source.add_all().unwrap();
    source.commit("CLI migration private commit").unwrap();
    let head = source.repository_metadata().unwrap().current_head.unwrap();
    source.close().unwrap();
    let repository = Repository::discover(&worktree).unwrap();
    repository.branch_create("side", Some(&head)).unwrap();
    repository
        .tag_create_annotated("release", Some(&head), "CLI migration private tag")
        .unwrap();
    repository
        .remote_add("origin", RemoteConfig::Memory)
        .unwrap();
    repository
        .set_remote_tracking_ref("origin", "tracked", &head)
        .unwrap();
    let original_config = repository.config().unwrap().remotes;
    let state = fixture.path().join("host-state");
    let invalid = launch(
        &worktree,
        &state,
        url,
        true,
        &[
            "prepare-migration",
            "--all-refs",
            "--map",
            "refs/heads/main=refs/heads/new-main",
            "--target-head",
            "refs/heads/new-main",
        ],
    );
    assert!(!invalid.status.success());
    assert!(!state.exists());
    assert!(!captures.exists());
    let prepared = success(launch(
        &worktree,
        &state,
        url,
        true,
        &[
            "prepare-migration",
            "--all-refs",
            "--map",
            "refs/heads/main=refs/heads/new-main",
            "--map",
            "refs/heads/side=refs/heads/new-side",
            "--map",
            "refs/tags/release=refs/tags/new-release",
            "--map",
            "refs/remotes/origin/tracked=refs/heads/tracked-history",
            "--target-head",
            "refs/heads/new-main",
        ],
    ));
    assert_eq!(prepared["status"], "prepared");
    assert_eq!(prepared["host_cutover"], "pending-trusted-writer-fence");
    assert!(!captures.exists());
    let initial = fs::read(state.join("migration-initialization/initialization")).unwrap();
    let lost = launch(&worktree, &state, url, false, &["resume-migration"]);
    assert!(!lost.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&lost.stdout).unwrap()["error"]["code"],
        "GRAFT_SDK_REMOTE_PUBLICATION_OUTCOME_UNKNOWN"
    );
    assert_eq!(
        fs::read(state.join("migration-initialization/initialization")).unwrap(),
        initial
    );
    let resumed = success(launch(&worktree, &state, url, false, &["resume-migration"]));
    assert_eq!(resumed["status"], "ready-for-independent-verification");
    assert_eq!(resumed["offline_devices"], "pending");
    let verification = fixture.path().join("verification");
    let verified = success(launch(
        &worktree,
        &state,
        url,
        false,
        &[
            "verify-migration",
            "--verification-directory",
            verification.to_str().unwrap(),
        ],
    ));
    assert_eq!(verified["status"], "independently-verified");
    assert_eq!(verified["host_cutover"], "pending-trusted-writer-fence");
    assert_eq!(verified["references"], 4);
    assert!(verified["dependencies"]["storage_pages"].as_u64().unwrap() > 0);
    assert!(verified["dependencies"]["payloads"].as_u64().unwrap() > 0);
    assert_eq!(
        Repository::discover(&worktree)
            .unwrap()
            .config()
            .unwrap()
            .remotes,
        original_config
    );
    for name in ["cutover-verification", "replayed-cutover-verification"] {
        let cutover_directory = fixture.path().join(name);
        let cutover = success(launch(
            &worktree,
            &state,
            url,
            false,
            &[
                "cutover-migration",
                "--verification-directory",
                cutover_directory.to_str().unwrap(),
            ],
        ));
        assert_eq!(cutover["status"], "host-cutover-confirmed");
        assert_eq!(cutover["host_cutover"], "confirmed");
        assert_eq!(cutover["offline_devices"], "pending");
        assert_eq!(cutover["references"], 4);
        let config = Repository::discover(&worktree).unwrap().config().unwrap();
        assert_eq!(
            config.remotes["origin"],
            RemoteConfig::EncryptedHttp {
                url: url.to_owned(),
                uuid: UUID.to_owned()
            }
        );
        assert_eq!(
            fs::read(worktree.join("private.bin")).unwrap(),
            b"CLI migration private attachment"
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM data", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
    let before = fs::read(&captures).unwrap();
    let writer = RepositorySession::new(&worktree);
    writer.open().unwrap();
    db.execute(
        "INSERT INTO data VALUES ('changed source after migration')",
        [],
    )
    .unwrap();
    writer.add_all().unwrap();
    writer.commit("changed source after migration").unwrap();
    writer.close().unwrap();
    let changed_verification = fixture.path().join("changed-verification");
    assert!(
        !launch(
            &worktree,
            &state,
            url,
            false,
            &[
                "verify-migration",
                "--verification-directory",
                changed_verification.to_str().unwrap()
            ]
        )
        .status
        .success()
    );
    assert!(!changed_verification.exists());
    assert_eq!(fs::read(&captures).unwrap(), before);
    fs::rename(
        state.join("migration-progress"),
        state.join("retained-progress"),
    )
    .unwrap();
    assert!(
        !launch(&worktree, &state, url, false, &["resume-migration"])
            .status
            .success()
    );
    assert!(!state.join("migration-progress").exists());
    assert_eq!(fs::read(&captures).unwrap(), before);
    let text = String::from_utf8(before).unwrap();
    for private in [
        "CLI migration private WAL row",
        "CLI migration private attachment",
        "CLI migration private tag",
        "new-main",
        "new-release",
    ] {
        assert!(!text.contains(private));
    }
}

#[test]
#[ignore = "requires built TypeScript remote service and loopback networking"]
fn encrypted_cli_multipart_restarts_retained_ciphertext_after_lost_abort_without_plaintext() {
    use graft::e2ee::{
        ChunkSize, EncryptedConfig, EncryptedHttpClient, FileCheckpointStore,
        FileMultipartJournalProvider, FileRefMutationJournalProvider, ObjectPublicationOutcome,
        ObjectSnapshot, PreparedEncryptedObject, RepositoryId, RootKey, SecretSession,
        SessionBinding,
    };
    use graft::remote::RemoteCredentials;
    use sha2::{Digest, Sha256};
    let fixture = tempfile::tempdir().unwrap();
    let identity = RepositoryId::from_bytes([
        0x40, 1, 2, 3, 4, 5, 0x46, 7, 0x88, 9, 10, 11, 12, 13, 14, 15,
    ])
    .unwrap();
    let root = RootKey::import(identity, zeroize::Zeroizing::new([42; 32]));
    let config = EncryptedConfig::create(&root, PASSWORD).unwrap();
    let input = fixture.path().join("multipart.json");
    let captures = fixture.path().join("multipart-captures.json");
    fs::write(&input, serde_json::to_vec(&json!({
        "encryption":{"profile":"encrypted-remote-v1","version":1,"suite":1,"uuid":UUID},
        "objects":[["e2ee/config",config.as_bytes()]],"enableMultipart":true,
        "maxRequestBytes":65536,"multipartPartBytes":16384,"multipartFault":"part-and-complete-loss",
        "dropMultipartAbortAcknowledgement":true,
    })).unwrap()).unwrap();
    let mut server = Service(
        Command::new("node")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/e2ee/remote-service.mjs"))
            .arg(input)
            .arg(&captures)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut ready = String::new();
    BufReader::new(server.0.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    let ready: Value = serde_json::from_str(&ready).unwrap();
    let url = ready["url"].as_str().unwrap();
    let worktree = fixture.path().join("worktree");
    let repository = RepositorySession::new(&worktree);
    repository.open().unwrap();
    repository.init().unwrap();
    repository.close().unwrap();
    let state = fixture.path().join("state");
    for path in [
        state.clone(),
        state.join("checkpoint"),
        state.join("ref-recovery"),
        state.join("multipart-recovery"),
        state.join("staging"),
    ] {
        fs::create_dir(&path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let binding =
        SessionBinding::new(format!("graft-cli:{url}"), "test-account".into(), identity).unwrap();
    let checkpoint = FileCheckpointStore::create(state.join("checkpoint"), &binding).unwrap();
    FileRefMutationJournalProvider::create(state.join("ref-recovery"), &binding).unwrap();
    let provider = Arc::new(
        FileMultipartJournalProvider::create(state.join("multipart-recovery"), &binding).unwrap(),
    );
    let secret = SecretSession::with_checkpoint_store(binding, Arc::new(checkpoint)).unwrap();
    secret.unlock_with_root(&config, root).unwrap();
    let credentials = RemoteCredentials::explicit();
    credentials
        .set_http_bearer_token("origin", "account-token".into())
        .unwrap();
    let client =
        EncryptedHttpClient::new(url, "origin".into(), credentials, secret.clone()).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (key, digest) = runtime.block_on(async {
        let bytes = vec![29; 131_079];
        let candidate = PreparedEncryptedObject::prepare(
            &secret.begin_operation().unwrap(),
            ObjectSnapshot {
                logical_key: "store/files/retained-cli-only".into(),
                plaintext_length: bytes.len() as u64,
                plaintext_sha256: Sha256::digest(&bytes).into(),
                chunk_size: ChunkSize::KiB16,
            },
            &mut bytes.as_slice(),
            &state.join("staging"),
            1024 * 1024,
        )
        .await
        .unwrap();
        let outcome = client
            .publish_object_multipart(&candidate, provider.clone(), &state.join("staging"))
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            ObjectPublicationOutcome::OutcomeUnknown { .. }
        ));
        (
            candidate.remote_key().to_owned(),
            *candidate.ciphertext_sha256(),
        )
    });
    runtime.block_on(secret.disconnect());
    drop(client);
    drop(provider);
    let listing = success(launch(
        &worktree,
        &state,
        url,
        false,
        &["multipart-uploads"],
    ));
    assert_eq!(listing["uploads"][0]["key"], key);
    assert_eq!(listing["uploads"][0]["server_session_recorded"], true);
    let allocations = fs::read(state.join("multipart-recovery/ref-allocations")).unwrap();
    let missing = format!("e2ee/files/{}", "f".repeat(64));
    assert!(
        !launch(
            &worktree,
            &state,
            url,
            false,
            &["resume-multipart", "--key", &missing]
        )
        .status
        .success()
    );
    assert_eq!(
        fs::read(state.join("multipart-recovery/ref-allocations")).unwrap(),
        allocations
    );
    let lost = launch(
        &worktree,
        &state,
        url,
        false,
        &["restart-multipart", "--key", &key],
    );
    assert!(!lost.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&lost.stdout).unwrap()["error"]["code"],
        "GRAFT_SDK_REMOTE_PUBLICATION_OUTCOME_UNKNOWN"
    );
    let recovered = success(launch(
        &worktree,
        &state,
        url,
        false,
        &["restart-multipart", "--key", &key],
    ));
    assert_eq!(recovered["status"], "confirmed");
    assert_eq!(
        success(launch(
            &worktree,
            &state,
            url,
            false,
            &["multipart-uploads"]
        ))["uploads"],
        json!([])
    );
    let capture: Vec<Value> = serde_json::from_slice(&fs::read(captures).unwrap()).unwrap();
    let starts: Vec<_> = capture
        .iter()
        .filter(|item| item["path"].as_str().unwrap().contains("/multipart-start/"))
        .collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(
        starts[0]["encryptionUploadId"],
        starts[1]["encryptionUploadId"]
    );
    assert_eq!(starts[0]["ciphertextSha256"], starts[1]["ciphertextSha256"]);
    assert_eq!(
        starts[1]["ciphertextSha256"],
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    assert_eq!(
        capture
            .iter()
            .filter(|item| item["droppedMultipartAbortResponse"] == true)
            .count(),
        1
    );
    assert_eq!(
        capture
            .iter()
            .filter(|item| item["partNumber"] == "1")
            .count(),
        2
    );
}

#[test]
#[ignore = "requires built TypeScript remote service and loopback networking"]
fn encrypted_cli_migration_recovers_hard_kills_at_publication_and_verification_boundaries() {
    use graft::e2ee::{
        FileInitializationJournal, InitializationJournal, RepositoryId, SessionBinding,
    };
    let fixture = tempfile::tempdir().unwrap();
    let control = fixture.path().join("hold-control");
    fs::write(&control, "").unwrap();
    let input = fixture.path().join("kill-fixture.json");
    let captures = fixture.path().join("kill-captures.json");
    fs::write(&input, serde_json::to_vec(&json!({
        "encryption":{"profile":"encrypted-remote-v1","version":1,"suite":1,"uuid":UUID},
        "migrationEncryption":{"profile":"encrypted-remote-v1","version":1,"suite":1,"uuid":UUID},
        "objects":[],"migrationHoldControl":control,
    })).unwrap()).unwrap();
    let mut server = Service(
        Command::new("node")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/e2ee/remote-service.mjs"))
            .arg(input)
            .arg(&captures)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let reader = BufReader::new(server.0.stdout.take().unwrap());
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if send.send(line).is_err() {
                break;
            }
        }
    });
    let event = || {
        serde_json::from_str::<Value>(
            &receive
                .recv_timeout(std::time::Duration::from_secs(45))
                .unwrap(),
        )
        .unwrap()
    };
    let ready = event();
    let url = ready["migrationUrl"].as_str().unwrap();
    let worktree = fixture.path().join("source");
    let source = RepositorySession::new(&worktree);
    source.open().unwrap();
    source.init().unwrap();
    let db = Connection::open(worktree.join("kill.db")).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL;PRAGMA wal_autocheckpoint=0;CREATE TABLE data(value TEXT);INSERT INTO data VALUES('migration hard kill private row');").unwrap();
    fs::write(
        worktree.join("private.bin"),
        b"migration hard kill attachment",
    )
    .unwrap();
    source.add_all().unwrap();
    source
        .commit("migration hard kill committed source")
        .unwrap();
    let head = source.repository_metadata().unwrap().current_head.unwrap();
    source.close().unwrap();
    let repo = Repository::discover(&worktree).unwrap();
    repo.remote_add("origin", RemoteConfig::Memory).unwrap();
    let state = fixture.path().join("state");
    success(launch(
        &worktree,
        &state,
        url,
        true,
        &[
            "prepare-migration",
            "--all-refs",
            "--map",
            "refs/heads/main=refs/heads/moved",
            "--target-head",
            "refs/heads/moved",
        ],
    ));
    let binding = SessionBinding::new(
        format!("graft-cli:{url}"),
        "test-account".into(),
        RepositoryId::from_bytes([
            0x40, 1, 2, 3, 4, 5, 0x46, 7, 0x88, 9, 10, 11, 12, 13, 14, 15,
        ])
        .unwrap(),
    )
    .unwrap();
    let initialization =
        FileInitializationJournal::open(state.join("migration-initialization"), &binding).unwrap();
    let root_config = initialization
        .load(&binding)
        .unwrap()
        .config
        .as_bytes()
        .to_vec();
    for boundary in [
        "config-before",
        "config-after",
        "dependency-after",
        "ref-after",
        "head-after",
    ] {
        fs::write(&control, boundary).unwrap();
        let mut interrupted = Service(spawn(&worktree, &state, url, false, &["resume-migration"]));
        assert_eq!(event()["migration_held"], boundary);
        interrupted.0.kill().unwrap();
        assert!(!interrupted.0.wait().unwrap().success());
        assert_eq!(
            initialization.load(&binding).unwrap().config.as_bytes(),
            root_config
        );
        assert_eq!(
            repo.config().unwrap().remotes["origin"],
            RemoteConfig::Memory
        );
        assert_eq!(
            repo.branches()
                .unwrap()
                .into_iter()
                .find(|branch| branch.current)
                .unwrap()
                .target
                .as_deref(),
            Some(head.as_str())
        );
    }
    fs::write(&control, "").unwrap();
    assert_eq!(
        success(launch(&worktree, &state, url, false, &["resume-migration"]))["status"],
        "ready-for-independent-verification"
    );
    fs::write(&control, "verification-read").unwrap();
    let interrupted_directory = fixture.path().join("interrupted-verification");
    let mut interrupted = Service(spawn(
        &worktree,
        &state,
        url,
        false,
        &[
            "verify-migration",
            "--verification-directory",
            interrupted_directory.to_str().unwrap(),
        ],
    ));
    assert_eq!(event()["migration_held"], "verification-read");
    interrupted.0.kill().unwrap();
    assert!(!interrupted.0.wait().unwrap().success());
    assert_eq!(
        repo.config().unwrap().remotes["origin"],
        RemoteConfig::Memory
    );
    fs::write(&control, "").unwrap();
    let verified_directory = fixture.path().join("cutover-verification");
    let switched = success(launch(
        &worktree,
        &state,
        url,
        false,
        &[
            "cutover-migration",
            "--verification-directory",
            verified_directory.to_str().unwrap(),
        ],
    ));
    assert_eq!(switched["host_cutover"], "confirmed");
    assert_eq!(switched["offline_devices"], "pending");
    assert_eq!(
        repo.config().unwrap().branches["main"].merge.as_deref(),
        Some("refs/heads/moved")
    );
    assert_eq!(
        repo.branches()
            .unwrap()
            .into_iter()
            .find(|branch| branch.current)
            .unwrap()
            .target
            .as_deref(),
        Some(head.as_str())
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM data", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let capture: Vec<Value> = serde_json::from_slice(&fs::read(captures).unwrap()).unwrap();
    for boundary in [
        "config-before",
        "config-after",
        "dependency-after",
        "ref-after",
        "head-after",
        "verification-read",
    ] {
        assert_eq!(
            capture
                .iter()
                .filter(|item| item["heldMigration"] == boundary)
                .count(),
            1
        );
    }
    assert_eq!(
        initialization.load(&binding).unwrap().config.as_bytes(),
        root_config
    );
}
