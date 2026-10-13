#![cfg(all(feature = "e2ee", not(target_arch = "wasm32")))]

use graft::repo::{MigrationPlanningBudget, MigrationRefSelection, MigrationVerificationBudget};
use graft_sdk::RepositorySession;
use rusqlite::Connection;
use std::fs;

#[test]
#[cfg(unix)]
fn migration_enrollment_retains_exact_config_and_target_across_repository_reopen() {
    use graft::e2ee::{
        EncryptedConfig, FileInitializationJournal, InitializationJournal, InitializationStage,
        RepositoryId, RootKey, SecretSession, SessionBinding,
    };
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    fs::create_dir(&source).unwrap();
    let private = |name: &str| {
        let path = directory.path().join(name);
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    };
    let target_directory = private("target");
    let enrollment_directory = private("enrollment");
    let initial_directory = private("initial");
    let other_target = private("other-target");
    let later_directory = private("later-enrollment");
    let repo = RepositorySession::new(&source);
    repo.open().unwrap();
    repo.init().unwrap();
    let db = Connection::open(source.join("history.db")).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE data(value TEXT); INSERT INTO data VALUES('migration enrollment WAL');").unwrap();
    repo.add_all().unwrap();
    repo.commit("migration enrollment source").unwrap();
    let source_plan = repo
        .plan_migration_source(
            MigrationRefSelection::All,
            MigrationPlanningBudget::default(),
        )
        .unwrap();
    let target_plan = repo
        .plan_migration_target_refs(
            &source_plan,
            vec![("refs/heads/main".into(), "refs/heads/migrated".into())],
            graft::repo::Head::branch("migrated"),
        )
        .unwrap();
    let binding = SessionBinding::new(
        "target.test".into(),
        "synthetic-account".into(),
        RepositoryId::generate().unwrap(),
    )
    .unwrap();
    let root = RootKey::generate(binding.repository()).unwrap();
    let config = EncryptedConfig::create(&root, b"migration enrollment password").unwrap();
    let alternate = EncryptedConfig::create(&root, b"alternate wrapper").unwrap();
    let fresh_binding = || {
        SessionBinding::new(
            binding.domain().to_owned(),
            binding.account().to_owned(),
            binding.repository(),
        )
        .unwrap()
    };
    let secret = SecretSession::new(fresh_binding(), None).unwrap();
    secret.unlock_with_root(&config, root).unwrap();
    let initial = FileInitializationJournal::create(&initial_directory, &binding, &config).unwrap();
    repo.create_migration_target_journal(&target_directory, &binding, &target_plan)
        .unwrap();
    repo.create_migration_enrollment_journal(
        &enrollment_directory,
        &target_directory,
        &secret,
        &initial,
    )
    .unwrap();
    let exact = fs::read(enrollment_directory.join("migration-enrollment")).unwrap();
    assert!(
        repo.create_migration_enrollment_journal(
            &enrollment_directory,
            &target_directory,
            &secret,
            &initial
        )
        .is_err()
    );
    let changed = repo
        .plan_migration_target_refs(
            &source_plan,
            vec![("refs/heads/main".into(), "refs/heads/changed".into())],
            graft::repo::Head::branch("changed"),
        )
        .unwrap();
    repo.create_migration_target_journal(&other_target, &binding, &changed)
        .unwrap();
    assert!(
        repo.restore_migration_enrollment_journal(
            &enrollment_directory,
            &other_target,
            &secret,
            &initial
        )
        .is_err()
    );
    let alternate_directory = private("alternate-initial");
    let alternate_initial =
        FileInitializationJournal::create(&alternate_directory, &binding, &alternate).unwrap();
    assert!(
        repo.restore_migration_enrollment_journal(
            &enrollment_directory,
            &target_directory,
            &secret,
            &alternate_initial
        )
        .is_err()
    );
    assert!(
        graft::e2ee::FileMigrationTargetJournal::open(&enrollment_directory, &binding).is_err()
    );
    repo.close().unwrap();
    let reopened = RepositorySession::new(&source);
    reopened.open().unwrap();
    let fresh = SecretSession::new(fresh_binding(), None).unwrap();
    fresh
        .unlock_with_root(
            &config,
            config
                .unlock(b"migration enrollment password", Some(binding.repository()))
                .unwrap(),
        )
        .unwrap();
    let checkpoint = fresh.checkpoint();
    let initial = FileInitializationJournal::open(&initial_directory, &binding).unwrap();
    let restored = reopened
        .restore_migration_enrollment_journal(
            &enrollment_directory,
            &target_directory,
            &fresh,
            &initial,
        )
        .unwrap();
    assert_eq!(
        restored.encode_private().unwrap(),
        target_plan.encode_private().unwrap()
    );
    assert_eq!(fresh.checkpoint(), checkpoint);
    initial
        .advance(
            &binding,
            InitializationStage::Prepared,
            InitializationStage::ConfigPublished,
        )
        .unwrap();
    assert!(
        reopened
            .create_migration_enrollment_journal(
                &later_directory,
                &target_directory,
                &fresh,
                &initial
            )
            .is_err()
    );
    assert_eq!(fs::read_dir(&later_directory).unwrap().count(), 0);
    let guarded = reopened
        .migration_initialization_journal(
            &enrollment_directory,
            &target_directory,
            fresh.clone(),
            std::sync::Arc::new(
                FileInitializationJournal::open(&initial_directory, &binding).unwrap(),
            ),
        )
        .unwrap();
    assert_eq!(
        guarded.load(&binding).unwrap().stage,
        InitializationStage::ConfigPublished
    );
    let cancellation = graft::repo::CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        graft::repo::with_cancellation(&cancellation, || guarded.load(&binding)),
        Err(graft::e2ee::EncryptionError::Cancelled)
    ));
    guarded.load(&binding).unwrap();
    let foreign = SessionBinding::new(
        binding.domain().to_owned(),
        "other-account".into(),
        binding.repository(),
    )
    .unwrap();
    assert!(matches!(
        guarded.load(&foreign),
        Err(graft::e2ee::EncryptionError::IdentityChanged)
    ));
    reopened
        .restore_migration_enrollment_journal(
            &enrollment_directory,
            &target_directory,
            &fresh,
            &initial,
        )
        .unwrap();
    db.execute("INSERT INTO data VALUES('later source')", [])
        .unwrap();
    reopened.add_all().unwrap();
    reopened.commit("changed source").unwrap();
    assert!(matches!(
        guarded.load(&binding),
        Err(graft::e2ee::EncryptionError::StaleObservation)
    ));
    assert!(
        reopened
            .restore_migration_enrollment_journal(
                &enrollment_directory,
                &target_directory,
                &fresh,
                &initial
            )
            .is_err()
    );
    assert_eq!(
        fs::read(enrollment_directory.join("migration-enrollment")).unwrap(),
        exact
    );
    fs::remove_file(enrollment_directory.join("migration-enrollment")).unwrap();
    assert!(
        reopened
            .restore_migration_enrollment_journal(
                &enrollment_directory,
                &target_directory,
                &fresh,
                &initial
            )
            .is_err()
    );
    assert!(!enrollment_directory.join("migration-enrollment").exists());
}

