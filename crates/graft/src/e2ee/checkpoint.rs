use crate::e2ee::native_fs::Metadata;

use super::{ConfigCheckpoint, Result, SessionBinding};

/// Trusted native, blocking metadata provider. Never receives a key or password.
/// load must fail for lost/corrupt existing enrollment; advance must atomically
/// reject rollback and same-generation forks against the latest durable record.
pub trait CheckpointStore: Send + Sync {
    fn load(&self, binding: &SessionBinding) -> Result<Option<ConfigCheckpoint>>;
    fn advance(&self, binding: &SessionBinding, candidate: &ConfigCheckpoint) -> Result<()>;

    /// Validate against the latest durable record and run `apply` while excluding
    /// checkpoint writers. Does not advance trust. Success requires running the
    /// callback exactly once. It must not reenter this store, and its error must
    /// be preserved. Providers without this operation cannot retire config
    /// transactions and fail closed without running the callback.
    fn with_validated_checkpoint(
        &self,
        _binding: &SessionBinding,
        _candidate: &ConfigCheckpoint,
        _apply: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        Err(super::EncryptionError::CheckpointUnavailable(
            std::io::ErrorKind::Unsupported,
        ))
    }
}

#[cfg(any(unix, windows))]
mod file {
    use super::*;
    use crate::e2ee::{EncryptionError, PROFILE, codec};
    use sha2::{Digest, Sha256};
    use std::{
        io::{Read, Write},
        path::{Path, PathBuf},
    };

    const RECORD: &str = "checkpoint";
    const LOCK: &str = "checkpoint.lock";
    const LEGACY_SIZE: usize = 105;
    const SIZE: usize = LEGACY_SIZE + 32;

    /// Unix owner-only checkpoint slot outside the repository. Explicit creation
    /// and existing opening are separate; missing enrollment is never recreated.
    /// Does not implement key remembering or protect against a local administrator.
    pub struct FileCheckpointStore {
        directory: PathBuf,
        identity: (u64, u64, u32),
        binding: [u8; 32],
    }

    impl FileCheckpointStore {
        pub fn create(directory: impl AsRef<Path>, binding: &SessionBinding) -> Result<Self> {
            let store = Self::prepare(directory.as_ref(), binding)?;
            let directory = store.directory_handle()?;
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(store.directory.join(LOCK))
                .map_err(io)?;
            let result = (|| {
                store.validate_file(&lock, LOCK)?;
                lock.try_lock().map_err(lock_error)?;
                lock.sync_all().map_err(io)?;
                store.write_record(None, true)
            })();
            if let Err(error) = result {
                // Only this live creator may remove its own unadmitted lock.
                // Existing/missing enrolled trust is never repaired on reopen.
                if store.directory_handle().is_ok()
                    && matches!(fs::symlink_metadata(store.directory.join(RECORD)), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
                    && store.validate_file(&lock, LOCK).is_ok()
                {
                    let _ = fs::remove_file(store.directory.join(LOCK));
                    let _ = fs::sync_directory(&directory);
                }
                return Err(error);
            }
            fs::sync_directory(&directory).map_err(io)?;
            Ok(store)
        }

        pub fn open(directory: impl AsRef<Path>, binding: &SessionBinding) -> Result<Self> {
            let store = Self::prepare(directory.as_ref(), binding)?;
            store.load(binding)?;
            Ok(store)
        }

        fn prepare(directory: &Path, binding: &SessionBinding) -> Result<Self> {
            if !directory.is_absolute() {
                return Err(EncryptionError::InvalidSessionBinding);
            }
            let metadata = fs::symlink_metadata(directory).map_err(io)?;
            if !metadata.is_dir() || metadata.mode() & 0o077 != 0 {
                return Err(permission());
            }
            Ok(Self {
                directory: directory.to_owned(),
                identity: (metadata.dev(), metadata.ino(), metadata.uid()),
                binding: binding_digest(binding)?,
            })
        }

        fn directory_handle(&self) -> Result<File> {
            let path = fs::symlink_metadata(&self.directory).map_err(io)?;
            if !path.is_dir()
                || path.mode() & 0o077 != 0
                || (path.dev(), path.ino(), path.uid()) != self.identity
            {
                return Err(permission());
            }
            let directory = fs::open(&self.directory).map_err(io)?;
            let opened = fs::file_metadata(&directory).map_err(io)?;
            if (opened.dev(), opened.ino(), opened.uid()) != self.identity {
                return Err(permission());
            }
            Ok(directory)
        }

        fn validate_file(&self, file: &File, name: &str) -> Result<Metadata> {
            let path = self.path_metadata(name)?;
            let opened = fs::file_metadata(file).map_err(io)?;
            if !path.is_file()
                || !opened.is_file()
                || opened.mode() & 0o077 != 0
                || opened.uid() != self.identity.2
                || opened.nlink() != 1
                || (path.dev(), path.ino()) != (opened.dev(), opened.ino())
            {
                return Err(permission());
            }
            Ok(opened)
        }

        fn path_metadata(&self, name: &str) -> Result<Metadata> {
            let metadata = fs::symlink_metadata(self.directory.join(name)).map_err(io)?;
            if !metadata.is_file()
                || metadata.mode() & 0o077 != 0
                || metadata.uid() != self.identity.2
                || metadata.nlink() != 1
            {
                return Err(permission());
            }
            Ok(metadata)
        }

        fn transaction(&self, binding: &SessionBinding) -> Result<(File, File)> {
            if self.binding != binding_digest(binding)? {
                return Err(EncryptionError::IdentityChanged);
            }
            let directory = self.directory_handle()?;
            self.path_metadata(LOCK)?;
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.directory.join(LOCK))
                .map_err(io)?;
            let metadata = self.validate_file(&lock, LOCK)?;
            if metadata.len() != 0 {
                return Err(EncryptionError::InvalidEncoding);
            }
            lock.try_lock().map_err(lock_error)?;
            Ok((directory, lock))
        }

