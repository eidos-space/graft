#![cfg(all(feature = "e2ee", unix))]

use graft::{
    e2ee::{
        ConfigMutationJournal, EncryptedConfig, FileCheckpointStore, FileConfigMutationJournal,
        RepositoryId, RootKey, SecretSession, SessionBinding,
    },
    remote::{EncryptedTransferBudget, RemoteConfig, RemoteCredentials},
};
use graft_sdk::RepositorySession;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Output, Stdio},
    sync::Arc,
};
use zeroize::Zeroizing;

struct Service(Child);
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn bounded_output(child: &mut Child) -> Output {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "synthetic CLI process timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .unwrap();
    Output { status, stdout, stderr }
}

#[test]
#[ignore = "requires built TypeScript remote service and loopback networking"]
fn encrypted_cli_initialization_recovers_actual_process_termination_at_publication_boundaries() {
    use graft::e2ee::{FileInitializationJournal, InitializationJournal, InitializationStage};
    const UUID: &str = "30010203-0405-4607-8809-0a0b0c0d0e0f";
    const PASSWORD: &[u8] = "initialization process termination 密码 \r\n".as_bytes();
    for boundary in ["config-before", "config-after", "head-after"] {
        let fixture = tempfile::tempdir().unwrap();
        let input = fixture.path().join("termination-fixture.json");
        let captures = fixture.path().join("termination-captures.json");
        fs::write(
            &input,
            serde_json::to_vec(&json!({
                "encryption":{"profile":"encrypted-remote-v1","version":1,"suite":1,"uuid":UUID},
                "objects":[], "holdInitialization":boundary,
            }))
            .unwrap(),
        )
        .unwrap();
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut service = Service(
            Command::new("node")
                .arg(workspace.join("tests/e2ee/remote-service.mjs"))
                .arg(input)
                .arg(&captures)
                .current_dir(workspace)
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let reader = BufReader::new(service.0.stdout.take().unwrap());
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        let read_event = || {
            serde_json::from_str::<Value>(
                &receive
                    .recv_timeout(std::time::Duration::from_secs(30))
                    .unwrap(),
            )
            .unwrap()
        };
        let ready = read_event();
        let url = ready["enrollmentUrl"].as_str().unwrap();
        let source = fixture.path().join("source");
        let writer = RepositorySession::new(&source);
        writer.open().unwrap();
        writer.init().unwrap();
        let db = Connection::open(source.join("termination.db")).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL;PRAGMA wal_autocheckpoint=0;CREATE TABLE data(value TEXT);INSERT INTO data VALUES('initialization termination WAL marker');").unwrap();
        fs::write(
            source.join("termination.bin"),
            b"initialization termination attachment marker",
        )
        .unwrap();
        writer.add_all().unwrap();
        writer
            .commit("initialization termination commit marker")
            .unwrap();
        let expected = writer.repository_metadata().unwrap().current_head.unwrap();
        writer.close().unwrap();
        let state = fixture.path().join("state");
        let launch = |action: &str, enroll: bool, destination: &Path| {
            let mut command = Command::new(env!("CARGO_BIN_EXE_graft"));
            command
                .arg("e2ee")
                .arg("--worktree")
                .arg(&source)
                .arg("--state-dir")
                .arg(&state)
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
                ]);
            if enroll {
                command.arg("--enroll");
            }
            command
                .arg(action)
                .arg("--verification-directory")
                .arg(destination)
                .env("GRAFT_E2EE_TEST_TOKEN", "account-token")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = Service(command.spawn().unwrap());
            child.0.stdin.take().unwrap().write_all(PASSWORD).unwrap();
            child
        };
        let destination = fixture.path().join("interrupted-verification");
        let mut killed = launch("initialize", true, &destination);
        assert_eq!(read_event()["initialization_held"], boundary);
        let binding = SessionBinding::new(
            format!("graft-cli:{url}"),
            "test-account".into(),
            RepositoryId::from_bytes([48, 1, 2, 3, 4, 5, 0x46, 7, 0x88, 9, 10, 11, 12, 13, 14, 15])
                .unwrap(),
        )
        .unwrap();
        let journal =
            FileInitializationJournal::open(state.join("initialization"), &binding).unwrap();
        let retained = journal.load(&binding).unwrap();
        assert_eq!(
            retained.stage,
            if boundary == "head-after" {
                InitializationStage::ConfigPublished
            } else {
                InitializationStage::Prepared
            }
        );
        assert_eq!(retained.intent.unwrap().commit(), expected);
        let exact_config = retained.config.as_bytes().to_vec();
        killed.0.kill().unwrap();
        assert!(!killed.0.wait().unwrap().success());
        assert_eq!(
            journal.load(&binding).unwrap().config.as_bytes(),
            exact_config
        );
        assert!(!destination.exists());
        let requests: Vec<Value> = serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
        let held: Vec<_> = requests
            .iter()
            .filter(|request| request["heldInitialization"] == boundary)
            .collect();
        assert_eq!(held.len(), 1);
        if boundary == "config-before" {
            assert!(held[0].get("responseStatus").is_none());
        } else {
            assert_eq!(held[0]["responseStatus"], 204);
        }
        let verified = fixture.path().join("resumed-verification");
        let mut resumed = launch("resume-initialization", false, &verified);
        let output = bounded_output(&mut resumed.0);
        assert!(
            output.status.success(),
            "{boundary}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap()["status"],
            "connected"
        );
        let completed = journal.load(&binding).unwrap();
        assert_eq!(completed.stage, InitializationStage::Connected);
        assert_eq!(completed.config.as_bytes(), exact_config);
        assert_eq!(completed.intent.unwrap().commit(), expected);
        assert_eq!(
            fs::read(verified.join("termination.bin")).unwrap(),
            b"initialization termination attachment marker"
        );
        assert_eq!(
            Connection::open(verified.join("termination.db"))
                .unwrap()
                .query_row("SELECT value FROM data", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "initialization termination WAL marker"
        );
        let requests: Vec<Value> = serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
        let creates: Vec<_> = requests
            .iter()
            .filter(|request| {
                request["method"] == "PUT"
                    && request["path"] == "/acme/enrollment/raw-if-not-exists/e2ee/config"
            })
            .collect();
        assert_eq!(
            creates.len(),
            if boundary == "config-before" { 2 } else { 1 }
        );
        let expected_hex: String = exact_config
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert!(
            creates
                .iter()
                .all(|request| request["body"] == expected_hex)
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "POST"
                    && request["path"]
                        .as_str()
                        .is_some_and(|path| path.starts_with("/acme/enrollment/cas/e2ee/head/")))
                .count(),
            1
        );
        let captured = fs::read(&captures).unwrap();
        for marker in [
            PASSWORD,
            b"initialization termination WAL marker",
            b"initialization termination attachment marker",
            b"initialization termination commit marker",
            expected.as_bytes(),
        ] {
            assert!(!captured.windows(marker.len()).any(|bytes| bytes == marker));
            let hex: String = marker.iter().map(|byte| format!("{byte:02x}")).collect();
            assert!(
                !captured
                    .windows(hex.len())
                    .any(|bytes| bytes == hex.as_bytes())
            );
        }
    }
}

