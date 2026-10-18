//! Host-private initialization recovery. Records contain wrapped config, never secrets.
use crate::e2ee::native_fs::{self as fs, File, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use super::EncryptionError;
use super::{EncryptedConfig, Result, SessionBinding};

/// Immutable trusted-host intent. Private metadata, not a remote routing hint.
/// No passwords or keys; do not expose this through diagnostics or plugin RPC.
#[derive(Clone, PartialEq, Eq)]
pub struct InitializationIntent {
    branch: String,
    commit: crate::repo::object::ObjectId,
}

impl InitializationIntent {
    pub fn new(branch: String, commit: String) -> Result<Self> {
        let reference = format!("refs/heads/{branch}");
        if reference.len() > 4096 {
            return Err(EncryptionError::TooLarge);
        }
        crate::repo::validate_full_ref(&reference).map_err(|_| EncryptionError::InvalidEncoding)?;
        super::reference::parse_commit(&commit)?;
        let commit = crate::repo::object::ObjectId::new(commit)
            .map_err(|_| EncryptionError::InvalidEncoding)?;
        Ok(Self { branch, commit })
    }
    pub fn branch(&self) -> &str {
        &self.branch
    }
    pub fn commit(&self) -> &str {
        self.commit.as_str()
    }
    /// Reject a changed source instead of replacing the retained intent.
    pub fn require_source(&self, branch: &str, commit: &str) -> Result<()> {
        if branch != self.branch || commit != self.commit.as_str() {
            return Err(EncryptionError::StaleObservation);
        }
        Ok(())
    }
}

/// A durable stage is advanced only after the caller has verified its effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum InitializationStage {
    Prepared = 0,
    ConfigPublished = 1,
    ObjectsPublished = 2,
    RefPublished = 3,
    Connected = 4,
}

#[cfg(any(unix, windows))]
impl InitializationStage {
    fn parse(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Prepared),
            1 => Ok(Self::ConfigPublished),
            2 => Ok(Self::ObjectsPublished),
            3 => Ok(Self::RefPublished),
            4 => Ok(Self::Connected),
            _ => Err(EncryptionError::InvalidEncoding),
        }
    }
}

/// The exact wrapped config must be authenticated before resuming publication.
/// A stage records local progress; it does not prove remote availability/freshness.
pub struct InitializationRecord {
    pub stage: InitializationStage,
    pub config: EncryptedConfig,
    pub intent: Option<InitializationIntent>,
}

/// Configuration publication is distinct from a fully connected repository.
pub enum InitialConfigPublicationOutcome {
    Confirmed,
    /// Exact candidate is still absent; retry must reuse the journal's bytes.
    Unconfirmed,
    /// Create-if-absent was rejected and another configuration was observed.
    /// This does not authenticate the winner or authorize using the losing root.
    Conflict,
    OutcomeUnknown {
        error: Option<super::EncryptedHttpError>,
    },
}

/// Blocking host-private metadata storage; no password or root-key input.
pub trait InitializationJournal: Send + Sync {
    fn load(&self, binding: &SessionBinding) -> Result<InitializationRecord>;
    /// Persist before initial data publication. Same intent is idempotent;
    /// replacement is forbidden. Providers must explicitly support this contract.
    fn bind_intent(&self, _binding: &SessionBinding, _intent: &InitializationIntent) -> Result<()> {
        Err(EncryptionError::UnsupportedProfile)
    }
    /// Exact adjacent-stage CAS. Repeating the same transition is idempotent.
    /// Caller must confirm the remote effect before requesting advancement.
    fn advance(
        &self,
        binding: &SessionBinding,
        expected: InitializationStage,
        next: InitializationStage,
    ) -> Result<()>;
}

#[cfg(any(unix, windows))]
mod file {
    use super::*;
    use crate::e2ee::{PROFILE, codec};
    use sha2::{Digest, Sha256};
    use std::{
        io::{Read, Write},
        path::{Path, PathBuf},
    };

    const RECORD: &str = "initialization";
    const LOCK: &str = "initialization.lock";
    const INTENT: &str = "initialization.intent";
    const SIZE: usize = 344;
    const LEGACY_SIZE: usize = 311;
    const MAX_INTENT: usize = 8192;

    /// Unix owner-only slot outside the repository. Create and reopen are explicit.
    /// Missing/corrupt state is never recreated. Does not protect against local admins.
    pub struct FileInitializationJournal {
        directory: PathBuf,
        identity: (u64, u64, u32),
        binding: [u8; 32],
        candidate: [u8; 206],
    }

    impl FileInitializationJournal {
        pub fn create(
            directory: impl AsRef<Path>,
            binding: &SessionBinding,
            config: &EncryptedConfig,
        ) -> Result<Self> {
            Self::create_prepared(directory.as_ref(), binding, config, None)
        }

        /// Install the private source intent before the prepared main record.
        /// A readable main record therefore never loses its originally selected source.
        pub fn create_with_intent(
            directory: impl AsRef<Path>,
            binding: &SessionBinding,
            config: &EncryptedConfig,
            intent: &InitializationIntent,
        ) -> Result<Self> {
            Self::create_prepared(directory.as_ref(), binding, config, Some(intent))
        }

        fn create_prepared(
            directory: &Path,
            binding: &SessionBinding,
            config: &EncryptedConfig,
            intent: Option<&InitializationIntent>,
        ) -> Result<Self> {
            if config.repository() != binding.repository() {
                return Err(EncryptionError::IdentityChanged);
            }
            if config.generation() != 1 {
                return Err(EncryptionError::InvalidEncoding);
            }
            let mut store = Self::prepare(directory, binding)?;
            store.candidate.copy_from_slice(config.as_bytes());
            let directory = store.directory_handle()?;
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(store.directory.join(LOCK))
                .map_err(io)?;
            store.validate_file(&lock, LOCK, 0)?;
            lock.try_lock().map_err(lock_error)?;
            #[cfg(test)]
            persistence_failure("file-sync", LOCK, &store.directory).map_err(io)?;
            lock.sync_all().map_err(io)?;
            #[cfg(test)]
            persistence_crash("lock-synced", LOCK, &store.directory);
            if let Some(intent) = intent {
                store.install_bytes(INTENT, &store.intent_bytes(intent)?, true)?;
                #[cfg(test)]
                persistence_failure("directory-sync", INTENT, &store.directory).map_err(io)?;
                fs::sync_directory(&directory).map_err(io)?;
                #[cfg(test)]
                persistence_crash("directory-synced", INTENT, &store.directory);
            }
            store.write_record(InitializationStage::Prepared, intent, true)?;
            #[cfg(test)]
            persistence_failure("directory-sync", RECORD, &store.directory).map_err(io)?;
            fs::sync_directory(&directory).map_err(io)?;
            #[cfg(test)]
            persistence_crash("directory-synced", RECORD, &store.directory);
            Ok(store)
        }