#[test]
#[cfg(unix)]
fn migration_sdk_private_journal_restores_in_new_session_and_rejects_stale_source() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    let private = directory.path().join("private");
    let target_private = directory.path().join("target-private");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&private).unwrap();
    fs::create_dir(&target_private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&target_private, fs::Permissions::from_mode(0o700)).unwrap();
    let session = RepositorySession::new(&source);
    session.open().unwrap();
    session.init().unwrap();
    let db = Connection::open(source.join("history.db")).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE data(value TEXT); INSERT INTO data VALUES('journal source WAL');").unwrap();
    fs::write(source.join("attachment.txt"), "retained attachment").unwrap();
    session.add_all().unwrap();
    session.commit("retained source").unwrap();
    let plan = session
        .plan_migration_source(
            MigrationRefSelection::All,
            MigrationPlanningBudget::default(),
        )
        .unwrap();
    let target_plan = session
        .plan_migration_target_refs(
            &plan,
            vec![("refs/heads/main".into(), "refs/heads/migrated".into())],
            graft::repo::Head::branch("migrated"),
        )
        .unwrap();
    assert_eq!(
        target_plan.references()["refs/heads/migrated"]
            .object
            .as_ref()
            .unwrap()
            .as_str(),
        plan.head_target().unwrap()
    );
    session
        .validate_migration_target_plan(&target_plan)
        .unwrap();
    let target_id = graft::e2ee::RepositoryId::generate().unwrap();
    let binding = graft::e2ee::SessionBinding::new(
        "target.test".into(),
        "synthetic-account".into(),
        target_id,
    )
    .unwrap();
    let unsafe_path = source.join("unsafe-journal");
    fs::create_dir(&unsafe_path).unwrap();
    fs::set_permissions(&unsafe_path, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        session
            .create_migration_source_journal(&unsafe_path, &binding, &plan)
            .is_err()
    );
    assert_eq!(fs::read_dir(&unsafe_path).unwrap().count(), 0);
    assert!(
        session
            .create_migration_target_journal(&unsafe_path, &binding, &target_plan)
            .is_err()
    );
    assert_eq!(fs::read_dir(&unsafe_path).unwrap().count(), 0);
    session
        .create_migration_target_journal(&target_private, &binding, &target_plan)
        .unwrap();
    let target_exact = fs::read(target_private.join("migration-target")).unwrap();
    assert!(
        session
            .create_migration_target_journal(&target_private, &binding, &target_plan)
            .is_err()
    );
    session
        .create_migration_source_journal(&private, &binding, &plan)
        .unwrap();
    let exact = fs::read(private.join("migration-source")).unwrap();
    assert!(
        session
            .create_migration_source_journal(&private, &binding, &plan)
            .is_err()
    );
    assert_eq!(fs::read(private.join("migration-source")).unwrap(), exact);
    let verified = session
        .verify_migration_source_dependencies(&plan, MigrationVerificationBudget::default())
        .unwrap();
    session.close().unwrap();
    let reopened = RepositorySession::new(&source);
    reopened.open().unwrap();
    let restored = reopened
        .restore_migration_source_journal(&private, &binding)
        .unwrap();
    let restored_target = reopened
        .restore_migration_target_journal(&target_private, &binding)
        .unwrap();
    assert_eq!(
        restored_target.encode_private().unwrap(),
        target_plan.encode_private().unwrap()
    );
    assert_eq!(
        restored_target.head(),
        &graft::repo::Head::branch("migrated")
    );
    assert_eq!(
        fs::read(target_private.join("migration-target")).unwrap(),
        target_exact
    );
    reopened
        .validate_migration_target_plan(&target_plan)
        .unwrap();
    assert_eq!(
        restored.encode_private().unwrap(),
        plan.encode_private().unwrap()
    );
    assert_eq!(
        reopened
            .verify_migration_source_dependencies(&restored, MigrationVerificationBudget::default())
            .unwrap(),
        verified
    );
    let wrong =
        graft::e2ee::SessionBinding::new("target.test".into(), "other-account".into(), target_id)
            .unwrap();
    assert!(
        reopened
            .restore_migration_source_journal(&private, &wrong)
            .is_err()
    );
    assert!(
        reopened
            .restore_migration_target_journal(&target_private, &wrong)
            .is_err()
    );
    assert!(
        reopened
            .restore_migration_target_journal(&private, &binding)
            .is_err()
    );
    assert!(
        reopened
            .restore_migration_source_journal(&target_private, &binding)
            .is_err()
    );
    db.execute("UPDATE data SET value='changed source WAL'", [])
        .unwrap();
    reopened.add_all().unwrap();
    reopened.commit("changed source").unwrap();
    assert!(
        reopened
            .restore_migration_target_journal(&target_private, &binding)
            .is_err()
    );
    assert_eq!(
        fs::read(target_private.join("migration-target")).unwrap(),
        target_exact
    );
    assert!(
        reopened
            .validate_migration_target_plan(&target_plan)
            .is_err()
    );
    assert!(
        reopened
            .restore_migration_source_journal(&private, &binding)
            .is_err()
    );
    assert_eq!(fs::read(private.join("migration-source")).unwrap(), exact);
    reopened.close().unwrap();
}

