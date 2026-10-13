#![cfg(all(feature = "e2ee", unix))]

use graft::repo::{
    Head, MigrationPlanningBudget, MigrationPublicationBudget, MigrationRefSelection,
};
use graft_sdk::{
    EncryptedMigrationCoordinator, EncryptedTransferBudget, MigrationCoordinatorOptions,
    MigrationJournalPaths, MigrationPublicationState, RemoteConfig, RemoteCredentials,
    RepositorySession,
    e2ee::{
        EncryptedConfig, FileInitializationJournal, FileRefMutationJournalProvider, RepositoryId,
        RootKey, SecretSession, SessionBinding,
    },
};
use std::{fs, os::unix::fs::PermissionsExt, sync::Arc};

struct Service(std::process::Child);
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn migration_coordinator_reopens_exact_prepared_intent_and_rejects_changed_source() {
    let directory = tempfile::tempdir().unwrap();
    let private = |name: &str| {
        let path = directory.path().join(name);
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    };
    let source_dir = private("source-worktree");
    let source = Arc::new(RepositorySession::new(&source_dir));
    source.open().unwrap();
    source.init().unwrap();
    fs::write(source_dir.join("private.txt"), "migration source").unwrap();
    source.add_all().unwrap();
    source.commit("fixed migration source").unwrap();
    let repository =
        RepositoryId::from_bytes([0, 1, 2, 3, 4, 5, 0x46, 7, 0x88, 9, 10, 11, 12, 13, 14, 15])
            .unwrap();
    let binding =
        SessionBinding::new("migration.test".into(), "account".into(), repository).unwrap();
    let root = RootKey::generate(repository).unwrap();
    let wrapped = EncryptedConfig::create(&root, b"coordinator synthetic password").unwrap();
    let secret = SecretSession::new(binding, None).unwrap();
    secret.unlock_with_root(&wrapped, root).unwrap();
    let paths = MigrationJournalPaths {
        source: private("source-intent"),
        target: private("target-intent"),
        enrollment: private("enrollment"),
        initialization: private("initialization"),
        progress: private("progress"),
    };
    let source_plan = source
        .plan_migration_source(
            MigrationRefSelection::All,
            MigrationPlanningBudget::default(),
        )
        .unwrap();
    let target = source
        .plan_migration_target_refs(
            &source_plan,
            vec![("refs/heads/main".into(), "refs/heads/moved".into())],
            Head::branch("moved"),
        )
        .unwrap();
    source
        .create_migration_source_journal(&paths.source, secret.binding(), &source_plan)
        .unwrap();
    source
        .create_migration_target_journal(&paths.target, secret.binding(), &target)
        .unwrap();
    let initial =
        FileInitializationJournal::create(&paths.initialization, secret.binding(), &wrapped)
            .unwrap();
    source
        .create_migration_enrollment_journal(&paths.enrollment, &paths.target, &secret, &initial)
        .unwrap();
    let config = RemoteConfig::EncryptedHttp {
        url: "http://127.0.0.1:1/acme/migration".into(),
        uuid: "00010203-0405-4607-8809-0a0b0c0d0e0f".into(),
    };
    let credentials = RemoteCredentials::explicit();
    credentials
        .set_http_bearer_token("vault", "synthetic-token".into())
        .unwrap();
    credentials
        .set_encrypted_session_with_journals(
            "vault",
            &config,
            secret.clone(),
            EncryptedTransferBudget {
                private_directory: private("staging"),
                max_buffered_plaintext: 4 * 1024 * 1024,
                max_ciphertext_spool: 16 * 1024 * 1024,
            },
            Arc::new(
                FileRefMutationJournalProvider::create(private("ref-recovery"), secret.binding())
                    .unwrap(),
            ),
        )
        .unwrap();
    let unsafe_credentials = RemoteCredentials::explicit();
    unsafe_credentials
        .set_http_bearer_token("vault", "synthetic-token".into())
        .unwrap();
    unsafe_credentials
        .set_encrypted_session_with_journals(
            "vault",
            &config,
            secret.clone(),
            EncryptedTransferBudget {
                private_directory: source_dir.clone(),
                max_buffered_plaintext: 4 * 1024 * 1024,
                max_ciphertext_spool: 16 * 1024 * 1024,
            },
            Arc::new(
                FileRefMutationJournalProvider::create(
                    private("unsafe-ref-recovery"),
                    secret.binding(),
                )
                .unwrap(),
            ),
        )
        .unwrap();
    assert!(
        EncryptedMigrationCoordinator::prepare(
            source.clone(),
            secret.clone(),
            unsafe_credentials,
            MigrationCoordinatorOptions {
                remote_name: "vault".into(),
                config: config.clone(),
                journals: paths.clone(),
                budget: MigrationPublicationBudget::default(),
            }
        )
        .is_err()
    );
    assert_eq!(fs::read_dir(&paths.progress).unwrap().count(), 0);
    let other_root = RootKey::generate(repository).unwrap();
    let other_config = EncryptedConfig::create(&other_root, b"other synthetic password").unwrap();
    let other_binding =
        SessionBinding::new("migration.test".into(), "account".into(), repository).unwrap();
    let other_secret = SecretSession::new(other_binding, None).unwrap();
    other_secret
        .unlock_with_root(&other_config, other_root)
        .unwrap();
    let other_credentials = RemoteCredentials::explicit();
    other_credentials
        .set_http_bearer_token("vault", "synthetic-token".into())
        .unwrap();
    other_credentials
        .set_encrypted_session_with_journals(
            "vault",
            &config,
            other_secret,
            EncryptedTransferBudget {
                private_directory: private("wrong-root-staging"),
                max_buffered_plaintext: 4 * 1024 * 1024,
                max_ciphertext_spool: 16 * 1024 * 1024,
            },
            Arc::new(
                FileRefMutationJournalProvider::create(
                    private("wrong-root-ref-recovery"),
                    secret.binding(),
                )
                .unwrap(),
            ),
        )
        .unwrap();
    assert!(
        EncryptedMigrationCoordinator::prepare(
            source.clone(),
            secret.clone(),
            other_credentials,
            MigrationCoordinatorOptions {
                remote_name: "vault".into(),
                config: config.clone(),
                journals: paths.clone(),
                budget: MigrationPublicationBudget::default(),
            }
        )
        .is_err()
    );
    assert_eq!(fs::read_dir(&paths.progress).unwrap().count(), 0);
    let coordinator = EncryptedMigrationCoordinator::prepare(
        source.clone(),
        secret.clone(),
        credentials.clone(),
        MigrationCoordinatorOptions {
            remote_name: "vault".into(),
            config,
            journals: paths.clone(),
            budget: MigrationPublicationBudget::default(),
        },
    )
    .unwrap();
    assert_eq!(
        coordinator.state().unwrap(),
        MigrationPublicationState::Prepared
    );
    assert!(
        EncryptedMigrationCoordinator::resume(
            source.clone(),
            secret.clone(),
            credentials.clone(),
            &paths.progress
        )
        .is_err()
    );
    let retained = fs::read(paths.progress.join("migration-progress")).unwrap();
    drop(coordinator);
    source.close().unwrap();
    let source = Arc::new(RepositorySession::new(&source_dir));
    source.open().unwrap();
    let resumed = EncryptedMigrationCoordinator::resume(
        source.clone(),
        secret.clone(),
        credentials.clone(),
        &paths.progress,
    )
    .unwrap();
    assert_eq!(
        resumed.state().unwrap(),
        MigrationPublicationState::Prepared
    );
    assert_eq!(
        fs::read(paths.progress.join("migration-progress")).unwrap(),
        retained
    );
    drop(resumed);
    fs::write(
        source_dir.join("private.txt"),
        "later source must not replace retained intent",
    )
    .unwrap();
    source.add_all().unwrap();
    source.commit("later source").unwrap();
    assert!(
        EncryptedMigrationCoordinator::resume(source, secret, credentials, &paths.progress)
            .is_err()
    );
    assert_eq!(
        fs::read(paths.progress.join("migration-progress")).unwrap(),
        retained
    );
}