#[test]
#[ignore = "requires built TypeScript remote service and loopback networking"]
fn encrypted_cli_initialization_race_publishes_only_the_winning_root_and_source() {
    use graft::e2ee::{FileInitializationJournal, InitializationJournal, InitializationStage};
    const UUID: &str = "20010203-0405-4607-8809-0a0b0c0d0e0f";
    let passwords = [
        b"first race synthetic password".as_slice(),
        b"second race synthetic password".as_slice(),
    ];
    let fixture = tempfile::tempdir().unwrap();
    let input = fixture.path().join("race-fixture.json");
    let captures = fixture.path().join("race-captures.json");
    fs::write(
        &input,
        serde_json::to_vec(&json!({
            "encryption":{"profile":"encrypted-remote-v1","version":1,"suite":1,"uuid":UUID},
            "objects":[], "gateInitialConfigCreates":true,
        }))
        .unwrap(),
    )
    .unwrap();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut service = Service(
        Command::new("node")
            .arg(workspace.join("tests/e2ee/remote-service.mjs"))
            .arg(input)
            .arg(&captures)
            .current_dir(&workspace)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut ready = String::new();
    BufReader::new(service.0.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    let ready: Value = serde_json::from_str(&ready).unwrap();
    let url = ready["enrollmentUrl"].as_str().unwrap();
    let sources = [
        fixture.path().join("first-source"),
        fixture.path().join("second-source"),
    ];
    let states = [
        fixture.path().join("first-state"),
        fixture.path().join("second-state"),
    ];
    let verified = [
        fixture.path().join("first-verification"),
        fixture.path().join("second-verification"),
    ];
    let mut heads = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        let writer = RepositorySession::new(source);
        writer.open().unwrap();
        writer.init().unwrap();
        let db = Connection::open(source.join("race.db")).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode=WAL;PRAGMA wal_autocheckpoint=0;CREATE TABLE data(value INTEGER);",
        )
        .unwrap();
        db.execute("INSERT INTO data VALUES(?)", [index as i64])
            .unwrap();
        fs::write(
            source.join("race.bin"),
            format!("race source synthetic attachment {index}"),
        )
        .unwrap();
        writer.add_all().unwrap();
        writer
            .commit(&format!("race source synthetic commit {index}"))
            .unwrap();
        heads.push(writer.repository_metadata().unwrap().current_head.unwrap());
        writer.close().unwrap();
    }
    assert_ne!(heads[0], heads[1]);
    let launch = |index: usize, action: &str, enroll: bool, destination: &Path| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_graft"));
        command
            .arg("e2ee")
            .arg("--worktree")
            .arg(&sources[index])
            .arg("--state-dir")
            .arg(&states[index])
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
            ]);
        if enroll {
            command.arg("--enroll");
        }
        command
            .arg(action)
            .arg("--verification-directory")
            .arg(destination)
            .env(
                "GRAFT_E2EE_TEST_TOKEN",
                if index == 0 {
                    "account-token"
                } else {
                    "renewed-account-token"
                },
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = Service(command.spawn().unwrap());
        child
            .0
            .stdin
            .take()
            .unwrap()
            .write_all(passwords[index])
            .unwrap();
        child
    };
    // Both create bodies must reach the service barrier before either is applied.
    let mut first = launch(0, "initialize", true, &verified[0]);
    let mut second = launch(1, "initialize", true, &verified[1]);
    let outputs = [bounded_output(&mut first.0), bounded_output(&mut second.0)];
    assert_eq!(
        outputs
            .iter()
            .filter(|output| output.status.success())
            .count(),
        1
    );
    let winner = usize::from(outputs[1].status.success());
    let loser = 1 - winner;
    assert_eq!(
        serde_json::from_slice::<Value>(&outputs[winner].stdout).unwrap()["status"],
        "connected"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&outputs[loser].stdout).unwrap()["error"]["code"],
        "GRAFT_SDK_REPOSITORY_STALE"
    );
    let binding = SessionBinding::new(
        format!("graft-cli:{url}"),
        "test-account".into(),
        RepositoryId::from_bytes([32, 1, 2, 3, 4, 5, 0x46, 7, 0x88, 9, 10, 11, 12, 13, 14, 15])
            .unwrap(),
    )
    .unwrap();
    let journals = [
        FileInitializationJournal::open(states[0].join("initialization"), &binding).unwrap(),
        FileInitializationJournal::open(states[1].join("initialization"), &binding).unwrap(),
    ];
    let winning = journals[winner].load(&binding).unwrap();
    let losing = journals[loser].load(&binding).unwrap();
    assert_eq!(winning.stage, InitializationStage::Connected);
    assert_eq!(losing.stage, InitializationStage::Prepared);
    assert_eq!(winning.intent.unwrap().commit(), heads[winner]);
    assert_eq!(losing.intent.unwrap().commit(), heads[loser]);
    assert_ne!(winning.config.as_bytes(), losing.config.as_bytes());
    let losing_root = losing
        .config
        .unlock(passwords[loser], Some(binding.repository()))
        .unwrap();
    assert!(winning.config.authenticate(&losing_root).is_err());
    let requests: Vec<Value> = serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
    let creates: Vec<_> = requests
        .iter()
        .filter(|request| {
            request["method"] == "PUT"
                && request["path"] == "/acme/enrollment/raw-if-not-exists/e2ee/config"
        })
        .collect();
    assert_eq!(creates.len(), 2);
    assert!(
        creates
            .iter()
            .all(|request| request["gatedInitialConfigCreate"] == true)
    );
    assert_eq!(
        creates
            .iter()
            .filter(|request| request["responseStatus"] == 204)
            .count(),
        1
    );
    assert_eq!(
        creates
            .iter()
            .filter(|request| request["responseStatus"] == 412)
            .count(),
        1
    );
    let winner_hex: String = winning
        .config
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(
        creates
            .iter()
            .find(|request| request["responseStatus"] == 204)
            .unwrap()["body"],
        winner_hex
    );
    let loser_label = if loser == 0 { "first" } else { "second" };
    let loser_mutations: Vec<_> = requests
        .iter()
        .filter(|request| {
            request["syntheticCredential"] == loser_label && request["method"] != "GET"
        })
        .collect();
    assert_eq!(loser_mutations.len(), 1);
    assert_eq!(
        loser_mutations[0]["path"],
        "/acme/enrollment/raw-if-not-exists/e2ee/config"
    );
    assert_eq!(loser_mutations[0]["responseStatus"], 412);
    let baseline = requests.len();
    let mut retry = launch(
        loser,
        "resume-initialization",
        false,
        &fixture.path().join("loser-retry-verification"),
    );
    let retried = bounded_output(&mut retry.0);
    assert!(!retried.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&retried.stdout).unwrap()["error"]["code"],
        "GRAFT_SDK_REMOTE_PUBLICATION_OUTCOME_UNKNOWN"
    );
    let requests: Vec<Value> = serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
    assert!(
        requests[baseline..]
            .iter()
            .all(|request| request["method"] == "GET")
    );
    assert_eq!(
        journals[loser].load(&binding).unwrap().config.as_bytes(),
        losing.config.as_bytes()
    );
    assert_eq!(
        journals[loser].load(&binding).unwrap().stage,
        InitializationStage::Prepared
    );
    assert_eq!(
        fs::read(verified[winner].join("race.bin")).unwrap(),
        format!("race source synthetic attachment {winner}").as_bytes()
    );
    assert_eq!(
        Connection::open(verified[winner].join("race.db"))
            .unwrap()
            .query_row("SELECT value FROM data", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        winner as i64
    );
    assert!(!verified[loser].exists());
    let mut repeat = launch(
        winner,
        "resume-initialization",
        false,
        &fixture.path().join("winner-repeat-verification"),
    );
    assert!(bounded_output(&mut repeat.0).status.success());
}