        fn read_record(&self) -> Result<(File, Option<ConfigCheckpoint>)> {
            let length = self.path_metadata(RECORD)?.len();
            if length != SIZE as u64 && length != LEGACY_SIZE as u64 {
                return Err(EncryptionError::InvalidEncoding);
            }
            let file = fs::open(self.directory.join(RECORD)).map_err(io)?;
            let metadata = self.validate_file(&file, RECORD)?;
            if metadata.len() != length {
                return Err(EncryptionError::InvalidEncoding);
            }
            let mut bytes = Vec::with_capacity(SIZE);
            (&file)
                .take(SIZE as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(io)?;
            if bytes.len() as u64 != length
                || !matches!(
                    (&bytes[..8], bytes.len()),
                    (b"GRAFTCS1", LEGACY_SIZE) | (b"GRAFTCS2", SIZE)
                )
            {
                return Err(EncryptionError::InvalidEncoding);
            }
            if bytes.len() == SIZE
                && bytes[LEGACY_SIZE..] != Sha256::digest(&bytes[..LEGACY_SIZE])[..]
            {
                return Err(EncryptionError::InvalidEncoding);
            }
            let bytes = &bytes[..LEGACY_SIZE];
            if bytes[8..40] != self.binding {
                return Err(EncryptionError::IdentityChanged);
            }
            let checkpoint = match bytes[40] {
                0 if bytes[41..].iter().all(|byte| *byte == 0) => None,
                1 => Some(ConfigCheckpoint::from_local_bytes(&bytes[41..])?),
                _ => return Err(EncryptionError::InvalidEncoding),
            };
            Ok((file, checkpoint))
        }

        fn write_record(&self, checkpoint: Option<&ConfigCheckpoint>, initial: bool) -> Result<()> {
            let mut bytes = [0_u8; SIZE];
            bytes[..8].copy_from_slice(b"GRAFTCS2");
            bytes[8..40].copy_from_slice(&self.binding);
            if let Some(checkpoint) = checkpoint {
                bytes[40] = 1;
                bytes[41..LEGACY_SIZE].copy_from_slice(&checkpoint.to_local_bytes());
            }
            let checksum = Sha256::digest(&bytes[..LEGACY_SIZE]);
            bytes[LEGACY_SIZE..].copy_from_slice(&checksum);
            let mut staging = tempfile::NamedTempFile::new_in(&self.directory).map_err(io)?;
            let metadata = fs::file_metadata(staging.as_file()).map_err(io)?;
            if metadata.mode() & 0o077 != 0 || metadata.uid() != self.identity.2 {
                return Err(permission());
            }
            staging.write_all(&bytes).map_err(io)?;
            staging.as_file().sync_all().map_err(io)?;
            if initial {
                fs::persist(staging, &self.directory.join(RECORD), false).map_err(io)?;
            } else {
                fs::persist(staging, &self.directory.join(RECORD), true).map_err(io)?;
            }
            Ok(())
        }
    }

