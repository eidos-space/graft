//! Durable exact-intent migration progress and exclusive native coordination.
use super::{EncryptionError, FileRefMutationJournal, Result, SessionBinding};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

const ALLOCATION: &str = "migration-progress.allocated";
const RECORD: &str = "migration-progress";
const MAX_INTENT: usize = 16 * 1024 * 1024;
const MAX_STEPS: u32 = 100_008;

/// Names and object identities are private host metadata, never diagnostics.
#[derive(Clone, PartialEq, Eq)]
pub struct MigrationProgressRecord {
    intent: Vec<u8>,
    completed: u32,
    total: u32,
}

impl MigrationProgressRecord {
    pub fn intent(&self) -> &[u8] {
        &self.intent
    }
    pub fn completed_steps(&self) -> u32 {
        self.completed
    }
    pub fn total_steps(&self) -> u32 {
        self.total
    }
}

/// Owner-only native state, with a distinct durable allocation record. Losing
/// either record never authorizes creating a replacement operation.
pub struct FileMigrationProgressJournal {
    store: Arc<FileRefMutationJournal>,
}

/// Holds the native cross-process lock for the complete coordinator operation.
/// This fences other coordinators, not arbitrary application/offline writers.
/// Inheritance through `fork` does not transfer the capability to the child.
pub struct MigrationProgressLease {
    creator_pid: u32,
    store: Arc<FileRefMutationJournal>,
    directory: File,
    _lock: File,
    path: PathBuf,
    binding: [u8; 32],
    owner: u32,
    allocation: [u8; 76],
}

impl Drop for MigrationProgressLease {
    fn drop(&mut self) {
        if self.creator_pid == std::process::id() {
            let _ = self._lock.unlock();
        }
    }
}

fn io(error: std::io::Error) -> EncryptionError {
    EncryptionError::CheckpointUnavailable(error.kind())
}

impl FileMigrationProgressJournal {
    pub fn create(
        path: impl AsRef<Path>,
        binding: &SessionBinding,
        intent: &[u8],
        total_steps: u32,
    ) -> Result<Self> {
        if intent.is_empty() || intent.len() > MAX_INTENT || !(1..=MAX_STEPS).contains(&total_steps)
        {
            return Err(EncryptionError::TooLarge);
        }
        let path = path.as_ref();
        let store = Arc::new(FileRefMutationJournal::create(path, binding)?);
        {
            let (directory, lock, path, digest, owner) = store.private_transaction(binding)?;
            let path = path.to_owned();
            let mut allocation = [0; 76];
            allocation[..8].copy_from_slice(b"GRAFTMPA");
            allocation[8..40].copy_from_slice(&digest);
            allocation[40..72].copy_from_slice(&Sha256::digest(intent));
            allocation[72..].copy_from_slice(&total_steps.to_be_bytes());
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path.join(ALLOCATION))
                .map_err(io)?;
            store.validate_private_file(&file, ALLOCATION)?;
            file.write_all(&allocation).map_err(io)?;
            file.sync_all().map_err(io)?;
            directory.sync_all().map_err(io)?;
            checkpoint("allocation-durable", &path);
            let lease = MigrationProgressLease {
                creator_pid: std::process::id(),
                store,
                directory,
                _lock: lock,
                path,
                binding: digest,
                owner,
                allocation,
            };
            lease.write(
                &MigrationProgressRecord {
                    intent: intent.to_vec(),
                    completed: 0,
                    total: total_steps,
                },
                true,
            )?;
            // Reopen below only after releasing the coordinator lease.
        }
        Self::open(path, binding)
    }

    pub fn open(path: impl AsRef<Path>, binding: &SessionBinding) -> Result<Self> {
        let journal = Self {
            store: Arc::new(FileRefMutationJournal::open(path, binding)?),
        };
        let store = journal.store.clone();
        let lease = journal.acquire(binding)?;
        drop(lease);
        Ok(Self { store })
    }

    pub fn acquire(self, binding: &SessionBinding) -> Result<MigrationProgressLease> {
        let (directory, lock, path, digest, owner) = self.store.private_transaction(binding)?;
        let path = path.to_owned();
        let mut allocation = [0; 76];
        let mut file = File::open(path.join(ALLOCATION)).map_err(io)?;
        if self.store.validate_private_file(&file, ALLOCATION)?.len() != 76 {
            return Err(EncryptionError::InvalidEncoding);
        }
        file.read_exact(&mut allocation).map_err(io)?;
        if &allocation[..8] != b"GRAFTMPA" || allocation[8..40] != digest {
            return Err(EncryptionError::IdentityChanged);
        }
        file.sync_all().map_err(io)?;
        let lease = MigrationProgressLease {
            creator_pid: std::process::id(),
            store: self.store,
            directory,
            _lock: lock,
            path,
            binding: digest,
            owner,
            allocation,
        };
        lease.snapshot()?;
        Ok(lease)
    }
}