#[test]
#[ignore = "requires built TypeScript service, Python 3 PTY helper and loopback networking"]
fn encrypted_cli_initialization_terminal_confirmation_is_hidden_and_fail_closed() {
    use graft::{
        e2ee::{FileInitializationJournal, InitializationJournal, InitializationStage},
        repo::Repository,
    };
    const UUID: &str = "10010203-0405-4607-8809-0a0b0c0d0e0f";
    const PASSWORD: &[u8] = "terminal initial synthetic 密码 ".as_bytes();
    let fixture = tempfile::tempdir().unwrap();
    let input = fixture.path().join("terminal-fixture.json");
    let captures = fixture.path().join("terminal-captures.json");
    fs::write(&input, serde_json::to_vec(&json!({
        "encryption":{"profile":"encrypted-remote-v1","version":1,"suite":1,"uuid":"00010203-0405-4607-8809-0a0b0c0d0e0f"},
        "terminalEncryption":{"profile":"encrypted-remote-v1","version":1,"suite":1,"uuid":UUID},
        "objects":[],
    })).unwrap()).unwrap();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut service = Service(
        Command::new("node")
            .arg(workspace.join("tests/e2ee/remote-service.mjs"))
            .arg(input)
            .arg(&captures)
            .current_dir(&workspace)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut ready = String::new();
    BufReader::new(service.0.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    let ready: Value = serde_json::from_str(&ready).unwrap();
    let url = ready["terminalUrl"].as_str().unwrap();
    let source = fixture.path().join("terminal-source");
    let writer = RepositorySession::new(&source);
    writer.open().unwrap();
    writer.init().unwrap();
    let db = Connection::open(source.join("terminal.db")).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL;PRAGMA wal_autocheckpoint=0;CREATE TABLE data(value TEXT);INSERT INTO data VALUES('terminal initialization WAL marker');").unwrap();
    fs::write(
        source.join("terminal.bin"),
        b"terminal initialization attachment marker",
    )
    .unwrap();
    writer.add_all().unwrap();
    writer
        .commit("terminal initialization commit marker")
        .unwrap();
    let expected = writer.repository_metadata().unwrap().current_head.unwrap();
    writer.close().unwrap();
    let invoke =
        |kind: &str, first: &[u8], confirmation: &[u8], state: &Path, destination: &Path| {
            let mut command = Command::new("python3");
            command
                .arg(workspace.join("tests/e2ee/cli-password-pty.py"))
                .arg(kind)
                .arg(env!("CARGO_BIN_EXE_graft"))
                .arg("e2ee")
                .arg("--worktree")
                .arg(&source)
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
                    "--enroll",
                    "initialize",
                    "--verification-directory",
                ])
                .arg(destination)
                .env("GRAFT_E2EE_TEST_TOKEN", "account-token")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command.spawn().unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(&serde_json::to_vec(&[first, confirmation]).unwrap())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice::<Value>(&output.stdout).unwrap()
        };
    let overlong = vec![b'x'; 1025];
    for (kind, first, confirmation, code) in [
        (
            "initialize-mismatch",
            PASSWORD,
            b"different synthetic confirmation".as_slice(),
            "GRAFT_SDK_INVALID_ARGUMENT",
        ),
        (
            "initialize-cancel-new",
            PASSWORD,
            PASSWORD,
            "GRAFT_SDK_CANCELLED",
        ),
        (
            "initialize-cancel-confirm",
            PASSWORD,
            PASSWORD,
            "GRAFT_SDK_CANCELLED",
        ),
        (
            "initialize-term-confirm",
            PASSWORD,
            PASSWORD,
            "GRAFT_SDK_CANCELLED",
        ),
        (
            "initialize-invalid-new",
            b"\xff".as_slice(),
            PASSWORD,
            "GRAFT_SDK_E2EE_INVALID_PASSWORD",
        ),
        (
            "initialize-invalid-confirm",
            PASSWORD,
            b"\xff".as_slice(),
            "GRAFT_SDK_E2EE_INVALID_PASSWORD",
        ),
        (
            "initialize-overlong-new",
            overlong.as_slice(),
            PASSWORD,
            "GRAFT_SDK_E2EE_INVALID_PASSWORD",
        ),
    ] {
        let state = fixture.path().join(format!("state-{kind}"));
        let destination = fixture.path().join(format!("verification-{kind}"));
        let result = invoke(kind, first, confirmation, &state, &destination);
        assert_ne!(result["status"], 0);
        assert_eq!(result["result"]["error"]["code"], code);
        assert_eq!(result["restored"], true);
        assert_eq!(result["echo_disabled"], true);
        assert!(!state.exists());
        assert!(!destination.exists());
        assert!(!captures.exists());
        assert!(
            !Repository::discover(&source)
                .unwrap()
                .config()
                .unwrap()
                .remotes
                .contains_key("origin")
        );
    }
    let state = fixture.path().join("connected-terminal-state");
    let destination = fixture.path().join("connected-terminal-verification");
    let result = invoke(
        "initialize-confirm",
        PASSWORD,
        PASSWORD,
        &state,
        &destination,
    );
    assert_eq!(result["status"], 0);
    assert_eq!(result["result"]["status"], "connected");
    assert_eq!(result["restored"], true);
    let binding = SessionBinding::new(
        format!("graft-cli:{url}"),
        "test-account".into(),
        RepositoryId::from_bytes([16, 1, 2, 3, 4, 5, 0x46, 7, 0x88, 9, 10, 11, 12, 13, 14, 15])
            .unwrap(),
    )
    .unwrap();
    let record = FileInitializationJournal::open(state.join("initialization"), &binding)
        .unwrap()
        .load(&binding)
        .unwrap();
    assert_eq!(record.stage, InitializationStage::Connected);
    assert_eq!(record.intent.unwrap().commit(), expected);
    record
        .config
        .unlock(PASSWORD, Some(binding.repository()))
        .unwrap();
    assert!(
        record
            .config
            .unlock(
                "terminal initial synthetic 密码".as_bytes(),
                Some(binding.repository())
            )
            .is_err()
    );
    assert_eq!(
        fs::read(destination.join("terminal.bin")).unwrap(),
        b"terminal initialization attachment marker"
    );
    assert_eq!(
        Connection::open(destination.join("terminal.db"))
            .unwrap()
            .query_row("SELECT value FROM data", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "terminal initialization WAL marker"
    );
    let captured = fs::read(&captures).unwrap();
    for marker in [
        PASSWORD,
        b"terminal initialization WAL marker",
        b"terminal initialization attachment marker",
        b"terminal initialization commit marker",
        expected.as_bytes(),
    ] {
        assert!(!captured.windows(marker.len()).any(|bytes| bytes == marker));
        let hex: String = marker.iter().map(|byte| format!("{byte:02x}")).collect();
        assert!(
            !captured
                .windows(hex.len())
                .any(|bytes| bytes == hex.as_bytes())
        );
    }
}

#[test]
#[ignore = "requires built TypeScript remote service and loopback networking"]
fn encrypted_cli_initialization_recovers_config_and_head_without_replacing_source() {
    use graft::e2ee::{FileInitializationJournal, InitializationJournal, InitializationStage};
    const UUID: &str = "00010203-0405-4607-8809-0a0b0c0d0e0f";
    const PASSWORD: &[u8] = "initial CLI synthetic 密码 \r\n".as_bytes();
    let fixture = tempfile::tempdir().unwrap();
    let input = fixture.path().join("fixture.json");
    let captures = fixture.path().join("captures.json");
    fs::write(
        &input,
        serde_json::to_vec(&json!({
            "encryption":{"profile":"encrypted-remote-v1","version":1,"suite":1,"uuid":UUID},
            "objects":[], "failInitialHeadCreates":true, "dropInitialConfigAcknowledgements":true,
        }))
        .unwrap(),
    )
    .unwrap();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut service = Service(
        Command::new("node")
            .arg(workspace.join("tests/e2ee/remote-service.mjs"))
            .arg(input)
            .arg(&captures)
            .current_dir(workspace)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut ready = String::new();
    BufReader::new(service.0.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    let ready: Value = serde_json::from_str(&ready).unwrap();
    let url = ready["enrollmentUrl"].as_str().unwrap();
    let source = fixture.path().join("source");
    let writer = RepositorySession::new(&source);
    writer.open().unwrap();
    writer.init().unwrap();
    let db = Connection::open(source.join("initial.db")).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL;PRAGMA wal_autocheckpoint=0;CREATE TABLE data(value TEXT);INSERT INTO data VALUES('CLI initialization WAL marker');").unwrap();
    fs::write(
        source.join("attachment.bin"),
        b"CLI initialization attachment marker",
    )
    .unwrap();
    writer.add_all().unwrap();
    writer.commit("CLI initialization commit marker").unwrap();
    let expected = writer.repository_metadata().unwrap().current_head.unwrap();
    writer.close().unwrap();
    let state = fixture.path().join("source-host-state");
    let invoke = |action: &str, enroll: bool, password: &[u8], destination: &Path| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_graft"));
        command
            .arg("e2ee")
            .arg("--worktree")
            .arg(&source)
            .arg("--state-dir")
            .arg(&state)
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
            ]);
        if enroll {
            command.arg("--enroll");
        }
        command
            .arg(action)
            .arg("--verification-directory")
            .arg(destination)
            .env("GRAFT_E2EE_TEST_TOKEN", "account-token")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(password).unwrap();
        child.wait_with_output().unwrap()
    };
    let first = invoke(
        "initialize",
        true,
        PASSWORD,
        &fixture.path().join("first-verification"),
    );
    assert!(!first.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&first.stdout).unwrap()["error"]["code"],
        "GRAFT_SDK_REMOTE_PUBLICATION_OUTCOME_UNKNOWN"
    );
    let binding = SessionBinding::new(
        format!("graft-cli:{url}"),
        "test-account".into(),
        RepositoryId::from_bytes([0, 1, 2, 3, 4, 5, 0x46, 7, 0x88, 9, 10, 11, 12, 13, 14, 15])
            .unwrap(),
    )
    .unwrap();
    let journal = FileInitializationJournal::open(state.join("initialization"), &binding).unwrap();
    let record = journal.load(&binding).unwrap();
    assert_eq!(record.stage, InitializationStage::Prepared);
    let exact_config = record.config.as_bytes().to_vec();
    assert_eq!(record.intent.unwrap().commit(), expected);
    let requests: Vec<Value> = serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
    let count = requests.len();
    let wrong = invoke(
        "resume-initialization",
        false,
        b"wrong initial password",
        &fixture.path().join("wrong-verification"),
    );
    assert!(!wrong.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&wrong.stdout).unwrap()["error"]["code"],
        "GRAFT_SDK_E2EE_UNLOCK_FAILED"
    );
    let requests: Vec<Value> = serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
    assert_eq!(requests.len(), count);
    let second = invoke(
        "resume-initialization",
        false,
        PASSWORD,
        &fixture.path().join("head-failure-verification"),
    );
    assert!(!second.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&second.stdout).unwrap()["error"]["code"],
        "GRAFT_SDK_REMOTE_PUBLICATION_UNCONFIRMED"
    );
    assert_eq!(
        journal.load(&binding).unwrap().stage,
        InitializationStage::ConfigPublished
    );
    let verified = fixture.path().join("connected-verification");
    let third = invoke("resume-initialization", false, PASSWORD, &verified);
    assert!(
        third.status.success(),
        "{}",
        String::from_utf8_lossy(&third.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&third.stdout).unwrap()["status"],
        "connected"
    );
    assert_eq!(
        journal.load(&binding).unwrap().stage,
        InitializationStage::Connected
    );
    assert_eq!(
        journal.load(&binding).unwrap().config.as_bytes(),
        exact_config
    );
    assert_eq!(
        fs::read(verified.join("attachment.bin")).unwrap(),
        b"CLI initialization attachment marker"
    );
    assert_eq!(
        Connection::open(verified.join("initial.db"))
            .unwrap()
            .query_row("SELECT value FROM data", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "CLI initialization WAL marker"
    );
    let repeat = invoke(
        "resume-initialization",
        false,
        PASSWORD,
        &fixture.path().join("idempotent-verification"),
    );
    assert!(
        repeat.status.success(),
        "{}",
        String::from_utf8_lossy(&repeat.stderr)
    );
    writer.open().unwrap();
    fs::write(source.join("later.txt"), b"later source marker").unwrap();
    writer.add_all().unwrap();
    writer.commit("later source commit marker").unwrap();
    writer.close().unwrap();
    let before: Vec<Value> = serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
    let changed = invoke(
        "resume-initialization",
        false,
        PASSWORD,
        &fixture.path().join("changed-source-verification"),
    );
    assert!(!changed.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&changed.stdout).unwrap()["error"]["code"],
        "GRAFT_SDK_E2EE_STALE_OBSERVATION"
    );
    let after: Vec<Value> = serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
    assert_eq!(after.len(), before.len());
    assert_eq!(
        journal.load(&binding).unwrap().config.as_bytes(),
        exact_config
    );
    assert_eq!(
        journal.load(&binding).unwrap().intent.unwrap().commit(),
        expected
    );
    assert_eq!(
        after
            .iter()
            .filter(|request| request["method"] == "PUT"
                && request["path"] == "/acme/enrollment/raw-if-not-exists/e2ee/config")
            .count(),
        1
    );
    assert!(
        after
            .iter()
            .any(|request| request["droppedInitialConfigResponse"] == true)
    );
    assert!(
        after
            .iter()
            .any(|request| request["droppedInitialConfigRead"] == true)
    );
    let captured = fs::read(&captures).unwrap();
    for marker in [
        PASSWORD,
        b"CLI initialization WAL marker",
        b"CLI initialization attachment marker",
        b"CLI initialization commit marker",
    ] {
        assert!(!captured.windows(marker.len()).any(|bytes| bytes == marker));
        let hex: String = marker.iter().map(|byte| format!("{byte:02x}")).collect();
        assert!(
            !captured
                .windows(hex.len())
                .any(|bytes| bytes == hex.as_bytes())
        );
    }
    assert!(
        !captured
            .windows(expected.len())
            .any(|bytes| bytes == expected.as_bytes())
    );
}

