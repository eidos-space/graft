#![cfg(all(feature = "e2ee", unix))]

use graft::{
    e2ee::{
        EncryptedConfig, FileCheckpointStore, RepositoryId, RootKey, SecretSession, SessionBinding,
    },
    remote::{EncryptedTransferBudget, RemoteConfig},
    repo::Repository,
};
use graft_sdk::{ApplyMergeOptions, PlanMergeOptions, RemoteCredentials, RepositorySession};
use rusqlite::Connection;
use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Stdio},
};
use zeroize::Zeroizing;

struct Service(Child);
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "requires built TypeScript service and loopback networking; pnpm test:e2ee-interop"]
fn encrypted_initialization_recovers_published_branch_and_missing_head_after_reopen() {
    use graft::e2ee::{
        EncryptedHttpClient, FileInitializationJournal, InitialConfigPublicationOutcome,
        InitializationIntent, InitializationJournal, InitializationStage as Stage, RefTarget,
    };
    use std::sync::Arc;
    let directory = tempfile::tempdir().unwrap();
    let private = |name: &str| {
        let path = directory.path().join(name);
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    };
    let source = private("source");
    let checkpoint = private("checkpoint");
    let staging = private("staging");
    let journal_path = private("journal");
    let mutation_journals = private("ref-mutations");
    let repository =
        RepositoryId::from_bytes([0, 1, 2, 3, 4, 5, 0x46, 7, 0x88, 9, 10, 11, 12, 13, 14, 15])
            .unwrap();
    let root = RootKey::generate(repository).unwrap();
    let candidate = EncryptedConfig::create(&root, b"sdk synthetic sync password").unwrap();
    drop(root);
    let fixture = directory.path().join("fixture.json");
    let captures = directory.path().join("captures.json");
    fs::write(
        &fixture,
        serde_json::to_vec(&serde_json::json!({
            "serviceLifetimeMs": 600000,
            "encryption": {"profile":"encrypted-remote-v1", "version":1, "suite":1,
                "uuid":"00010203-0405-4607-8809-0a0b0c0d0e0f"},
            "objects": [], "failInitialHeadCreates": true,
            "dropEnrollmentConfigMutationAcknowledgements": true,
        }))
        .unwrap(),
    )
    .unwrap();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut service = Service(
        Command::new("node")
            .arg(workspace.join("tests/e2ee/remote-service.mjs"))
            .arg(&fixture)
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
    let ready: serde_json::Value = serde_json::from_str(&ready).unwrap();
    let config = RemoteConfig::EncryptedHttp {
        url: ready["enrollmentUrl"].as_str().unwrap().into(),
        uuid: "00010203-0405-4607-8809-0a0b0c0d0e0f".into(),
    };
    let source_secret = native_session(&checkpoint, repository, &candidate);
    let source_credentials = durable_credentials(
        &config,
        source_secret.clone(),
        &staging,
        &mutation_journals,
        true,
    );
    // The direct client below retains connections while synchronous SDK work
    // runs between block_on calls. Keep its I/O driver active through idle closes.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let initial_client = EncryptedHttpClient::new(
        ready["enrollmentUrl"].as_str().unwrap(),
        "origin".into(),
        source_credentials.clone(),
        source_secret.clone(),
    )
    .unwrap();
    let journal = Arc::new(
        FileInitializationJournal::create(&journal_path, source_secret.binding(), &candidate)
            .unwrap(),
    );
    assert!(matches!(
        runtime
            .block_on(initial_client.publish_initial_config(journal.clone()))
            .unwrap(),
        InitialConfigPublicationOutcome::Confirmed
    ));
    let writer =
        RepositorySession::new_with_remote_credentials(&source, None, source_credentials.clone());
    writer.open().unwrap();
    writer.init().unwrap();
    writer
        .add_remote_with_config("origin", config.clone())
        .unwrap();
    let db = Connection::open(source.join("initial-private.db")).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE initial_data(value TEXT); INSERT INTO initial_data VALUES('initial WAL private marker');").unwrap();
    let payload = b"initial attachment private marker\n".repeat(4096);
    fs::write(source.join("initial-private.txt"), &payload).unwrap();
    writer.add_all().unwrap();
    writer.commit("initial enrollment private commit").unwrap();
    let expected = writer.repository_metadata().unwrap().current_head.unwrap();
    journal
        .bind_intent(
            source_secret.binding(),
            &InitializationIntent::new("main".into(), expected.clone()).unwrap(),
        )
        .unwrap();
    let intent = journal
        .load(source_secret.binding())
        .unwrap()
        .intent
        .unwrap();
    let failed = writer
        .push_initialization_intent("origin", &intent)
        .unwrap_err();
    assert_eq!(
        failed.code(),
        graft_sdk::SdkErrorCode::RemotePublicationUnconfirmed
    );
    assert_eq!(
        journal.load(source_secret.binding()).unwrap().stage,
        Stage::ConfigPublished
    );
    assert!(
        runtime
            .block_on(initial_client.read_ref("HEAD"))
            .unwrap()
            .target()
            .is_none()
    );
    // A new operation cannot replace the pending initial HEAD candidate.
    let adapter = config
        .clone()
        .build_with_credentials("origin", &source_credentials)
        .unwrap();
    let before_divergent: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
    let divergent = runtime.block_on(
        adapter.put_raw_if_not_exists("HEAD", "ref: refs/heads/do-not-replace-pending\n"),
    );
    assert!(matches!(
        divergent,
        Err(graft::remote::RemoteErr::Encryption(
            graft::e2ee::EncryptedHttpError::Encryption(
                graft::e2ee::EncryptionError::StaleObservation
            )
        ))
    ));
    let after_divergent: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
    assert!(
        after_divergent[before_divergent.len()..]
            .iter()
            .all(|request| request["method"] == "GET")
    );
    drop(adapter);
    assert!(
        Repository::open(&source)
            .unwrap()
            .remote_tracking_ref("origin", "main")
            .unwrap()
            .is_none()
    );

    // Fresh caches/root session validate the published dependency closure while
    // HEAD is still missing. Explicit branch selection is needed at this point.
    let verify = private("verify-before-head");
    let verify_secret = native_session(&private("verify-checkpoint"), repository, &candidate);
    let verify_credentials = credentials(&config, verify_secret, &private("verify-staging"));
    let verifier =
        RepositorySession::new_with_remote_credentials(&verify, None, verify_credentials.clone());
    verifier.open().unwrap();
    verifier
        .clone_repository_with_config(config.clone(), Some("main"))
        .unwrap();
    assert_eq!(
        verifier
            .repository_metadata()
            .unwrap()
            .current_head
            .as_deref(),
        Some(expected.as_str())
    );
    assert_eq!(
        fs::read(verify.join("initial-private.txt")).unwrap(),
        payload
    );
    let value: String = Connection::open(verify.join("initial-private.db"))
        .unwrap()
        .query_row("SELECT value FROM initial_data", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, "initial WAL private marker");
    verifier.close().unwrap();
    runtime
        .block_on(verify_credentials.clear_encrypted_session("origin"))
        .unwrap();
    journal
        .advance(
            source_secret.binding(),
            Stage::ConfigPublished,
            Stage::ObjectsPublished,
        )
        .unwrap();
    writer.close().unwrap();
    runtime
        .block_on(source_credentials.clear_encrypted_session("origin"))
        .unwrap();
    drop(initial_client);
    drop(journal);

    // Restore private state and unwrap the original config into a new session.
    let binding =
        SessionBinding::new("native.test".into(), "test-account".into(), repository).unwrap();
    let journal = Arc::new(FileInitializationJournal::open(&journal_path, &binding).unwrap());
    let checkpoint_store = FileCheckpointStore::open(&checkpoint, &binding).unwrap();
    let resumed_secret =
        SecretSession::with_checkpoint_store(binding, Arc::new(checkpoint_store)).unwrap();
    resumed_secret
        .unlock_with_root(
            &candidate,
            candidate
                .unlock(b"sdk synthetic sync password", Some(repository))
                .unwrap(),
        )
        .unwrap();
    let resumed_credentials = durable_credentials(
        &config,
        resumed_secret.clone(),
        &staging,
        &mutation_journals,
        false,
    );
    let resumed =
        RepositorySession::new_with_remote_credentials(&source, None, resumed_credentials.clone());
    resumed.open().unwrap();
    let before_retry: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
    let intent = journal
        .load(resumed_secret.binding())
        .unwrap()
        .intent
        .unwrap();
    assert_eq!(intent.commit(), expected);
    assert_eq!(intent.branch(), "main");
    let wrong_source = InitializationIntent::new("main".into(), "f".repeat(64)).unwrap();
    let rejected = resumed
        .push_initialization_intent("origin", &wrong_source)
        .unwrap_err();
    assert_eq!(
        rejected.code(),
        graft_sdk::SdkErrorCode::Encrypted(graft_sdk::EncryptedErrorCode::StaleObservation)
    );
    let after_rejection: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
    assert_eq!(after_rejection.len(), before_retry.len());
    let pushed = resumed
        .push_initialization_intent("origin", &intent)
        .unwrap();
    assert_eq!(pushed["commits"], 0);
    let client = EncryptedHttpClient::new(
        ready["enrollmentUrl"].as_str().unwrap(),
        "origin".into(),
        resumed_credentials.clone(),
        resumed_secret.clone(),
    )
    .unwrap();
    assert!(
        matches!(runtime.block_on(client.read_ref("HEAD")).unwrap().target(),
        Some(RefTarget::Symbolic(branch)) if branch == "refs/heads/main")
    );
    journal
        .advance(
            resumed_secret.binding(),
            Stage::ObjectsPublished,
            Stage::RefPublished,
        )
        .unwrap();
    assert_eq!(
        journal.load(resumed_secret.binding()).unwrap().stage,
        Stage::RefPublished
    );

    // Default-branch clone in another independent destination is the connected gate.
    let target = private("connected-verifier");
    let target_secret = native_session(&private("connected-checkpoint"), repository, &candidate);
    let target_credentials = credentials(
        &config,
        target_secret.clone(),
        &private("connected-staging"),
    );
    let reader =
        RepositorySession::new_with_remote_credentials(&target, None, target_credentials.clone());
    reader.open().unwrap();
    reader
        .clone_repository_with_config(config.clone(), None)
        .unwrap();
    assert_eq!(
        reader
            .repository_metadata()
            .unwrap()
            .current_head
            .as_deref(),
        Some(expected.as_str())
    );
    assert_eq!(
        fs::read(target.join("initial-private.txt")).unwrap(),
        payload
    );
    let value: String = Connection::open(target.join("initial-private.db"))
        .unwrap()
        .query_row("SELECT value FROM initial_data", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, "initial WAL private marker");
    journal
        .advance(
            resumed_secret.binding(),
            Stage::RefPublished,
            Stage::Connected,
        )
        .unwrap();
    let connected =
        FileInitializationJournal::open(&journal_path, resumed_secret.binding()).unwrap();
    assert_eq!(
        connected.load(resumed_secret.binding()).unwrap().stage,
        Stage::Connected
    );
    // A different observed head remains a competing publication even when it
    // is an ancestor. Initialization cannot reuse ordinary fast-forward push.
    fs::write(
        source.join("initial-private.txt"),
        b"initial competing descendant marker",
    )
    .unwrap();
    resumed.add_all().unwrap();
    resumed
        .commit("initial competing descendant commit")
        .unwrap();
    let descendant = resumed.repository_metadata().unwrap().current_head.unwrap();
    let before_competing: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
    let descendant_intent = InitializationIntent::new("main".into(), descendant.clone()).unwrap();
    let error = resumed
        .push_initialization_intent("origin", &descendant_intent)
        .unwrap_err();
    assert_eq!(
        error.code(),
        graft_sdk::SdkErrorCode::Encrypted(graft_sdk::EncryptedErrorCode::StaleObservation)
    );
    let after_competing: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
    assert!(after_competing.len() > before_competing.len());
    assert!(
        after_competing[before_competing.len()..]
            .iter()
            .all(|request| request["method"] == "GET")
    );
    assert!(matches!(
        runtime.block_on(client.read_ref("refs/heads/main")).unwrap().target(),
        Some(RefTarget::Commit(head))
            if head.iter().map(|byte| format!("{byte:02x}")).collect::<String>() == expected
    ));
    assert_eq!(
        resumed.repository_metadata().unwrap().current_head,
        Some(descendant.clone())
    );
    let requests: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
    let rejected: Vec<_> = requests
        .iter()
        .filter(|request| request["rejectedInitialHeadCreate"] == true)
        .collect();
    assert_eq!(rejected.len(), 3);
    assert!(
        rejected
            .iter()
            .all(|request| request["body"] == rejected[0]["body"])
    );
    let head_mutations: Vec<_> = requests
        .iter()
        .filter(|request| {
            request["method"] == "POST"
                && request["path"]
                    .as_str()
                    .unwrap()
                    .contains("/cas/e2ee/head/")
        })
        .collect();
    assert_eq!(head_mutations.len(), 4);
    assert!(
        head_mutations
            .iter()
            .all(|request| request["body"] == rejected[0]["body"])
    );
    assert!(
        requests[before_retry.len()..]
            .iter()
            .all(|request| request["method"] != "PUT")
    );
    let is_branch_publication = |request: &&serde_json::Value| {
        let path = request["path"].as_str().unwrap();
        request["method"] == "POST"
            && (path.contains("/cas/e2ee/refs/")
                || path.contains("/e2ee/receive-bundle/e2ee/refs/"))
    };
    assert_eq!(requests.iter().filter(is_branch_publication).count(), 1);
    assert_eq!(
        requests[before_retry.len()..]
            .iter()
            .filter(is_branch_publication)
            .count(),
        0,
        "recovery must not republish the already confirmed branch through either endpoint"
    );
    let encoded = serde_json::to_string(&requests).unwrap();
    for marker in [
        "initial competing descendant marker",
        "initial competing descendant commit",
        descendant.as_str(),
        "sdk synthetic sync password",
        "initial-private.db",
        "initial-private.txt",
        "initial WAL private marker",
        "initial attachment private marker",
        "initial enrollment private commit",
        &expected,
    ] {
        assert!(!encoded.contains(marker));
        assert!(
            !encoded.contains(
                &marker
                    .as_bytes()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            )
        );
    }
    reader.close().unwrap();
    resumed.close().unwrap();
    // Interleave two independently unlocked clients after an absent ref has
    // been observed. The winner publishes before the loser consumes its token.
    let loser = Repository::init(private("race-loser"))
        .unwrap()
        .with_remote_credentials(resumed_credentials.clone());
    loser.remote_add("origin", config.clone()).unwrap();
    let losing_commit = loser.commit("initial race losing commit").unwrap();
    loser
        .branch_create("race", Some(&losing_commit.id))
        .unwrap();
    let race_intent = InitializationIntent::new("race".into(), losing_commit.id.clone()).unwrap();
    let observed = loser.observe_remote_branch_head("origin", "race").unwrap();
    assert_eq!(observed.head(), None);
    let winner = Repository::init(private("race-winner"))
        .unwrap()
        .with_remote_credentials(target_credentials.clone());
    winner.remote_add("origin", config.clone()).unwrap();
    let winning_commit = winner.commit("initial race winning commit").unwrap();
    winner
        .branch_create("race", Some(&winning_commit.id))
        .unwrap();
    winner.push("origin", "race").unwrap();
    let winner_ref = runtime
        .block_on(client.read_ref("refs/heads/race"))
        .unwrap();
    assert!(matches!(winner_ref.target(), Some(RefTarget::Commit(head))
        if head.iter().map(|byte| format!("{byte:02x}")).collect::<String>() == winning_commit.id));
    let losing_result = loser.push_initialization_intent_with_observed_head_and_snapshots(
        "origin",
        &race_intent,
        observed,
        graft::PreparedSnapshotPush::default(),
    );
    assert!(
        matches!(
            losing_result,
            Err(graft::repo::RepoErr::RemoteRefChanged { .. })
        ),
        "unexpected losing publication outcome: {losing_result:?}"
    );
    assert_eq!(loser.remote_tracking_ref("origin", "race").unwrap(), None);
    let retained_ref = runtime
        .block_on(client.read_ref("refs/heads/race"))
        .unwrap();
    assert!(retained_ref.target() == winner_ref.target());
    assert_eq!(retained_ref.expected_bytes(), winner_ref.expected_bytes());
    assert!(
        matches!(runtime.block_on(client.read_ref("HEAD")).unwrap().target(),
        Some(RefTarget::Symbolic(branch)) if branch == "refs/heads/main")
    );
    let raced_requests = fs::read_to_string(&captures).unwrap();
    for marker in [
        "initial race losing commit",
        "initial race winning commit",
        losing_commit.id.as_str(),
        winning_commit.id.as_str(),
    ] {
        assert!(!raced_requests.contains(marker));
        assert!(
            !raced_requests.contains(
                &marker
                    .as_bytes()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            )
        );
    }
    {
        use graft::e2ee::{
            CheckpointStore, ConfigMutationJournal, ConfigPublicationOutcome,
            FileConfigMutationJournal, PasswordResetAuthorizer,
        };
        struct SyntheticNativeAuthorization {
            repository: RepositoryId,
            calls: std::sync::atomic::AtomicUsize,
        }
        impl PasswordResetAuthorizer for SyntheticNativeAuthorization {
            fn authorize(&self, binding: &SessionBinding) -> graft::e2ee::Result<()> {
                assert_eq!(binding.domain(), "native.test");
                assert_eq!(binding.account(), "test-account");
                assert_eq!(binding.repository(), self.repository);
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(()) // Test-only host authorization; not OS-authentication evidence.
            }
        }
        let reset_directory = private("password-reset-journal");
        let reset_journal = Arc::new(
            FileConfigMutationJournal::create(&reset_directory, resumed_secret.binding()).unwrap(),
        );
        runtime
            .block_on(client.ensure_no_pending_config_mutation(reset_journal.clone()))
            .unwrap();
        let authorizer = Arc::new(SyntheticNativeAuthorization {
            repository,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let prepared_reset = runtime
            .block_on(client.prepare_password_reset(
                Zeroizing::new(b"sdk recovered reset password".to_vec()),
                authorizer.clone(),
            ))
            .unwrap();
        assert_eq!(
            authorizer.calls.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let reset_record = prepared_reset
            .record(&resumed_secret.begin_operation().unwrap())
            .unwrap();
        let replacement = EncryptedConfig::parse(reset_record.replacement_bytes()).unwrap();
        assert_eq!(replacement.generation(), 2);
        let before_checkpoint = resumed_secret.checkpoint();
        assert!(matches!(
            runtime
                .block_on(client.publish_config_mutation(&prepared_reset, reset_journal.clone(),))
                .unwrap(),
            ConfigPublicationOutcome::OutcomeUnknown { .. }
        ));
        assert!(
            reset_journal.load(resumed_secret.binding()).unwrap() == Some(reset_record.clone())
        );
        assert!(resumed_secret.checkpoint() == before_checkpoint);
        assert!(
            FileCheckpointStore::open(&checkpoint, resumed_secret.binding())
                .unwrap()
                .load(resumed_secret.binding())
                .unwrap()
                == before_checkpoint
        );
        let before_pending = fs::read(&captures).unwrap();
        assert!(matches!(
            runtime.block_on(client.ensure_no_pending_config_mutation(reset_journal.clone())),
            Err(graft::e2ee::EncryptedHttpError::Encryption(
                graft::e2ee::EncryptionError::StaleObservation
            ))
        ));
        assert_eq!(fs::read(&captures).unwrap(), before_pending);
        runtime
            .block_on(resumed_credentials.clear_encrypted_session("origin"))
            .unwrap();
        drop(reset_journal);
        let binding =
            SessionBinding::new("native.test".into(), "test-account".into(), repository).unwrap();
        let reset_checkpoint = Arc::new(FileCheckpointStore::open(&checkpoint, &binding).unwrap());
        let recovered_secret =
            SecretSession::with_checkpoint_store(binding, reset_checkpoint.clone()).unwrap();
        let recovered_credentials = credentials(
            &config,
            recovered_secret.clone(),
            &private("reset-recovered-staging"),
        );
        let recovered_client = EncryptedHttpClient::new(
            ready["enrollmentUrl"].as_str().unwrap(),
            "origin".into(),
            recovered_credentials.clone(),
            recovered_secret.clone(),
        )
        .unwrap();
        assert!(matches!(
            runtime.block_on(
                recovered_client.unlock(Zeroizing::new(b"sdk synthetic sync password".to_vec()),)
            ),
            Err(graft::e2ee::EncryptedHttpError::Encryption(
                graft::e2ee::EncryptionError::UnlockFailed
            ))
        ));
        runtime
            .block_on(
                recovered_client.unlock(Zeroizing::new(b"sdk recovered reset password".to_vec())),
            )
            .unwrap();
        let reopened_reset = Arc::new(
            FileConfigMutationJournal::open(&reset_directory, recovered_secret.binding()).unwrap(),
        );
        let restored_reset = runtime
            .block_on(recovered_client.restore_config_mutation(reopened_reset.clone()))
            .unwrap();
        assert!(
            restored_reset
                .record(&recovered_secret.begin_operation().unwrap())
                .unwrap()
                == reset_record
        );
        let before_recovery: Vec<serde_json::Value> =
            serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
        assert!(matches!(
            runtime
                .block_on(
                    recovered_client
                        .reconcile_config_mutation(&restored_reset, reopened_reset.clone(),)
                )
                .unwrap(),
            ConfigPublicationOutcome::Confirmed
        ));
        let after_recovery: Vec<serde_json::Value> =
            serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
        assert!(
            after_recovery[before_recovery.len()..]
                .iter()
                .all(|request| request["method"] == "GET")
        );
        assert!(
            reopened_reset
                .load(recovered_secret.binding())
                .unwrap()
                .is_none()
        );
        assert!(
            reopened_reset
                .load_recoverable(recovered_secret.binding())
                .unwrap()
                == Some(reset_record)
        );
        assert!(
            reset_checkpoint.load(recovered_secret.binding()).unwrap()
                == recovered_secret.checkpoint()
        );
        let unwrapped = replacement
            .unlock(b"sdk recovered reset password", Some(repository))
            .unwrap();
        candidate.authenticate(&unwrapped).unwrap();
        // An already-unlocked independent device still authenticates the same root.
        let peer_client = EncryptedHttpClient::new(
            ready["enrollmentUrl"].as_str().unwrap(),
            "origin".into(),
            target_credentials.clone(),
            target_secret,
        )
        .unwrap();
        assert!(
            matches!(runtime.block_on(peer_client.read_ref("refs/heads/main")).unwrap().target(),
            Some(RefTarget::Commit(head)) if head.iter().map(|byte| format!("{byte:02x}")).collect::<String>() == expected)
        );
        let reset_target = private("reset-new-device");
        let device_binding =
            SessionBinding::new("native.test".into(), "test-account".into(), repository).unwrap();
        let device_secret = SecretSession::with_checkpoint_store(
            device_binding,
            Arc::new(
                FileCheckpointStore::create(
                    private("reset-device-checkpoint"),
                    recovered_secret.binding(),
                )
                .unwrap(),
            ),
        )
        .unwrap();
        let device_credentials = credentials(
            &config,
            device_secret.clone(),
            &private("reset-device-staging"),
        );
        let device_client = EncryptedHttpClient::new(
            ready["enrollmentUrl"].as_str().unwrap(),
            "origin".into(),
            device_credentials.clone(),
            device_secret,
        )
        .unwrap();
        runtime
            .block_on(
                device_client.unlock(Zeroizing::new(b"sdk recovered reset password".to_vec())),
            )
            .unwrap();
        let reset_reader = RepositorySession::new_with_remote_credentials(
            &reset_target,
            None,
            device_credentials.clone(),
        );
        reset_reader.open().unwrap();
        reset_reader
            .clone_repository_with_config(config, None)
            .unwrap();
        assert_eq!(
            reset_reader
                .repository_metadata()
                .unwrap()
                .current_head
                .as_deref(),
            Some(expected.as_str())
        );
        assert_eq!(
            fs::read(reset_target.join("initial-private.txt")).unwrap(),
            payload
        );
        let value: String = Connection::open(reset_target.join("initial-private.db"))
            .unwrap()
            .query_row("SELECT value FROM initial_data", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "initial WAL private marker");
        reset_reader.close().unwrap();
        runtime
            .block_on(device_credentials.clear_encrypted_session("origin"))
            .unwrap();
        runtime
            .block_on(recovered_credentials.clear_encrypted_session("origin"))
            .unwrap();
        let final_requests = fs::read_to_string(&captures).unwrap();
        let records: Vec<serde_json::Value> = serde_json::from_str(&final_requests).unwrap();
        assert!(
            records
                .iter()
                .any(|request| request["droppedConfigMutationResponse"] == true)
        );
        assert!(
            records
                .iter()
                .any(|request| request["droppedConfigMutationRead"] == true)
        );
        for marker in [
            "sdk recovered reset password",
            "initial WAL private marker",
            expected.as_str(),
        ] {
            assert!(!final_requests.contains(marker));
            assert!(
                !final_requests.contains(
                    &marker
                        .as_bytes()
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                )
            );
        }
    }
    runtime
        .block_on(target_credentials.clear_encrypted_session("origin"))
        .unwrap();
    runtime
        .block_on(resumed_credentials.clear_encrypted_session("origin"))
        .unwrap();
}

#[test]
#[ignore = "requires built TypeScript service and loopback networking; pnpm test:e2ee-interop"]
fn encrypted_sqlite_large_snapshot_streams_multipart_and_clones_beyond_read_budget() {
    use graft::e2ee::{FileMultipartJournalProvider, FileRefMutationJournalProvider};
    use std::sync::Arc;
    let directory = tempfile::tempdir().unwrap();
    let private = |name: &str| {
        let path = directory.path().join(name);
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    };
    let source = private("source");
    let target = private("target");
    let checkpoint = private("checkpoint");
    let target_checkpoint = private("target-checkpoint");
    let staging = private("staging");
    let target_staging = private("target-staging");
    let journals = private("journals");
    let uploads = private("uploads");
    let uuid = "00010203-0405-4607-8809-0a0b0c0d0e0f";
    let repository =
        RepositoryId::from_bytes([0, 1, 2, 3, 4, 5, 0x46, 7, 0x88, 9, 10, 11, 12, 13, 14, 15])
            .unwrap();
    let root = RootKey::generate(repository).unwrap();
    let candidate = EncryptedConfig::create(&root, b"sdk synthetic sync password").unwrap();
    drop(root);
    let fixture = directory.path().join("fixture.json");
    let captures = directory.path().join("capture.json");
    fs::write(
        &fixture,
        serde_json::to_vec(&serde_json::json!({
            "serviceLifetimeMs": 600000,
            "encryption": {"profile":"encrypted-remote-v1","version":1,"suite":1,"uuid":uuid},
            "objects": [["e2ee/config",candidate.as_bytes()]], "enableMultipart":true,
            "maxRequestBytes":1024*1024, "multipartPartBytes":256*1024,
        }))
        .unwrap(),
    )
    .unwrap();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut service = Service(
        Command::new("node")
            .arg(workspace.join("tests/e2ee/remote-service.mjs"))
            .arg(fixture)
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
    let ready: serde_json::Value = serde_json::from_str(&ready).unwrap();
    let config = RemoteConfig::EncryptedHttp {
        url: ready["url"].as_str().unwrap().into(),
        uuid: uuid.into(),
    };
    let secret = native_session(&checkpoint, repository, &candidate);
    let source_credentials = RemoteCredentials::explicit();
    source_credentials
        .set_http_bearer_token("origin", "account-token".into())
        .unwrap();
    let ref_journals =
        Arc::new(FileRefMutationJournalProvider::create(&journals, secret.binding()).unwrap());
    let multipart =
        Arc::new(FileMultipartJournalProvider::create(&uploads, secret.binding()).unwrap());
    source_credentials
        .set_encrypted_session_with_recovery(
            "origin",
            &config,
            secret,
            EncryptedTransferBudget {
                private_directory: staging,
                max_buffered_plaintext: 4 * 1024 * 1024,
                max_ciphertext_spool: 32 * 1024 * 1024,
            },
            ref_journals,
            multipart,
        )
        .unwrap();
    Repository::init(&source)
        .unwrap()
        .remote_add("origin", config.clone())
        .unwrap();
    let writer = RepositorySession::new_with_remote_credentials(&source, None, source_credentials);
    writer.open().unwrap();
    let connection = Connection::open(source.join("large.db")).unwrap();
    connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE blobs(id INTEGER PRIMARY KEY, payload BLOB NOT NULL); INSERT INTO blobs VALUES(1,randomblob(7*1024*1024+33));").unwrap();
    let first: Vec<u8> = connection
        .query_row("SELECT payload FROM blobs WHERE id=1", [], |row| row.get(0))
        .unwrap();
    writer.add_all().unwrap();
    writer
        .commit("large incompressible encrypted SQLite snapshot")
        .unwrap();
    // Before streaming, this segment exceeded the 4 MiB plaintext budget and
    // failed publication even though multipart and disk budgets allowed it.
    writer.push(Some("origin"), Some("main")).unwrap();
    connection
        .execute_batch("INSERT INTO blobs VALUES(2,randomblob(2*1024*1024+17));")
        .unwrap();
    let second: Vec<u8> = connection
        .query_row("SELECT payload FROM blobs WHERE id=2", [], |row| row.get(0))
        .unwrap();
    writer.add_all().unwrap();
    writer.commit("second committed WAL segment").unwrap();
    writer.push(Some("origin"), Some("main")).unwrap();
    let expected_head = writer.repository_metadata().unwrap().current_head;
    let target_secret = native_session(&target_checkpoint, repository, &candidate);
    let target_credentials = credentials(&config, target_secret, &target_staging);
    let clone = RepositorySession::new_with_remote_credentials(&target, None, target_credentials);
    clone.open().unwrap();
    clone
        .clone_repository_with_config(config, Some("main"))
        .unwrap();
    assert_eq!(
        clone.repository_metadata().unwrap().current_head,
        expected_head
    );
    let restored = Connection::open(target.join("large.db")).unwrap();
    for (id, expected) in [(1, &first), (2, &second)] {
        let actual: Vec<u8> = restored
            .query_row("SELECT payload FROM blobs WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(actual, *expected);
    }
    assert_eq!(
        restored
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    let capture: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(captures).unwrap()).unwrap();
    assert!(capture.iter().any(|request| {
        request["path"]
            .as_str()
            .is_some_and(|path| path.contains("/multipart-start/e2ee/segments/"))
    }));
    assert!(capture.iter().any(|request| request["partNumber"] == "2"));
    assert!(capture.iter().any(|request| {
        request["path"]
            .as_str()
            .is_some_and(|path| path.contains("/raw/e2ee/segments/"))
            && request["responseStatus"] == 206
    }));
}

fn native_session(
    directory: &Path,
    repository: RepositoryId,
    config: &EncryptedConfig,
) -> SecretSession {
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    let binding =
        SessionBinding::new("native.test".into(), "test-account".into(), repository).unwrap();
    let store = FileCheckpointStore::create(directory, &binding).unwrap();
    let session =
        SecretSession::with_checkpoint_store(binding, std::sync::Arc::new(store)).unwrap();
    // Each device independently unwraps the fixture; no source object cache or
    // shared secret-session handle is available to the target.
    let root = config
        .unlock(b"sdk synthetic sync password", Some(repository))
        .unwrap();
    session.unlock_with_root(config, root).unwrap();
    session
}

fn credentials(config: &RemoteConfig, session: SecretSession, staging: &Path) -> RemoteCredentials {
    fs::set_permissions(staging, fs::Permissions::from_mode(0o700)).unwrap();
    let credentials = RemoteCredentials::explicit();
    credentials
        .set_http_bearer_token("origin", "account-token".into())
        .unwrap();
    credentials
        .set_encrypted_session(
            "origin",
            config,
            session,
            EncryptedTransferBudget {
                private_directory: staging.to_path_buf(),
                max_buffered_plaintext: 4 * 1024 * 1024,
                max_ciphertext_spool: 16 * 1024 * 1024,
            },
        )
        .unwrap();
    credentials
}

fn durable_credentials(
    config: &RemoteConfig,
    session: SecretSession,
    staging: &Path,
    journals: &Path,
    create: bool,
) -> RemoteCredentials {
    fs::set_permissions(staging, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(journals, fs::Permissions::from_mode(0o700)).unwrap();
    let credentials = RemoteCredentials::explicit();
    let provider = if create {
        graft::e2ee::FileRefMutationJournalProvider::create(journals, session.binding())
    } else {
        graft::e2ee::FileRefMutationJournalProvider::open(journals, session.binding())
    }
    .unwrap();
    credentials
        .set_encrypted_session_with_journals(
            "origin",
            config,
            session,
            EncryptedTransferBudget {
                private_directory: staging.to_owned(),
                max_buffered_plaintext: 4 * 1024 * 1024,
                max_ciphertext_spool: 16 * 1024 * 1024,
            },
            std::sync::Arc::new(provider),
        )
        .unwrap();
    credentials
        .set_bound_http_bearer_token(
            "origin",
            "native.test",
            "test-account",
            "account-token".into(),
        )
        .unwrap();
    credentials
}

#[test]
#[ignore = "requires built TypeScript service and loopback networking; pnpm test:e2ee-interop"]
fn encrypted_sdk_wal_snapshot_fetch_and_offline_apply_interoperate() {
    let fixture_directory = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let source_checkpoint = tempfile::tempdir().unwrap();
    let target_checkpoint = tempfile::tempdir().unwrap();
    let source_staging = tempfile::tempdir().unwrap();
    let target_staging = tempfile::tempdir().unwrap();
    let cloned_directory = tempfile::tempdir().unwrap();
    let cloned_checkpoint = tempfile::tempdir().unwrap();
    let cloned_staging = tempfile::tempdir().unwrap();
    let uuid = "00010203-0405-4607-8809-0a0b0c0d0e0f";
    let repository =
        RepositoryId::from_bytes([0, 1, 2, 3, 4, 5, 0x46, 7, 0x88, 9, 10, 11, 12, 13, 14, 15])
            .unwrap();
    let root = RootKey::import(repository, Zeroizing::new(std::array::from_fn(|i| i as u8)));
    let encrypted_config = EncryptedConfig::create(&root, b"sdk synthetic sync password").unwrap();
    drop(root);
    let migration_uuid = "10010203-0405-4607-8809-0a0b0c0d0e0f";
    let migration_repository =
        RepositoryId::from_bytes([16, 1, 2, 3, 4, 5, 0x46, 7, 0x88, 9, 10, 11, 12, 13, 14, 15])
            .unwrap();
    let migration_root = RootKey::generate(migration_repository).unwrap();
    let migration_config =
        EncryptedConfig::create(&migration_root, b"sdk synthetic sync password").unwrap();
    drop(migration_root);
    assert_ne!(migration_config.repository(), encrypted_config.repository());
    // Rebinding the source key bytes to the new UUID still cannot authenticate
    // its config: the destination has a genuinely different root, not just ID.
    let wrong_root = RootKey::import(
        migration_repository,
        Zeroizing::new(std::array::from_fn(|i| i as u8)),
    );
    assert!(migration_config.authenticate(&wrong_root).is_err());
    drop(wrong_root);
    let fixture = fixture_directory.path().join("fixture.json");
    let captures = fixture_directory.path().join("requests.json");
    fs::write(
        &fixture,
        serde_json::to_vec(&serde_json::json!({
            "serviceLifetimeMs": 600000,
            "encryption": { "profile":"encrypted-remote-v1", "version":1, "suite":1, "uuid":uuid },
            "objects": [["e2ee/config", encrypted_config.as_bytes()]],
            "migrationEncryption": { "profile":"encrypted-remote-v1", "version":1,
                "suite":1, "uuid":migration_uuid },
        }))
        .unwrap(),
    )
    .unwrap();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut service = Service(
        Command::new("node")
            .arg(workspace.join("tests/e2ee/remote-service.mjs"))
            .arg(&fixture)
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
    let ready: serde_json::Value = serde_json::from_str(&ready).unwrap();
    let config = RemoteConfig::EncryptedHttp {
        url: ready["integrationUrl"].as_str().unwrap().into(),
        uuid: uuid.into(),
    };
    let source_secret = native_session(source_checkpoint.path(), repository, &encrypted_config);
    let target_secret = native_session(target_checkpoint.path(), repository, &encrypted_config);
    let source_mutation_journals = tempfile::tempdir().unwrap();
    let source_credentials = durable_credentials(
        &config,
        source_secret.clone(),
        source_staging.path(),
        source_mutation_journals.path(),
        true,
    );
    let target_credentials = credentials(&config, target_secret.clone(), target_staging.path());
    for directory in [source.path(), target.path()] {
        Repository::init(directory)
            .unwrap()
            .remote_add("origin", config.clone())
            .unwrap();
    }
    let writer = RepositorySession::new_with_remote_credentials(
        source.path(),
        None,
        source_credentials.clone(),
    );
    let reader = RepositorySession::new_with_remote_credentials(
        target.path(),
        None,
        target_credentials.clone(),
    );
    writer.open().unwrap();
    reader.open().unwrap();
    let database = source.path().join("sdk-private.db");
    let connection = Connection::open(&database).unwrap();
    connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE private_items(id INTEGER PRIMARY KEY, value TEXT); INSERT INTO private_items VALUES(1,'sdk first WAL private marker');").unwrap();
    let payload = vec![73_u8; 1024 * 1024 + 33];
    fs::write(source.path().join("sdk-private.bin"), &payload).unwrap();
    assert!(
        fs::metadata(source.path().join("sdk-private.db-wal"))
            .unwrap()
            .len()
            > 32
    );
    writer.add_all().unwrap();
    writer.commit("sdk first encrypted private commit").unwrap();
    writer.push(Some("origin"), Some("main")).unwrap();
    let first = writer.repository_metadata().unwrap().current_head.unwrap();
    let initial = reader.fetch_for_checkout("origin", "main").unwrap();
    assert_eq!(initial["head"], first);
    assert!(!target.path().join("sdk-private.db").exists());
    assert!(!target.path().join("sdk-private.bin").exists());
    assert!(reader.repository_metadata().unwrap().current_head.is_none());
    let initial_plan = reader
        .plan_merge(&PlanMergeOptions {
            revision: first.clone(),
            expected_head: None,
        })
        .unwrap();
    reader
        .apply_merge(&ApplyMergeOptions {
            revision: first.clone(),
            expected_head: None,
            plan_token: initial_plan.plan_token,
        })
        .unwrap();
    connection
        .execute_batch("INSERT INTO private_items VALUES(2,'sdk second WAL private marker');")
        .unwrap();
    writer.add_all().unwrap();
    writer
        .commit("sdk second encrypted private commit")
        .unwrap();
    writer.reopen().unwrap();
    writer.push(Some("origin"), Some("main")).unwrap();
    let second = writer.repository_metadata().unwrap().current_head.unwrap();
    assert!(
        fs::metadata(source.path().join("sdk-private.db-wal"))
            .unwrap()
            .len()
            > 32
    );
    assert_eq!(
        writer.push(Some("origin"), Some("main")).unwrap()["commits"],
        0
    );
    // A separate target receives historical storage exclusively through the
    // frozen migration phase, never through writer.push or a source cache.
    let storage_config = RemoteConfig::EncryptedHttp {
        url: ready["migrationUrl"].as_str().unwrap().into(),
        uuid: migration_uuid.into(),
    };
    let storage_staging = tempfile::tempdir().unwrap();
    let storage_journals = tempfile::tempdir().unwrap();
    let migration_checkpoint = tempfile::tempdir().unwrap();
    let migration_secret = native_session(
        migration_checkpoint.path(),
        migration_repository,
        &migration_config,
    );
    let storage_credentials = durable_credentials(
        &storage_config,
        migration_secret.clone(),
        storage_staging.path(),
        storage_journals.path(),
        true,
    );
    let storage_remote = std::sync::Arc::new(
        storage_config
            .clone()
            .build_with_credentials("origin", &storage_credentials)
            .unwrap(),
    );
    let migration_source_repo = Repository::open(source.path()).unwrap();
    migration_source_repo
        .tag_create("migration-light-source", Some(&first))
        .unwrap();
    let migration_annotated = migration_source_repo
        .tag_create_annotated(
            "migration-annotated-source",
            Some(&second),
            "migration annotation private marker",
        )
        .unwrap();
    let frozen = writer
        .plan_migration_source(
            graft::repo::MigrationRefSelection::All,
            graft::repo::MigrationPlanningBudget::default(),
        )
        .unwrap();
    let budget = graft::repo::MigrationVerificationBudget::default();
    let migration_ref = "refs/heads/migration-history";
    let mappings = frozen
        .references()
        .keys()
        .enumerate()
        .map(|(index, source)| {
            (
                source.clone(),
                if source.starts_with("refs/tags/") {
                    format!("refs/tags/migration-target-{index}")
                } else if source == "refs/heads/main" {
                    migration_ref.to_owned()
                } else {
                    format!("refs/heads/migration-retained-{index}")
                },
            )
        })
        .collect();
    let target_plan = writer
        .plan_migration_target_refs(
            &frozen,
            mappings,
            graft::repo::Head::branch("migration-history"),
        )
        .unwrap();
    // Retain source, mapping and exact config before publishing to an empty
    // target with a fresh UUID/root. No ordinary push populates this remote.
    let source_intent = tempfile::tempdir().unwrap();
    let target_intent = tempfile::tempdir().unwrap();
    let enrollment_intent = tempfile::tempdir().unwrap();
    let initialization_directory = tempfile::tempdir().unwrap();
    for directory in [
        &source_intent,
        &target_intent,
        &enrollment_intent,
        &initialization_directory,
    ] {
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    }
    writer
        .create_migration_source_journal(source_intent.path(), migration_secret.binding(), &frozen)
        .unwrap();
    writer
        .create_migration_target_journal(
            target_intent.path(),
            migration_secret.binding(),
            &target_plan,
        )
        .unwrap();
    let initialization = std::sync::Arc::new(
        graft::e2ee::FileInitializationJournal::create(
            initialization_directory.path(),
            migration_secret.binding(),
            &migration_config,
        )
        .unwrap(),
    );
    writer
        .create_migration_enrollment_journal(
            enrollment_intent.path(),
            target_intent.path(),
            &migration_secret,
            initialization.as_ref(),
        )
        .unwrap();
    let guarded = writer
        .migration_initialization_journal(
            enrollment_intent.path(),
            target_intent.path(),
            migration_secret.clone(),
            initialization,
        )
        .unwrap();
    let migration_runtime = tokio::runtime::Runtime::new().unwrap();
    let initializer = graft::e2ee::EncryptedHttpClient::new(
        ready["migrationUrl"].as_str().unwrap(),
        "origin".into(),
        storage_credentials,
        migration_secret.clone(),
    )
    .unwrap();
    assert!(matches!(
        migration_runtime
            .block_on(initializer.publish_initial_config(guarded))
            .unwrap(),
        graft::e2ee::InitialConfigPublicationOutcome::Confirmed
    ));
    // Continue with authenticated retained intent after enrollment.
    let reopened_initialization = graft::e2ee::FileInitializationJournal::open(
        initialization_directory.path(),
        migration_secret.binding(),
    )
    .unwrap();
    let retained_source = writer
        .restore_migration_source_journal(source_intent.path(), migration_secret.binding())
        .unwrap();
    assert_eq!(
        retained_source.encode_private().unwrap(),
        frozen.encode_private().unwrap()
    );
    let retained_target = writer
        .restore_migration_enrollment_journal(
            enrollment_intent.path(),
            target_intent.path(),
            &migration_secret,
            &reopened_initialization,
        )
        .unwrap();
    assert_eq!(
        retained_target.encode_private().unwrap(),
        target_plan.encode_private().unwrap()
    );
    let frozen = retained_source;
    let target_plan = retained_target;
    let before_storage = fs::read(&captures).unwrap();
    assert!(
        writer
            .publish_migration_dependencies(
                &frozen,
                storage_remote.clone(),
                graft::repo::MigrationPublicationBudget {
                    verification: budget,
                    max_storage_batch_pages: 2,
                    max_pack_bytes: 0,
                }
            )
            .is_err()
    );
    assert!(
        writer
            .publish_migration_storage_dependencies(&frozen, storage_remote.clone(), budget, 0)
            .is_err()
    );
    assert!(
        writer
            .publish_migration_storage_dependencies(
                &frozen,
                storage_remote.clone(),
                graft::repo::MigrationVerificationBudget { max_storage_pages: 0, ..budget },
                64
            )
            .is_err()
    );
    assert!(
        writer
            .publish_migration_storage_dependencies(&frozen, storage_remote.clone(), budget, 1)
            .is_err()
    );
    assert_eq!(fs::read(&captures).unwrap(), before_storage);
    for _ in 0..2 {
        let published = writer
            .publish_migration_dependencies(
                &frozen,
                storage_remote.clone(),
                graft::repo::MigrationPublicationBudget {
                    verification: budget,
                    max_storage_batch_pages: 2,
                    max_pack_bytes: 1024,
                },
            )
            .unwrap();
        assert_eq!(published.repository_objects, frozen.objects().len());
        let verified = published.verified;
        assert!(verified.snapshots >= 2);
        assert!(verified.storage_commits >= 2);
        assert!(verified.storage_pages >= 2);
    }
    let storage_checkpoint = tempfile::tempdir().unwrap();
    let storage_reader_staging = tempfile::tempdir().unwrap();
    let storage_secret = native_session(
        storage_checkpoint.path(),
        migration_repository,
        &migration_config,
    );
    let storage_reader_credentials = credentials(
        &storage_config,
        storage_secret,
        storage_reader_staging.path(),
    );
    let storage_reader_remote = std::sync::Arc::new(
        storage_config
            .clone()
            .build_with_credentials("origin", &storage_reader_credentials)
            .unwrap(),
    );
    let independent_storage =
        graft::setup::setup_graft_temporary(RemoteConfig::Memory, None).unwrap();
    for snapshot in frozen.snapshots().values() {
        let mut target_snapshot = graft::snapshot::Snapshot::empty();
        target_snapshot.page_count = snapshot.page_count;
        for range in &snapshot.ranges {
            target_snapshot.append(range.log.clone(), range.start..=range.end);
        }
        independent_storage
            .snapshot_hydrate_from(target_snapshot, storage_reader_remote.clone())
            .unwrap();
        for range in &snapshot.ranges {
            for commit in &range.commits {
                assert_eq!(
                    independent_storage
                        .recompute_commit_hash(&range.log, commit.lsn)
                        .unwrap()
                        .as_ref(),
                    Some(&commit.commit_hash)
                );
            }
        }
    }
    // Use the frozen target mapping and registered durable publisher after
    // complete dependency transfer, without changing source refs or stages.
    let before_unmapped = fs::read(&captures).unwrap();
    assert!(
        writer
            .publish_migration_target_branch(
                &target_plan,
                storage_remote.clone(),
                "refs/heads/migration-unmapped"
            )
            .is_err()
    );
    assert_eq!(fs::read(&captures).unwrap(), before_unmapped);
    assert!(
        writer
            .publish_migration_target_tag(
                &target_plan,
                storage_remote.clone(),
                "refs/tags/migration-unmapped-tag"
            )
            .is_err()
    );
    assert_eq!(fs::read(&captures).unwrap(), before_unmapped);
    let reader_runtime = tokio::runtime::Runtime::new().unwrap();
    for tag in target_plan
        .references()
        .values()
        .filter(|reference| reference.target_ref.starts_with("refs/tags/"))
    {
        for _ in 0..2 {
            writer
                .publish_migration_target_tag(&target_plan, storage_remote.clone(), &tag.target_ref)
                .unwrap();
        }
        let observed = reader_runtime
            .block_on(storage_reader_remote.observe_ref(&tag.target_ref))
            .unwrap();
        let bytes = reader_runtime
            .block_on(storage_reader_remote.read_observed_tag_object(&tag.target_ref, &observed))
            .unwrap()
            .unwrap();
        let expected = migration_source_repo
            .read_object(tag.object.as_ref().unwrap().as_str())
            .unwrap()
            .canonical_bytes();
        assert_eq!(bytes.as_ref(), expected.as_slice());
    }
    let light_tag = target_plan
        .references()
        .values()
        .find(|tag| tag.target_ref.starts_with("refs/tags/") && !tag.annotated_tag)
        .unwrap();
    let observed = reader_runtime
        .block_on(storage_remote.observe_ref(&light_tag.target_ref))
        .unwrap();
    reader_runtime
        .block_on(
            storage_remote.publish_observed_tag_ref(
                &light_tag.target_ref,
                &observed,
                migration_source_repo
                    .read_object(&second)
                    .unwrap()
                    .canonical_bytes()
                    .into(),
            ),
        )
        .unwrap();
    let before_foreign_tag: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
    assert!(
        writer
            .publish_migration_target_tag(
                &target_plan,
                storage_remote.clone(),
                &light_tag.target_ref
            )
            .is_err()
    );
    let after_foreign_tag: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
    assert_eq!(
        before_foreign_tag
            .iter()
            .filter(|row| row["method"] == "POST")
            .count(),
        after_foreign_tag
            .iter()
            .filter(|row| row["method"] == "POST")
            .count()
    );
    let observed = reader_runtime
        .block_on(storage_remote.observe_ref(&light_tag.target_ref))
        .unwrap();
    assert_eq!(
        observed.bytes().unwrap().as_ref(),
        format!("{second}\n").as_bytes()
    );
    reader_runtime
        .block_on(
            storage_remote.publish_observed_tag_ref(
                &light_tag.target_ref,
                &observed,
                migration_source_repo
                    .read_object(&first)
                    .unwrap()
                    .canonical_bytes()
                    .into(),
            ),
        )
        .unwrap();
    let before_missing_head: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
    assert!(
        writer
            .publish_migration_target_head(&target_plan, storage_remote.clone())
            .is_err()
    );
    let after_missing_head: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
    assert_eq!(
        before_missing_head
            .iter()
            .filter(|row| row["method"] == "POST")
            .count(),
        after_missing_head
            .iter()
            .filter(|row| row["method"] == "POST")
            .count()
    );
    for _ in 0..2 {
        writer
            .publish_migration_target_branch(&target_plan, storage_remote.clone(), migration_ref)
            .unwrap();
    }
    for _ in 0..2 {
        writer
            .publish_migration_target_head(&target_plan, storage_remote.clone())
            .unwrap();
    }
    let unregistered = credentials(&storage_config, migration_secret, storage_staging.path());
    let unregistered = std::sync::Arc::new(
        storage_config
            .clone()
            .build_with_credentials("origin", &unregistered)
            .unwrap(),
    );
    let before_unregistered = fs::read(&captures).unwrap();
    assert!(
        writer
            .publish_migration_target_branch(&target_plan, unregistered.clone(), migration_ref)
            .is_err()
    );
    assert!(
        writer
            .publish_migration_target_tag(&target_plan, unregistered.clone(), &light_tag.target_ref)
            .is_err()
    );
    assert!(
        writer
            .publish_migration_target_refs(&target_plan, unregistered.clone())
            .is_err()
    );
    assert!(
        writer
            .publish_migration_target_head(&target_plan, unregistered)
            .is_err()
    );
    assert_eq!(fs::read(&captures).unwrap(), before_unregistered);
    let foreign_branch = target_plan
        .references()
        .keys()
        .find(|name| name.starts_with("refs/heads/") && name.as_str() != migration_ref)
        .unwrap();
    let publication_runtime = tokio::runtime::Runtime::new().unwrap();
    let foreign_observed = publication_runtime
        .block_on(storage_remote.observe_ref(foreign_branch))
        .unwrap();
    publication_runtime
        .block_on(storage_remote.publish_observed_branch_ref(
            foreign_branch,
            &foreign_observed,
            &first,
        ))
        .unwrap();
    let before_foreign: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
    assert!(
        writer
            .publish_migration_target_branch(&target_plan, storage_remote.clone(), foreign_branch)
            .is_err()
    );
    let after_foreign: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
    assert_eq!(
        before_foreign
            .iter()
            .filter(|row| row["method"] == "POST")
            .count(),
        after_foreign
            .iter()
            .filter(|row| row["method"] == "POST")
            .count()
    );
    assert_eq!(
        publication_runtime
            .block_on(storage_remote.observe_ref(foreign_branch))
            .unwrap()
            .bytes()
            .unwrap()
            .as_ref(),
        format!("{first}\n").as_bytes()
    );
    let head_observed = publication_runtime
        .block_on(storage_remote.observe_ref("HEAD"))
        .unwrap();
    publication_runtime
        .block_on(storage_remote.publish_observed_head_ref(
            &head_observed,
            &graft::repo::Head::Detached { commit: first.clone() },
        ))
        .unwrap();
    let before_foreign_head: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
    assert!(
        writer
            .publish_migration_target_head(&target_plan, storage_remote.clone())
            .is_err()
    );
    let after_foreign_head: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&captures).unwrap()).unwrap();
    assert_eq!(
        before_foreign_head
            .iter()
            .filter(|row| row["method"] == "POST")
            .count(),
        after_foreign_head
            .iter()
            .filter(|row| row["method"] == "POST")
            .count()
    );
    let head_observed = publication_runtime
        .block_on(storage_remote.observe_ref("HEAD"))
        .unwrap();
    publication_runtime
        .block_on(storage_remote.publish_observed_head_ref(
            &head_observed,
            &graft::repo::Head::branch("migration-history"),
        ))
        .unwrap();
    let foreign_observed = publication_runtime
        .block_on(storage_remote.observe_ref(foreign_branch))
        .unwrap();
    publication_runtime
        .block_on(storage_remote.compare_and_delete_observed_ref(foreign_branch, &foreign_observed))
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            writer
                .publish_migration_target_refs(&target_plan, storage_remote.clone())
                .unwrap(),
            target_plan.references().len()
        );
    }
    for target in target_plan.references().values() {
        let observed = reader_runtime
            .block_on(storage_reader_remote.observe_ref(&target.target_ref))
            .unwrap();
        let expected = target
            .object
            .as_ref()
            .map(|id| format!("{}\n", id.as_str()));
        assert_eq!(
            observed.bytes().map(|bytes| bytes.as_ref()),
            expected.as_ref().map(|value| value.as_bytes())
        );
    }
    let restored_directory = tempfile::tempdir().unwrap();
    let restored = RepositorySession::new_with_remote_credentials(
        restored_directory.path(),
        None,
        storage_reader_credentials,
    );
    restored.open().unwrap();
    restored
        .clone_repository_with_config(storage_config, None)
        .unwrap();
    assert_eq!(
        fs::read(restored_directory.path().join("sdk-private.bin")).unwrap(),
        payload
    );
    let count: i64 = Connection::open(restored_directory.path().join("sdk-private.db"))
        .unwrap()
        .query_row("SELECT count(*) FROM private_items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
    restored
        .restore(&graft_sdk::RestoreOptions {
            source: Some(first.clone()),
            expected_head: Some(second.clone()),
            require_clean: true,
            path: "sdk-private.db".into(),
        })
        .unwrap();
    let count: i64 = Connection::open(restored_directory.path().join("sdk-private.db"))
        .unwrap()
        .query_row("SELECT count(*) FROM private_items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    restored.close().unwrap();
    drop(independent_storage);
    drop(storage_reader_remote);
    drop(storage_remote);
    reader.reopen().unwrap();
    let fetched = reader.fetch_for_checkout("origin", "main").unwrap();
    assert_eq!(fetched["head"], second);
    let cloned_secret = native_session(cloned_checkpoint.path(), repository, &encrypted_config);
    let cloned_credentials = credentials(&config, cloned_secret.clone(), cloned_staging.path());
    let RemoteConfig::EncryptedHttp { url, .. } = &config else {
        unreachable!()
    };
    for (credentials, rejected_config) in [
        (RemoteCredentials::explicit(), config.clone()),
        (
            cloned_credentials.clone(),
            RemoteConfig::Http { url: url.clone(), token_env: None },
        ),
        (
            cloned_credentials.clone(),
            RemoteConfig::EncryptedHttp {
                url: url.clone(),
                uuid: "00010203-0405-4607-8809-0a0b0c0d0e10".into(),
            },
        ),
    ] {
        let rejected_directory = tempfile::tempdir().unwrap();
        let rejected = RepositorySession::new_with_remote_credentials(
            rejected_directory.path(),
            None,
            credentials,
        );
        rejected.open().unwrap();
        let before = fs::read(&captures).unwrap();
        assert!(
            rejected
                .clone_repository_with_config(rejected_config, Some("main"))
                .is_err()
        );
        assert_eq!(fs::read(&captures).unwrap(), before);
        assert!(!rejected_directory.path().join(".graft").exists());
        rejected.close().unwrap();
    }
    let cloned = RepositorySession::new_with_remote_credentials(
        cloned_directory.path(),
        None,
        cloned_credentials.clone(),
    );
    cloned.open().unwrap();
    assert!(!cloned_directory.path().join(".graft").exists());
    cloned
        .clone_repository_with_config(config.clone(), None)
        .unwrap();
    assert_eq!(
        cloned
            .repository_metadata()
            .unwrap()
            .current_head
            .as_deref(),
        Some(second.as_str())
    );
    Connection::open(cloned_directory.path().join("sdk-private.db"))
        .unwrap()
        .execute_batch(
            "UPDATE private_items SET value='sdk local divergent private marker' WHERE id=1;",
        )
        .unwrap();
    cloned.add_all().unwrap();
    cloned.commit("sdk local divergent private commit").unwrap();
    let local_head = cloned.repository_metadata().unwrap().current_head.unwrap();
    connection
        .execute_batch(
            "UPDATE private_items SET value='sdk remote divergent private marker' WHERE id=2;",
        )
        .unwrap();
    writer.add_all().unwrap();
    writer
        .commit("sdk remote divergent private commit")
        .unwrap();
    writer.push(Some("origin"), Some("main")).unwrap();
    let remote_head = writer.repository_metadata().unwrap().current_head.unwrap();
    assert_eq!(
        cloned.fetch_for_checkout("origin", "main").unwrap()["head"],
        remote_head
    );
    let divergent_plan = cloned
        .plan_merge(&PlanMergeOptions {
            revision: remote_head.clone(),
            expected_head: Some(local_head.clone()),
        })
        .unwrap();
    assert_eq!(divergent_plan.kind, graft_sdk::MergePlanKind::ThreeWay);
    let applied = cloned
        .apply_merge(&ApplyMergeOptions {
            revision: remote_head.clone(),
            expected_head: Some(local_head.clone()),
            plan_token: divergent_plan.plan_token,
        })
        .unwrap();
    assert!(matches!(
        applied.merge,
        graft_sdk::MergeStatus::Merging { unmerged_count: 0, .. }
    ));
    cloned.reopen().unwrap();
    let graft_sdk::MergeStatus::Merging { state_token, unmerged_count: 0, .. } =
        cloned.get_merge_status().unwrap()
    else {
        panic!("expected a ready, automatically merged SQLite candidate");
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime
        .block_on(cloned_credentials.clear_encrypted_session("origin"))
        .unwrap();
    let before_completion = fs::read(&captures).unwrap();
    let completed = cloned
        .continue_merge(&graft_sdk::ContinueMergeOptions {
            message: "sdk divergent encrypted merge private commit".into(),
            expected_state_token: state_token,
        })
        .unwrap();
    assert_eq!(fs::read(&captures).unwrap(), before_completion);
    assert_eq!(completed.merge, graft_sdk::MergeStatus::None);
    let merged_head = cloned.repository_metadata().unwrap().current_head.unwrap();
    let merged_commit = Repository::open(cloned_directory.path())
        .unwrap()
        .read_commit(&merged_head)
        .unwrap();
    assert_eq!(
        merged_commit.parents,
        vec![local_head.clone(), remote_head.clone()]
    );
    let merged_database = Connection::open(cloned_directory.path().join("sdk-private.db")).unwrap();
    let merged_rows: Vec<String> = merged_database
        .prepare("SELECT value FROM private_items ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        merged_rows,
        [
            "sdk local divergent private marker",
            "sdk remote divergent private marker"
        ]
    );
    drop(merged_database);
    let reopened_root = encrypted_config
        .unlock(b"sdk synthetic sync password", Some(repository))
        .unwrap();
    cloned_secret.reconnect().unwrap();
    cloned_secret
        .unlock_with_root(&encrypted_config, reopened_root)
        .unwrap();
    cloned_credentials
        .set_encrypted_session(
            "origin",
            &config,
            cloned_secret.clone(),
            EncryptedTransferBudget {
                private_directory: cloned_staging.path().to_path_buf(),
                max_buffered_plaintext: 4 * 1024 * 1024,
                max_ciphertext_spool: 16 * 1024 * 1024,
            },
        )
        .unwrap();
    cloned.push(Some("origin"), Some("main")).unwrap();
    let (conflict_local_head, conflict_remote_head, resolved_head) = {
        let recovered_directory = tempfile::tempdir().unwrap();
        let recovered_checkpoint = tempfile::tempdir().unwrap();
        let recovered_staging = tempfile::tempdir().unwrap();
        let recovered_secret =
            native_session(recovered_checkpoint.path(), repository, &encrypted_config);
        let recovered_credentials =
            credentials(&config, recovered_secret, recovered_staging.path());
        let recovered = RepositorySession::new_with_remote_credentials(
            recovered_directory.path(),
            None,
            recovered_credentials.clone(),
        );
        recovered.open().unwrap();
        recovered
            .clone_repository_with_config(config.clone(), None)
            .unwrap();
        assert_eq!(
            recovered
                .repository_metadata()
                .unwrap()
                .current_head
                .as_deref(),
            Some(merged_head.as_str())
        );
        let recovered_repository = Repository::open(recovered_directory.path()).unwrap();
        assert_eq!(
            recovered_repository
                .read_commit(&merged_head)
                .unwrap()
                .parents,
            vec![local_head.clone(), remote_head.clone()]
        );
        for id in [&first, &second, &local_head, &remote_head] {
            assert_eq!(recovered_repository.read_commit(id).unwrap().id, *id);
        }
        let database = Connection::open(recovered_directory.path().join("sdk-private.db")).unwrap();
        let rows: Vec<String> = database
            .prepare("SELECT value FROM private_items ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows, merged_rows);
        assert_eq!(
            fs::read(recovered_directory.path().join("sdk-private.bin")).unwrap(),
            payload
        );
        drop(database);
        Connection::open(cloned_directory.path().join("sdk-private.db")).unwrap()
            .execute_batch("UPDATE private_items SET value='sdk conflict local row one' WHERE id=1; UPDATE private_items SET value='sdk conflict local row two' WHERE id=2;").unwrap();
        cloned.add_all().unwrap();
        cloned
            .commit("sdk conflicting local private commit")
            .unwrap();
        let conflict_local_head = cloned.repository_metadata().unwrap().current_head.unwrap();
        Connection::open(recovered_directory.path().join("sdk-private.db")).unwrap()
            .execute_batch("UPDATE private_items SET value='sdk conflict remote row one' WHERE id=1; UPDATE private_items SET value='sdk conflict remote row two' WHERE id=2;").unwrap();
        recovered.add_all().unwrap();
        recovered
            .commit("sdk conflicting remote private commit")
            .unwrap();
        recovered.push(Some("origin"), Some("main")).unwrap();
        let conflict_remote_head = recovered
            .repository_metadata()
            .unwrap()
            .current_head
            .unwrap();
        cloned.fetch_for_checkout("origin", "main").unwrap();
        let conflict_plan = cloned
            .plan_merge(&PlanMergeOptions {
                revision: conflict_remote_head.clone(),
                expected_head: Some(conflict_local_head.clone()),
            })
            .unwrap();
        let applied = cloned
            .apply_merge(&ApplyMergeOptions {
                revision: conflict_remote_head.clone(),
                expected_head: Some(conflict_local_head.clone()),
                plan_token: conflict_plan.plan_token,
            })
            .unwrap();
        let graft_sdk::MergeStatus::Merging { state_token, unmerged_count: 1, .. } = applied.merge
        else {
            panic!("expected an unresolved SQLite path with two conflicting rows");
        };
        runtime
            .block_on(cloned_credentials.clear_encrypted_session("origin"))
            .unwrap();
        let before_resolution = fs::read(&captures).unwrap();
        let conflicts = cloned
            .list_merge_conflicts(&graft_sdk::ListMergeConflictsOptions {
                path: "sdk-private.db".into(),
                limit: 10,
                after: None,
                expected_state_token: state_token.clone(),
            })
            .unwrap();
        assert_eq!(conflicts.items.len(), 2);
        assert!(
            conflicts
                .items
                .iter()
                .all(|item| item["kind"] == "row" && item["status"] == "unresolved")
        );
        let first_resolution = cloned
            .resolve_merge_row(&graft_sdk::ResolveMergeRowOptions {
                path: "sdk-private.db".into(),
                table: "private_items".into(),
                identity: serde_json::json!(1),
                result: graft_sdk::MergePathResult::Ours,
                expected_state_token: state_token.clone(),
            })
            .unwrap();
        let graft_sdk::MergeStatus::Merging {
            state_token: partial_token,
            unmerged_count: 1,
            ..
        } = first_resolution.merge
        else {
            panic!("expected one unresolved row after the first choice");
        };
        cloned.reopen().unwrap();
        let graft_sdk::MergeStatus::Merging { state_token: reopened_token, .. } =
            cloned.get_merge_status().unwrap()
        else {
            panic!("expected durable partial row resolution");
        };
        assert_eq!(reopened_token, partial_token);
        let partial = cloned
            .list_merge_conflicts(&graft_sdk::ListMergeConflictsOptions {
                path: "sdk-private.db".into(),
                limit: 10,
                after: None,
                expected_state_token: reopened_token.clone(),
            })
            .unwrap();
        assert_eq!(
            partial
                .items
                .iter()
                .filter(|item| item["status"] == "resolved")
                .count(),
            1
        );
        let stale = cloned
            .resolve_merge_row(&graft_sdk::ResolveMergeRowOptions {
                path: "sdk-private.db".into(),
                table: "private_items".into(),
                identity: serde_json::json!(2),
                result: graft_sdk::MergePathResult::Theirs,
                expected_state_token: state_token,
            })
            .unwrap_err();
        assert_eq!(stale.code(), graft_sdk::SdkErrorCode::RepositoryStale);
        let cancelled = graft_sdk::CancellationToken::new();
        cancelled.cancel();
        let cancelled_error = graft_sdk::with_cancellation(&cancelled, || {
            cloned.resolve_merge_row(&graft_sdk::ResolveMergeRowOptions {
                path: "sdk-private.db".into(),
                table: "private_items".into(),
                identity: serde_json::json!(2),
                result: graft_sdk::MergePathResult::Theirs,
                expected_state_token: reopened_token.clone(),
            })
        })
        .unwrap_err();
        assert_eq!(cancelled_error.code(), graft_sdk::SdkErrorCode::Cancelled);
        let graft_sdk::MergeStatus::Merging { state_token: unchanged_token, .. } =
            cloned.get_merge_status().unwrap()
        else {
            panic!("expected unchanged partial merge after cancellation");
        };
        assert_eq!(unchanged_token, reopened_token);
        let second_resolution = cloned
            .resolve_merge_row(&graft_sdk::ResolveMergeRowOptions {
                path: "sdk-private.db".into(),
                table: "private_items".into(),
                identity: serde_json::json!(2),
                result: graft_sdk::MergePathResult::Theirs,
                expected_state_token: reopened_token,
            })
            .unwrap();
        let graft_sdk::MergeStatus::Merging {
            state_token: ready_token,
            unmerged_count: 0,
            ..
        } = second_resolution.merge
        else {
            panic!("expected a ready SQLite candidate after both row choices");
        };
        cloned
            .continue_merge(&graft_sdk::ContinueMergeOptions {
                message: "sdk resolved encrypted conflict private commit".into(),
                expected_state_token: ready_token,
            })
            .unwrap();
        assert_eq!(fs::read(&captures).unwrap(), before_resolution);
        let resolved_head = cloned.repository_metadata().unwrap().current_head.unwrap();
        assert_eq!(
            Repository::open(cloned_directory.path())
                .unwrap()
                .read_commit(&resolved_head)
                .unwrap()
                .parents,
            vec![conflict_local_head.clone(), conflict_remote_head.clone()]
        );
        cloned_secret.reconnect().unwrap();
        cloned_secret
            .unlock_with_root(
                &encrypted_config,
                encrypted_config
                    .unlock(b"sdk synthetic sync password", Some(repository))
                    .unwrap(),
            )
            .unwrap();
        cloned_credentials
            .set_encrypted_session(
                "origin",
                &config,
                cloned_secret.clone(),
                EncryptedTransferBudget {
                    private_directory: cloned_staging.path().to_path_buf(),
                    max_buffered_plaintext: 4 * 1024 * 1024,
                    max_ciphertext_spool: 16 * 1024 * 1024,
                },
            )
            .unwrap();
        cloned.push(Some("origin"), Some("main")).unwrap();
        recovered.fetch_for_checkout("origin", "main").unwrap();
        let recovery_plan = recovered
            .plan_merge(&PlanMergeOptions {
                revision: resolved_head.clone(),
                expected_head: Some(conflict_remote_head.clone()),
            })
            .unwrap();
        recovered
            .apply_merge(&ApplyMergeOptions {
                revision: resolved_head.clone(),
                expected_head: Some(conflict_remote_head.clone()),
                plan_token: recovery_plan.plan_token,
            })
            .unwrap();
        let database = Connection::open(recovered_directory.path().join("sdk-private.db")).unwrap();
        let rows: Vec<String> = database
            .prepare("SELECT value FROM private_items ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            ["sdk conflict local row one", "sdk conflict remote row two"]
        );
        runtime
            .block_on(recovered_credentials.clear_encrypted_session("origin"))
            .unwrap();
        recovered.close().unwrap();
        (conflict_local_head, conflict_remote_head, resolved_head)
    };
    {
        let final_directory = tempfile::tempdir().unwrap();
        let final_checkpoint = tempfile::tempdir().unwrap();
        let final_staging = tempfile::tempdir().unwrap();
        let final_secret = native_session(final_checkpoint.path(), repository, &encrypted_config);
        let final_credentials = credentials(&config, final_secret, final_staging.path());
        let final_reader = RepositorySession::new_with_remote_credentials(
            final_directory.path(),
            None,
            final_credentials.clone(),
        );
        final_reader.open().unwrap();
        final_reader
            .clone_repository_with_config(config.clone(), None)
            .unwrap();
        assert_eq!(
            final_reader
                .repository_metadata()
                .unwrap()
                .current_head
                .as_deref(),
            Some(resolved_head.as_str())
        );
        let repository = Repository::open(final_directory.path()).unwrap();
        assert_eq!(
            repository.read_commit(&resolved_head).unwrap().parents,
            vec![conflict_local_head.clone(), conflict_remote_head.clone()]
        );
        for id in [
            &first,
            &second,
            &local_head,
            &remote_head,
            &merged_head,
            &conflict_local_head,
            &conflict_remote_head,
        ] {
            assert_eq!(repository.read_commit(id).unwrap().id, *id);
        }
        let database = Connection::open(final_directory.path().join("sdk-private.db")).unwrap();
        let rows: Vec<String> = database
            .prepare("SELECT value FROM private_items ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            ["sdk conflict local row one", "sdk conflict remote row two"]
        );
        assert_eq!(
            fs::read(final_directory.path().join("sdk-private.bin")).unwrap(),
            payload
        );
        runtime
            .block_on(final_credentials.clear_encrypted_session("origin"))
            .unwrap();
        final_reader.close().unwrap();
    }
    assert_eq!(
        fs::read(cloned_directory.path().join("sdk-private.bin")).unwrap(),
        payload
    );
    let cloned_database = Connection::open(cloned_directory.path().join("sdk-private.db")).unwrap();
    assert_eq!(
        cloned_database
            .query_row("SELECT count(*) FROM private_items", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        Repository::open(cloned_directory.path())
            .unwrap()
            .read_commit(&second)
            .unwrap()
            .parent
            .as_deref(),
        Some(first.as_str())
    );
    drop(cloned_database);
    cloned.reopen().unwrap();
    assert_eq!(
        cloned.list_remotes().unwrap().remotes[0].kind,
        graft_sdk::SafeRemoteKind::EncryptedHttp
    );
    let before = fs::read(&captures).unwrap();
    assert!(
        cloned
            .clone_repository_with_config(config.clone(), None)
            .is_err()
    );
    assert_eq!(fs::read(&captures).unwrap(), before);
    assert_eq!(
        cloned
            .repository_metadata()
            .unwrap()
            .current_head
            .as_deref(),
        Some(resolved_head.as_str())
    );
    assert_eq!(
        reader
            .repository_metadata()
            .unwrap()
            .current_head
            .as_deref(),
        Some(first.as_str())
    );
    let active = Connection::open(target.path().join("sdk-private.db")).unwrap();
    assert_eq!(
        active
            .query_row("SELECT count(*) FROM private_items", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    active.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let guarded_plan = reader
        .plan_merge(&PlanMergeOptions {
            revision: second.clone(),
            expected_head: Some(first.clone()),
        })
        .unwrap();
    let busy = reader
        .apply_merge(&ApplyMergeOptions {
            revision: second.clone(),
            expected_head: Some(first.clone()),
            plan_token: guarded_plan.plan_token,
        })
        .unwrap_err();
    assert_eq!(busy.code(), graft_sdk::SdkErrorCode::RepositoryCommand);
    assert!(
        busy.message()
            .contains("while another transaction is active")
    );
    assert_eq!(
        reader
            .repository_metadata()
            .unwrap()
            .current_head
            .as_deref(),
        Some(first.as_str())
    );
    assert_eq!(
        active
            .query_row("SELECT count(*) FROM private_items", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    active.execute_batch("ROLLBACK;").unwrap();
    drop(active);
    // Remove both network service and secret admission before materialization.
    drop(service);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime
        .block_on(cloned_credentials.clear_encrypted_session("origin"))
        .unwrap();
    assert_eq!(
        cloned_secret.state(),
        graft::e2ee::SecretSessionState::Disconnected
    );
    cloned.close().unwrap();
    runtime
        .block_on(target_credentials.clear_encrypted_session("origin"))
        .unwrap();
    assert_eq!(
        target_secret.state(),
        graft::e2ee::SecretSessionState::Disconnected
    );
    let revision = fetched["head"].as_str().unwrap().to_owned();
    let plan = reader
        .plan_merge(&PlanMergeOptions {
            revision: revision.clone(),
            expected_head: Some(first.clone()),
        })
        .unwrap();
    reader
        .apply_merge(&ApplyMergeOptions {
            revision,
            expected_head: Some(first.clone()),
            plan_token: plan.plan_token,
        })
        .unwrap();
    assert_eq!(
        fs::read(target.path().join("sdk-private.bin")).unwrap(),
        payload
    );
    let restored = Connection::open(target.path().join("sdk-private.db")).unwrap();
    let rows: Vec<String> = restored
        .prepare("SELECT value FROM private_items ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        rows,
        [
            "sdk first WAL private marker",
            "sdk second WAL private marker"
        ]
    );
    assert_eq!(
        Repository::open(target.path())
            .unwrap()
            .read_commit(&second)
            .unwrap()
            .parent
            .as_deref(),
        Some(first.as_str())
    );
    runtime
        .block_on(source_credentials.clear_encrypted_session("origin"))
        .unwrap();
    assert_eq!(
        source_secret.state(),
        graft::e2ee::SecretSessionState::Disconnected
    );
    writer.close().unwrap();
    reader.close().unwrap();
    let captured = fs::read_to_string(captures).unwrap();
    let requests: serde_json::Value = serde_json::from_str(&captured).unwrap();
    assert!(
        requests
            .as_array()
            .unwrap()
            .iter()
            .all(|request| request["authenticated"] == true)
    );
    for marker in [
        "sdk-private.db",
        "sdk-private.bin",
        "sdk first WAL private marker",
        "sdk second WAL private marker",
        "sdk first encrypted private commit",
        "sdk second encrypted private commit",
        "sdk synthetic sync password",
        "migration-history",
        "migration-light-source",
        "migration-annotated-source",
        "migration annotation private marker",
        "migration-target-",
        migration_annotated.object.as_str(),
        "sdk local divergent private marker",
        "sdk remote divergent private marker",
        "sdk local divergent private commit",
        "sdk remote divergent private commit",
        "sdk divergent encrypted merge private commit",
        local_head.as_str(),
        remote_head.as_str(),
        merged_head.as_str(),
        conflict_local_head.as_str(),
        conflict_remote_head.as_str(),
        resolved_head.as_str(),
        "sdk conflict local row one",
        "sdk conflict local row two",
        "sdk conflict remote row one",
        "sdk conflict remote row two",
        "sdk conflicting local private commit",
        "sdk conflicting remote private commit",
        "sdk resolved encrypted conflict private commit",
        first.as_str(),
        second.as_str(),
    ] {
        assert!(!captured.contains(marker));
        let hex = marker
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert!(!captured.contains(&hex));
    }
    let secret_hex = (0_u8..32)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert!(!captured.contains(&secret_hex));
}