        pub fn open(directory: impl AsRef<Path>, binding: &SessionBinding) -> Result<Self> {
            let mut store = Self::prepare(directory.as_ref(), binding)?;
            let (_directory, _lock) = store.transaction(binding)?;
            let (_file, record) = store.read_record(false)?;
            if record.config.repository() != binding.repository() {
                return Err(EncryptionError::IdentityChanged);
            }
            store.candidate.copy_from_slice(record.config.as_bytes());
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
                binding: digest(binding)?,
                candidate: [0; 206],
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
            let file = fs::open(&self.directory).map_err(io)?;
            let opened = fs::file_metadata(&file).map_err(io)?;
            if (opened.dev(), opened.ino(), opened.uid()) != self.identity {
                return Err(permission());
            }
            Ok(file)
        }

        fn validate_file(&self, file: &File, name: &str, length: u64) -> Result<()> {
            let path = self.path_metadata(name, length)?;
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
            if opened.len() != length {
                return Err(EncryptionError::InvalidEncoding);
            }
            Ok(())
        }

        fn path_metadata(&self, name: &str, length: u64) -> Result<fs::Metadata> {
            let metadata = fs::symlink_metadata(self.directory.join(name)).map_err(io)?;
            if !metadata.is_file()
                || metadata.mode() & 0o077 != 0
                || metadata.uid() != self.identity.2
                || metadata.nlink() != 1
            {
                return Err(permission());
            }
            if metadata.len() != length {
                return Err(EncryptionError::InvalidEncoding);
            }
            Ok(metadata)
        }

        fn transaction(&self, binding: &SessionBinding) -> Result<(File, File)> {
            if self.binding != digest(binding)? {
                return Err(EncryptionError::IdentityChanged);
            }
            let directory = self.directory_handle()?;
            self.path_metadata(LOCK, 0)?;
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.directory.join(LOCK))
                .map_err(io)?;
            self.validate_file(&lock, LOCK, 0)?;
            lock.try_lock().map_err(lock_error)?;
            Ok((directory, lock))
        }

