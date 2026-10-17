#![cfg(all(feature = "e2ee", unix))]

use graft_sdk::{RepositorySession, e2ee::EncryptionError};
use std::{fs, os::unix::fs::symlink};

#[test]
fn initial_publication_rejects_a_different_persisted_remote_binding() {
    use graft_sdk::{EncryptedErrorCode, RemoteConfig, SdkErrorCode, e2ee::InitializationIntent};
    let fixture = tempfile::tempdir().unwrap();
    let worktree = fixture.path().join("source");
    let session = RepositorySession::new(&worktree);
    session.open().unwrap();
    session.init().unwrap();
    fs::write(worktree.join("binding.txt"), b"binding synthetic content").unwrap();
    session.add_all().unwrap();
    session.commit("binding synthetic commit").unwrap();
    let head = session.repository_metadata().unwrap().current_head.unwrap();
    let uuid = "00010203-0405-4607-8809-0a0b0c0d0e0f";
    let actual = RemoteConfig::EncryptedHttp {
        url: "https://encrypted.invalid/acme/source".into(),
        uuid: uuid.into(),
    };
    session.add_remote_with_config("vault", actual).unwrap();
    let expected = RemoteConfig::EncryptedHttp {
        url: "https://encrypted.invalid/acme/other".into(),
        uuid: uuid.into(),
    };
    let intent = InitializationIntent::new("main".into(), head.clone()).unwrap();
    assert_eq!(
        session
            .validate_initialization_remote_config("vault", &expected)
            .unwrap_err()
            .code(),
        SdkErrorCode::Encrypted(EncryptedErrorCode::StaleObservation)
    );
    let error = session
        .push_initialization_intent_with_config("vault", &expected, &intent)
        .unwrap_err();
    assert_eq!(
        error.code(),
        SdkErrorCode::Encrypted(EncryptedErrorCode::StaleObservation)
    );
    assert_eq!(
        session.repository_metadata().unwrap().current_head,
        Some(head)
    );
    session.close().unwrap();
}

#[test]
fn initial_publication_rechecks_binding_after_successful_preflight() {
    use graft_sdk::{
        EncryptedErrorCode, RemoteConfig, RemoteConfigureOptions, SdkErrorCode,
        e2ee::InitializationIntent,
    };
    let expected = RemoteConfig::EncryptedHttp {
        url: "https://encrypted.invalid/acme/source".into(),
        uuid: "00010203-0405-4607-8809-0a0b0c0d0e0f".into(),
    };
    let replacements = [
        Some(RemoteConfig::EncryptedHttp {
            url: "https://encrypted.invalid/acme/other".into(),
            uuid: "00010203-0405-4607-8809-0a0b0c0d0e0f".into(),
        }),
        Some(RemoteConfig::EncryptedHttp {
            url: "https://encrypted.invalid/acme/source".into(),
            uuid: "10010203-0405-4607-8809-0a0b0c0d0e0f".into(),
        }),
        Some(RemoteConfig::Http {
            url: "https://encrypted.invalid/acme/source".into(),
            token_env: None,
        }),
        None,
    ];
    for replacement in replacements {
        let fixture = tempfile::tempdir().unwrap();
        let worktree = fixture.path().join("source");
        let session = RepositorySession::new(&worktree);
        session.open().unwrap();
        session.init().unwrap();
        fs::write(worktree.join("binding.txt"), b"binding replacement content").unwrap();
        session.add_all().unwrap();
        session.commit("binding replacement commit").unwrap();
        let head = session.repository_metadata().unwrap().current_head.unwrap();
        let intent = InitializationIntent::new("main".into(), head.clone()).unwrap();
        session
            .add_remote_with_config("vault", expected.clone())
            .unwrap();
        session
            .validate_initialization_remote_config("vault", &expected)
            .unwrap();
        if let Some(RemoteConfig::Http { url, .. }) = &replacement {
            // Exercise replacement through the still-open SDK, without reopening.
            session
                .configure_remote(&RemoteConfigureOptions {
                    name: "vault".into(),
                    url: url.clone(),
                    bearer_token: None,
                    overwrite: true,
                    upstream_branch: None,
                })
                .unwrap();
        } else {
            session.close().unwrap();
            {
                let repository = graft::repo::Repository::open(&worktree).unwrap();
                if let Some(replacement) = replacement {
                    repository.remote_set_url("vault", replacement).unwrap();
                } else {
                    repository.remote_remove("vault").unwrap();
                }
            }
            session.open().unwrap();
        }
        let error = session
            .push_initialization_intent_with_config("vault", &expected, &intent)
            .unwrap_err();
        assert_eq!(
            error.code(),
            SdkErrorCode::Encrypted(EncryptedErrorCode::StaleObservation)
        );
        assert_eq!(
            session.repository_metadata().unwrap().current_head,
            Some(head)
        );
        session.close().unwrap();
    }
}

#[test]
fn encrypted_host_storage_is_separate_from_discovered_worktree_and_aliases() {
    let fixture = tempfile::tempdir().unwrap();
    let worktree = fixture.path().join("worktree");
    let outside = fixture.path().join("host-state");
    fs::create_dir(&outside).unwrap();
    graft::repo::Repository::init(&worktree).unwrap();
    let inside = worktree.join("private-state");
    let nested = worktree.join("nested");
    fs::create_dir(&inside).unwrap();
    fs::create_dir(&nested).unwrap();
    let alias = fixture.path().join("alias");
    symlink(&worktree, &alias).unwrap();
    for target in [&worktree, &nested, &worktree.join(".graft"), &alias] {
        let session = RepositorySession::new(target);
        session
            .validate_encrypted_host_directories(std::slice::from_ref(&outside))
            .unwrap();
        for storage in [&inside, &alias.join("private-state"), fixture.path()] {
            assert_eq!(
                session.validate_encrypted_host_directories(&[storage.to_owned()]),
                Err(EncryptionError::InvalidSessionBinding)
            );
        }
    }
}

#[test]
fn encrypted_host_storage_cannot_overlap_relocated_repository_metadata() {
    let fixture = tempfile::tempdir().unwrap();
    let worktree = fixture.path().join("worktree");
    graft::repo::Repository::init(&worktree).unwrap();
    let relocated = fixture.path().join("relocated-metadata");
    fs::rename(worktree.join(".graft"), &relocated).unwrap();
    symlink(&relocated, worktree.join(".graft")).unwrap();
    let inside = relocated.join("private-state");
    fs::create_dir(&inside).unwrap();
    assert_eq!(
        RepositorySession::new(&worktree).validate_encrypted_host_directories(&[inside]),
        Err(EncryptionError::InvalidSessionBinding)
    );
}

#[test]
fn encrypted_host_storage_validation_supports_new_destinations_without_creating_them() {
    let fixture = tempfile::tempdir().unwrap();
    let storage = fixture.path().join("host-state");
    fs::create_dir(&storage).unwrap();
    let destination = fixture.path().join("new").join("destination");
    RepositorySession::new(&destination)
        .validate_encrypted_host_directories(std::slice::from_ref(&storage))
        .unwrap();
    assert!(!destination.exists());
    let overlapping = storage.join("new").join("destination");
    assert_eq!(
        RepositorySession::new(&overlapping).validate_encrypted_host_directories(&[storage]),
        Err(EncryptionError::InvalidSessionBinding)
    );
    assert!(!overlapping.exists());
}