impl MigrationProgressLease {
    pub fn snapshot(&self) -> Result<MigrationProgressRecord> {
        if self.creator_pid != std::process::id() {
            return Err(EncryptionError::StaleObservation);
        }
        let allocation = File::open(self.path.join(ALLOCATION)).map_err(io)?;
        if self
            .store
            .validate_private_file(&allocation, ALLOCATION)?
            .len()
            != 76
        {
            return Err(EncryptionError::InvalidEncoding);
        }
        let mut current_allocation = [0; 76];
        (&allocation)
            .read_exact(&mut current_allocation)
            .map_err(io)?;
        if current_allocation != self.allocation {
            return Err(EncryptionError::StaleObservation);
        }
        let file = File::open(self.path.join(RECORD)).map_err(io)?;
        let size = self.store.validate_private_file(&file, RECORD)?.len();
        if !(81..=(MAX_INTENT + 80) as u64).contains(&size) {
            return Err(EncryptionError::InvalidEncoding);
        }
        let mut bytes = Vec::new();
        (&file)
            .take((MAX_INTENT + 81) as u64)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        if bytes.len() as u64 != size || &bytes[..8] != b"GRAFTMP1" || bytes[8..40] != self.binding
        {
            return Err(EncryptionError::InvalidEncoding);
        }
        let end = bytes.len() - 32;
        let completed = u32::from_be_bytes(
            bytes[40..44]
                .try_into()
                .map_err(|_| EncryptionError::InvalidEncoding)?,
        );
        let total = u32::from_be_bytes(
            bytes[44..48]
                .try_into()
                .map_err(|_| EncryptionError::InvalidEncoding)?,
        );
        let intent = &bytes[48..end];
        if completed > total
            || !(1..=MAX_STEPS).contains(&total)
            || self.allocation[72..] != total.to_be_bytes()
            || self.allocation[40..72] != Sha256::digest(intent)[..]
            || bytes[end..] != Sha256::digest(&bytes[..end])[..]
        {
            return Err(EncryptionError::InvalidEncoding);
        }
        allocation.sync_all().map_err(io)?;
        file.sync_all().map_err(io)?;
        self.directory.sync_all().map_err(io)?;
        Ok(MigrationProgressRecord {
            intent: intent.to_vec(),
            completed,
            total,
        })
    }

    /// Record exactly one independently confirmed effect. Repeating the last
    /// transition is idempotent; skipping ahead or rewinding is forbidden.
    pub fn confirm_step(&self, expected_completed: u32) -> Result<MigrationProgressRecord> {
        let mut record = self.snapshot()?;
        let next = expected_completed
            .checked_add(1)
            .ok_or(EncryptionError::TooLarge)?;
        if next > record.total
            || (record.completed != expected_completed && record.completed != next)
        {
            return Err(EncryptionError::StaleObservation);
        }
        if record.completed == expected_completed {
            record.completed = next;
            self.write(&record, false)?;
        }
        self.snapshot()
    }

    fn write(&self, record: &MigrationProgressRecord, initial: bool) -> Result<()> {
        if self.creator_pid != std::process::id() {
            return Err(EncryptionError::StaleObservation);
        }
        let mut bytes = b"GRAFTMP1".to_vec();
        bytes.extend_from_slice(&self.binding);
        bytes.extend_from_slice(&record.completed.to_be_bytes());
        bytes.extend_from_slice(&record.total.to_be_bytes());
        bytes.extend_from_slice(&record.intent);
        bytes.extend_from_slice(&Sha256::digest(&bytes));
        let mut staging = tempfile::NamedTempFile::new_in(&self.path).map_err(io)?;
        let metadata = staging.as_file().metadata().map_err(io)?;
        if metadata.mode() & 0o077 != 0 || metadata.uid() != self.owner || metadata.nlink() != 1 {
            return Err(EncryptionError::CheckpointUnavailable(
                std::io::ErrorKind::PermissionDenied,
            ));
        }
        staging.write_all(&bytes).map_err(io)?;
        staging.as_file().sync_all().map_err(io)?;
        checkpoint("temporary-synced", &self.path);
        // Revalidate the pinned directory using the still-present allocation.
        let allocation = File::open(self.path.join(ALLOCATION)).map_err(io)?;
        self.store.validate_private_file(&allocation, ALLOCATION)?;
        if initial {
            staging
                .persist_noclobber(self.path.join(RECORD))
                .map_err(|error| io(error.error))?;
        } else {
            staging
                .persist(self.path.join(RECORD))
                .map_err(|error| io(error.error))?;
        }
        checkpoint("record-installed", &self.path);
        self.directory.sync_all().map_err(io)?;
        checkpoint("record-durable", &self.path);
        Ok(())
    }
}

