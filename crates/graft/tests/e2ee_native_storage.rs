#![cfg(all(feature = "e2ee", any(unix, windows)))]
use graft::e2ee::{
    CheckpointStore, ChunkSize, EncryptedConfig, FileCheckpointStore, FileInitializationJournal,
    FileMultipartJournal, FileMultipartJournalProvider, FileRefMutationJournalProvider,
    InitializationIntent, InitializationJournal, InitializationStage, MultipartJournalProvider,
    ObjectSnapshot, PreparedEncryptedObject, RefMutationJournalProvider, RepositoryId, RootKey,
    SecretSession, SessionBinding, native_fs,
};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc};

fn private(parent: &Path, name: &str) -> std::path::PathBuf {
    let path = parent.join(name);
    native_fs::create_private_directory(&path).unwrap();
    native_fs::validate_private_directory(&path).unwrap();
    path
}

#[test]
fn temporary_verification_directory_has_native_private_ownership_and_permissions() {
    let temp = tempfile::tempdir().unwrap();
    let staging = private(temp.path(), "staging");
    let verification = native_fs::private_tempdir("verification-", &staging).unwrap();
    let path = verification.path().to_owned();
    native_fs::validate_private_directory(&path).unwrap();
    let nested = private(&path, "checkpoint");
    native_fs::validate_private_directory(&nested).unwrap();
    drop(verification);
    assert!(!path.exists());
}

#[cfg(windows)]
#[test]
fn windows_private_storage_rejects_broad_acls_and_directory_junctions() {
    let temp = tempfile::tempdir().unwrap();
    let parent = private(temp.path(), "private-parent");
    let inheriting = private(&parent, "inheriting");
    assert!(
        std::process::Command::new("icacls")
            .arg(&inheriting)
            .arg("/inheritance:e")
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(native_fs::validate_private_directory(&inheriting).is_err());
    let broad = private(temp.path(), "broad");
    assert!(
        std::process::Command::new("icacls")
            .arg(&broad)
            .args(["/grant", "*S-1-1-0:(OI)(CI)F"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(native_fs::validate_private_directory(&broad).is_err());
    let target = private(temp.path(), "target");
    let alias = temp.path().join("alias");
    assert!(
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&alias)
            .arg(&target)
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(native_fs::validate_private_directory(&alias).is_err());
}

#[test]
fn native_checkpoint_and_initialization_reopen_preserve_trust_and_source() {
    let temp = tempfile::tempdir().unwrap();
    let path = private(temp.path(), "checkpoint");
    let root = RootKey::generate(RepositoryId::generate().unwrap()).unwrap();
    let binding =
        SessionBinding::new("native-storage".into(), "account".into(), root.repository()).unwrap();
    let config = EncryptedConfig::create(&root, b"native journal password").unwrap();
    let checkpoint = config.checkpoint(&root).unwrap();
    let store = FileCheckpointStore::create(&path, &binding).unwrap();
    assert!(store.load(&binding).unwrap().is_none());
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(path.join("checkpoint"))
                .unwrap()
                .file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_TEMPORARY,
            0
        );
    }
    store.advance(&binding, &checkpoint).unwrap();
    drop(store);
    let store = FileCheckpointStore::open(&path, &binding).unwrap();
    assert_eq!(store.load(&binding).unwrap(), Some(checkpoint.clone()));
    // Existing CS1 enrollment remains readable, then upgrades on confirmation.
    let mut old = std::fs::read(path.join("checkpoint")).unwrap();
    old.truncate(105);
    old[..8].copy_from_slice(b"GRAFTCS1");
    std::fs::write(path.join("checkpoint"), old).unwrap();
    store.advance(&binding, &checkpoint).unwrap();
    let mut bytes = std::fs::read(path.join("checkpoint")).unwrap();
    assert_eq!(&bytes[..8], b"GRAFTCS2");
    bytes[104] ^= 1;
    std::fs::write(path.join("checkpoint"), bytes).unwrap();
    assert!(store.load(&binding).is_err());

    let initialization = private(temp.path(), "initialization");
    let intent = InitializationIntent::new("main".into(), "1".repeat(64)).unwrap();
    let journal =
        FileInitializationJournal::create_with_intent(&initialization, &binding, &config, &intent)
            .unwrap();
    journal
        .advance(
            &binding,
            InitializationStage::Prepared,
            InitializationStage::ConfigPublished,
        )
        .unwrap();
    drop(journal);
    let reopened = FileInitializationJournal::open(&initialization, &binding).unwrap();
    assert!(reopened.load(&binding).unwrap().intent.as_ref() == Some(&intent));
    std::fs::remove_file(initialization.join("initialization.intent")).unwrap();
    assert!(reopened.load(&binding).is_err());

    let refs = private(temp.path(), "refs");
    let provider = FileRefMutationJournalProvider::create(&refs, &binding).unwrap();
    let key = format!("e2ee/refs/{}", "1".repeat(64));
    assert!(
        provider
            .journal(&binding, &key)
            .unwrap()
            .load(&binding)
            .unwrap()
            .is_none()
    );
    drop(provider);
    assert!(
        FileRefMutationJournalProvider::open(&refs, &binding)
            .unwrap()
            .journal(&binding, &key)
            .unwrap()
            .load(&binding)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn native_multipart_namespace_rejects_lost_enrollment() {
    let temp = tempfile::tempdir().unwrap();
    let root = RootKey::generate(RepositoryId::generate().unwrap()).unwrap();
    let binding = SessionBinding::new(
        "native-multipart".into(),
        "account".into(),
        root.repository(),
    )
    .unwrap();
    let config = EncryptedConfig::create(&root, b"native multipart password").unwrap();
    let session = SecretSession::new(binding, None).unwrap();
    session
        .unlock(
            config,
            zeroize::Zeroizing::new(b"native multipart password".to_vec()),
        )
        .await
        .unwrap();
    let operation = session.begin_operation().unwrap();
    let staging = private(temp.path(), "staging");
    let recovery = private(temp.path(), "recovery");
    let provider =
        Arc::new(FileMultipartJournalProvider::create(&recovery, session.binding()).unwrap());
    let bytes = b"retained immutable plaintext";
    let candidate = PreparedEncryptedObject::prepare(
        &operation,
        ObjectSnapshot {
            logical_key: "store/files/native-retry".into(),
            plaintext_length: bytes.len() as u64,
            plaintext_sha256: Sha256::digest(bytes).into(),
            chunk_size: ChunkSize::KiB16,
        },
        &mut bytes.as_slice(),
        &staging,
        100_000,
    )
    .await
    .unwrap();
    // HTTP transport enrolls the actual candidate; this public surface cannot
    // manufacture an admitted recovery receipt from a mere directory listing.
    let slot = provider
        .directory(session.binding(), candidate.remote_key())
        .unwrap();
    FileMultipartJournal::open(&slot, session.binding()).unwrap();
    assert!(
        provider
            .retained_uploads(session.binding())
            .unwrap()
            .is_empty()
    );
    std::fs::remove_file(slot.join("ref-mutation")).unwrap();
    assert!(provider.retained_uploads(session.binding()).is_err());
}