#[test]
fn migration_sdk_inventory_keeps_committed_wal_history_and_external_dependencies() {
    let directory = tempfile::tempdir().unwrap();
    let session = RepositorySession::new(directory.path());
    session.open().unwrap();
    session.init().unwrap();
    // Source inventory must not need a remote key, credential or live endpoint.
    session
        .add_remote_with_config(
            "old",
            graft::remote::RemoteConfig::EncryptedHttp {
                url: "http://127.0.0.1:1/synthetic/old".into(),
                uuid: "00010203-0405-4607-8809-0a0b0c0d0e0f".into(),
            },
        )
        .unwrap();
    session
        .config_set("files.inline_text_threshold", "128 B")
        .unwrap();
    let db = Connection::open(directory.path().join("history.db")).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE data(value TEXT); INSERT INTO data VALUES('first committed WAL version');").unwrap();
    let attachment = b"migration external attachment\n".repeat(8192);
    fs::write(directory.path().join("attachment.txt"), &attachment).unwrap();
    session.add_all().unwrap();
    session.commit("first history").unwrap();
    let first = session.repository_metadata().unwrap().current_head.unwrap();
    db.execute("UPDATE data SET value='second committed WAL version'", [])
        .unwrap();
    session.add_all().unwrap();
    session.commit("second history").unwrap();
    let second = session.repository_metadata().unwrap().current_head.unwrap();
    let before = session.repository_metadata().unwrap();
    let plan = session
        .plan_migration_source(
            MigrationRefSelection::All,
            MigrationPlanningBudget::default(),
        )
        .unwrap();
    assert_eq!(plan.head_target(), Some(second.as_str()));
    for id in [&first, &second] {
        assert!(plan.objects().iter().any(|object| object.as_str() == id));
    }
    assert_eq!(plan.snapshots().len(), 2);
    assert_eq!(plan.external_payloads().len(), 1);
    let verified = session
        .verify_migration_source_dependencies(&plan, MigrationVerificationBudget::default())
        .unwrap();
    let repo = graft::repo::Repository::discover(directory.path()).unwrap();
    let object = repo
        .object_store()
        .path_for(&graft::repo::object::ObjectId::new(&first).unwrap());
    let original = fs::read(&object).unwrap();
    fs::write(&object, b"changed after source planning").unwrap();
    assert!(
        session
            .verify_migration_source_dependencies(&plan, MigrationVerificationBudget::default())
            .is_err()
    );
    fs::remove_file(&object).unwrap();
    assert!(
        session
            .verify_migration_source_dependencies(&plan, MigrationVerificationBudget::default())
            .is_err()
    );
    fs::write(&object, original).unwrap();
    assert_eq!(verified.payloads, 1);
    assert_eq!(verified.payload_bytes, attachment.len() as u64);
    assert_eq!(verified.snapshots, 2);
    assert!(verified.storage_commits >= 2);
    assert!(verified.storage_pages > 0);
    for budget in [
        MigrationVerificationBudget {
            max_payload_bytes: 1,
            ..Default::default()
        },
        MigrationVerificationBudget {
            max_storage_commits: 1,
            ..Default::default()
        },
        MigrationVerificationBudget {
            max_storage_pages: 0,
            ..Default::default()
        },
    ] {
        assert!(
            session
                .verify_migration_source_dependencies(&plan, budget)
                .is_err()
        );
    }
    let payload = plan.external_payloads().keys().next().unwrap().as_str();
    let path = directory
        .path()
        .join(".graft/store/files")
        .join(&payload[..2])
        .join(&payload[2..]);
    let saved = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    assert!(
        session
            .verify_migration_source_dependencies(&plan, MigrationVerificationBudget::default())
            .is_err()
    );
    let mut corrupt = saved.clone();
    corrupt[0] ^= 1;
    fs::write(&path, &corrupt).unwrap();
    assert!(
        session
            .verify_migration_source_dependencies(&plan, MigrationVerificationBudget::default())
            .is_err()
    );
    fs::write(&path, &saved[..saved.len() - 1]).unwrap();
    assert!(
        session
            .verify_migration_source_dependencies(&plan, MigrationVerificationBudget::default())
            .is_err()
    );
    fs::write(&path, saved).unwrap();
    assert_eq!(
        session
            .verify_migration_source_dependencies(&plan, MigrationVerificationBudget::default())
            .unwrap(),
        verified
    );
    assert_eq!(
        plan.external_payloads().values().copied().next(),
        Some(attachment.len() as u64)
    );
    // Local uncommitted edits are not silently promoted into the selected history.
    db.execute("UPDATE data SET value='uncommitted local WAL edit'", [])
        .unwrap();
    session.validate_migration_source_plan(&plan).unwrap();
    assert_eq!(
        session
            .verify_migration_source_dependencies(&plan, MigrationVerificationBudget::default())
            .unwrap(),
        verified
    );
    let repeated = session
        .plan_migration_source(
            MigrationRefSelection::All,
            MigrationPlanningBudget::default(),
        )
        .unwrap();
    assert_eq!(repeated.objects(), plan.objects());
    assert_eq!(repeated.snapshots(), plan.snapshots());
    assert_eq!(
        session.repository_metadata().unwrap().current_head,
        before.current_head
    );
    assert_eq!(
        fs::read(directory.path().join("attachment.txt")).unwrap(),
        attachment
    );
    assert_eq!(
        db.query_row("SELECT value FROM data", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "uncommitted local WAL edit"
    );
    // The private plan survives a native SDK session close/reopen.
    session.close().unwrap();
    let reopened = RepositorySession::new(directory.path());
    reopened.open().unwrap();
    reopened.validate_migration_source_plan(&plan).unwrap();
    assert_eq!(
        reopened
            .verify_migration_source_dependencies(&plan, MigrationVerificationBudget::default())
            .unwrap(),
        verified
    );
    reopened.add_all().unwrap();
    reopened.commit("third history").unwrap();
    assert!(reopened.validate_migration_source_plan(&plan).is_err());
    reopened.close().unwrap();
}

#[test]
fn migration_sdk_rejects_valid_object_hashes_with_wrong_storage_identity_or_page_count() {
    use graft::repo::{Repository, object::Object};
    let directory = tempfile::tempdir().unwrap();
    let session = RepositorySession::new(directory.path());
    session.open().unwrap();
    session.init().unwrap();
    let db = Connection::open(directory.path().join("source.db")).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL;CREATE TABLE data(value TEXT);INSERT INTO data VALUES('retained source');").unwrap();
    session.add_all().unwrap();
    session.commit("canonical source").unwrap();
    let repo = Repository::discover(directory.path()).unwrap();
    let head = repo.head_target().unwrap().unwrap();
    let Object::Commit(commit) = repo.read_object(&head).unwrap() else {
        panic!("expected commit")
    };
    let Object::Tree(tree) = repo.read_object(commit.tree.as_str()).unwrap() else {
        panic!("expected tree")
    };
    let Object::Blob(graft::repo::object::BlobObject::SqliteSnapshot(snapshot)) =
        repo.read_object(tree.entries[0].oid.as_str()).unwrap()
    else {
        panic!("expected snapshot")
    };
    for bad_count in [false, true] {
        let mut changed = snapshot.clone();
        if bad_count {
            changed.page_count = graft::core::page_count::PageCount::ZERO;
        } else {
            changed.ranges[0].commits[0].commit_hash = graft::core::commit_hash::CommitHash::ZERO;
        }
        let blob = repo
            .object_store()
            .write(&Object::Blob(
                graft::repo::object::BlobObject::SqliteSnapshot(changed),
            ))
            .unwrap();
        let mut changed_tree = tree.clone();
        changed_tree.entries[0].oid = blob;
        let mut changed_commit = commit.clone();
        changed_commit.tree = repo
            .object_store()
            .write(&Object::Tree(changed_tree))
            .unwrap();
        let id = repo
            .object_store()
            .write(&Object::Commit(changed_commit))
            .unwrap();
        fs::write(repo.graft_dir().join("refs/heads/main"), format!("{id}\n")).unwrap();
        let plan = session
            .plan_migration_source(
                MigrationRefSelection::All,
                MigrationPlanningBudget::default(),
            )
            .unwrap();
        assert!(
            session
                .verify_migration_source_dependencies(&plan, MigrationVerificationBudget::default())
                .is_err()
        );
    }
    fs::write(
        repo.graft_dir().join("refs/heads/main"),
        format!("{head}\n"),
    )
    .unwrap();
    let plan = session
        .plan_migration_source(
            MigrationRefSelection::All,
            MigrationPlanningBudget::default(),
        )
        .unwrap();
    assert_eq!(
        session
            .verify_migration_source_dependencies(&plan, MigrationVerificationBudget::default())
            .unwrap()
            .snapshots,
        1
    );
    assert_eq!(
        db.query_row("SELECT value FROM data", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "retained source"
    );
    session.close().unwrap();
}