#[test]
#[ignore = "requires built TypeScript service and loopback networking"]
fn migration_coordinator_reopens_each_stage_and_independently_recovers_all_refs() {
    use graft_sdk::MigrationVerificationOptions;
    use std::{
        io::{BufRead, BufReader},
        path::Path,
        process::{Command, Stdio},
    };
    let directory = tempfile::tempdir().unwrap();
    let private = |name: &str| {
        let path = directory.path().join(name);
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    };
    let fixture = directory.path().join("fixture.json");
    let captures = directory.path().join("captures.json");
    fs::write(
        &fixture,
        serde_json::to_vec(&serde_json::json!({
            "encryption": { "profile": "encrypted-remote-v1", "version": 1, "suite": 1,
                "uuid": "00010203-0405-4607-8809-0a0b0c0d0e0f" }, "objects": [],
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
    let source_dir = private("source");
    let source = Arc::new(RepositorySession::new(&source_dir));
    source.open().unwrap();
    source.init().unwrap();
    let db = rusqlite::Connection::open(source_dir.join("private.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TABLE rows(value TEXT); INSERT INTO rows VALUES ('first private snapshot')",
    )
    .unwrap();
    source.add_all().unwrap();
    source.commit("first private revision").unwrap();
    let repository = graft::repo::Repository::open(&source_dir).unwrap();
    repository.branch_create("older", None).unwrap();
    repository.branch_create_unborn("empty").unwrap();
    repository.tag_create("old-tag", None).unwrap();
    db.execute("INSERT INTO rows VALUES ('second private snapshot')", [])
        .unwrap();
    drop(db);
    fs::write(
        source_dir.join("private.txt"),
        "migration recovery must use remote bytes",
    )
    .unwrap();
    source.add_all().unwrap();
    source.commit("second private revision").unwrap();
    let id = RepositoryId::from_bytes([0, 1, 2, 3, 4, 5, 0x46, 7, 0x88, 9, 10, 11, 12, 13, 14, 15])
        .unwrap();
    let binding = SessionBinding::new("migration.test".into(), "account".into(), id).unwrap();
    let root = RootKey::generate(id).unwrap();
    let password = b"coordinator synthetic password";
    let wrapped = EncryptedConfig::create(&root, password).unwrap();
    let secret = SecretSession::new(binding, None).unwrap();
    secret.unlock_with_root(&wrapped, root).unwrap();
    let paths = MigrationJournalPaths {
        source: private("source-plan"),
        target: private("target-plan"),
        enrollment: private("enrollment"),
        initialization: private("initialization"),
        progress: private("progress"),
    };
    let frozen = source
        .plan_migration_source(
            MigrationRefSelection::All,
            MigrationPlanningBudget::default(),
        )
        .unwrap();
    let target = source
        .plan_migration_target_refs(
            &frozen,
            vec![
                ("refs/heads/main".into(), "refs/heads/moved".into()),
                ("refs/heads/older".into(), "refs/heads/old-history".into()),
                ("refs/heads/empty".into(), "refs/heads/future".into()),
                ("refs/tags/old-tag".into(), "refs/tags/retained".into()),
            ],
            Head::branch("moved"),
        )
        .unwrap();
    assert!(frozen.snapshots().len() >= 2);
    source
        .create_migration_source_journal(&paths.source, secret.binding(), &frozen)
        .unwrap();
    source
        .create_migration_target_journal(&paths.target, secret.binding(), &target)
        .unwrap();
    let initial =
        FileInitializationJournal::create(&paths.initialization, secret.binding(), &wrapped)
            .unwrap();
    source
        .create_migration_enrollment_journal(&paths.enrollment, &paths.target, &secret, &initial)
        .unwrap();
    let config = RemoteConfig::EncryptedHttp {
        url: ready["enrollmentUrl"].as_str().unwrap().into(),
        uuid: "00010203-0405-4607-8809-0a0b0c0d0e0f".into(),
    };
    let credentials = RemoteCredentials::explicit();
    credentials
        .set_http_bearer_token("vault", "account-token".into())
        .unwrap();
    credentials
        .set_encrypted_session_with_journals(
            "vault",
            &config,
            secret.clone(),
            EncryptedTransferBudget {
                private_directory: private("staging"),
                max_buffered_plaintext: 4 * 1024 * 1024,
                max_ciphertext_spool: 16 * 1024 * 1024,
            },
            Arc::new(
                FileRefMutationJournalProvider::create(private("ref-recovery"), secret.binding())
                    .unwrap(),
            ),
        )
        .unwrap();
    let mut coordinator = EncryptedMigrationCoordinator::prepare(
        source.clone(),
        secret.clone(),
        credentials.clone(),
        MigrationCoordinatorOptions {
            remote_name: "vault".into(),
            config: config.clone(),
            journals: paths.clone(),
            budget: MigrationPublicationBudget::default(),
        },
    )
    .unwrap();
    let mut completed = 0;
    while coordinator.state().unwrap() != MigrationPublicationState::ReadyForIndependentVerification
    {
        coordinator.publish_next().unwrap();
        completed += 1;
        drop(coordinator);
        coordinator = EncryptedMigrationCoordinator::resume(
            source.clone(),
            secret.clone(),
            credentials.clone(),
            &paths.progress,
        )
        .unwrap();
    }
    assert_eq!(completed, target.references().len() + 3);
    let options = |prefix: &str| MigrationVerificationOptions {
        worktree: private(&format!("{prefix}-worktree")),
        checkpoint: private(&format!("{prefix}-checkpoint")),
        transfer: EncryptedTransferBudget {
            private_directory: private(&format!("{prefix}-staging")),
            max_buffered_plaintext: 4 * 1024 * 1024,
            max_ciphertext_spool: 16 * 1024 * 1024,
        },
        bearer_token: "account-token".into(),
    };
    assert!(
        coordinator
            .verify_independently(
                zeroize::Zeroizing::new(b"wrong password".to_vec()),
                options("wrong")
            )
            .is_err()
    );
    let verification = coordinator
        .verify_independently(zeroize::Zeroizing::new(password.to_vec()), options("good"))
        .unwrap();
    assert_eq!(verification.references, 4);
    assert_eq!(verification.repository_objects, frozen.objects().len());
    assert!(verification.dependencies.snapshots >= 2);
    assert!(verification.dependencies.storage_commits >= 2);
    assert!(verification.dependencies.storage_pages >= 2);
    coordinator
        .validate_independent_verification(&verification)
        .unwrap();
    // A non-HEAD tag disappearance invalidates the complete selection proof.
    let remote = config
        .build_with_credentials("vault", &credentials)
        .unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let observation = runtime
        .block_on(remote.observe_ref("refs/tags/retained"))
        .unwrap();
    runtime
        .block_on(remote.compare_and_delete_observed_ref("refs/tags/retained", &observation))
        .unwrap();
    assert!(
        coordinator
            .validate_independent_verification(&verification)
            .is_err()
    );
    assert!(
        coordinator
            .verify_independently(
                zeroize::Zeroizing::new(password.to_vec()),
                options("missing-tag")
            )
            .is_err()
    );
    source
        .publish_migration_target_tag(&target, Arc::new(remote), "refs/tags/retained")
        .unwrap();
    let unrelated_source = RepositorySession::new(private("unrelated-source"));
    unrelated_source.open().unwrap();
    unrelated_source.init().unwrap();
    let mut unrelated_fence =
        graft_sdk::RepositoryMigrationFence::acquire(&unrelated_source).unwrap();
    let before_unrelated_fence = fs::read(&captures).unwrap();
    assert!(
        coordinator
            .verify_and_cutover(
                &mut unrelated_fence,
                zeroize::Zeroizing::new(password.to_vec()),
                options("unrelated-fence")
            )
            .is_err()
    );
    assert_eq!(fs::read(&captures).unwrap(), before_unrelated_fence);
    drop(unrelated_fence);
    struct TestFence {
        held: bool,
        canonical: Option<graft_sdk::RepositoryMigrationFence>,
        fail_after_commit: bool,
        receipt: std::path::PathBuf,
    }
    impl graft_sdk::MigrationWriteFence for TestFence {
        fn covers_repository(&self, repository: &graft::repo::Repository) -> bool {
            self.canonical
                .as_ref()
                .is_some_and(|fence| fence.covers_repository(repository))
        }
        fn check_held(&self) -> graft_sdk::e2ee::Result<()> {
            if self.held {
                self.canonical.as_ref().unwrap().check_held()
            } else {
                Err(graft_sdk::e2ee::EncryptionError::SessionBusy)
            }
        }
        fn commit_cutover(
            &mut self,
            intent: graft_sdk::MigrationCutoverIntent<'_>,
        ) -> graft_sdk::e2ee::Result<()> {
            assert_eq!(intent.remote_name, "vault");
            assert_eq!(intent.target.references().len(), 4);
            if self.receipt.exists() {
                assert_eq!(fs::read(&self.receipt).unwrap(), intent.id);
            } else {
                fs::write(&self.receipt, intent.id).unwrap();
                fs::File::open(&self.receipt).unwrap().sync_all().unwrap();
                fs::File::open(self.receipt.parent().unwrap())
                    .unwrap()
                    .sync_all()
                    .unwrap();
            }
            self.canonical.as_mut().unwrap().commit_cutover(intent)?;
            if self.fail_after_commit {
                self.fail_after_commit = false;
                return Err(graft_sdk::e2ee::EncryptionError::CheckpointUnavailable(
                    std::io::ErrorKind::Interrupted,
                ));
            }
            Ok(())
        }
    }
    let mut fence = TestFence {
        held: false,
        canonical: None,
        fail_after_commit: true,
        receipt: directory.path().join("cutover-receipt"),
    };
    let before_rejected_cutover = fs::read(&captures).unwrap();
    assert!(
        coordinator
            .verify_and_cutover(
                &mut fence,
                zeroize::Zeroizing::new(password.to_vec()),
                options("unfenced")
            )
            .is_err()
    );
    assert_eq!(fs::read(&captures).unwrap(), before_rejected_cutover);
    assert!(!fence.receipt.exists());
    fence.held = true;
    fence.canonical = Some(graft_sdk::RepositoryMigrationFence::acquire(&source).unwrap());
    let concurrent = source.clone();
    assert_eq!(
        std::thread::spawn(move || concurrent.add_all().unwrap_err().code())
            .join()
            .unwrap(),
        graft_sdk::SdkErrorCode::RepositoryBusy
    );
    assert!(
        coordinator
            .verify_and_cutover(
                &mut fence,
                zeroize::Zeroizing::new(password.to_vec()),
                options("interrupted-cutover")
            )
            .is_err()
    );
    assert!(fence.receipt.exists());
    let switched = repository.config().unwrap();
    assert!(matches!(
        switched.remotes.get("vault"),
        Some(RemoteConfig::EncryptedHttp { .. })
    ));
    assert_eq!(
        switched.branches["main"].merge.as_deref(),
        Some("refs/heads/moved")
    );
    assert_eq!(
        switched.branches["older"].merge.as_deref(),
        Some("refs/heads/old-history")
    );
    assert_eq!(
        fs::read_to_string(source_dir.join("private.txt")).unwrap(),
        "migration recovery must use remote bytes"
    );
    assert_eq!(
        coordinator.state().unwrap(),
        MigrationPublicationState::ReadyForIndependentVerification
    );
    drop(coordinator);
    let mut coordinator =
        EncryptedMigrationCoordinator::resume(source.clone(), secret, credentials, &paths.progress)
            .unwrap();
    coordinator
        .verify_and_cutover(
            &mut fence,
            zeroize::Zeroizing::new(password.to_vec()),
            options("replayed-cutover"),
        )
        .unwrap();
    assert_eq!(
        coordinator.state().unwrap(),
        MigrationPublicationState::HostCutoverConfirmed
    );
    drop(fence);
    source.add_all().unwrap();
}