#[test]
#[ignore = "requires built TypeScript remote service, Python 3 PTY helper and loopback networking"]
fn encrypted_cli_clones_fetches_pulls_and_pushes_using_protected_input() {
    const UUID: &str = "00010203-0405-4607-8809-0a0b0c0d0e0f";
    const PASSWORD: &[u8] = "cli synthetic 密码 ".as_bytes();
    let fixture = tempfile::tempdir().unwrap();
    let private = |name: &str| {
        let path = fixture.path().join(name);
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    };
    let id = RepositoryId::from_bytes([0, 1, 2, 3, 4, 5, 0x46, 7, 0x88, 9, 10, 11, 12, 13, 14, 15])
        .unwrap();
    let config =
        EncryptedConfig::create(&RootKey::import(id, Zeroizing::new([42; 32])), PASSWORD).unwrap();
    let input = fixture.path().join("fixture.json");
    fs::write(
        &input,
        serde_json::to_vec(
            &json!({"config":config.as_bytes(),"cliDropConfigAcknowledgements":true,"encryption":{
        "profile":"encrypted-remote-v1","version":1,"suite":1,"uuid":UUID}}),
        )
        .unwrap(),
    )
    .unwrap();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut service = Service(
        Command::new("node")
            .arg(workspace.join("tests/e2ee/benchmark-service.mjs"))
            .arg(input)
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let service_output = BufReader::new(service.0.stdout.take().unwrap());
    let (lines, service_lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in service_output.lines() {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let ready = service_lines
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    let mut service_command = |command: &str| {
        if command != "await-cli-config-hold" {
            writeln!(service.0.stdin.as_mut().unwrap(), "{command}").unwrap();
        }
        let result = service_lines
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap();
        serde_json::from_str::<Value>(&result).unwrap()
    };
    let origin: Value = serde_json::from_str(&ready).unwrap();
    let url = format!("{}/encrypted", origin["origin"].as_str().unwrap());
    let remote = RemoteConfig::EncryptedHttp { url: url.clone(), uuid: UUID.into() };
    let binding = SessionBinding::new("cli-test".into(), "benchmark".into(), id).unwrap();
    let checkpoint = FileCheckpointStore::create(private("source-checkpoint"), &binding).unwrap();
    let secret = SecretSession::with_checkpoint_store(binding, Arc::new(checkpoint)).unwrap();
    secret
        .unlock_with_root(&config, RootKey::import(id, Zeroizing::new([42; 32])))
        .unwrap();
    let credentials = RemoteCredentials::explicit();
    credentials
        .set_http_bearer_token("origin", "benchmark-token".into())
        .unwrap();
    credentials
        .set_encrypted_session(
            "origin",
            &remote,
            secret,
            EncryptedTransferBudget {
                private_directory: private("source-staging"),
                max_buffered_plaintext: 4 * 1024 * 1024,
                max_ciphertext_spool: 32 * 1024 * 1024,
            },
        )
        .unwrap();
    let source = private("source");
    let writer = RepositorySession::new_with_remote_credentials(&source, None, credentials);
    writer.open().unwrap();
    writer.init().unwrap();
    writer.add_remote_with_config("origin", remote).unwrap();
    let database = Connection::open(source.join("data.db")).unwrap();
    database.execute_batch("PRAGMA journal_mode=WAL;PRAGMA wal_autocheckpoint=0;CREATE TABLE data(value TEXT);INSERT INTO data VALUES('initial');").unwrap();
    fs::write(source.join("attachment.bin"), b"cli synthetic attachment").unwrap();
    writer.add_all().unwrap();
    writer.commit("cli initial").unwrap();
    writer.push(Some("origin"), Some("main")).unwrap();
    let target = fixture.path().join("clone");
    let state = fixture.path().join("clone-state");
    let invoke_on = |action: &str, enroll: bool, password: &[u8], state: &Path, worktree: &Path| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_graft"));
        command
            .arg("e2ee")
            .arg("--worktree")
            .arg(worktree)
            .arg("--state-dir")
            .arg(state)
            .args([
                "--url",
                &url,
                "--uuid",
                UUID,
                "--account",
                "benchmark",
                "--token-env",
                "GRAFT_E2EE_TEST_TOKEN",
                "--password-stdin",
            ]);
        if enroll {
            command.arg("--enroll");
        }
        command
            .arg(action)
            .env("GRAFT_E2EE_TEST_TOKEN", "benchmark-token")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(password).unwrap();
        child.wait_with_output().unwrap()
    };
    let invoke = |action: &str, enroll: bool, password: &[u8], state: &Path| {
        invoke_on(action, enroll, password, state, &target)
    };
    let check = |action: &str, enroll: bool| {
        let output = invoke(action, enroll, PASSWORD, &state);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    check("clone", true);
    assert_eq!(
        fs::read(target.join("attachment.bin")).unwrap(),
        b"cli synthetic attachment"
    );
    let read_value = || {
        Connection::open(target.join("data.db"))
            .unwrap()
            .query_row("SELECT value FROM data", [], |r| r.get::<_, String>(0))
            .unwrap()
    };
    assert_eq!(read_value(), "initial");
    database
        .execute("UPDATE data SET value='remote update'", [])
        .unwrap();
    writer.add_all().unwrap();
    writer.commit("cli remote update").unwrap();
    writer.push(Some("origin"), Some("main")).unwrap();
    check("fetch", false);
    assert_eq!(read_value(), "initial");
    check("pull", false);
    assert_eq!(read_value(), "remote update");
    Connection::open(target.join("data.db"))
        .unwrap()
        .execute("UPDATE data SET value='cli update'", [])
        .unwrap();
    for args in [
        vec!["add", "--all"],
        vec!["commit", "-m", "cli local update"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_graft"))
            .current_dir(&target)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    check("push", false);
    assert_eq!(
        writer
            .pull(Some("origin"), Some("main"))
            .unwrap_err()
            .code(),
        graft_sdk::SdkErrorCode::RepositoryBusy
    );
    drop(database);
    writer.pull(Some("origin"), Some("main")).unwrap();
    assert_eq!(
        Connection::open(source.join("data.db"))
            .unwrap()
            .query_row("SELECT value FROM data", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "cli update"
    );
    let wrong = invoke("fetch", false, b"wrong synthetic password", &state);
    assert!(!wrong.status.success());
    assert!(!String::from_utf8_lossy(&wrong.stderr).contains("wrong synthetic password"));
    let error: Value = serde_json::from_slice(&wrong.stdout).unwrap();
    assert_eq!(error["error"]["code"], "GRAFT_SDK_E2EE_UNLOCK_FAILED");
    assert!(!String::from_utf8_lossy(&wrong.stdout).contains("wrong synthetic password"));
    let missing = invoke("fetch", false, PASSWORD, &fixture.path().join("lost-state"));
    assert!(!missing.status.success());
    let error: Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(
        error["error"]["code"],
        "GRAFT_SDK_E2EE_CHECKPOINT_UNAVAILABLE"
    );
    assert!(!fixture.path().join("lost-state").exists());
    let invoke_terminal = |kind: &str, secret: &[u8], enroll: bool, state: &Path| {
        let mut command = Command::new("python3");
        command
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/e2ee/cli-password-pty.py"))
            .arg(kind)
            .arg(env!("CARGO_BIN_EXE_graft"))
            .arg("e2ee")
            .arg("--worktree")
            .arg(&target)
            .arg("--state-dir")
            .arg(state)
            .args([
                "--url",
                &url,
                "--uuid",
                UUID,
                "--account",
                "benchmark",
                "--token-env",
                "GRAFT_E2EE_TEST_TOKEN",
            ]);
        if enroll {
            command.arg("--enroll");
        }
        command
            .arg(if kind.starts_with("change-") {
                "change-password"
            } else {
                "fetch"
            })
            .env("GRAFT_E2EE_TEST_TOKEN", "benchmark-token")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(secret).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let interactive = invoke_terminal("enter", PASSWORD, false, &state);
    assert_eq!(interactive["status"], 0);
    assert_eq!(interactive["restored"], true);
    let edited = invoke_terminal("enter-edited", PASSWORD, false, &state);
    assert_eq!(edited["status"], 0);
    for kind in [
        "ctrl-c", "ctrl-d", "ctrl-z", "escape", "sigterm", "sigint", "sighup", "sigquit", "sigtstp",
    ] {
        let cancelled_state = fixture.path().join(format!("cancelled-{kind}"));
        let result = invoke_terminal(
            kind,
            b"synthetic cancelled password",
            true,
            &cancelled_state,
        );
        assert_ne!(result["status"], 0);
        assert_eq!(result["result"]["error"]["code"], "GRAFT_SDK_CANCELLED");
        assert!(!cancelled_state.exists());
    }
    for invalid in [vec![b'x'; 1025], b"\xff".to_vec()] {
        let result = invoke_terminal("enter", &invalid, false, &state);
        assert_ne!(result["status"], 0);
        assert_eq!(
            result["result"]["error"]["code"],
            "GRAFT_SDK_E2EE_INVALID_PASSWORD"
        );
    }
    const TERMINAL_PASSWORD: &[u8] = "terminal replacement synthetic 密码 ".as_bytes();
    for kind in [
        "change-mismatch",
        "change-cancel-new",
        "change-cancel-confirm",
        "change-term-confirm",
    ] {
        let confirmation = if kind == "change-mismatch" {
            b"different synthetic confirmation".as_slice()
        } else {
            TERMINAL_PASSWORD
        };
        let inputs = serde_json::to_vec(&[PASSWORD, TERMINAL_PASSWORD, confirmation]).unwrap();
        let result = invoke_terminal(kind, &inputs, false, &state);
        assert_ne!(result["status"], 0);
        assert_eq!(result["restored"], true);
        assert_eq!(
            result["result"]["error"]["code"],
            if kind == "change-mismatch" {
                "GRAFT_SDK_INVALID_ARGUMENT"
            } else {
                "GRAFT_SDK_CANCELLED"
            }
        );
        assert!(invoke("fetch", false, PASSWORD, &state).status.success());
    }
    assert_eq!(
        service_command("snapshot")["cli_config_fault"]["cas_bodies"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let inputs = serde_json::to_vec(&[PASSWORD, TERMINAL_PASSWORD, TERMINAL_PASSWORD]).unwrap();
    let result = invoke_terminal("change-confirm", &inputs, false, &state);
    assert_eq!(result["status"], 0);
    assert_eq!(result["result"]["status"], "confirmed");
    assert!(
        invoke("fetch", false, TERMINAL_PASSWORD, &state)
            .status
            .success()
    );
    assert_eq!(service_command("arm-cli-config-fault")["armed"], true);
    // The new password travels through an inherited descriptor, never argv/environment.
    const NEW_PASSWORD: &[u8] = "replacement synthetic 密码 \r\n".as_bytes();
    let replacement = fixture.path().join("synthetic-password-input");
    fs::write(&replacement, NEW_PASSWORD).unwrap();
    fs::set_permissions(&replacement, fs::Permissions::from_mode(0o600)).unwrap();
    let mut change = Command::new("python3");
    change.args(["-c", "import os,sys; f=open(sys.argv[1],'rb'); os.dup2(f.fileno(),3); os.set_inheritable(3,True); os.execv(sys.argv[2],sys.argv[2:])"])
        .arg(&replacement).arg(env!("CARGO_BIN_EXE_graft"))
        .arg("e2ee").arg("--worktree").arg(&target)
        .arg("--state-dir").arg(&state)
        .args(["--url", &url, "--uuid", UUID, "--account", "benchmark", "--token-env", "GRAFT_E2EE_TEST_TOKEN", "--password-stdin", "change-password", "--new-password-fd", "3"])
        .env("GRAFT_E2EE_TEST_TOKEN", "benchmark-token")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = change.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(TERMINAL_PASSWORD)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["error"]["code"],
        "GRAFT_SDK_REMOTE_PUBLICATION_OUTCOME_UNKNOWN"
    );
    let local_binding =
        SessionBinding::new(format!("graft-cli:{url}"), "benchmark".into(), id).unwrap();
    let journal =
        FileConfigMutationJournal::open(state.join("config-recovery"), &local_binding).unwrap();
    let candidate = journal.load(&local_binding).unwrap().unwrap();
    let retained = candidate.to_local_bytes().unwrap();
    let blocked = invoke("change-password", false, NEW_PASSWORD, &state);
    assert!(!blocked.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&blocked.stdout).unwrap()["error"]["code"],
        "GRAFT_SDK_E2EE_STALE_OBSERVATION"
    );
    assert_eq!(
        journal
            .load(&local_binding)
            .unwrap()
            .unwrap()
            .to_local_bytes()
            .unwrap(),
        retained
    );
    let fault = service_command("snapshot");
    assert_eq!(fault["cli_config_fault"]["dropped_ack"], true);
    assert_eq!(fault["cli_config_fault"]["dropped_read"], true);
    assert_eq!(
        fault["cli_config_fault"]["cas_bodies"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let exact_hex: String = candidate
        .replacement_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(fault["cli_config_fault"]["cas_bodies"][1], exact_hex);
    for action in ["fetch", "resume-password-change"] {
        let output = invoke(action, false, NEW_PASSWORD, &state);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output
                .stdout
                .windows(NEW_PASSWORD.len())
                .any(|bytes| bytes == NEW_PASSWORD)
        );
    }
    assert!(journal.load(&local_binding).unwrap().is_none());
    assert_eq!(
        journal
            .load_recoverable(&local_binding)
            .unwrap()
            .unwrap()
            .to_local_bytes()
            .unwrap(),
        retained
    );
    assert_eq!(
        service_command("snapshot")["cli_config_fault"]["cas_bodies"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    // A failed dispatch has not changed the remote. Retry must use the retained
    // ciphertext rather than generate a different random wrapping envelope.
    const RETRY_PASSWORD: &[u8] = "retry synthetic 密码 \r\n".as_bytes();
    fs::write(&replacement, RETRY_PASSWORD).unwrap();
    assert_eq!(
        service_command("arm-cli-config-dispatch-fault")["armed"],
        true
    );
    let mut child = change.spawn().unwrap();
    child.stdin.take().unwrap().write_all(NEW_PASSWORD).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["error"]["code"],
        "GRAFT_SDK_REMOTE_PUBLICATION_OUTCOME_UNKNOWN"
    );
    let retry_candidate = journal.load(&local_binding).unwrap().unwrap();
    let retry_bytes = retry_candidate.to_local_bytes().unwrap();
    assert!(
        invoke("fetch", false, NEW_PASSWORD, &state)
            .status
            .success()
    );
    assert!(
        !invoke("fetch", false, RETRY_PASSWORD, &state)
            .status
            .success()
    );
    let resumed = invoke("resume-password-change", false, NEW_PASSWORD, &state);
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&resumed.stdout).unwrap()["status"],
        "confirmed"
    );
    assert!(journal.load(&local_binding).unwrap().is_none());
    assert_eq!(
        journal
            .load_recoverable(&local_binding)
            .unwrap()
            .unwrap()
            .to_local_bytes()
            .unwrap(),
        retry_bytes
    );
    let snapshot = service_command("snapshot");
    assert_eq!(snapshot["cli_config_fault"]["dropped_dispatch"], true);
    let bodies = snapshot["cli_config_fault"]["cas_bodies"]
        .as_array()
        .unwrap();
    assert_eq!(bodies.len(), 4);
    assert_eq!(bodies[2], bodies[3]);
    let expected: String = retry_candidate
        .replacement_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(bodies[3], expected);

    // Kill the actual CLI after durable remote CAS, before any acknowledgement.
    const CRASH_PASSWORD: &[u8] = "crash recovery synthetic 密码 \r\n".as_bytes();
    fs::write(&replacement, CRASH_PASSWORD).unwrap();
    assert_eq!(service_command("arm-cli-config-hold")["armed"], true);
    let mut killed = Service(change.spawn().unwrap());
    killed
        .0
        .stdin
        .take()
        .unwrap()
        .write_all(RETRY_PASSWORD)
        .unwrap();
    assert_eq!(
        service_command("await-cli-config-hold")["config_commit_held"],
        true
    );
    let crash_candidate = journal.load(&local_binding).unwrap().unwrap();
    let crash_bytes = crash_candidate.to_local_bytes().unwrap();
    killed.0.kill().unwrap();
    assert!(!killed.0.wait().unwrap().success());
    assert_eq!(
        journal
            .load(&local_binding)
            .unwrap()
            .unwrap()
            .to_local_bytes()
            .unwrap(),
        crash_bytes
    );
    let resumed = invoke("resume-password-change", false, CRASH_PASSWORD, &state);
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert!(journal.load(&local_binding).unwrap().is_none());
    assert_eq!(
        journal
            .load_recoverable(&local_binding)
            .unwrap()
            .unwrap()
            .to_local_bytes()
            .unwrap(),
        crash_bytes
    );
    let snapshot = service_command("snapshot");
    assert_eq!(snapshot["cli_config_fault"]["held_commit"], true);
    assert_eq!(
        snapshot["cli_config_fault"]["cas_bodies"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    // Kill after local journal binding but before the service applies the CAS.
    const FINAL_PASSWORD: &[u8] = "uncommitted crash recovery 密码 \r\n".as_bytes();
    fs::write(&replacement, FINAL_PASSWORD).unwrap();
    assert_eq!(
        service_command("arm-cli-config-dispatch-hold")["armed"],
        true
    );
    let mut killed_before_commit = Service(change.spawn().unwrap());
    killed_before_commit
        .0
        .stdin
        .take()
        .unwrap()
        .write_all(CRASH_PASSWORD)
        .unwrap();
    assert_eq!(
        service_command("await-cli-config-hold")["config_dispatch_held"],
        true
    );
    let uncommitted = journal.load(&local_binding).unwrap().unwrap();
    let uncommitted_bytes = uncommitted.to_local_bytes().unwrap();
    killed_before_commit.0.kill().unwrap();
    assert!(!killed_before_commit.0.wait().unwrap().success());
    assert_eq!(
        journal
            .load(&local_binding)
            .unwrap()
            .unwrap()
            .to_local_bytes()
            .unwrap(),
        uncommitted_bytes
    );
    assert!(
        !invoke("fetch", false, FINAL_PASSWORD, &state)
            .status
            .success()
    );
    let resumed = invoke("resume-password-change", false, CRASH_PASSWORD, &state);
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert!(journal.load(&local_binding).unwrap().is_none());
    assert_eq!(
        journal
            .load_recoverable(&local_binding)
            .unwrap()
            .unwrap()
            .to_local_bytes()
            .unwrap(),
        uncommitted_bytes
    );
    let snapshot = service_command("snapshot");
    assert_eq!(snapshot["cli_config_fault"]["held_dispatch"], true);
    let bodies = snapshot["cli_config_fault"]["cas_bodies"]
        .as_array()
        .unwrap();
    assert_eq!(bodies.len(), 7);
    assert_eq!(bodies[5], bodies[6]);
    let expected: String = uncommitted
        .replacement_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(bodies[6], expected);
    let rejected = invoke("fetch", false, PASSWORD, &state);
    assert!(!rejected.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&rejected.stdout).unwrap()["error"]["code"],
        "GRAFT_SDK_E2EE_UNLOCK_FAILED"
    );
    writer.fetch(Some("origin"), Some("main")).unwrap();
    assert_eq!(read_value(), "cli update");
    let independent = fixture.path().join("independent-new-password-clone");
    let independent_state = fixture.path().join("independent-host-state");
    let cloned = invoke_on(
        "clone",
        true,
        FINAL_PASSWORD,
        &independent_state,
        &independent,
    );
    assert!(
        cloned.status.success(),
        "{}",
        String::from_utf8_lossy(&cloned.stderr)
    );
    assert_eq!(
        fs::read(independent.join("attachment.bin")).unwrap(),
        b"cli synthetic attachment"
    );
    assert_eq!(
        Connection::open(independent.join("data.db"))
            .unwrap()
            .query_row("SELECT value FROM data", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "cli update"
    );
    writer.close().unwrap();
}
