use crate::e2ee::native_fs::Metadata;
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;

use super::{RefMutationRecord, Result, SessionBinding};

/// Blocking host-private metadata provider. Bind an empty slot; identical
/// rebinding reconfirms durability. Replace only after exact retirement.
/// Lost/corrupt records must never be recreated.
pub trait RefMutationJournal: Send + Sync {
    fn load(&self, binding: &SessionBinding) -> Result<Option<RefMutationRecord>>;
    fn bind(&self, binding: &SessionBinding, candidate: &RefMutationRecord) -> Result<()>;
    /// Retire only after confirmed publication or conclusive conflict. Must
    /// compare the exact retained candidate; never clear a newer transaction.
    fn retire(&self, _binding: &SessionBinding, _candidate: &RefMutationRecord) -> Result<()> {
        Err(super::EncryptionError::UnsupportedProfile)
    }
}

/// Compiled trusted-host metadata provider, never ordinary plugin RPC.
pub trait RefMutationJournalProvider: Send + Sync {
    fn journal(
        &self,
        binding: &SessionBinding,
        opaque_key: &str,
    ) -> Result<std::sync::Arc<dyn RefMutationJournal>>;
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

    const RECORD: &str = "ref-mutation";
    const LOCK: &str = "ref-mutation.lock";
    const MAX_SIZE: usize = 40 * 1024;
    const ALLOCATIONS: &str = "ref-allocations";
    const MAX_ALLOCATIONS: usize = 262_144;

    /// Immutable private migration source bytes, bound to the chosen target identity.
    /// The trusted host owns encoding, source revalidation and directory isolation.
    pub struct FileMigrationJournal<const TARGET: bool, const ENROLLMENT: bool = false> {
        store: FileRefMutationJournal,
    }

    pub type FileMigrationSourceJournal = FileMigrationJournal<false>;
    /// Immutable target mapping/HEAD intent in a distinct private namespace.
    pub type FileMigrationTargetJournal = FileMigrationJournal<true>;
    /// Exact generation-1 config and target intent; never a publication receipt.
    pub type FileMigrationEnrollmentJournal = FileMigrationJournal<false, true>;