fn checkpoint(_phase: &str, _path: &Path) {
    #[cfg(test)]
    if std::env::var("GRAFT_MIGRATION_PROGRESS_CRASH").as_deref() == Ok(_phase)
        && std::env::var_os("GRAFT_MIGRATION_PROGRESS_PATH").as_deref() == Some(_path.as_os_str())
    {
        std::process::exit(67);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::e2ee::RepositoryId;
    use std::{fs, os::unix::fs::PermissionsExt, process::Command};

    fn binding() -> SessionBinding {
        SessionBinding::new(
            "migration.test".into(),
            "account".into(),
            RepositoryId::from_bytes([0, 1, 2, 3, 4, 5, 0x46, 7, 0x88, 9, 10, 11, 12, 13, 14, 15])
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn migration_progress_is_exclusive_exact_and_fails_closed_on_lost_state() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let identity = binding();
        let store = FileMigrationProgressJournal::create(
            directory.path(),
            &identity,
            b"fixed private intent",
            4,
        )
        .unwrap();
        let competing = FileMigrationProgressJournal::open(directory.path(), &identity).unwrap();
        let mut lease = store.acquire(&identity).unwrap();
        assert!(competing.acquire(&identity).is_err());
        lease.creator_pid = std::process::id().wrapping_add(1);
        assert!(matches!(
            lease.snapshot(),
            Err(EncryptionError::StaleObservation)
        ));
        assert!(matches!(
            lease.confirm_step(0),
            Err(EncryptionError::StaleObservation)
        ));
        lease.creator_pid = std::process::id();
        assert!(FileMigrationProgressJournal::open(directory.path(), &identity).is_err());
        assert!(lease.confirm_step(1).is_err());
        assert_eq!(lease.confirm_step(0).unwrap().completed_steps(), 1);
        assert_eq!(lease.confirm_step(0).unwrap().completed_steps(), 1);
        assert_eq!(lease.confirm_step(1).unwrap().completed_steps(), 2);
        assert!(lease.confirm_step(0).is_err());
        drop(lease);
        let lease = FileMigrationProgressJournal::open(directory.path(), &identity)
            .unwrap()
            .acquire(&identity)
            .unwrap();
        assert_eq!(lease.snapshot().unwrap().intent(), b"fixed private intent");
        assert_eq!(lease.snapshot().unwrap().completed_steps(), 2);
        let record = fs::read(directory.path().join(RECORD)).unwrap();
        fs::remove_file(directory.path().join(RECORD)).unwrap();
        assert!(lease.snapshot().is_err());
        drop(lease);
        assert!(FileMigrationProgressJournal::open(directory.path(), &identity).is_err());
        assert!(
            FileMigrationProgressJournal::create(directory.path(), &identity, b"replacement", 4)
                .is_err()
        );
        fs::write(directory.path().join(RECORD), record).unwrap();
        fs::set_permissions(
            directory.path().join(RECORD),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let foreign = SessionBinding::new(
            "migration.test".into(),
            "other-account".into(),
            identity.repository(),
        )
        .unwrap();
        assert!(FileMigrationProgressJournal::open(directory.path(), &foreign).is_err());
        fs::remove_file(directory.path().join(ALLOCATION)).unwrap();
        assert!(FileMigrationProgressJournal::open(directory.path(), &identity).is_err());
    }

    #[test]
    #[ignore = "subprocess fixture"]
    fn migration_progress_crash_child() {
        let path = std::env::var_os("GRAFT_MIGRATION_PROGRESS_PATH").unwrap();
        let identity = binding();
        let lease = FileMigrationProgressJournal::open(Path::new(&path), &identity)
            .unwrap()
            .acquire(&identity)
            .unwrap();
        lease.confirm_step(0).unwrap();
        panic!("expected configured process exit");
    }

    #[test]
    fn migration_progress_recovers_exact_step_after_process_death() {
        for phase in ["temporary-synced", "record-installed", "record-durable"] {
            let directory = tempfile::tempdir().unwrap();
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let identity = binding();
            FileMigrationProgressJournal::create(
                directory.path(),
                &identity,
                b"process-private-intent",
                2,
            )
            .unwrap();
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "e2ee::migration_progress::tests::migration_progress_crash_child",
                    "--ignored",
                ])
                .env("GRAFT_MIGRATION_PROGRESS_PATH", directory.path())
                .env("GRAFT_MIGRATION_PROGRESS_CRASH", phase)
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(67));
            let lease = FileMigrationProgressJournal::open(directory.path(), &identity)
                .unwrap()
                .acquire(&identity)
                .unwrap();
            let snapshot = lease.snapshot().unwrap();
            assert_eq!(snapshot.intent(), b"process-private-intent");
            assert!(snapshot.completed_steps() <= 1);
            assert_eq!(lease.confirm_step(0).unwrap().completed_steps(), 1);
        }
    }
}