    impl CheckpointStore for FileCheckpointStore {
        fn load(&self, binding: &SessionBinding) -> Result<Option<ConfigCheckpoint>> {
            let (_directory, _lock) = self.transaction(binding)?;
            let (_, checkpoint) = self.read_record()?;
            if checkpoint
                .as_ref()
                .is_some_and(|checkpoint| checkpoint.repository() != binding.repository())
            {
                return Err(EncryptionError::IdentityChanged);
            }
            Ok(checkpoint)
        }
        fn advance(&self, binding: &SessionBinding, candidate: &ConfigCheckpoint) -> Result<()> {
            if candidate.repository() != binding.repository() {
                return Err(EncryptionError::IdentityChanged);
            }
            let (directory, _lock) = self.transaction(binding)?;
            let (file, previous) = self.read_record()?;
            if let Some(previous) = &previous {
                previous.require_successor(candidate)?;
            }
            if previous.as_ref() == Some(candidate)
                && fs::file_metadata(&file).map_err(io)?.len() == SIZE as u64
            {
                // Reconfirm durability after a possibly lost/failed prior fsync acknowledgement.
                file.sync_all().map_err(io)?;
            } else {
                self.write_record(Some(candidate), false)?;
            }
            fs::sync_directory(&directory).map_err(io)?;
            Ok(())
        }

        fn with_validated_checkpoint(
            &self,
            binding: &SessionBinding,
            candidate: &ConfigCheckpoint,
            apply: &mut dyn FnMut() -> Result<()>,
        ) -> Result<()> {
            if candidate.repository() != binding.repository() {
                return Err(EncryptionError::IdentityChanged);
            }
            let (_directory, _lock) = self.transaction(binding)?;
            let (_, previous) = self.read_record()?;
            if let Some(previous) = &previous {
                previous.require_successor(candidate)?;
            }
            apply()
        }
    }