    impl<const TARGET: bool, const ENROLLMENT: bool> FileMigrationJournal<TARGET, ENROLLMENT> {
        const NAME: &'static str = if ENROLLMENT {
            "migration-enrollment"
        } else if TARGET {
            "migration-target"
        } else {
            "migration-source"
        };
        const ALLOCATION: &'static str = if ENROLLMENT {
            "migration-enrollment.allocated"
        } else if TARGET {
            "migration-target.allocated"
        } else {
            "migration-source.allocated"
        };
        const MAGIC: &'static [u8; 8] = if ENROLLMENT {
            b"GRAFTME1"
        } else if TARGET {
            b"GRAFTMT1"
        } else {
            b"GRAFTMS1"
        };
        const ALLOCATION_MAGIC: &'static [u8; 8] = if ENROLLMENT {
            b"GRAFTEA1"
        } else if TARGET {
            b"GRAFTTA1"
        } else {
            b"GRAFTMA1"
        };
        const MAX_BYTES: usize = 16 * 1024 * 1024;

        pub fn create(
            directory: impl AsRef<Path>,
            binding: &SessionBinding,
            bytes: &[u8],
        ) -> Result<Self> {
            if bytes.is_empty() || bytes.len() > Self::MAX_BYTES {
                return Err(EncryptionError::TooLarge);
            }
            let journal = Self {
                store: FileRefMutationJournal::create(directory, binding)?,
            };
            #[cfg(test)]
            migration_crash("namespace-created", &journal.store.directory);
            let (directory, _lock) = journal.store.transaction(binding)?;
            let mut allocation = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(journal.store.directory.join(Self::ALLOCATION))
                .map_err(io)?;
            #[cfg(test)]
            migration_crash("allocation-created", &journal.store.directory);
            #[cfg(test)]
            migration_io("allocation-write", &journal.store.directory).map_err(io)?;
            allocation.write_all(Self::ALLOCATION_MAGIC).map_err(io)?;
            #[cfg(test)]
            migration_crash("allocation-partial", &journal.store.directory);
            #[cfg(test)]
            migration_io("allocation-partial-write", &journal.store.directory).map_err(io)?;
            allocation.write_all(&journal.store.binding).map_err(io)?;
            #[cfg(test)]
            migration_crash("allocation-written", &journal.store.directory);
            journal.store.validate_file(&allocation, Self::ALLOCATION)?;
            #[cfg(test)]
            migration_io("allocation-sync", &journal.store.directory).map_err(io)?;
            allocation.sync_all().map_err(io)?;
            #[cfg(test)]
            migration_crash("allocation-synced", &journal.store.directory);
            #[cfg(test)]
            migration_io("allocation-directory-sync", &journal.store.directory).map_err(io)?;
            fs::sync_directory(&directory).map_err(io)?;
            #[cfg(test)]
            migration_crash("allocation-durable", &journal.store.directory);
            let mut encoded = Vec::with_capacity(bytes.len() + 80);
            encoded.extend_from_slice(Self::MAGIC);
            encoded.extend_from_slice(&journal.store.binding);
            encoded.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            encoded.extend_from_slice(bytes);
            encoded.extend_from_slice(&Sha256::digest(&encoded));
            let mut staging = fs::private_named_tempfile(&journal.store.directory).map_err(io)?;
            let metadata = fs::file_metadata(staging.as_file()).map_err(io)?;
            if metadata.mode() & 0o077 != 0 || metadata.uid() != journal.store.identity.2 {
                return Err(permission());
            }
            #[cfg(test)]
            migration_crash("temporary-created", &journal.store.directory);
            #[cfg(test)]
            migration_io("temporary-write", &journal.store.directory).map_err(io)?;
            #[cfg(test)]
            if let Err(error) = migration_io("temporary-partial-write", &journal.store.directory) {
                staging
                    .write_all(&encoded[..encoded.len() / 2])
                    .map_err(io)?;
                return Err(io(error));
            }
            staging.write_all(&encoded).map_err(io)?;
            #[cfg(test)]
            migration_crash("temporary-written", &journal.store.directory);
            #[cfg(test)]
            migration_io("temporary-sync", &journal.store.directory).map_err(io)?;
            staging.as_file().sync_all().map_err(io)?;
            #[cfg(test)]
            migration_crash("temporary-synced", &journal.store.directory);
            #[cfg(test)]
            migration_io("record-install", &journal.store.directory).map_err(io)?;
            fs::persist(staging, &journal.store.directory.join(Self::NAME), false).map_err(io)?;
            #[cfg(test)]
            migration_crash("record-installed", &journal.store.directory);
            #[cfg(test)]
            migration_io("record-directory-sync", &journal.store.directory).map_err(io)?;
            fs::sync_directory(&directory).map_err(io)?;
            #[cfg(test)]
            migration_crash("record-durable", &journal.store.directory);
            Ok(journal)
        }

        pub fn open(directory: impl AsRef<Path>, binding: &SessionBinding) -> Result<Self> {
            let journal = Self {
                store: FileRefMutationJournal::open(directory, binding)?,
            };
            journal.load(binding)?;
            Ok(journal)
        }

        pub fn load(&self, binding: &SessionBinding) -> Result<Vec<u8>> {
            let (directory, _lock) = self.store.transaction(binding)?;
            if self.store.read_record()?.1.is_some() {
                return Err(EncryptionError::InvalidEncoding);
            }
            let allocation = fs::open(self.store.directory.join(Self::ALLOCATION)).map_err(io)?;
            let metadata = self.store.validate_file(&allocation, Self::ALLOCATION)?;
            if metadata.len() != 40 {
                return Err(EncryptionError::InvalidEncoding);
            }
            let mut evidence = Vec::new();
            (&allocation)
                .take(41)
                .read_to_end(&mut evidence)
                .map_err(io)?;
            if evidence.len() != 40
                || &evidence[..8] != Self::ALLOCATION_MAGIC
                || evidence[8..] != self.store.binding
            {
                return Err(EncryptionError::IdentityChanged);
            }
            let file = fs::open(self.store.directory.join(Self::NAME)).map_err(io)?;
            let metadata = self.store.validate_file(&file, Self::NAME)?;
            if metadata.len() < 81 || metadata.len() > (Self::MAX_BYTES + 80) as u64 {
                return Err(EncryptionError::InvalidEncoding);
            }
            let mut encoded = Vec::new();
            (&file)
                .take((Self::MAX_BYTES + 81) as u64)
                .read_to_end(&mut encoded)
                .map_err(io)?;
            if encoded.len() as u64 != metadata.len() || &encoded[..8] != Self::MAGIC {
                return Err(EncryptionError::InvalidEncoding);
            }
            let length = u64::from_be_bytes(
                encoded[40..48]
                    .try_into()
                    .map_err(|_| EncryptionError::InvalidEncoding)?,
            );
            let end = encoded.len() - 32;
            if encoded[8..40] != self.store.binding
                || length != (end - 48) as u64
                || encoded[end..] != Sha256::digest(&encoded[..end])[..]
            {
                return Err(EncryptionError::InvalidEncoding);
            }
            // Reconfirm durability after an interrupted install/directory sync.
            #[cfg(test)]
            migration_io("recovery-allocation-sync", &self.store.directory).map_err(io)?;
            allocation.sync_all().map_err(io)?;
            #[cfg(test)]
            migration_io("recovery-record-sync", &self.store.directory).map_err(io)?;
            file.sync_all().map_err(io)?;
            #[cfg(test)]
            migration_io("recovery-directory-sync", &self.store.directory).map_err(io)?;
            fs::sync_directory(&directory).map_err(io)?;
            Ok(encoded[48..end].to_vec())
        }
    }

    /// Identity-bound config transaction outside repository/plugin storage.
    /// Reuses the native owner-only transaction lock and empty enrollment anchor.
    pub struct FileConfigMutationJournal {
        store: FileRefMutationJournal,
    }

    impl FileConfigMutationJournal {
        pub fn create(directory: impl AsRef<Path>, binding: &SessionBinding) -> Result<Self> {
            let store = FileRefMutationJournal::create(directory, binding)?;
            Self::initialize(store, binding)
        }
        /// Enroll config state inside an already created, identity-bound native
        /// recovery namespace. Never replace existing config transaction state.
        pub fn create_in_namespace(
            directory: impl AsRef<Path>,
            binding: &SessionBinding,
        ) -> Result<Self> {
            let store = FileRefMutationJournal::open(directory, binding)?;
            Self::initialize(store, binding)
        }
        fn initialize(store: FileRefMutationJournal, binding: &SessionBinding) -> Result<Self> {
            let (directory, _lock) = store.transaction(binding)?;
            if store.read_record()?.1.is_some() {
                return Err(EncryptionError::InvalidEncoding);
            }
            let journal = Self { store };
            // Durable class allocation prevents explicit namespace enrollment
            // from mistaking a lost transaction record for first creation.
            let mut allocation = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(journal.store.directory.join("config-mutation.allocated"))
                .map_err(io)?;
            journal
                .store
                .validate_file(&allocation, "config-mutation.allocated")?;
            allocation.write_all(b"GRAFTCA1").map_err(io)?;
            allocation.write_all(&journal.store.binding).map_err(io)?;
            allocation.sync_all().map_err(io)?;
            fs::sync_directory(&directory).map_err(io)?;
            journal.write(None, false, true)?;
            fs::sync_directory(&directory).map_err(io)?;
            Ok(journal)
        }
        pub fn open(directory: impl AsRef<Path>, binding: &SessionBinding) -> Result<Self> {
            let journal = Self {
                store: FileRefMutationJournal::open(directory, binding)?,
            };
            super::super::ConfigMutationJournal::load(&journal, binding)?;
            Ok(journal)
        }
        fn read(&self) -> Result<(File, Option<super::super::ConfigMutationRecord>, bool)> {
            self.store.path_metadata("config-mutation.allocated")?;
            let allocation =
                fs::open(self.store.directory.join("config-mutation.allocated")).map_err(io)?;
            if self
                .store
                .validate_file(&allocation, "config-mutation.allocated")?
                .len()
                != 40
            {
                return Err(EncryptionError::InvalidEncoding);
            }
            let mut allocated = Vec::new();
            (&allocation)
                .take(41)
                .read_to_end(&mut allocated)
                .map_err(io)?;
            if allocated.len() != 40
                || &allocated[..8] != b"GRAFTCA1"
                || allocated[8..] != self.store.binding
            {
                return Err(EncryptionError::InvalidEncoding);
            }
            let length = self.store.path_metadata("config-mutation")?.len();
            if !(73..=585).contains(&length) {
                return Err(EncryptionError::InvalidEncoding);
            }
            let file = fs::open(self.store.directory.join("config-mutation")).map_err(io)?;
            if self.store.validate_file(&file, "config-mutation")?.len() != length {
                return Err(EncryptionError::InvalidEncoding);
            }
            let mut bytes = Vec::with_capacity(length as usize);
            (&file).take(586).read_to_end(&mut bytes).map_err(io)?;
            if bytes.len() != length as usize
                || (&bytes[..8] != b"GRAFTCJ1" && &bytes[..8] != b"GRAFTCJ2")
            {
                return Err(EncryptionError::InvalidEncoding);
            }
            if bytes[8..40] != self.store.binding {
                return Err(EncryptionError::IdentityChanged);
            }
            let end = bytes.len() - 32;
            if bytes[end..] != Sha256::digest(&bytes[..end])[..] {
                return Err(EncryptionError::InvalidEncoding);
            }
            let record = match bytes[40] {
                0 if end == 41 => None,
                1 => Some(super::super::ConfigMutationRecord::from_local_bytes(
                    &bytes[41..end],
                )?),
                2 if &bytes[..8] == b"GRAFTCJ2" => Some(
                    super::super::ConfigMutationRecord::from_local_bytes(&bytes[41..end])?,
                ),
                _ => return Err(EncryptionError::InvalidEncoding),
            };
            Ok((file, record, bytes[40] == 2))
        }
        fn write(
            &self,
            record: Option<&super::super::ConfigMutationRecord>,
            retired: bool,
            initial: bool,
        ) -> Result<()> {
            let mut bytes = b"GRAFTCJ2".to_vec();
            bytes.extend_from_slice(&self.store.binding);
            bytes.push(if retired {
                2
            } else {
                u8::from(record.is_some())
            });
            if let Some(record) = record {
                bytes.extend_from_slice(&record.to_local_bytes()?);
            }
            bytes.extend_from_slice(&Sha256::digest(&bytes));
            let mut staging = fs::private_named_tempfile(&self.store.directory).map_err(io)?;
            let metadata = fs::file_metadata(staging.as_file()).map_err(io)?;
            if metadata.mode() & 0o077 != 0 || metadata.uid() != self.store.identity.2 {
                return Err(permission());
            }
            staging.write_all(&bytes).map_err(io)?;
            staging.as_file().sync_all().map_err(io)?;
            if initial {
                fs::persist(
                    staging,
                    &self.store.directory.join("config-mutation"),
                    false,
                )
                .map_err(io)?;
            } else {
                fs::persist(staging, &self.store.directory.join("config-mutation"), true)
                    .map_err(io)?;
            }
            Ok(())
        }
    }
    impl super::super::ConfigMutationJournal for FileConfigMutationJournal {
        fn load(
            &self,
            binding: &SessionBinding,
        ) -> Result<Option<super::super::ConfigMutationRecord>> {
            let (_directory, _lock) = self.store.transaction(binding)?;
            if self.store.read_record()?.1.is_some() {
                return Err(EncryptionError::InvalidEncoding);
            }
            let (_, record, retired) = self.read()?;
            Ok(if retired { None } else { record })
        }
        fn load_recoverable(
            &self,
            binding: &SessionBinding,
        ) -> Result<Option<super::super::ConfigMutationRecord>> {
            let (_directory, _lock) = self.store.transaction(binding)?;
            if self.store.read_record()?.1.is_some() {
                return Err(EncryptionError::InvalidEncoding);
            }
            Ok(self.read()?.1)
        }
        fn bind(
            &self,
            binding: &SessionBinding,
            record: &super::super::ConfigMutationRecord,
        ) -> Result<()> {
            let (directory, _lock) = self.store.transaction(binding)?;
            if self.store.read_record()?.1.is_some() {
                return Err(EncryptionError::InvalidEncoding);
            }
            let (file, previous, retired) = self.read()?;
            match previous {
                Some(previous) if retired && previous != *record => {
                    // Never revive an older transaction after retirement. A
                    // fresh authenticated config may belong to another device
                    // at the same generation as our retired replacement.
                    let previous =
                        super::super::EncryptedConfig::parse(previous.replacement_bytes())?;
                    let expected = super::super::EncryptedConfig::parse(record.expected_bytes())?;
                    if expected.generation() < previous.generation() {
                        return Err(EncryptionError::StaleObservation);
                    }
                    drop(file);
                    self.write(Some(record), false, false)?;
                }
                Some(previous) if previous != *record => {
                    return Err(EncryptionError::StaleObservation);
                }
                Some(_) => file.sync_all().map_err(io)?,
                None => {
                    drop(file);
                    self.write(Some(record), false, false)?;
                }
            }
            fs::sync_directory(&directory).map_err(io)
        }
        fn retire(
            &self,
            binding: &SessionBinding,
            record: &super::super::ConfigMutationRecord,
        ) -> Result<()> {
            let (directory, _lock) = self.store.transaction(binding)?;
            if self.store.read_record()?.1.is_some() {
                return Err(EncryptionError::InvalidEncoding);
            }
            let (file, previous, retired) = self.read()?;
            match previous {
                Some(previous) if previous != *record => {
                    return Err(EncryptionError::StaleObservation);
                }
                Some(_) if !retired => {
                    drop(file);
                    self.write(Some(record), true, false)?;
                }
                Some(_) => file.sync_all().map_err(io)?,
                None => {
                    return Err(EncryptionError::CheckpointUnavailable(
                        std::io::ErrorKind::NotFound,
                    ));
                }
            }
            fs::sync_directory(&directory).map_err(io)
        }
    }

    struct Allocation {
        digest: [u8; 32],
        admitted: bool,
    }

    /// Unix owner-only active mutation slot outside the repository. Explicit creation
    /// and existing opening are separate; missing enrollment is never recreated.
    /// Does not implement key remembering or protect against a local administrator.
    pub struct FileRefMutationJournal {
        directory: PathBuf,
        identity: (u64, u64, u32),
        binding: [u8; 32],
    }

    /// One opaque-key slot per ref under a host-selected private directory.
    /// Existing damaged slots fail closed; account bindings cannot be changed.
    pub struct FileRefMutationJournalProvider {
        root: FileRefMutationJournal,
    }

    impl FileRefMutationJournalProvider {
        /// Non-mutating storage preflight; does not enroll or admit recovery slots.
        pub fn validate_directory(
            directory: impl AsRef<Path>,
            binding: &SessionBinding,
        ) -> Result<()> {
            FileRefMutationJournal::prepare(directory.as_ref(), binding)?.directory_handle()?;
            Ok(())
        }

        /// Explicit first enrollment; never overwrite an existing namespace.
        pub fn create(directory: impl AsRef<Path>, binding: &SessionBinding) -> Result<Self> {
            let root = FileRefMutationJournal::create(directory, binding)?;
            let (directory, _lock) = root.transaction(binding)?;
            let provider = Self { root };
            provider.write_allocations(&[], true)?;
            fs::sync_directory(&directory).map_err(io)?;
            Ok(provider)
        }

        /// Reopen only; missing namespace evidence cannot enroll a new namespace.
        pub fn open(directory: impl AsRef<Path>, binding: &SessionBinding) -> Result<Self> {
            let root = FileRefMutationJournal::open(directory, binding)?;
            let (_directory, _lock) = root.transaction(binding)?;
            let provider = Self { root };
            provider.read_allocations()?;
            Ok(provider)
        }

        fn read_allocations(&self) -> Result<Vec<Allocation>> {
            let length = self.root.path_metadata(ALLOCATIONS)?.len();
            let maximum = 72 + 33 * MAX_ALLOCATIONS;
            if length < 72 || length > maximum as u64 {
                return Err(EncryptionError::InvalidEncoding);
            }
            let file = fs::open(self.root.directory.join(ALLOCATIONS)).map_err(io)?;
            if self.root.validate_file(&file, ALLOCATIONS)?.len() != length {
                return Err(EncryptionError::InvalidEncoding);
            }
            let mut bytes = Vec::with_capacity(length as usize);
            (&file)
                .take(maximum as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(io)?;
            if bytes.len() != length as usize {
                return Err(EncryptionError::InvalidEncoding);
            }
            let width = match &bytes[..8] {
                b"GRAFTRA1" => 32,
                b"GRAFTRA2" => 33,
                _ => return Err(EncryptionError::InvalidEncoding),
            };
            if (bytes.len() - 72) % width != 0 || (bytes.len() - 72) / width > MAX_ALLOCATIONS {
                return Err(EncryptionError::InvalidEncoding);
            }
            if bytes[8..40] != self.root.binding {
                return Err(EncryptionError::IdentityChanged);
            }
            let end = bytes.len() - 32;
            if bytes[end..] != Sha256::digest(&bytes[..end])[..] {
                return Err(EncryptionError::InvalidEncoding);
            }
            let entries = bytes[40..end]
                .chunks_exact(width)
                .map(|entry| {
                    let admitted = if width == 32 {
                        // Legacy allocations may have admitted a candidate. Never repair them.
                        true
                    } else {
                        match entry[32] {
                            0 => false,
                            1 => true,
                            _ => return Err(EncryptionError::InvalidEncoding),
                        }
                    };
                    Ok(Allocation {
                        digest: <[u8; 32]>::try_from(&entry[..32])
                            .expect("exact allocation digest"),
                        admitted,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            if entries
                .windows(2)
                .any(|pair| pair[0].digest >= pair[1].digest)
            {
                return Err(EncryptionError::InvalidEncoding);
            }
            Ok(entries)
        }

        fn write_allocations(&self, entries: &[Allocation], initial: bool) -> Result<()> {
            if entries.len() > MAX_ALLOCATIONS {
                return Err(EncryptionError::TooLarge);
            }
            let mut bytes = Vec::with_capacity(72 + 33 * entries.len());
            bytes.extend_from_slice(b"GRAFTRA2");
            bytes.extend_from_slice(&self.root.binding);
            for entry in entries {
                bytes.extend_from_slice(&entry.digest);
                bytes.push(u8::from(entry.admitted));
            }
            bytes.extend_from_slice(&Sha256::digest(&bytes));
            let mut staging = fs::private_named_tempfile(&self.root.directory).map_err(io)?;
            staging.write_all(&bytes).map_err(io)?;
            staging.as_file().sync_all().map_err(io)?;
            if initial {
                fs::persist(staging, &self.root.directory.join(ALLOCATIONS), false).map_err(io)?;
            } else {
                fs::persist(staging, &self.root.directory.join(ALLOCATIONS), true).map_err(io)?;
            }
            Ok(())
        }
    }

    impl FileRefMutationJournalProvider {
        /// Read admitted allocation paths without allocating or repairing any slot.
        pub(in crate::e2ee) fn existing_private_slots(
            &self,
            binding: &SessionBinding,
        ) -> Result<Vec<PathBuf>> {
            let (_parent, _lock) = self.root.transaction(binding)?;
            self.read_allocations()?
                .into_iter()
                .filter(|entry| entry.admitted)
                .map(|entry| {
                    let name = entry
                        .digest
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>();
                    let path = self.root.directory.join(name);
                    Ok(path)
                })
                .collect()
        }

        pub(in crate::e2ee) fn existing_private_slot(
            &self,
            binding: &SessionBinding,
            key: &str,
        ) -> Result<FileRefMutationJournal> {
            let (_parent, _lock) = self.root.transaction(binding)?;
            let digest: [u8; 32] = Sha256::digest(key.as_bytes()).into();
            let entries = self.read_allocations()?;
            let index = entries
                .binary_search_by_key(&digest, |entry| entry.digest)
                .map_err(|_| EncryptionError::StaleObservation)?;
            if !entries[index].admitted {
                return Err(EncryptionError::StaleObservation);
            }
            let name = digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            FileRefMutationJournal::open(self.root.directory.join(name), binding)
        }

        pub(in crate::e2ee) fn private_slot(
            &self,
            binding: &SessionBinding,
            key: &str,
            initialize: &mut dyn FnMut(&FileRefMutationJournal) -> Result<()>,
        ) -> Result<FileRefMutationJournal> {
            if binding_digest(binding)? != self.root.binding {
                return Err(EncryptionError::IdentityChanged);
            }
            // Only allocation/lookup holds this lock; ref publication remains independent.
            let (parent, _lock) = self.root.transaction(binding)?;
            if self.root.read_record()?.1.is_some() {
                return Err(EncryptionError::InvalidEncoding);
            }
            let digest: [u8; 32] = Sha256::digest(key.as_bytes()).into();
            let name = digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let path = self.root.directory.join(name);
            let mut allocations = self.read_allocations()?;
            let index = match allocations.binary_search_by_key(&digest, |entry| entry.digest) {
                Ok(index) if allocations[index].admitted => {
                    let store = FileRefMutationJournal::open(&path, binding)?;
                    // Reconfirm a visible admission whose previous directory sync
                    // may have failed; never let a later bind outrun durable admission.
                    fs::sync_directory(&parent).map_err(io)?;
                    return Ok(store);
                }
                Ok(index) => index,
                Err(index) => {
                    allocations.insert(index, Allocation { digest, admitted: false });
                    self.write_allocations(&allocations, false)?;
                    fs::sync_directory(&parent).map_err(io)?;
                    #[cfg(test)]
                    allocation_crash("reserved", &self.root.directory);
                    index
                }
            };
            // A reserved slot has never escaped this provider. Its missing empty
            // initialization files may be completed, but existing data is never overwritten.
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(error) => return Err(io(error)),
            }
            #[cfg(test)]
            allocation_crash("directory", &self.root.directory);
            let store = FileRefMutationJournal::complete_unadmitted(&path, binding)?;
            initialize(&store)?;
            allocations[index].admitted = true;
            self.write_allocations(&allocations, false)?;
            #[cfg(test)]
            allocation_crash("admission-written", &self.root.directory);
            fs::sync_directory(&parent).map_err(io)?;
            #[cfg(test)]
            allocation_crash("admitted", &self.root.directory);
            Ok(store)
        }
    }

    impl RefMutationJournalProvider for FileRefMutationJournalProvider {
        fn journal(
            &self,
            binding: &SessionBinding,
            key: &str,
        ) -> Result<std::sync::Arc<dyn RefMutationJournal>> {
            crate::e2ee::reference::validate_remote_key(key)?;
            Ok(std::sync::Arc::new(self.private_slot(
                binding,
                key,
                &mut |_| Ok(()),
            )?))
        }
    }

    impl FileRefMutationJournal {
        /// Share the hardened native journal boundary with sibling private records.
        /// The returned lock must remain held for every record read or replacement.
        pub(in crate::e2ee) fn private_transaction(
            &self,
            binding: &SessionBinding,
        ) -> Result<(File, File, &Path, [u8; 32], u32)> {
            let (directory, lock) = self.transaction(binding)?;
            if self.read_record()?.1.is_some() {
                return Err(EncryptionError::InvalidEncoding);
            }
            Ok((
                directory,
                lock,
                &self.directory,
                self.binding,
                self.identity.2,
            ))
        }

        pub(in crate::e2ee) fn validate_private_file(
            &self,
            file: &File,
            name: &str,
        ) -> Result<Metadata> {
            self.directory_handle()?;
            self.validate_file(file, name)
        }

        // Only a durable unadmitted reservation authorizes this completion path.
        fn complete_unadmitted(directory: &Path, binding: &SessionBinding) -> Result<Self> {
            let store = Self::prepare(directory, binding)?;
            let directory = store.directory_handle()?;
            let lock_path = store.directory.join(LOCK);
            let lock = match fs::symlink_metadata(&lock_path) {
                Ok(_) => OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&lock_path)
                    .map_err(io)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&lock_path)
                    .map_err(io)?,
                Err(error) => return Err(io(error)),
            };
            if store.validate_file(&lock, LOCK)?.len() != 0 {
                return Err(EncryptionError::InvalidEncoding);
            }
            lock.try_lock().map_err(lock_error)?;
            lock.sync_all().map_err(io)?;
            #[cfg(test)]
            allocation_crash("lock", store.directory.parent().expect("slot parent"));
            match fs::symlink_metadata(store.directory.join(RECORD)) {
                Ok(_) => {
                    let (file, record) = store.read_record()?;
                    if record.is_some() {
                        return Err(EncryptionError::StaleObservation);
                    }
                    file.sync_all().map_err(io)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    store.write_record(None, true)?
                }
                Err(error) => return Err(io(error)),
            }
            fs::sync_directory(&directory).map_err(io)?;
            #[cfg(test)]
            allocation_crash(
                "empty-record",
                store.directory.parent().expect("slot parent"),
            );
            Ok(store)
        }

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
            store.validate_file(&lock, LOCK)?;
            lock.try_lock().map_err(lock_error)?;
            #[cfg(test)]
            migration_crash("namespace-lock-created", &store.directory);
            lock.sync_all().map_err(io)?;
            #[cfg(test)]
            migration_crash("namespace-lock-synced", &store.directory);
            store.write_record(None, true)?;
            #[cfg(test)]
            migration_crash("namespace-anchor-installed", &store.directory);
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

        fn read_record(&self) -> Result<(File, Option<RefMutationRecord>)> {
            let (file, record, retired) = self.read_record_state()?;
            Ok((file, if retired { None } else { record }))
        }

        fn read_record_state(&self) -> Result<(File, Option<RefMutationRecord>, bool)> {
            let length = self.path_metadata(RECORD)?.len();
            if !(73..=MAX_SIZE as u64).contains(&length) {
                return Err(EncryptionError::InvalidEncoding);
            }
            let file = fs::open(self.directory.join(RECORD)).map_err(io)?;
            if self.validate_file(&file, RECORD)?.len() != length {
                return Err(EncryptionError::InvalidEncoding);
            }
            let mut bytes = Vec::with_capacity(length as usize);
            (&file)
                .take(MAX_SIZE as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(io)?;
            if bytes.len() != length as usize || &bytes[..8] != b"GRAFTRJ1" {
                return Err(EncryptionError::InvalidEncoding);
            }
            if bytes[8..40] != self.binding {
                return Err(EncryptionError::IdentityChanged);
            }
            let end = bytes.len() - 32;
            if bytes[end..] != Sha256::digest(&bytes[..end])[..] {
                return Err(EncryptionError::InvalidEncoding);
            }
            let record = match bytes[40] {
                0 if end == 41 => None,
                1 | 2 => Some(RefMutationRecord::from_local_bytes(&bytes[41..end])?),
                _ => return Err(EncryptionError::InvalidEncoding),
            };
            Ok((file, record, bytes[40] == 2))
        }

        fn write_record(
            &self,
            checkpoint: Option<&RefMutationRecord>,
            initial: bool,
        ) -> Result<()> {
            self.write_record_state(checkpoint, initial, false)
        }

        fn write_record_state(
            &self,
            checkpoint: Option<&RefMutationRecord>,
            initial: bool,
            retired: bool,
        ) -> Result<()> {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(b"GRAFTRJ1");
            bytes.extend_from_slice(&self.binding);
            bytes.push(if retired {
                2
            } else {
                u8::from(checkpoint.is_some())
            });
            if let Some(record) = checkpoint {
                bytes.extend_from_slice(&record.to_local_bytes()?);
            }
            bytes.extend_from_slice(&Sha256::digest(&bytes));
            if bytes.len() > MAX_SIZE {
                return Err(EncryptionError::TooLarge);
            }
            let mut staging = fs::private_named_tempfile(&self.directory).map_err(io)?;
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

    impl RefMutationJournal for FileRefMutationJournal {
        fn load(&self, binding: &SessionBinding) -> Result<Option<RefMutationRecord>> {
            let (_directory, _lock) = self.transaction(binding)?;
            Ok(self.read_record()?.1)
        }
        fn bind(&self, binding: &SessionBinding, candidate: &RefMutationRecord) -> Result<()> {
            RefMutationRecord::from_local_bytes(&candidate.to_local_bytes()?)?;
            let (directory, _lock) = self.transaction(binding)?;
            let (file, previous) = self.read_record()?;
            if let Some(previous) = previous {
                if previous != *candidate {
                    return Err(EncryptionError::StaleObservation);
                }
                file.sync_all().map_err(io)?;
            } else {
                drop(file);
                self.write_record(Some(candidate), false)?;
            }
            fs::sync_directory(&directory).map_err(io)?;
            Ok(())
        }
        fn retire(&self, binding: &SessionBinding, candidate: &RefMutationRecord) -> Result<()> {
            let (directory, _lock) = self.transaction(binding)?;
            let (file, previous, retired) = self.read_record_state()?;
            match previous {
                Some(previous) if previous == *candidate => {
                    if retired {
                        file.sync_all().map_err(io)?;
                    } else {
                        drop(file);
                        self.write_record_state(Some(candidate), false, true)?;
                    }
                }
                Some(_) => return Err(EncryptionError::StaleObservation),
                None => {
                    return Err(EncryptionError::CheckpointUnavailable(
                        std::io::ErrorKind::NotFound,
                    ));
                }
            }
            fs::sync_directory(&directory).map_err(io)?;
            Ok(())
        }
    }

    #[cfg(test)]
    std::thread_local! {
        static MIGRATION_FAILURE: std::cell::RefCell<Option<(&'static str, PathBuf)>> = const {
            std::cell::RefCell::new(None)
        };
    }

    #[cfg(test)]
    pub(super) fn with_migration_failure<T>(
        phase: &'static str,
        directory: &Path,
        operation: impl FnOnce() -> T,
    ) -> T {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                MIGRATION_FAILURE.with(|slot| *slot.borrow_mut() = None);
            }
        }
        MIGRATION_FAILURE.with(|slot| {
            let mut slot = slot.borrow_mut();
            assert!(slot.is_none(), "nested migration persistence failure");
            *slot = Some((phase, directory.to_owned()));
        });
        let _reset = Reset;
        operation()
    }

    #[cfg(test)]
    fn migration_io(phase: &str, directory: &Path) -> std::io::Result<()> {
        let fail = MIGRATION_FAILURE.with(|slot| {
            slot.borrow()
                .as_ref()
                .is_some_and(|(expected, path)| *expected == phase && path == directory)
        });
        if fail {
            return Err(std::io::Error::other(
                "synthetic migration persistence failure",
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn migration_crash(phase: &str, directory: &Path) {
        if std::env::var("GRAFT_TEST_MIGRATION_PHASE").as_deref() == Ok(phase)
            && std::env::var_os("GRAFT_TEST_MIGRATION_DIRECTORY").as_deref()
                == Some(directory.as_os_str())
        {
            // Abrupt exit: no unwinding or temporary-file cleanup. Not power loss.
            std::process::exit(73);
        }
    }

    #[cfg(test)]
    fn allocation_crash(phase: &str, directory: &Path) {
        if std::env::var("GRAFT_TEST_ALLOCATION_PHASE").as_deref() == Ok(phase)
            && std::env::var_os("GRAFT_TEST_ALLOCATION_DIRECTORY").as_deref()
                == Some(directory.as_os_str())
        {
            // Real process exit: no stack unwinding or Rust cleanup at this write boundary.
            std::process::exit(73);
        }
    }

    fn binding_digest(binding: &SessionBinding) -> Result<[u8; 32]> {
        Ok(Sha256::digest(codec::encode(&[
            b"graft/local-ref-mutation/v1",
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
pub use file::{
    FileConfigMutationJournal, FileMigrationEnrollmentJournal, FileMigrationSourceJournal,
    FileMigrationTargetJournal, FileRefMutationJournal, FileRefMutationJournalProvider,
};

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::e2ee::{EncryptedRef, EncryptionError, RefTarget, tests::fixture_root};
    use std::{fs, os::unix::fs::PermissionsExt};

    fn binding(account: &str) -> SessionBinding {
        SessionBinding::new(
            "mutation.test".into(),
            account.into(),
            fixture_root().repository(),
        )
        .unwrap()
    }
    fn record() -> RefMutationRecord {
        let replacement = EncryptedRef::seal(
            &fixture_root(),
            "refs/heads/private-journal-name",
            &RefTarget::Commit([7; 32]),
        )
        .unwrap();
        RefMutationRecord {
            key: replacement.remote_key().into(),
            expected: None,
            replacement: Some(replacement.as_bytes().to_vec()),
        }
    }

    #[test]
    fn migration_target_journal_is_bound_and_cannot_accept_source_class_records() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        for directory in [&source, &target] {
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let identity = binding("account");
        FileMigrationSourceJournal::create(source.path(), &identity, b"private source").unwrap();
        let journal =
            FileMigrationTargetJournal::create(target.path(), &identity, b"private target")
                .unwrap();
        assert_eq!(journal.load(&identity).unwrap(), b"private target");
        assert!(FileMigrationSourceJournal::open(target.path(), &identity).is_err());
        assert!(FileMigrationTargetJournal::open(source.path(), &identity).is_err());
        assert!(
            FileMigrationTargetJournal::open(target.path(), &binding("other-account")).is_err()
        );
        let other_uuid = SessionBinding::new(
            "mutation.test".into(),
            "account".into(),
            crate::e2ee::RepositoryId::generate().unwrap(),
        )
        .unwrap();
        assert!(FileMigrationTargetJournal::open(target.path(), &other_uuid).is_err());
        let record = target.path().join("migration-target");
        let exact = fs::read(&record).unwrap();
        fs::write(
            &record,
            fs::read(source.path().join("migration-source")).unwrap(),
        )
        .unwrap();
        assert!(journal.load(&identity).is_err());
        fs::write(&record, &exact).unwrap();
        let allocation = target.path().join("migration-target.allocated");
        let exact_allocation = fs::read(&allocation).unwrap();
        fs::write(
            &allocation,
            fs::read(source.path().join("migration-source.allocated")).unwrap(),
        )
        .unwrap();
        assert!(journal.load(&identity).is_err());
        fs::write(&allocation, exact_allocation).unwrap();
        assert_eq!(journal.load(&identity).unwrap(), b"private target");
        fs::remove_file(&record).unwrap();
        assert!(FileMigrationTargetJournal::open(target.path(), &identity).is_err());
        assert!(
            FileMigrationTargetJournal::create(target.path(), &identity, b"replacement").is_err()
        );
        assert!(!record.exists());
    }

    #[test]
    fn migration_returned_io_failures_never_report_creation_or_recovery_success() {
        migration_io_failures::<false, false>();
        migration_io_failures::<true, false>();
        migration_io_failures::<false, true>();
    }

    fn migration_io_failures<const TARGET: bool, const ENROLLMENT: bool>() {
        for phase in [
            "allocation-write",
            "allocation-partial-write",
            "allocation-sync",
            "allocation-directory-sync",
            "temporary-write",
            "temporary-partial-write",
            "temporary-sync",
            "record-install",
            "record-directory-sync",
        ] {
            let directory = tempfile::tempdir().unwrap();
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let identity = binding("account");
            let bytes = b"retained migration source\n".repeat(4096);
            let result = file::with_migration_failure(phase, directory.path(), || {
                file::FileMigrationJournal::<TARGET, ENROLLMENT>::create(
                    directory.path(),
                    &identity,
                    &bytes,
                )
            });
            assert!(
                matches!(
                    result,
                    Err(EncryptionError::CheckpointUnavailable(
                        std::io::ErrorKind::Other
                    ))
                ),
                "{phase}"
            );
            assert!(
                file::FileMigrationJournal::<TARGET, ENROLLMENT>::create(
                    directory.path(),
                    &identity,
                    b"new source"
                )
                .is_err()
            );
            let reopened =
                file::FileMigrationJournal::<TARGET, ENROLLMENT>::open(directory.path(), &identity);
            if phase == "record-directory-sync" {
                assert_eq!(reopened.unwrap().load(&identity).unwrap(), bytes);
            } else {
                assert!(reopened.is_err(), "{phase}");
            }
        }
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let identity = binding("account");
        let bytes = b"exact durable source";
        let journal = file::FileMigrationJournal::<TARGET, ENROLLMENT>::create(
            directory.path(),
            &identity,
            bytes,
        )
        .unwrap();
        let name = if ENROLLMENT {
            "migration-enrollment"
        } else if TARGET {
            "migration-target"
        } else {
            "migration-source"
        };
        let exact = fs::read(directory.path().join(name)).unwrap();
        for phase in [
            "recovery-allocation-sync",
            "recovery-record-sync",
            "recovery-directory-sync",
        ] {
            let result =
                file::with_migration_failure(phase, directory.path(), || journal.load(&identity));
            assert!(
                matches!(
                    result,
                    Err(EncryptionError::CheckpointUnavailable(
                        std::io::ErrorKind::Other
                    ))
                ),
                "{phase}"
            );
            let reopened = file::with_migration_failure(phase, directory.path(), || {
                file::FileMigrationJournal::<TARGET, ENROLLMENT>::open(directory.path(), &identity)
            });
            assert!(
                matches!(
                    reopened,
                    Err(EncryptionError::CheckpointUnavailable(
                        std::io::ErrorKind::Other
                    ))
                ),
                "{phase}"
            );
            assert_eq!(fs::read(directory.path().join(name)).unwrap(), exact);
            assert_eq!(
                file::FileMigrationJournal::<TARGET, ENROLLMENT>::open(directory.path(), &identity)
                    .unwrap()
                    .load(&identity)
                    .unwrap(),
                bytes
            );
        }
    }

    #[test]
    fn migration_persistence_child() {
        let Some(directory) = std::env::var_os("GRAFT_TEST_MIGRATION_DIRECTORY") else {
            return;
        };
        let directory = std::path::Path::new(&directory);
        if std::env::var("GRAFT_TEST_MIGRATION_TARGET").as_deref() == Ok("true") {
            migration_persistence_run::<true>(directory);
        } else {
            migration_persistence_run::<false>(directory);
        }
    }

    fn migration_persistence_run<const TARGET: bool>(directory: &std::path::Path) {
        let bytes = b"synthetic private frozen migration source\n".repeat(4096);
        let identity = binding("account");
        match std::env::var("GRAFT_TEST_MIGRATION_OPERATION")
            .unwrap()
            .as_str()
        {
            "create" => {
                file::FileMigrationJournal::<TARGET>::create(directory, &identity, &bytes).unwrap();
                panic!("migration persistence boundary was not reached");
            }
            "read-complete" => {
                let journal =
                    file::FileMigrationJournal::<TARGET>::open(directory, &identity).unwrap();
                assert_eq!(journal.load(&identity).unwrap(), bytes);
                assert!(
                    file::FileMigrationJournal::<TARGET>::open(
                        directory,
                        &binding("other-account")
                    )
                    .is_err()
                );
                assert!(
                    file::FileMigrationJournal::<TARGET>::create(
                        directory,
                        &identity,
                        b"replacement source"
                    )
                    .is_err()
                );
                assert_eq!(journal.load(&identity).unwrap(), bytes);
            }
            "read-incomplete" => {
                assert!(file::FileMigrationJournal::<TARGET>::open(directory, &identity).is_err());
                assert!(
                    file::FileMigrationJournal::<TARGET>::create(
                        directory,
                        &identity,
                        b"replacement source"
                    )
                    .is_err()
                );
            }
            _ => panic!("unknown synthetic migration operation"),
        }
    }

    fn migration_child(
        directory: &std::path::Path,
        operation: &str,
        phase: &str,
        target: bool,
    ) -> i32 {
        // Bound every child wait and reap it even if an assertion unwinds.
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = Child(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "e2ee::mutation_journal::tests::migration_persistence_child",
                    "--nocapture",
                ])
                .env("GRAFT_TEST_MIGRATION_DIRECTORY", directory)
                .env("GRAFT_TEST_MIGRATION_OPERATION", operation)
                .env("GRAFT_TEST_MIGRATION_PHASE", phase)
                .env("GRAFT_TEST_MIGRATION_TARGET", target.to_string())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                return status.code().expect("migration child terminated by signal");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "migration child timed out: {operation}/{phase}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    #[ignore = "run migration persistence subprocess matrix separately with --test-threads=1"]
    fn migration_creation_process_exit_recovers_only_exact_installed_intent() {
        for target in [false, true] {
            for phase in [
                "namespace-lock-created",
                "namespace-lock-synced",
                "namespace-anchor-installed",
                "namespace-created",
                "allocation-created",
                "allocation-partial",
                "allocation-written",
                "allocation-synced",
                "allocation-durable",
                "temporary-created",
                "temporary-written",
                "temporary-synced",
                "record-installed",
                "record-durable",
            ] {
                let directory = tempfile::tempdir().unwrap();
                fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
                assert_eq!(
                    migration_child(directory.path(), "create", phase, target),
                    73,
                    "{phase}"
                );
                let snapshot = || {
                    let mut files: Vec<_> = fs::read_dir(directory.path())
                        .unwrap()
                        .map(|entry| {
                            let entry = entry.unwrap();
                            (entry.file_name(), fs::read(entry.path()).unwrap())
                        })
                        .collect();
                    files.sort();
                    files
                };
                let before = snapshot();
                let complete = matches!(phase, "record-installed" | "record-durable");
                assert_eq!(
                    migration_child(
                        directory.path(),
                        if complete {
                            "read-complete"
                        } else {
                            "read-incomplete"
                        },
                        "",
                        target
                    ),
                    0,
                    "{phase}"
                );
                assert_eq!(
                    snapshot(),
                    before,
                    "recovery changed retained files at {phase}"
                );
            }
        }
    }

    #[test]
    fn migration_journal_retains_exact_bytes_identity_and_allocation() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let identity = binding("account");
        let bytes = b"private immutable source plan";
        let journal =
            FileMigrationSourceJournal::create(directory.path(), &identity, bytes).unwrap();
        assert_eq!(journal.load(&identity).unwrap(), bytes);
        let reopened = FileMigrationSourceJournal::open(directory.path(), &identity).unwrap();
        assert_eq!(reopened.load(&identity).unwrap(), bytes);
        assert!(
            FileMigrationSourceJournal::open(directory.path(), &binding("other-account")).is_err()
        );
        let other_domain = SessionBinding::new(
            "other.test".into(),
            "account".into(),
            fixture_root().repository(),
        )
        .unwrap();
        assert!(FileMigrationSourceJournal::open(directory.path(), &other_domain).is_err());
        assert!(
            FileMigrationSourceJournal::create(directory.path(), &identity, b"replacement")
                .is_err()
        );
        assert_eq!(reopened.load(&identity).unwrap(), bytes);
        let path = directory.path().join("migration-source");
        let exact = fs::read(&path).unwrap();
        let mut damaged = exact.clone();
        damaged[48] ^= 1;
        fs::write(&path, damaged).unwrap();
        assert!(FileMigrationSourceJournal::open(directory.path(), &identity).is_err());
        fs::write(&path, &exact).unwrap();
        fs::hard_link(&path, directory.path().join("linked-record")).unwrap();
        assert!(reopened.load(&identity).is_err());
        fs::remove_file(directory.path().join("linked-record")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(reopened.load(&identity).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_file(directory.path().join("migration-source.allocated")).unwrap();
        assert!(FileMigrationSourceJournal::open(directory.path(), &identity).is_err());
        assert!(FileMigrationSourceJournal::create(directory.path(), &identity, bytes).is_err());
        assert_eq!(fs::read(&path).unwrap(), exact);
    }

    #[test]
    fn migration_journal_missing_or_redirected_record_cannot_be_recreated() {
        for redirect in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let identity = binding("account");
            let journal =
                FileMigrationSourceJournal::create(directory.path(), &identity, b"frozen plan")
                    .unwrap();
            let path = directory.path().join("migration-source");
            let exact = fs::read(&path).unwrap();
            fs::remove_file(&path).unwrap();
            if redirect {
                let other = directory.path().join("redirected-record");
                fs::write(&other, &exact).unwrap();
                fs::set_permissions(&other, fs::Permissions::from_mode(0o600)).unwrap();
                std::os::unix::fs::symlink(other, &path).unwrap();
            }
            assert!(journal.load(&identity).is_err());
            assert!(FileMigrationSourceJournal::open(directory.path(), &identity).is_err());
            assert!(
                FileMigrationSourceJournal::create(directory.path(), &identity, b"new plan")
                    .is_err()
            );
            assert!(directory.path().join("migration-source.allocated").exists());
            if !redirect {
                assert!(!path.exists());
            }
        }
    }

    #[test]
    fn config_journal_reopens_exact_bytes_and_never_clears_newer_or_lost_records() {
        use crate::e2ee::{
            ConfigMutationJournal, ConfigMutationRecord, EncryptedConfig, FileConfigMutationJournal,
        };
        use sha2::{Digest, Sha256};
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let old =
            EncryptedConfig::create(&fixture_root(), b"config journal synthetic old").unwrap();
        let next = old
            .password_change_candidate(&fixture_root(), b"config journal synthetic new")
            .unwrap();
        let last = next
            .password_change_candidate(&fixture_root(), b"config journal synthetic later")
            .unwrap();
        let record = |expected: &EncryptedConfig, replacement: &EncryptedConfig| {
            ConfigMutationRecord::from_local_bytes(
                &crate::e2ee::codec::encode(&[expected.as_bytes(), replacement.as_bytes()])
                    .unwrap(),
            )
            .unwrap()
        };
        let first = record(&old, &next);
        let second = record(&next, &last);
        let journal =
            FileConfigMutationJournal::create(directory.path(), &binding("account")).unwrap();
        journal.bind(&binding("account"), &first).unwrap();
        let reopened =
            FileConfigMutationJournal::open(directory.path(), &binding("account")).unwrap();
        assert!(reopened.load(&binding("account")).unwrap() == Some(first.clone()));
        assert_eq!(
            reopened.bind(&binding("account"), &second),
            Err(EncryptionError::StaleObservation)
        );
        reopened.retire(&binding("account"), &first).unwrap();
        let retired =
            FileConfigMutationJournal::open(directory.path(), &binding("account")).unwrap();
        assert!(retired.load(&binding("account")).unwrap().is_none());
        assert!(retired.load_recoverable(&binding("account")).unwrap() == Some(first.clone()));
        retired.retire(&binding("account"), &first).unwrap();
        retired.bind(&binding("account"), &first).unwrap();
        assert!(retired.load(&binding("account")).unwrap().is_none());
        reopened.bind(&binding("account"), &second).unwrap();
        assert_eq!(
            journal.retire(&binding("account"), &first),
            Err(EncryptionError::StaleObservation)
        );
        reopened.retire(&binding("account"), &second).unwrap();
        assert_eq!(
            journal.bind(&binding("account"), &first),
            Err(EncryptionError::StaleObservation)
        );
        assert!(reopened.load_recoverable(&binding("account")).unwrap() == Some(second.clone()));
        assert!(FileConfigMutationJournal::open(directory.path(), &binding("other")).is_err());
        let path = directory.path().join("config-mutation");
        let original = fs::read(&path).unwrap();
        // Legacy pending records remain readable, but legacy empty records
        // cannot manufacture a receipt or a completed transaction.
        let mut legacy = b"GRAFTCJ1".to_vec();
        legacy.extend_from_slice(&original[8..40]);
        legacy.push(1);
        legacy.extend_from_slice(&second.to_local_bytes().unwrap());
        legacy.extend_from_slice(&Sha256::digest(&legacy));
        fs::write(&path, &legacy).unwrap();
        assert!(reopened.load_recoverable(&binding("account")).unwrap() == Some(second));
        legacy.truncate(40);
        legacy.push(0);
        legacy.extend_from_slice(&Sha256::digest(&legacy));
        fs::write(&path, legacy).unwrap();
        assert!(
            reopened
                .load_recoverable(&binding("account"))
                .unwrap()
                .is_none()
        );
        let mut corrupted = original.clone();
        corrupted[41] ^= 1;
        fs::write(&path, corrupted).unwrap();
        assert!(reopened.load(&binding("account")).is_err());
        fs::write(&path, original).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(FileConfigMutationJournal::open(directory.path(), &binding("account")).is_err());
        assert!(journal.bind(&binding("account"), &first).is_err());
        assert!(
            FileConfigMutationJournal::create_in_namespace(directory.path(), &binding("account"))
                .is_err()
        );
        assert!(!path.exists());
    }

    #[test]
    fn ref_journal_retirement_never_clears_a_newer_candidate() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let provider =
            FileRefMutationJournalProvider::create(directory.path(), &binding("account")).unwrap();
        let old = record();
        let next = record();
        let slot = provider.journal(&binding("account"), &old.key).unwrap();
        assert!(matches!(
            slot.retire(&binding("account"), &old),
            Err(EncryptionError::CheckpointUnavailable(
                std::io::ErrorKind::NotFound
            ))
        ));
        slot.bind(&binding("account"), &old).unwrap();
        assert_eq!(
            slot.retire(&binding("account"), &next),
            Err(EncryptionError::StaleObservation)
        );
        slot.retire(&binding("account"), &old).unwrap();
        slot.retire(&binding("account"), &old).unwrap();
        assert_eq!(
            slot.retire(&binding("account"), &next),
            Err(EncryptionError::StaleObservation)
        );
        slot.bind(&binding("account"), &next).unwrap();
        let reopened = provider.journal(&binding("account"), &old.key).unwrap();
        assert_eq!(
            slot.retire(&binding("account"), &old),
            Err(EncryptionError::StaleObservation)
        );
        assert!(reopened.load(&binding("account")).unwrap() == Some(next));
        assert!(provider.journal(&binding("other"), &old.key).is_err());
        assert!(
            provider
                .journal(&binding("account"), "refs/heads/private-journal-name")
                .is_err()
        );
    }

    #[test]
    fn ref_journal_provider_is_per_ref_and_never_repairs_missing_records() {
        use sha2::{Digest, Sha256};
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let provider =
            FileRefMutationJournalProvider::create(directory.path(), &binding("account")).unwrap();
        let first = record();
        let slot = provider.journal(&binding("account"), &first.key).unwrap();
        slot.bind(&binding("account"), &first).unwrap();
        let name = Sha256::digest(first.key.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = directory.path().join(name);
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.join("ref-mutation.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        let other = EncryptedRef::seal(
            &fixture_root(),
            "refs/heads/independent",
            &RefTarget::Commit([8; 32]),
        )
        .unwrap();
        assert!(
            provider
                .journal(&binding("account"), other.remote_key())
                .is_ok()
        );
        assert!(matches!(
            provider.journal(&binding("account"), &first.key),
            Err(EncryptionError::SessionBusy)
        ));
        drop(lock);
        fs::remove_file(path.join("ref-mutation")).unwrap();
        assert!(provider.journal(&binding("account"), &first.key).is_err());
        assert!(!path.join("ref-mutation").exists());
    }

    #[test]
    fn ref_journal_namespace_loss_never_reenrolls_slots_or_root() {
        use sha2::{Digest, Sha256};
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let provider =
            FileRefMutationJournalProvider::create(directory.path(), &binding("account")).unwrap();
        let candidate = record();
        provider
            .journal(&binding("account"), &candidate.key)
            .unwrap()
            .bind(&binding("account"), &candidate)
            .unwrap();
        drop(provider);
        let name = Sha256::digest(candidate.key.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let slot = directory.path().join(name);
        fs::remove_dir_all(&slot).unwrap();
        let reopened =
            FileRefMutationJournalProvider::open(directory.path(), &binding("account")).unwrap();
        assert!(
            reopened
                .journal(&binding("account"), &candidate.key)
                .is_err()
        );
        assert!(!slot.exists());
        assert!(
            FileRefMutationJournalProvider::create(directory.path(), &binding("account")).is_err()
        );
        assert!(FileRefMutationJournalProvider::open(directory.path(), &binding("other")).is_err());
        drop(reopened);
        fs::remove_file(directory.path().join("ref-allocations")).unwrap();
        assert!(
            FileRefMutationJournalProvider::open(directory.path(), &binding("account")).is_err()
        );
        assert!(!directory.path().join("ref-allocations").exists());
        // Recreated owner-only directory is still not an enrolled namespace.
        let recreated = tempfile::tempdir().unwrap();
        fs::set_permissions(recreated.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            FileRefMutationJournalProvider::open(recreated.path(), &binding("account")).is_err()
        );
        assert!(!recreated.path().join("ref-mutation").exists());
    }

    #[test]
    fn ref_journal_interrupted_allocation_and_corrupt_namespace_fail_closed() {
        use sha2::{Digest, Sha256};
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let provider =
            FileRefMutationJournalProvider::create(directory.path(), &binding("account")).unwrap();
        let candidate = record();
        let name = Sha256::digest(candidate.key.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let slot = directory.path().join(name);
        // Force allocation failure after its durable reservation, before admission.
        fs::write(&slot, b"obstruction").unwrap();
        assert!(
            provider
                .journal(&binding("account"), &candidate.key)
                .is_err()
        );
        fs::remove_file(&slot).unwrap();
        drop(provider);
        let reopened =
            FileRefMutationJournalProvider::open(directory.path(), &binding("account")).unwrap();
        let resumed = reopened
            .journal(&binding("account"), &candidate.key)
            .unwrap();
        assert!(resumed.load(&binding("account")).unwrap().is_none());
        let path = directory.path().join("ref-allocations");
        let mut bytes = fs::read(&path).unwrap();
        bytes[40] ^= 1;
        fs::write(&path, bytes).unwrap();
        assert!(
            reopened
                .journal(&binding("account"), &candidate.key)
                .is_err()
        );
        assert!(
            FileRefMutationJournalProvider::open(directory.path(), &binding("account")).is_err()
        );
    }

    #[test]
    fn ref_allocation_crash_child() {
        let Some(directory) = std::env::var_os("GRAFT_TEST_ALLOCATION_DIRECTORY") else {
            return;
        };
        let provider = FileRefMutationJournalProvider::open(
            std::path::Path::new(&directory),
            &binding("account"),
        )
        .unwrap();
        provider
            .journal(&binding("account"), &record().key)
            .unwrap();
        panic!("allocation crash boundary was not reached");
    }

    #[test]
    fn ref_allocation_process_exit_resumes_only_unadmitted_empty_slots() {
        use sha2::{Digest, Sha256};
        for phase in [
            "reserved",
            "directory",
            "lock",
            "empty-record",
            "admission-written",
            "admitted",
        ] {
            let directory = tempfile::tempdir().unwrap();
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
            drop(
                FileRefMutationJournalProvider::create(directory.path(), &binding("account"))
                    .unwrap(),
            );
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "e2ee::mutation_journal::tests::ref_allocation_crash_child",
                    "--nocapture",
                ])
                .env("GRAFT_TEST_ALLOCATION_DIRECTORY", directory.path())
                .env("GRAFT_TEST_ALLOCATION_PHASE", phase)
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(73), "{phase}");
            let provider =
                FileRefMutationJournalProvider::open(directory.path(), &binding("account"))
                    .unwrap();
            let candidate = record();
            let slot = provider
                .journal(&binding("account"), &candidate.key)
                .unwrap();
            assert!(slot.load(&binding("account")).unwrap().is_none(), "{phase}");
            slot.bind(&binding("account"), &candidate).unwrap();
            drop(slot);
            drop(provider);
            let provider =
                FileRefMutationJournalProvider::open(directory.path(), &binding("account"))
                    .unwrap();
            assert!(
                provider
                    .journal(&binding("account"), &candidate.key)
                    .unwrap()
                    .load(&binding("account"))
                    .unwrap()
                    == Some(candidate.clone())
            );
            // Once admitted, even an empty slot may not be reconstructed from absence.
            let name = Sha256::digest(candidate.key.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let path = directory.path().join(name);
            fs::remove_dir_all(&path).unwrap();
            assert!(
                provider
                    .journal(&binding("account"), &candidate.key)
                    .is_err(),
                "{phase}"
            );
            assert!(!path.exists());
        }
    }

    #[test]
    fn ref_allocation_reserved_state_never_overwrites_existing_candidate_or_corruption() {
        use sha2::{Digest, Sha256};
        for active in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let provider =
                FileRefMutationJournalProvider::create(directory.path(), &binding("account"))
                    .unwrap();
            let candidate = record();
            let slot = provider
                .journal(&binding("account"), &candidate.key)
                .unwrap();
            if active {
                slot.bind(&binding("account"), &candidate).unwrap();
            }
            let name = Sha256::digest(candidate.key.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let record_path = directory.path().join(name).join("ref-mutation");
            if !active {
                fs::write(&record_path, b"corrupt record").unwrap();
            }
            let original = fs::read(&record_path).unwrap();
            // Manufacture an inconsistent reserved state to prove completion never
            // overwrites either an active candidate or a malformed existing record.
            let manifest_path = directory.path().join("ref-allocations");
            let mut manifest = fs::read(&manifest_path).unwrap();
            manifest.truncate(manifest.len() - 32);
            manifest[72] = 0;
            manifest.extend_from_slice(&Sha256::digest(&manifest));
            fs::write(&manifest_path, manifest).unwrap();
            drop(provider);
            let provider =
                FileRefMutationJournalProvider::open(directory.path(), &binding("account"))
                    .unwrap();
            assert!(
                provider
                    .journal(&binding("account"), &candidate.key)
                    .is_err()
            );
            assert_eq!(fs::read(&record_path).unwrap(), original);
        }
    }

    #[test]
    fn ref_allocation_legacy_evidence_never_authorizes_repair() {
        use sha2::{Digest, Sha256};
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let provider =
            FileRefMutationJournalProvider::create(directory.path(), &binding("account")).unwrap();
        let candidate = record();
        let slot = provider
            .journal(&binding("account"), &candidate.key)
            .unwrap();
        slot.bind(&binding("account"), &candidate).unwrap();
        let path = directory.path().join("ref-allocations");
        let current = fs::read(&path).unwrap();
        let mut legacy = b"GRAFTRA1".to_vec();
        legacy.extend_from_slice(&current[8..72]); // binding + the one digest; omit admission byte.
        legacy.extend_from_slice(&Sha256::digest(&legacy));
        fs::write(&path, legacy).unwrap();
        drop(provider);
        let provider =
            FileRefMutationJournalProvider::open(directory.path(), &binding("account")).unwrap();
        assert!(
            provider
                .journal(&binding("account"), &candidate.key)
                .unwrap()
                .load(&binding("account"))
                .unwrap()
                == Some(candidate.clone())
        );
        let name = Sha256::digest(candidate.key.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = directory.path().join(name);
        fs::remove_dir_all(&path).unwrap();
        assert!(
            provider
                .journal(&binding("account"), &candidate.key)
                .is_err()
        );
        assert!(!path.exists());
    }

    #[test]
    fn ref_journal_reopen_is_bound_immutable_and_contains_only_ciphertext() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let store = FileRefMutationJournal::create(directory.path(), &binding("account")).unwrap();
        assert!(store.load(&binding("account")).unwrap().is_none());
        let candidate = record();
        store.bind(&binding("account"), &candidate).unwrap();
        let resumed = FileRefMutationJournal::open(directory.path(), &binding("account")).unwrap();
        assert!(resumed.load(&binding("account")).unwrap() == Some(candidate.clone()));
        resumed.bind(&binding("account"), &candidate).unwrap();
        assert_eq!(
            resumed.bind(&binding("account"), &record()),
            Err(EncryptionError::StaleObservation)
        );
        assert!(FileRefMutationJournal::open(directory.path(), &binding("other")).is_err());
        let bytes = fs::read(directory.path().join("ref-mutation")).unwrap();
        assert!(
            !bytes
                .windows(b"private-journal-name".len())
                .any(|bytes| bytes == b"private-journal-name")
        );
        let secret: [u8; 32] = std::array::from_fn(|index| index as u8);
        assert!(!bytes.windows(32).any(|bytes| bytes == secret));
        fs::remove_file(directory.path().join("ref-mutation")).unwrap();
        assert!(store.load(&binding("account")).is_err());
        assert!(store.bind(&binding("account"), &candidate).is_err());
        assert!(FileRefMutationJournal::create(directory.path(), &binding("account")).is_err());
    }

    #[test]
    fn ref_journal_rejects_corruption_unsafe_links_and_busy_storage() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let store = FileRefMutationJournal::create(directory.path(), &binding("account")).unwrap();
        store.bind(&binding("account"), &record()).unwrap();
        let path = directory.path().join("ref-mutation");
        let original = fs::read(&path).unwrap();
        for index in [0, 8, 40, 80, original.len() - 1] {
            let mut bytes = original.clone();
            bytes[index] ^= 1;
            fs::write(&path, bytes).unwrap();
            assert!(store.load(&binding("account")).is_err());
        }
        fs::write(&path, original).unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.path().join("ref-mutation.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        assert!(matches!(
            store.load(&binding("account")),
            Err(EncryptionError::SessionBusy)
        ));
        drop(lock);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.load(&binding("account")).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::hard_link(&path, directory.path().join("alias")).unwrap();
        assert!(store.load(&binding("account")).is_err());
    }
}
use crate::e2ee::native_fs::{self as fs, File, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