        fn read_record(&self, require_candidate: bool) -> Result<(File, InitializationRecord)> {
            let length = fs::symlink_metadata(self.directory.join(RECORD))
                .map_err(io)?
                .len();
            if length != SIZE as u64 && length != LEGACY_SIZE as u64 {
                return Err(EncryptionError::InvalidEncoding);
            }
            self.path_metadata(RECORD, length)?;
            let file = fs::open(self.directory.join(RECORD)).map_err(io)?;
            self.validate_file(&file, RECORD, length)?;
            let mut bytes = Vec::with_capacity(SIZE);
            (&file)
                .take(SIZE as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(io)?;
            if bytes.len() != length as usize {
                return Err(EncryptionError::InvalidEncoding);
            }
            let checksum_end = match (&bytes[..8], bytes.len()) {
                (b"GRAFTIJ1", LEGACY_SIZE) => 279,
                (b"GRAFTIJ2", SIZE) => 312,
                _ => return Err(EncryptionError::InvalidEncoding),
            };
            if bytes[8..40] != self.binding {
                return Err(EncryptionError::IdentityChanged);
            }
            if bytes[checksum_end..] != Sha256::digest(&bytes[..checksum_end])[..]
                || bytes[247..279] != Sha256::digest(&bytes[41..247])[..]
            {
                return Err(EncryptionError::InvalidEncoding);
            }
            if require_candidate && bytes[41..247] != self.candidate {
                return Err(EncryptionError::StaleObservation);
            }
            let config = EncryptedConfig::parse(&bytes[41..247])?;
            if config.generation() != 1 {
                return Err(EncryptionError::InvalidEncoding);
            }
            let intent = if checksum_end == 312 {
                match bytes[279] {
                    0 if bytes[280..312].iter().all(|byte| *byte == 0) => None,
                    1 => {
                        let value = self
                            .read_intent(config.as_bytes(), Some(&bytes[280..312]))?
                            .ok_or(EncryptionError::CheckpointUnavailable(
                                std::io::ErrorKind::NotFound,
                            ))?;
                        Some(value.1)
                    }
                    _ => return Err(EncryptionError::InvalidEncoding),
                }
            } else {
                None
            };
            Ok((
                file,
                InitializationRecord {
                    stage: InitializationStage::parse(bytes[40])?,
                    config,
                    intent,
                },
            ))
        }

        fn write_record(
            &self,
            stage: InitializationStage,
            intent: Option<&InitializationIntent>,
            initial: bool,
        ) -> Result<()> {
            let mut bytes = [0; SIZE];
            bytes[..8].copy_from_slice(b"GRAFTIJ2");
            bytes[8..40].copy_from_slice(&self.binding);
            bytes[40] = stage as u8;
            bytes[41..247].copy_from_slice(&self.candidate);
            bytes[247..279].copy_from_slice(&Sha256::digest(self.candidate));
            if let Some(intent) = intent {
                bytes[279] = 1;
                bytes[280..312].copy_from_slice(&Sha256::digest(self.intent_bytes(intent)?));
            }
            let checksum = Sha256::digest(&bytes[..312]);
            bytes[312..].copy_from_slice(&checksum);
            self.install_bytes(RECORD, &bytes, initial)
        }

        fn install_bytes(&self, name: &str, bytes: &[u8], initial: bool) -> Result<()> {
            let mut staging = tempfile::NamedTempFile::new_in(&self.directory).map_err(io)?;
            let metadata = fs::file_metadata(staging.as_file()).map_err(io)?;
            if metadata.mode() & 0o077 != 0 || metadata.uid() != self.identity.2 {
                return Err(permission());
            }
            #[cfg(test)]
            if let Err(error) = persistence_failure("partial-write", name, &self.directory) {
                staging.write_all(&bytes[..bytes.len() / 2]).map_err(io)?;
                return Err(io(error));
            }
            #[cfg(test)]
            persistence_failure("write", name, &self.directory).map_err(io)?;
            staging.write_all(bytes).map_err(io)?;
            #[cfg(test)]
            persistence_crash("written", name, &self.directory);
            #[cfg(test)]
            persistence_failure("file-sync", name, &self.directory).map_err(io)?;
            staging.as_file().sync_all().map_err(io)?;
            #[cfg(test)]
            persistence_crash("file-synced", name, &self.directory);
            #[cfg(test)]
            persistence_failure("install", name, &self.directory).map_err(io)?;
            if initial {
                fs::persist(staging, &self.directory.join(name), false).map_err(io)?;
            } else {
                fs::persist(staging, &self.directory.join(name), true).map_err(io)?;
            }
            #[cfg(test)]
            persistence_crash("installed", name, &self.directory);
            Ok(())
        }

        fn intent_bytes(&self, intent: &InitializationIntent) -> Result<Vec<u8>> {
            let payload = codec::encode(&[intent.branch().as_bytes(), intent.commit().as_bytes()])?;
            let mut bytes = Vec::with_capacity(104 + payload.len());
            bytes.extend_from_slice(b"GRAFTIP1");
            bytes.extend_from_slice(&self.binding);
            bytes.extend_from_slice(&Sha256::digest(self.candidate));
            bytes.extend_from_slice(&payload);
            bytes.extend_from_slice(&Sha256::digest(&bytes));
            Ok(bytes)
        }

        fn read_intent(
            &self,
            config: &[u8],
            expected: Option<&[u8]>,
        ) -> Result<Option<(File, InitializationIntent)>> {
            let metadata = match fs::symlink_metadata(self.directory.join(INTENT)) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(io(error)),
            };
            if metadata.len() > MAX_INTENT as u64 || metadata.len() < 114 {
                return Err(EncryptionError::InvalidEncoding);
            }
            self.path_metadata(INTENT, metadata.len())?;
            let file = fs::open(self.directory.join(INTENT)).map_err(io)?;
            self.validate_file(&file, INTENT, metadata.len())?;
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            (&file)
                .take(MAX_INTENT as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(io)?;
            if bytes.len() != metadata.len() as usize || &bytes[..8] != b"GRAFTIP1" {
                return Err(EncryptionError::InvalidEncoding);
            }
            if bytes[8..40] != self.binding || bytes[40..72] != Sha256::digest(config)[..] {
                return Err(EncryptionError::IdentityChanged);
            }
            let end = bytes.len() - 32;
            if bytes[end..] != Sha256::digest(&bytes[..end])[..] {
                return Err(EncryptionError::InvalidEncoding);
            }
            if expected.is_some_and(|value| value != &Sha256::digest(&bytes)[..]) {
                return Err(EncryptionError::StaleObservation);
            }
            let fields = codec::decode(&bytes[72..end], 2, MAX_INTENT)?;
            let branch =
                std::str::from_utf8(fields[0]).map_err(|_| EncryptionError::InvalidEncoding)?;
            let commit =
                std::str::from_utf8(fields[1]).map_err(|_| EncryptionError::InvalidEncoding)?;
            Ok(Some((
                file,
                InitializationIntent::new(branch.to_owned(), commit.to_owned())?,
            )))
        }
    }

    impl InitializationJournal for FileInitializationJournal {
        fn load(&self, binding: &SessionBinding) -> Result<InitializationRecord> {
            let (_directory, _lock) = self.transaction(binding)?;
            let (_, record) = self.read_record(true)?;
            if record.config.repository() != binding.repository() {
                return Err(EncryptionError::IdentityChanged);
            }
            Ok(record)
        }

        fn bind_intent(
            &self,
            binding: &SessionBinding,
            intent: &InitializationIntent,
        ) -> Result<()> {
            let (directory, _lock) = self.transaction(binding)?;
            let (record_file, record) = self.read_record(true)?;
            if record.config.repository() != binding.repository() {
                return Err(EncryptionError::IdentityChanged);
            }
            if let Some(previous) = &record.intent {
                if previous != intent {
                    return Err(EncryptionError::StaleObservation);
                }
                let (file, _) = self.read_intent(record.config.as_bytes(), None)?.ok_or(
                    EncryptionError::CheckpointUnavailable(std::io::ErrorKind::NotFound),
                )?;
                #[cfg(test)]
                persistence_failure("file-sync", INTENT, &self.directory).map_err(io)?;
                file.sync_all().map_err(io)?;
                #[cfg(test)]
                persistence_failure("file-sync", RECORD, &self.directory).map_err(io)?;
                record_file.sync_all().map_err(io)?;
                #[cfg(test)]
                persistence_failure("directory-sync", RECORD, &self.directory).map_err(io)?;
                return fs::sync_directory(&directory).map_err(io);
            }
            if !matches!(
                record.stage,
                InitializationStage::Prepared | InitializationStage::ConfigPublished
            ) {
                return Err(EncryptionError::StaleObservation);
            }
            // An interrupted bind may leave the sidecar but not its main-record
            // digest. Only explicit binding of the same candidate may adopt it.
            if let Some((file, previous)) = self.read_intent(record.config.as_bytes(), None)? {
                if previous != *intent {
                    return Err(EncryptionError::StaleObservation);
                }
                #[cfg(test)]
                persistence_failure("file-sync", INTENT, &self.directory).map_err(io)?;
                file.sync_all().map_err(io)?;
            } else {
                self.install_bytes(INTENT, &self.intent_bytes(intent)?, true)?;
            }
            #[cfg(test)]
            persistence_failure("directory-sync", INTENT, &self.directory).map_err(io)?;
            fs::sync_directory(&directory).map_err(io)?;
            #[cfg(test)]
            persistence_crash("directory-synced", INTENT, &self.directory);
            self.write_record(record.stage, Some(intent), false)?;
            #[cfg(test)]
            persistence_failure("directory-sync", RECORD, &self.directory).map_err(io)?;
            fs::sync_directory(&directory).map_err(io)?;
            #[cfg(test)]
            persistence_crash("directory-synced", RECORD, &self.directory);
            Ok(())
        }