    fn binding_digest(binding: &SessionBinding) -> Result<[u8; 32]> {
        Ok(Sha256::digest(codec::encode(&[
            b"graft/local-checkpoint/v1",
            PROFILE,
            binding.domain().as_bytes(),
            binding.account().as_bytes(),
            binding.repository().as_bytes(),
        ])?)
        .into())
    }
    fn io(error: std::io::Error) -> EncryptionError {
        EncryptionError::CheckpointUnavailable(error.kind())
    }
    fn lock_error(error: std::fs::TryLockError) -> EncryptionError {
        match error {
            std::fs::TryLockError::WouldBlock => EncryptionError::SessionBusy,
            std::fs::TryLockError::Error(error) => io(error),
        }
    }
    fn permission() -> EncryptionError {
        EncryptionError::CheckpointUnavailable(std::io::ErrorKind::PermissionDenied)
    }
}

#[cfg(any(unix, windows))]
pub use file::FileCheckpointStore;

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::e2ee::{
        EncryptedConfig, EncryptionError, SecretSession, SecretSessionState, tests::fixture_root,
    };
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        sync::{Arc, OnceLock},
    };

    fn binding() -> SessionBinding {
        SessionBinding::new(
            "checkpoint-domain".into(),
            "checkpoint-account".into(),
            fixture_root().repository(),
        )
        .unwrap()
    }
    fn directory() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }
    fn configs() -> &'static (EncryptedConfig, EncryptedConfig, EncryptedConfig) {
        static CONFIGS: OnceLock<(EncryptedConfig, EncryptedConfig, EncryptedConfig)> =
            OnceLock::new();
        CONFIGS.get_or_init(|| {
            let old = EncryptedConfig::create(&fixture_root(), b"checkpoint password").unwrap();
            let next = old
                .password_change_candidate(&fixture_root(), b"new checkpoint password")
                .unwrap();
            let fork = old
                .password_change_candidate(&fixture_root(), b"fork checkpoint password")
                .unwrap();
            (old, next, fork)
        })
    }

    #[test]
    fn checkpoint_restart_rejects_rollback_and_fork_before_unlocking() {
        let directory = directory();
        let store = Arc::new(FileCheckpointStore::create(directory.path(), &binding()).unwrap());
        let session = SecretSession::with_checkpoint_store(binding(), store.clone()).unwrap();
        let (old, next, fork) = configs();
        session.unlock_with_root(old, fixture_root()).unwrap();
        session
            .begin_operation()
            .unwrap()
            .observe_config(next)
            .unwrap();
        drop(session);
        drop(store);
        let store = Arc::new(FileCheckpointStore::open(directory.path(), &binding()).unwrap());
        let reopened = SecretSession::with_checkpoint_store(binding(), store.clone()).unwrap();
        assert_eq!(
            reopened.unlock_with_root(old, fixture_root()),
            Err(EncryptionError::ConfigRollbackDetected)
        );
        assert_eq!(reopened.state(), SecretSessionState::Locked);
        assert_eq!(
            reopened.unlock_with_root(fork, fixture_root()),
            Err(EncryptionError::ConfigRollbackDetected)
        );
        reopened.unlock_with_root(next, fixture_root()).unwrap();
        assert_eq!(
            store.load(&binding()).unwrap(),
            Some(next.checkpoint(&fixture_root()).unwrap())
        );
        let bytes = fs::read(directory.path().join("checkpoint")).unwrap();
        assert_eq!(bytes.len(), 137);
        let secret: Vec<u8> = (0..32).collect();
        assert!(!bytes.windows(32).any(|window| window == secret));
    }

    #[test]
    fn checkpoint_other_account_missing_corrupt_and_insecure_state_fail_closed() {
        let directory = directory();
        let store = FileCheckpointStore::create(directory.path(), &binding()).unwrap();
        let other = SessionBinding::new(
            "checkpoint-domain".into(),
            "other-account".into(),
            fixture_root().repository(),
        )
        .unwrap();
        assert!(matches!(
            FileCheckpointStore::open(directory.path(), &other),
            Err(EncryptionError::IdentityChanged)
        ));
        let record = directory.path().join("checkpoint");
        let valid = fs::read(&record).unwrap();
        fs::write(&record, b"GRAFTCS1").unwrap();
        assert!(store.load(&binding()).is_err());
        assert!(FileCheckpointStore::create(directory.path(), &binding()).is_err());
        fs::write(&record, &valid).unwrap();
        fs::set_permissions(&record, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.load(&binding()).is_err());
        fs::set_permissions(&record, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_file(&record).unwrap();
        assert!(matches!(
            store.load(&binding()),
            Err(EncryptionError::CheckpointUnavailable(
                std::io::ErrorKind::NotFound
            ))
        ));
        assert!(FileCheckpointStore::create(directory.path(), &binding()).is_err());
        assert!(!record.exists());
    }

    #[test]
    fn checkpoint_shared_writers_recheck_disk_and_never_wait_on_another_lock() {
        let directory = directory();
        let first = FileCheckpointStore::create(directory.path(), &binding()).unwrap();
        let second = FileCheckpointStore::open(directory.path(), &binding()).unwrap();
        let (old, next, _) = configs();
        first
            .advance(&binding(), &old.checkpoint(&fixture_root()).unwrap())
            .unwrap();
        second
            .advance(&binding(), &next.checkpoint(&fixture_root()).unwrap())
            .unwrap();
        assert_eq!(
            first.advance(&binding(), &old.checkpoint(&fixture_root()).unwrap()),
            Err(EncryptionError::ConfigRollbackDetected)
        );
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.path().join("checkpoint.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        assert_eq!(first.load(&binding()), Err(EncryptionError::SessionBusy));
        drop(lock);
        assert_eq!(
            first.load(&binding()).unwrap(),
            Some(next.checkpoint(&fixture_root()).unwrap())
        );
    }

    #[test]
    fn checkpoint_validation_holds_shared_writer_lock_and_never_advances_trust() {
        let directory = directory();
        let store = FileCheckpointStore::create(directory.path(), &binding()).unwrap();
        let peer = FileCheckpointStore::open(directory.path(), &binding()).unwrap();
        let (old, next, fork) = configs();
        let old = old.checkpoint(&fixture_root()).unwrap();
        let next = next.checkpoint(&fixture_root()).unwrap();
        let fork = fork.checkpoint(&fixture_root()).unwrap();
        store.advance(&binding(), &old).unwrap();
        let mut calls = 0;
        assert_eq!(
            store.with_validated_checkpoint(&binding(), &next, &mut || {
                calls += 1;
                assert_eq!(
                    peer.advance(&binding(), &fork),
                    Err(EncryptionError::SessionBusy)
                );
                Err(EncryptionError::CheckpointUnavailable(
                    std::io::ErrorKind::Other,
                ))
            }),
            Err(EncryptionError::CheckpointUnavailable(
                std::io::ErrorKind::Other
            ))
        );
        assert_eq!(calls, 1);
        assert_eq!(store.load(&binding()).unwrap(), Some(old.clone()));
        store
            .with_validated_checkpoint(&binding(), &next, &mut || {
                calls += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(store.load(&binding()).unwrap(), Some(old));
        peer.advance(&binding(), &fork).unwrap();
        assert_eq!(
            store.with_validated_checkpoint(&binding(), &next, &mut || {
                calls += 1;
                Ok(())
            }),
            Err(EncryptionError::ConfigRollbackDetected)
        );
        assert_eq!(calls, 2);
        assert_eq!(store.load(&binding()).unwrap(), Some(fork));
    }

    #[test]
    fn checkpoint_refuses_symlinks_hardlinks_and_changed_directory_identity() {
        let directory = directory();
        let store = FileCheckpointStore::create(directory.path(), &binding()).unwrap();
        let record = directory.path().join("checkpoint");
        let saved = directory.path().join("saved");
        fs::rename(&record, &saved).unwrap();
        std::os::unix::fs::symlink(&saved, &record).unwrap();
        assert!(store.load(&binding()).is_err());
        fs::remove_file(&record).unwrap();
        fs::hard_link(&saved, &record).unwrap();
        assert!(store.load(&binding()).is_err());
        fs::remove_file(&record).unwrap();
        fs::rename(&saved, &record).unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(store.load(&binding()).is_err());
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let replacement = directory.path().with_extension("replacement");
        fs::rename(directory.path(), &replacement).unwrap();
        fs::create_dir(directory.path()).unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(store.load(&binding()).is_err());
        fs::remove_dir(directory.path()).unwrap();
        fs::rename(replacement, directory.path()).unwrap();
    }

    struct FailingStore;
    impl CheckpointStore for FailingStore {
        fn load(&self, _: &SessionBinding) -> Result<Option<ConfigCheckpoint>> {
            Ok(None)
        }
        fn advance(&self, _: &SessionBinding, _: &ConfigCheckpoint) -> Result<()> {
            Err(EncryptionError::CheckpointUnavailable(
                std::io::ErrorKind::PermissionDenied,
            ))
        }
    }
    #[test]
    fn checkpoint_write_failure_does_not_admit_root_or_advance_memory_trust() {
        let session =
            SecretSession::with_checkpoint_store(binding(), Arc::new(FailingStore)).unwrap();
        assert!(matches!(
            session.unlock_with_root(&configs().0, fixture_root()),
            Err(EncryptionError::CheckpointUnavailable(_))
        ));
        assert_eq!(session.state(), SecretSessionState::Locked);
        assert!(session.checkpoint().is_none());
        assert!(matches!(
            session.begin_operation(),
            Err(EncryptionError::Locked)
        ));
    }
}
use crate::e2ee::native_fs::{self as fs, File, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