        fn advance(
            &self,
            binding: &SessionBinding,
            expected: InitializationStage,
            next: InitializationStage,
        ) -> Result<()> {
            if (expected as u8).checked_add(1) != Some(next as u8) {
                return Err(EncryptionError::StaleObservation);
            }
            let (directory, _lock) = self.transaction(binding)?;
            let (file, record) = self.read_record(true)?;
            if record.config.repository() != binding.repository() {
                return Err(EncryptionError::IdentityChanged);
            }
            if next as u8 >= InitializationStage::ObjectsPublished as u8 && record.intent.is_none()
            {
                return Err(EncryptionError::StaleObservation);
            }
            if record.stage == next {
                #[cfg(test)]
                persistence_failure("file-sync", RECORD, &self.directory).map_err(io)?;
                file.sync_all().map_err(io)?;
            } else if record.stage == expected {
                self.write_record(next, record.intent.as_ref(), false)?;
            } else {
                return Err(EncryptionError::StaleObservation);
            }
            #[cfg(test)]
            persistence_failure("directory-sync", RECORD, &self.directory).map_err(io)?;
            fs::sync_directory(&directory).map_err(io)?;
            #[cfg(test)]
            persistence_crash("directory-synced", RECORD, &self.directory);
            Ok(())
        }
    }

    #[cfg(test)]
    struct PersistenceFailure {
        phase: &'static str,
        artifact: &'static str,
        directory: PathBuf,
    }

    #[cfg(test)]
    std::thread_local! {
        static PERSISTENCE_FAILURE: std::cell::RefCell<Option<PersistenceFailure>> = const {
            std::cell::RefCell::new(None)
        };
    }

    #[cfg(test)]
    pub(super) fn with_persistence_failure<T>(
        phase: &'static str,
        artifact: &'static str,
        directory: &Path,
        operation: impl FnOnce() -> T,
    ) -> T {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                PERSISTENCE_FAILURE.with(|slot| *slot.borrow_mut() = None);
            }
        }
        PERSISTENCE_FAILURE.with(|slot| {
            let mut slot = slot.borrow_mut();
            assert!(slot.is_none(), "nested synthetic persistence fault");
            *slot = Some(PersistenceFailure {
                phase,
                artifact,
                directory: directory.to_owned(),
            });
        });
        let _reset = Reset;
        operation()
    }

    #[cfg(test)]
    fn persistence_failure(phase: &str, artifact: &str, directory: &Path) -> std::io::Result<()> {
        let fail = PERSISTENCE_FAILURE.with(|slot| {
            slot.borrow().as_ref().is_some_and(|fault| {
                fault.phase == phase && fault.artifact == artifact && fault.directory == directory
            })
        });
        if fail {
            return Err(std::io::Error::other(
                "synthetic initialization persistence failure",
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn persistence_crash(phase: &str, artifact: &str, directory: &Path) {
        if std::env::var("GRAFT_TEST_INITIALIZATION_PHASE").as_deref() == Ok(phase)
            && std::env::var("GRAFT_TEST_INITIALIZATION_ARTIFACT").as_deref() == Ok(artifact)
            && std::env::var_os("GRAFT_TEST_INITIALIZATION_DIRECTORY").as_deref()
                == Some(directory.as_os_str())
        {
            // Abrupt process exit without Rust unwinding or temporary-file cleanup.
            // This tests process interruption, not power loss or storage failure.
            std::process::exit(73);
        }
    }

    fn digest(binding: &SessionBinding) -> Result<[u8; 32]> {
        Ok(Sha256::digest(codec::encode(&[
            b"graft/local-initialization/v1",
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
    fn permission() -> EncryptionError {
        EncryptionError::CheckpointUnavailable(std::io::ErrorKind::PermissionDenied)
    }
    fn lock_error(error: std::fs::TryLockError) -> EncryptionError {
        match error {
            std::fs::TryLockError::WouldBlock => EncryptionError::SessionBusy,
            std::fs::TryLockError::Error(error) => io(error),
        }
    }
}

#[cfg(any(unix, windows))]
pub use file::FileInitializationJournal;

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::e2ee::{RootKey, tests::fixture_root};
    use sha2::{Digest, Sha256};
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        sync::OnceLock,
    };

    fn binding() -> SessionBinding {
        SessionBinding::new(
            "initialization.test".into(),
            "synthetic-account".into(),
            fixture_root().repository(),
        )
        .unwrap()
    }
    fn directory() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }
    fn config() -> &'static EncryptedConfig {
        static CONFIG: OnceLock<EncryptedConfig> = OnceLock::new();
        CONFIG.get_or_init(|| {
            EncryptedConfig::create(&fixture_root(), b"initialization synthetic password").unwrap()
        })
    }

    fn intent() -> InitializationIntent {
        InitializationIntent::new("main".into(), "a".repeat(64)).unwrap()
    }

    const STAGES: [InitializationStage; 5] = [
        InitializationStage::Prepared,
        InitializationStage::ConfigPublished,
        InitializationStage::ObjectsPublished,
        InitializationStage::RefPublished,
        InitializationStage::Connected,
    ];

    #[test]
    fn initialization_persistence_crash_child() {
        let Some(directory) = std::env::var_os("GRAFT_TEST_INITIALIZATION_DIRECTORY") else {
            return;
        };
        let path = std::path::Path::new(&directory);
        let candidate =
            EncryptedConfig::parse(&fs::read(path.join("test-config")).unwrap()).unwrap();
        let operation = std::env::var("GRAFT_TEST_INITIALIZATION_OPERATION").unwrap();
        match operation.as_str() {
            "create" => {
                FileInitializationJournal::create_with_intent(
                    path,
                    &binding(),
                    &candidate,
                    &intent(),
                )
                .unwrap();
            }
            "bind" => {
                FileInitializationJournal::open(path, &binding())
                    .unwrap()
                    .bind_intent(&binding(), &intent())
                    .unwrap();
            }
            "advance" => {
                let next: usize = std::env::var("GRAFT_TEST_INITIALIZATION_NEXT")
                    .unwrap()
                    .parse()
                    .unwrap();
                FileInitializationJournal::open(path, &binding())
                    .unwrap()
                    .advance(&binding(), STAGES[next - 1], STAGES[next])
                    .unwrap();
            }
            _ => panic!("unexpected synthetic operation"),
        }
        panic!("initialization persistence boundary was not reached");
    }

    fn crash_at(
        directory: &std::path::Path,
        operation: &str,
        artifact: &str,
        phase: &str,
        next: usize,
    ) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "e2ee::initialization::tests::initialization_persistence_crash_child",
                "--nocapture",
            ])
            .env("GRAFT_TEST_INITIALIZATION_DIRECTORY", directory)
            .env("GRAFT_TEST_INITIALIZATION_OPERATION", operation)
            .env("GRAFT_TEST_INITIALIZATION_ARTIFACT", artifact)
            .env("GRAFT_TEST_INITIALIZATION_PHASE", phase)
            .env("GRAFT_TEST_INITIALIZATION_NEXT", next.to_string())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(73),
            "{operation}/{artifact}/{phase}"
        );
    }

    #[test]
    #[ignore = "run subprocess persistence matrix separately with --test-threads=1"]
    fn initialization_creation_process_exit_never_reconstructs_partial_state() {
        let mut boundaries = vec![("initialization.lock", "lock-synced")];
        for artifact in ["initialization.intent", "initialization"] {
            for phase in ["written", "file-synced", "installed", "directory-synced"] {
                boundaries.push((artifact, phase));
            }
        }
        for (artifact, phase) in boundaries {
            let directory = directory();
            fs::write(directory.path().join("test-config"), config().as_bytes()).unwrap();
            crash_at(directory.path(), "create", artifact, phase, 0);
            let readable =
                artifact == "initialization" && matches!(phase, "installed" | "directory-synced");
            let reopened = FileInitializationJournal::open(directory.path(), &binding());
            if readable {
                let store = reopened.unwrap();
                let record = store.load(&binding()).unwrap();
                assert_eq!(record.stage, InitializationStage::Prepared);
                assert_eq!(record.config.as_bytes(), config().as_bytes());
                assert!(record.intent == Some(intent()));
                let changed = InitializationIntent::new("other".into(), "b".repeat(64)).unwrap();
                assert_eq!(
                    store.bind_intent(&binding(), &changed),
                    Err(EncryptionError::StaleObservation)
                );
            } else {
                assert!(reopened.is_err(), "{artifact}/{phase}");
                assert!(!directory.path().join("initialization").exists());
            }
            // Repeating explicit create must not repair an interrupted allocation.
            assert!(
                FileInitializationJournal::create_with_intent(
                    directory.path(),
                    &binding(),
                    config(),
                    &intent()
                )
                .is_err()
            );
            assert_eq!(directory.path().join("initialization").exists(), readable);
        }
    }

    #[test]
    #[ignore = "run subprocess persistence matrix separately with --test-threads=1"]
    fn initialization_binding_and_every_stage_survive_process_exit_without_replacement() {
        for artifact in ["initialization.intent", "initialization"] {
            for phase in ["written", "file-synced", "installed", "directory-synced"] {
                let directory = directory();
                fs::write(directory.path().join("test-config"), config().as_bytes()).unwrap();
                drop(
                    FileInitializationJournal::create(directory.path(), &binding(), config())
                        .unwrap(),
                );
                crash_at(directory.path(), "bind", artifact, phase, 0);
                let store = FileInitializationJournal::open(directory.path(), &binding()).unwrap();
                let record = store.load(&binding()).unwrap();
                assert_eq!(record.config.as_bytes(), config().as_bytes());
                assert_eq!(record.stage, InitializationStage::Prepared);
                let bound = artifact == "initialization"
                    && matches!(phase, "installed" | "directory-synced");
                assert_eq!(record.intent.is_some(), bound);
                if directory.path().join("initialization.intent").exists() {
                    let changed = InitializationIntent::new("main".into(), "b".repeat(64)).unwrap();
                    assert_eq!(
                        store.bind_intent(&binding(), &changed),
                        Err(EncryptionError::StaleObservation)
                    );
                }
                store.bind_intent(&binding(), &intent()).unwrap();
                assert!(
                    FileInitializationJournal::open(directory.path(), &binding())
                        .unwrap()
                        .load(&binding())
                        .unwrap()
                        .intent
                        == Some(intent())
                );
            }
        }
        for next in 1..STAGES.len() {
            for phase in ["written", "file-synced", "installed", "directory-synced"] {
                let directory = directory();
                fs::write(directory.path().join("test-config"), config().as_bytes()).unwrap();
                let store = FileInitializationJournal::create_with_intent(
                    directory.path(),
                    &binding(),
                    config(),
                    &intent(),
                )
                .unwrap();
                for index in 1..next {
                    store
                        .advance(&binding(), STAGES[index - 1], STAGES[index])
                        .unwrap();
                }
                drop(store);
                crash_at(directory.path(), "advance", "initialization", phase, next);
                let store = FileInitializationJournal::open(directory.path(), &binding()).unwrap();
                let record = store.load(&binding()).unwrap();
                assert_eq!(record.config.as_bytes(), config().as_bytes());
                assert!(record.intent == Some(intent()));
                assert_eq!(
                    record.stage,
                    if matches!(phase, "installed" | "directory-synced") {
                        STAGES[next]
                    } else {
                        STAGES[next - 1]
                    }
                );
                store
                    .advance(&binding(), STAGES[next - 1], STAGES[next])
                    .unwrap();
                assert_eq!(
                    FileInitializationJournal::open(directory.path(), &binding())
                        .unwrap()
                        .load(&binding())
                        .unwrap()
                        .stage,
                    STAGES[next]
                );
            }
        }
    }

    #[test]
    fn initialization_intent_requires_canonical_ref_and_commit() {
        for branch in ["", "../main", "main.lock", "main\n", "a//b"] {
            assert!(InitializationIntent::new(branch.into(), "a".repeat(64)).is_err());
        }
        for commit in ["A".repeat(64), "a".repeat(63), "g".repeat(64)] {
            assert!(InitializationIntent::new("main".into(), commit).is_err());
        }
        assert!(InitializationIntent::new("分支/测试".into(), "a".repeat(64)).is_ok());
        assert!(InitializationIntent::new("a".repeat(4096), "a".repeat(64)).is_err());
    }

    #[test]
    fn initialization_creation_io_failures_never_repair_partial_state() {
        let mut boundaries = vec![("initialization.lock", "file-sync")];
        for artifact in ["initialization.intent", "initialization"] {
            for phase in [
                "write",
                "partial-write",
                "file-sync",
                "install",
                "directory-sync",
            ] {
                boundaries.push((artifact, phase));
            }
        }
        for (artifact, phase) in boundaries {
            let directory = directory();
            let result = file::with_persistence_failure(phase, artifact, directory.path(), || {
                FileInitializationJournal::create_with_intent(
                    directory.path(),
                    &binding(),
                    config(),
                    &intent(),
                )
            });
            assert!(
                matches!(
                    result,
                    Err(EncryptionError::CheckpointUnavailable(
                        std::io::ErrorKind::Other
                    ))
                ),
                "{artifact}/{phase}"
            );
            let readable = artifact == "initialization" && phase == "directory-sync";
            match FileInitializationJournal::open(directory.path(), &binding()) {
                Ok(store) => {
                    assert!(readable);
                    let record = store.load(&binding()).unwrap();
                    assert_eq!(record.stage, InitializationStage::Prepared);
                    assert_eq!(record.config.as_bytes(), config().as_bytes());
                    assert!(record.intent == Some(intent()));
                    store.bind_intent(&binding(), &intent()).unwrap();
                }
                Err(_) => assert!(!readable),
            }
            assert!(
                FileInitializationJournal::create_with_intent(
                    directory.path(),
                    &binding(),
                    config(),
                    &intent()
                )
                .is_err()
            );
            assert_eq!(directory.path().join("initialization").exists(), readable);
        }
    }

    #[test]
    fn initialization_binding_io_failures_preserve_exact_intent_and_report_failed_confirmation() {
        for artifact in ["initialization.intent", "initialization"] {
            for phase in [
                "write",
                "partial-write",
                "file-sync",
                "install",
                "directory-sync",
            ] {
                let directory = directory();
                let store =
                    FileInitializationJournal::create(directory.path(), &binding(), config())
                        .unwrap();
                let result =
                    file::with_persistence_failure(phase, artifact, directory.path(), || {
                        store.bind_intent(&binding(), &intent())
                    });
                assert_eq!(
                    result,
                    Err(EncryptionError::CheckpointUnavailable(
                        std::io::ErrorKind::Other
                    )),
                    "{artifact}/{phase}"
                );
                let reopened =
                    FileInitializationJournal::open(directory.path(), &binding()).unwrap();
                let record = reopened.load(&binding()).unwrap();
                assert_eq!(record.config.as_bytes(), config().as_bytes());
                assert_eq!(record.stage, InitializationStage::Prepared);
                assert_eq!(
                    record.intent.is_some(),
                    artifact == "initialization" && phase == "directory-sync"
                );
                if directory.path().join("initialization.intent").exists() {
                    let changed =
                        InitializationIntent::new("other".into(), "b".repeat(64)).unwrap();
                    assert_eq!(
                        reopened.bind_intent(&binding(), &changed),
                        Err(EncryptionError::StaleObservation)
                    );
                }
                reopened.bind_intent(&binding(), &intent()).unwrap();
                // A readable candidate is not an acknowledgement of a later failed sync.
                for (artifact, phase) in [
                    ("initialization.intent", "file-sync"),
                    ("initialization", "file-sync"),
                    ("initialization", "directory-sync"),
                ] {
                    let result =
                        file::with_persistence_failure(phase, artifact, directory.path(), || {
                            reopened.bind_intent(&binding(), &intent())
                        });
                    assert_eq!(
                        result,
                        Err(EncryptionError::CheckpointUnavailable(
                            std::io::ErrorKind::Other
                        ))
                    );
                    assert!(reopened.load(&binding()).unwrap().intent == Some(intent()));
                }
                reopened.bind_intent(&binding(), &intent()).unwrap();
            }
        }
    }

    #[test]
    fn initialization_every_stage_io_failure_retains_candidate_and_requires_durable_retry() {
        for next in 1..STAGES.len() {
            for phase in [
                "write",
                "partial-write",
                "file-sync",
                "install",
                "directory-sync",
            ] {
                let directory = directory();
                let store = FileInitializationJournal::create_with_intent(
                    directory.path(),
                    &binding(),
                    config(),
                    &intent(),
                )
                .unwrap();
                for index in 1..next {
                    store
                        .advance(&binding(), STAGES[index - 1], STAGES[index])
                        .unwrap();
                }
                let result = file::with_persistence_failure(
                    phase,
                    "initialization",
                    directory.path(),
                    || store.advance(&binding(), STAGES[next - 1], STAGES[next]),
                );
                assert_eq!(
                    result,
                    Err(EncryptionError::CheckpointUnavailable(
                        std::io::ErrorKind::Other
                    ))
                );
                let reopened =
                    FileInitializationJournal::open(directory.path(), &binding()).unwrap();
                let record = reopened.load(&binding()).unwrap();
                assert_eq!(record.config.as_bytes(), config().as_bytes());
                assert!(record.intent == Some(intent()));
                assert_eq!(
                    record.stage,
                    if phase == "directory-sync" {
                        STAGES[next]
                    } else {
                        STAGES[next - 1]
                    }
                );
                reopened
                    .advance(&binding(), STAGES[next - 1], STAGES[next])
                    .unwrap();
                // Idempotent advancement still must report file/directory sync failure.
                for phase in ["file-sync", "directory-sync"] {
                    let result = file::with_persistence_failure(
                        phase,
                        "initialization",
                        directory.path(),
                        || reopened.advance(&binding(), STAGES[next - 1], STAGES[next]),
                    );
                    assert_eq!(
                        result,
                        Err(EncryptionError::CheckpointUnavailable(
                            std::io::ErrorKind::Other
                        ))
                    );
                    assert_eq!(reopened.load(&binding()).unwrap().stage, STAGES[next]);
                }
                reopened
                    .advance(&binding(), STAGES[next - 1], STAGES[next])
                    .unwrap();
            }
        }
    }

    #[test]
    fn initialization_intent_survives_reopen_and_cannot_be_replaced() {
        let directory = directory();
        let store =
            FileInitializationJournal::create(directory.path(), &binding(), config()).unwrap();
        assert_eq!(
            store.advance(
                &binding(),
                InitializationStage::Prepared,
                InitializationStage::ObjectsPublished
            ),
            Err(EncryptionError::StaleObservation)
        );
        store.bind_intent(&binding(), &intent()).unwrap();
        let resumed = FileInitializationJournal::open(directory.path(), &binding()).unwrap();
        resumed.bind_intent(&binding(), &intent()).unwrap();
        assert!(resumed.load(&binding()).unwrap().intent == Some(intent()));
        for changed in [
            InitializationIntent::new("other".into(), "a".repeat(64)).unwrap(),
            InitializationIntent::new("main".into(), "b".repeat(64)).unwrap(),
        ] {
            assert_eq!(
                resumed.bind_intent(&binding(), &changed),
                Err(EncryptionError::StaleObservation)
            );
        }
        assert_eq!(
            intent().require_source("main", &"b".repeat(64)),
            Err(EncryptionError::StaleObservation)
        );
        assert!(intent().require_source("main", &"a".repeat(64)).is_ok());
        fs::remove_file(directory.path().join("initialization.intent")).unwrap();
        assert!(resumed.load(&binding()).is_err());
        assert!(resumed.bind_intent(&binding(), &intent()).is_err());
    }

    #[test]
    fn initialization_intent_recovers_only_the_exact_interrupted_binding() {
        let directory = directory();
        let store =
            FileInitializationJournal::create(directory.path(), &binding(), config()).unwrap();
        let path = directory.path().join("initialization");
        let prepared = fs::read(&path).unwrap();
        store.bind_intent(&binding(), &intent()).unwrap();
        // Simulate a crash after the sidecar became durable but before the
        // main record published its digest. The orphan cannot select a source.
        fs::write(&path, prepared).unwrap();
        let resumed = FileInitializationJournal::open(directory.path(), &binding()).unwrap();
        assert!(resumed.load(&binding()).unwrap().intent.is_none());
        let changed = InitializationIntent::new("main".into(), "b".repeat(64)).unwrap();
        assert_eq!(
            resumed.bind_intent(&binding(), &changed),
            Err(EncryptionError::StaleObservation)
        );
        resumed.bind_intent(&binding(), &intent()).unwrap();
        assert!(resumed.load(&binding()).unwrap().intent == Some(intent()));
    }

    #[test]
    fn initialization_legacy_records_require_explicit_source_binding() {
        let directory = directory();
        let store =
            FileInitializationJournal::create(directory.path(), &binding(), config()).unwrap();
        let path = directory.path().join("initialization");
        let mut legacy = fs::read(&path).unwrap()[..279].to_vec();
        legacy[..8].copy_from_slice(b"GRAFTIJ1");
        legacy.extend_from_slice(&Sha256::digest(&legacy));
        fs::write(&path, &legacy).unwrap();
        assert!(store.load(&binding()).unwrap().intent.is_none());
        store
            .advance(
                &binding(),
                InitializationStage::Prepared,
                InitializationStage::ConfigPublished,
            )
            .unwrap();
        assert_eq!(
            store.advance(
                &binding(),
                InitializationStage::ConfigPublished,
                InitializationStage::ObjectsPublished
            ),
            Err(EncryptionError::StaleObservation)
        );
        store.bind_intent(&binding(), &intent()).unwrap();
        store
            .advance(
                &binding(),
                InitializationStage::ConfigPublished,
                InitializationStage::ObjectsPublished,
            )
            .unwrap();
        assert_eq!(&fs::read(&path).unwrap()[..8], b"GRAFTIJ2");
        legacy[40] = InitializationStage::ObjectsPublished as u8;
        let checksum = Sha256::digest(&legacy[..279]);
        legacy[279..].copy_from_slice(&checksum);
        fs::write(&path, legacy).unwrap();
        assert!(store.load(&binding()).unwrap().intent.is_none());
        assert_eq!(
            store.bind_intent(&binding(), &intent()),
            Err(EncryptionError::StaleObservation)
        );
    }

    #[test]
    fn initialization_intent_rejects_corrupt_and_unsafe_sidecars() {
        let directory = directory();
        let store =
            FileInitializationJournal::create(directory.path(), &binding(), config()).unwrap();
        store.bind_intent(&binding(), &intent()).unwrap();
        let path = directory.path().join("initialization.intent");
        let original = fs::read(&path).unwrap();
        for index in [0, 8, 40, 75, original.len() - 1] {
            let mut bytes = original.clone();
            bytes[index] ^= 1;
            fs::write(&path, bytes).unwrap();
            assert!(store.load(&binding()).is_err());
        }
        fs::write(&path, &original).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.load(&binding()).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::hard_link(&path, directory.path().join("alias")).unwrap();
        assert!(store.load(&binding()).is_err());
    }

    #[test]
    fn initialization_stages_reopen_with_exact_candidate_and_no_secret() {
        let directory = directory();
        let store =
            FileInitializationJournal::create(directory.path(), &binding(), config()).unwrap();
        let candidate = config().as_bytes().to_vec();
        store.bind_intent(&binding(), &intent()).unwrap();
        let stages = [
            InitializationStage::Prepared,
            InitializationStage::ConfigPublished,
            InitializationStage::ObjectsPublished,
            InitializationStage::RefPublished,
            InitializationStage::Connected,
        ];
        for (index, stage) in stages.into_iter().enumerate() {
            if index > 0 {
                store.advance(&binding(), stages[index - 1], stage).unwrap();
            }
            let resumed = FileInitializationJournal::open(directory.path(), &binding()).unwrap();
            let record = resumed.load(&binding()).unwrap();
            assert_eq!(record.stage, stage);
            assert_eq!(record.config.as_bytes(), candidate);
            assert!(record.intent.as_ref() == Some(&intent()));
            record.config.authenticate(&fixture_root()).unwrap();
            let bytes = fs::read(directory.path().join("initialization")).unwrap();
            assert_eq!(bytes.len(), 344);
            for bytes in [
                bytes,
                fs::read(directory.path().join("initialization.intent")).unwrap(),
            ] {
                let secret: [u8; 32] = std::array::from_fn(|index| index as u8);
                assert!(!bytes.windows(32).any(|value| value == secret));
                assert!(
                    !bytes
                        .windows(b"initialization synthetic password".len())
                        .any(|value| value == b"initialization synthetic password")
                );
            }
        }
        let recovered = FileInitializationJournal::open(directory.path(), &binding())
            .unwrap()
            .load(&binding())
            .unwrap()
            .config
            .unlock(
                b"initialization synthetic password",
                Some(binding().repository()),
            )
            .unwrap();
        config().authenticate(&recovered).unwrap();
    }

    #[test]
    fn initialization_stage_cas_rejects_skip_regression_and_stale_handles() {
        use InitializationStage::*;
        let directory = directory();
        let first =
            FileInitializationJournal::create(directory.path(), &binding(), config()).unwrap();
        let second = FileInitializationJournal::open(directory.path(), &binding()).unwrap();
        first.bind_intent(&binding(), &intent()).unwrap();
        assert_eq!(
            first.advance(&binding(), Prepared, RefPublished),
            Err(EncryptionError::StaleObservation)
        );
        first
            .advance(&binding(), Prepared, ConfigPublished)
            .unwrap();
        second
            .advance(&binding(), Prepared, ConfigPublished)
            .unwrap();
        first
            .advance(&binding(), ConfigPublished, ObjectsPublished)
            .unwrap();
        assert_eq!(
            second.advance(&binding(), Prepared, ConfigPublished),
            Err(EncryptionError::StaleObservation)
        );
        assert_eq!(
            second.advance(&binding(), ObjectsPublished, Prepared),
            Err(EncryptionError::StaleObservation)
        );
        let before = fs::read(directory.path().join("initialization")).unwrap();
        assert!(FileInitializationJournal::create(directory.path(), &binding(), config()).is_err());
        assert_eq!(
            fs::read(directory.path().join("initialization")).unwrap(),
            before
        );
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.path().join("initialization.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        assert_eq!(
            first.advance(&binding(), ObjectsPublished, RefPublished),
            Err(EncryptionError::SessionBusy)
        );
        drop(lock);
        second
            .advance(&binding(), ObjectsPublished, RefPublished)
            .unwrap();
    }

    #[test]
    fn initialization_rejects_missing_corrupt_and_foreign_records() {
        let directory = directory();
        assert!(FileInitializationJournal::open(directory.path(), &binding()).is_err());
        let store =
            FileInitializationJournal::create(directory.path(), &binding(), config()).unwrap();
        let other = SessionBinding::new(
            "initialization.test".into(),
            "other-account".into(),
            binding().repository(),
        )
        .unwrap();
        assert!(matches!(
            FileInitializationJournal::open(directory.path(), &other),
            Err(EncryptionError::IdentityChanged)
        ));
        assert!(matches!(
            store.load(&other),
            Err(EncryptionError::IdentityChanged)
        ));
        let path = directory.path().join("initialization");
        let original = fs::read(&path).unwrap();
        for index in [0, 40, 126, 278, 310] {
            let mut bytes = original.clone();
            bytes[index] = 255;
            fs::write(&path, bytes).unwrap();
            assert!(FileInitializationJournal::open(directory.path(), &binding()).is_err());
        }
        let mut bytes = original.clone();
        bytes[40] = InitializationStage::Connected as u8;
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            FileInitializationJournal::open(directory.path(), &binding()),
            Err(EncryptionError::InvalidEncoding)
        ));
        fs::write(&path, &original[..original.len() - 1]).unwrap();
        assert!(FileInitializationJournal::open(directory.path(), &binding()).is_err());
        fs::remove_file(&path).unwrap();
        assert!(FileInitializationJournal::open(directory.path(), &binding()).is_err());
        assert!(FileInitializationJournal::create(directory.path(), &binding(), config()).is_err());
    }

    #[test]
    fn initialization_rejects_replaced_candidate_even_with_matching_digest() {
        let directory = directory();
        let store =
            FileInitializationJournal::create(directory.path(), &binding(), config()).unwrap();
        let alternate =
            EncryptedConfig::create(&fixture_root(), b"other initialization password").unwrap();
        let path = directory.path().join("initialization");
        let mut bytes = fs::read(&path).unwrap();
        bytes[41..247].copy_from_slice(alternate.as_bytes());
        bytes[247..279].copy_from_slice(&Sha256::digest(alternate.as_bytes()));
        let checksum = Sha256::digest(&bytes[..312]);
        bytes[312..].copy_from_slice(&checksum);
        fs::write(path, bytes).unwrap();
        assert!(matches!(
            store.load(&binding()),
            Err(EncryptionError::StaleObservation)
        ));
        assert_eq!(
            store.advance(
                &binding(),
                InitializationStage::Prepared,
                InitializationStage::ConfigPublished
            ),
            Err(EncryptionError::StaleObservation)
        );
        let foreign = RootKey::generate(crate::e2ee::RepositoryId::generate().unwrap()).unwrap();
        let config = EncryptedConfig::create(&foreign, b"foreign initialization password").unwrap();
        assert!(matches!(
            FileInitializationJournal::create(directory.path(), &binding(), &config),
            Err(EncryptionError::IdentityChanged)
        ));
    }

    #[test]
    fn initialization_rejects_unsafe_paths_permissions_and_links() {
        let directory = directory();
        let store =
            FileInitializationJournal::create(directory.path(), &binding(), config()).unwrap();
        let path = directory.path().join("initialization");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.load(&binding()).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let link = directory.path().join("hard-link");
        fs::hard_link(&path, &link).unwrap();
        assert!(store.load(&binding()).is_err());
        fs::remove_file(&link).unwrap();
        let saved = directory.path().join("saved");
        fs::rename(&path, &saved).unwrap();
        symlink(&saved, &path).unwrap();
        assert!(store.load(&binding()).is_err());
        fs::remove_file(&path).unwrap();
        fs::rename(&saved, &path).unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(store.load(&binding()).is_err());
        assert!(FileInitializationJournal::open(directory.path(), &binding()).is_err());
    }
}
