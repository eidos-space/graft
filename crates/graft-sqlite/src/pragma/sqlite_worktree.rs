use graft::volume_writer::VolumeWriter;
use rusqlite::{Connection, ErrorCode, OpenFlags, backup::Backup};
use std::collections::VecDeque;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(target_os = "macos")]
use std::{
    ffi::{CString, c_char, c_int},
    os::unix::ffi::OsStrExt,
};
use std::{
    fs::OpenOptions,
    io::{BufReader, BufWriter},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tempfile::TempDir;

use super::*;

const PAGE_HASH_CACHE_MAGIC: &[u8; 16] = b"graft-page-index";
const PAGE_HASH_CACHE_VERSION: u32 = 4;
const PAGE_HASH_BYTES: usize = 32;
const PAGE_HASH_CHUNK_PAGES: usize = 64;
const PAGE_HASH_CHUNK_BYTES: usize = PAGE_HASH_CHUNK_PAGES * PAGESIZE.as_usize();
const PAGE_HASH_CACHE_HEADER_BYTES: usize = PAGE_HASH_CACHE_MAGIC.len() + 12;
const PAGE_HASH_CACHE_CHECKSUM_BYTES: usize = 32;
const PAGE_SCAN_BUFFER_BYTES: usize = 4 * 1024 * 1024;
const MIN_PAGE_HASH_CACHE_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PAGE_HASH_CACHE_ENTRIES: usize = 4;
const MAX_WORKTREE_DIFF_PROBES: usize = 16;
const WORKTREE_DIFF_PROBE_VERSION: u32 = 4;
const MAX_PERSISTED_DIFF_PROBE_BYTES: u64 = 64 * 1024;
const PAGE_OWNERSHIP_CACHE_VERSION: u32 = 1;
const MAX_PAGE_OWNERSHIP_CACHE_BYTES: u64 = 16 * 1024 * 1024;
const SQLITE_FILE_CHANGE_COUNTER_OFFSET: usize = 24;
const SQLITE_VERSION_VALID_FOR_OFFSET: usize = 92;
const SQLITE_LIBRARY_VERSION_OFFSET: usize = 96;
const SQLITE_VOLATILE_HEADER_FIELD_BYTES: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct WorktreeFileFingerprint {
    len: u64,
    modified_nanos: Option<u128>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    ctime_seconds: i64,
    #[cfg(unix)]
    ctime_nanos: i64,
}

#[derive(Clone, PartialEq, Eq)]
struct WorktreeDiffProbeIdentity {
    path: PathBuf,
    expected_index: PathBuf,
    fingerprint: WorktreeFileFingerprint,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct WorktreeDiffProbe {
    matches: bool,
    table_candidates: Option<BTreeSet<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SqlitePageOwnershipRange {
    first_page: u32,
    last_page: u32,
    table: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SqlitePageOwnershipIndex {
    page_count: u32,
    ranges: Vec<SqlitePageOwnershipRange>,
}

#[derive(Serialize, Deserialize)]
struct PersistedSqlitePageOwnershipPayload {
    version: u32,
    index: SqlitePageOwnershipIndex,
}

#[derive(Serialize, Deserialize)]
struct PersistedSqlitePageOwnership {
    payload: PersistedSqlitePageOwnershipPayload,
    checksum: String,
}

impl SqlitePageOwnershipIndex {
    fn table_for_page(&self, page_number: u32) -> Option<&str> {
        let index = self
            .ranges
            .partition_point(|range| range.last_page < page_number);
        self.ranges.get(index).and_then(|range| {
            (range.first_page <= page_number && page_number <= range.last_page)
                .then_some(range.table.as_str())
        })
    }
}

#[derive(Serialize, Deserialize)]
struct PersistedWorktreeDiffProbePayload {
    version: u32,
    fingerprint: WorktreeFileFingerprint,
    probe: WorktreeDiffProbe,
}

#[derive(Serialize, Deserialize)]
struct PersistedWorktreeDiffProbe {
    payload: PersistedWorktreeDiffProbePayload,
    checksum: String,
}

static WORKTREE_DIFF_PROBES: OnceLock<
    Mutex<VecDeque<(WorktreeDiffProbeIdentity, WorktreeDiffProbe)>>,
> = OnceLock::new();

fn worktree_diff_probe_identity(
    path: &Path,
    cache: &SqlitePageHashCache,
    expected: &CommitFileState,
) -> Result<WorktreeDiffProbeIdentity, ErrCtx> {
    let metadata = std::fs::metadata(path)?;
    Ok(WorktreeDiffProbeIdentity {
        path: path.to_path_buf(),
        expected_index: cache.path_for_state(expected)?,
        fingerprint: WorktreeFileFingerprint {
            len: metadata.len(),
            modified_nanos: metadata.modified().ok().and_then(system_time_nanos),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            ctime_seconds: metadata.ctime(),
            #[cfg(unix)]
            ctime_nanos: metadata.ctime_nsec(),
        },
    })
}

fn system_time_nanos(time: SystemTime) -> Option<u128> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|value| value.as_nanos())
}

fn load_worktree_diff_probe(identity: &WorktreeDiffProbeIdentity) -> Option<WorktreeDiffProbe> {
    WORKTREE_DIFF_PROBES
        .get_or_init(|| Mutex::new(VecDeque::new()))
        .lock()
        .iter()
        .rev()
        .find(|(candidate, _)| candidate == identity)
        .map(|(_, probe)| probe.clone())
}

fn store_worktree_diff_probe(identity: WorktreeDiffProbeIdentity, probe: WorktreeDiffProbe) {
    let mut probes = WORKTREE_DIFF_PROBES
        .get_or_init(|| Mutex::new(VecDeque::new()))
        .lock();
    probes.retain(|(candidate, _)| candidate != &identity);
    probes.push_back((identity, probe));
    while probes.len() > MAX_WORKTREE_DIFF_PROBES {
        probes.pop_front();
    }
}

/// A content-addressed page index for one repository `SQLite` path.
///
/// The file name binds the index to an exact `CommitFileState`. The body is checksummed before any
/// page hashes are trusted, so a missing, stale, truncated, or corrupted cache only disables the
/// optimization and falls back to the authoritative page comparison.
struct SqlitePageHashCache {
    directory: PathBuf,
}

impl SqlitePageHashCache {
    fn new(repo: &Repository, key: &str) -> Self {
        let key_hash = blake3::hash(key.as_bytes()).to_hex();
        Self {
            directory: repo
                .graft_dir()
                .join("cache")
                .join("sqlite-pages")
                .join(key_hash.as_str()),
        }
    }

    fn path_for_state(&self, state: &CommitFileState) -> Result<PathBuf, ErrCtx> {
        let state_hash = sqlite_page_index_state_hash(state)?;
        Ok(self.directory.join(format!("pages-v4-{state_hash}.bin")))
    }

    fn probe_path_for_state(&self, state: &CommitFileState) -> Result<PathBuf, ErrCtx> {
        let state_hash = sqlite_page_index_state_hash(state)?;
        Ok(self
            .directory
            .join(format!("worktree-probe-v4-{state_hash}.json")))
    }

    fn ownership_path_for_state(&self, state: &CommitFileState) -> Result<PathBuf, ErrCtx> {
        let state_hash = sqlite_page_index_state_hash(state)?;
        Ok(self
            .directory
            .join(format!("page-ownership-v1-{state_hash}.json")))
    }

    fn load_ownership(
        &self,
        state: &CommitFileState,
    ) -> Result<Option<SqlitePageOwnershipIndex>, ErrCtx> {
        let path = self.ownership_path_for_state(state)?;
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) if metadata.len() <= MAX_PAGE_OWNERSHIP_CACHE_BYTES => metadata,
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        debug_assert!(metadata.len() <= MAX_PAGE_OWNERSHIP_CACHE_BYTES);
        let persisted: PersistedSqlitePageOwnership = serde_json::from_slice(&std::fs::read(path)?)
            .map_err(|error| {
                ErrCtx::InvalidCommand(
                    format!("failed to decode persisted SQLite page ownership: {error}").into(),
                )
            })?;
        let payload_bytes = serde_json::to_vec(&persisted.payload).map_err(|error| {
            ErrCtx::InvalidCommand(
                format!("failed to verify persisted SQLite page ownership: {error}").into(),
            )
        })?;
        if persisted.payload.version != PAGE_OWNERSHIP_CACHE_VERSION
            || persisted.payload.index.page_count != state.snapshot.page_count.to_u32()
            || blake3::hash(&payload_bytes).to_hex().as_str() != persisted.checksum
        {
            return Ok(None);
        }
        Ok(Some(persisted.payload.index))
    }

    fn load_probe(
        &self,
        state: &CommitFileState,
        fingerprint: &WorktreeFileFingerprint,
    ) -> Result<Option<WorktreeDiffProbe>, ErrCtx> {
        let path = self.probe_path_for_state(state)?;
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) if metadata.len() <= MAX_PERSISTED_DIFF_PROBE_BYTES => metadata,
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        debug_assert!(metadata.len() <= MAX_PERSISTED_DIFF_PROBE_BYTES);
        let persisted: PersistedWorktreeDiffProbe = serde_json::from_slice(&std::fs::read(path)?)
            .map_err(|error| {
            ErrCtx::InvalidCommand(
                format!("failed to decode persisted SQLite worktree probe: {error}").into(),
            )
        })?;
        let payload_bytes = serde_json::to_vec(&persisted.payload).map_err(|error| {
            ErrCtx::InvalidCommand(
                format!("failed to verify persisted SQLite worktree probe: {error}").into(),
            )
        })?;
        if persisted.payload.version != WORKTREE_DIFF_PROBE_VERSION
            || &persisted.payload.fingerprint != fingerprint
            || blake3::hash(&payload_bytes).to_hex().as_str() != persisted.checksum
        {
            return Ok(None);
        }
        Ok(Some(persisted.payload.probe))
    }

    fn store_probe(
        &self,
        state: &CommitFileState,
        fingerprint: &WorktreeFileFingerprint,
        probe: &WorktreeDiffProbe,
    ) -> Result<(), ErrCtx> {
        std::fs::create_dir_all(&self.directory)?;
        let payload = PersistedWorktreeDiffProbePayload {
            version: WORKTREE_DIFF_PROBE_VERSION,
            fingerprint: fingerprint.clone(),
            probe: probe.clone(),
        };
        let payload_bytes = serde_json::to_vec(&payload).map_err(|error| {
            ErrCtx::InvalidCommand(
                format!("failed to encode persisted SQLite worktree probe: {error}").into(),
            )
        })?;
        let persisted = PersistedWorktreeDiffProbe {
            payload,
            checksum: blake3::hash(&payload_bytes).to_hex().to_string(),
        };
        let bytes = serde_json::to_vec(&persisted).map_err(|error| {
            ErrCtx::InvalidCommand(
                format!("failed to encode persisted SQLite worktree probe: {error}").into(),
            )
        })?;
        let final_path = self.probe_path_for_state(state)?;
        write_atomic_cache_file(&self.directory, &final_path, "worktree-probe-v4", &bytes)
    }

    fn store_ownership(
        &self,
        state: &CommitFileState,
        index: SqlitePageOwnershipIndex,
    ) -> Result<(), ErrCtx> {
        std::fs::create_dir_all(&self.directory)?;
        let payload = PersistedSqlitePageOwnershipPayload {
            version: PAGE_OWNERSHIP_CACHE_VERSION,
            index,
        };
        let payload_bytes = serde_json::to_vec(&payload).map_err(|error| {
            ErrCtx::InvalidCommand(
                format!("failed to encode persisted SQLite page ownership: {error}").into(),
            )
        })?;
        let persisted = PersistedSqlitePageOwnership {
            payload,
            checksum: blake3::hash(&payload_bytes).to_hex().to_string(),
        };
        let bytes = serde_json::to_vec(&persisted).map_err(|error| {
            ErrCtx::InvalidCommand(
                format!("failed to encode persisted SQLite page ownership: {error}").into(),
            )
        })?;
        let final_path = self.ownership_path_for_state(state)?;
        write_atomic_cache_file(&self.directory, &final_path, "page-ownership-v1", &bytes)
    }

    fn load(&self, state: &CommitFileState) -> Result<Option<Vec<[u8; PAGE_HASH_BYTES]>>, ErrCtx> {
        let path = self.path_for_state(state)?;
        let expected_page_count = state.snapshot.page_count.to_u32() as usize;
        let expected_hash_count = page_hash_chunk_count(expected_page_count);
        let expected_len = PAGE_HASH_CACHE_HEADER_BYTES
            .checked_add(
                expected_hash_count
                    .checked_mul(PAGE_HASH_BYTES)
                    .ok_or_else(page_hash_cache_size_error)?,
            )
            .and_then(|value| value.checked_add(PAGE_HASH_CACHE_CHECKSUM_BYTES))
            .ok_or_else(page_hash_cache_size_error)?;
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) if metadata.len() == expected_len as u64 => metadata,
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        debug_assert_eq!(metadata.len(), expected_len as u64);
        let bytes = std::fs::read(path)?;
        if bytes.len() != expected_len
            || &bytes[..PAGE_HASH_CACHE_MAGIC.len()] != PAGE_HASH_CACHE_MAGIC
        {
            return Ok(None);
        }
        let version_offset = PAGE_HASH_CACHE_MAGIC.len();
        let version = u32::from_le_bytes(
            bytes[version_offset..version_offset + 4]
                .try_into()
                .expect("page-index version has a fixed width"),
        );
        let page_count = u32::from_le_bytes(
            bytes[version_offset + 4..version_offset + 8]
                .try_into()
                .expect("page-index count has a fixed width"),
        );
        let chunk_pages = u32::from_le_bytes(
            bytes[version_offset + 8..PAGE_HASH_CACHE_HEADER_BYTES]
                .try_into()
                .expect("page-index chunk size has a fixed width"),
        );
        if version != PAGE_HASH_CACHE_VERSION
            || page_count as usize != expected_page_count
            || chunk_pages as usize != PAGE_HASH_CHUNK_PAGES
        {
            return Ok(None);
        }
        let checksum_offset = bytes.len() - PAGE_HASH_CACHE_CHECKSUM_BYTES;
        let actual_checksum = blake3::hash(&bytes[..checksum_offset]);
        if actual_checksum.as_bytes() != &bytes[checksum_offset..] {
            return Ok(None);
        }
        let mut hashes = Vec::with_capacity(expected_hash_count);
        for hash in
            bytes[PAGE_HASH_CACHE_HEADER_BYTES..checksum_offset].chunks_exact(PAGE_HASH_BYTES)
        {
            hashes.push(
                hash.try_into()
                    .expect("validated page-index hashes have a fixed width"),
            );
        }
        Ok(Some(hashes))
    }

    fn store(
        &self,
        state: &CommitFileState,
        hashes: &[[u8; PAGE_HASH_BYTES]],
    ) -> Result<(), ErrCtx> {
        let page_count = state.snapshot.page_count.to_u32() as usize;
        if hashes.len() != page_hash_chunk_count(page_count) {
            return Err(page_hash_cache_size_error());
        }
        std::fs::create_dir_all(&self.directory)?;
        let final_path = self.path_for_state(state)?;
        let temp_path = self.directory.join(format!(
            ".pages-v4-{}-{}.tmp",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ));
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)?;
        let mut writer = BufWriter::new(file);
        let mut checksum = blake3::Hasher::new();
        write_page_hash_cache_bytes(&mut writer, &mut checksum, PAGE_HASH_CACHE_MAGIC)?;
        write_page_hash_cache_bytes(
            &mut writer,
            &mut checksum,
            &PAGE_HASH_CACHE_VERSION.to_le_bytes(),
        )?;
        write_page_hash_cache_bytes(
            &mut writer,
            &mut checksum,
            &(page_count as u32).to_le_bytes(),
        )?;
        write_page_hash_cache_bytes(
            &mut writer,
            &mut checksum,
            &(PAGE_HASH_CHUNK_PAGES as u32).to_le_bytes(),
        )?;
        for hash in hashes {
            write_page_hash_cache_bytes(&mut writer, &mut checksum, hash)?;
        }
        writer.write_all(checksum.finalize().as_bytes())?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);
        if let Err(error) = std::fs::rename(&temp_path, &final_path) {
            let _ = std::fs::remove_file(&temp_path);
            if !final_path.exists() {
                return Err(error.into());
            }
        }
        prune_page_hash_cache(&self.directory, &final_path);
        Ok(())
    }
}

fn write_atomic_cache_file(
    directory: &Path,
    final_path: &Path,
    prefix: &str,
    bytes: &[u8],
) -> Result<(), ErrCtx> {
    let temp_path = directory.join(format!(
        ".{prefix}-{}-{}.tmp",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
    ));
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)?;
    let mut writer = BufWriter::new(file);
    writer.write_all(bytes)?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    if let Err(error) = std::fs::rename(&temp_path, final_path) {
        let _ = std::fs::remove_file(&temp_path);
        if !final_path.exists() {
            return Err(error.into());
        }
    }
    Ok(())
}

fn sqlite_page_index_state_hash(state: &CommitFileState) -> Result<blake3::Hash, ErrCtx> {
    let encoded = serde_json::to_vec(state).map_err(|error| {
        ErrCtx::InvalidCommand(format!("failed to encode SQLite page-index state: {error}").into())
    })?;
    Ok(blake3::hash(&encoded))
}

fn page_hash_cache_size_error() -> ErrCtx {
    ErrCtx::InvalidCommand("SQLite page-index size exceeds supported limits".into())
}

fn write_page_hash_cache_bytes(
    writer: &mut BufWriter<File>,
    checksum: &mut blake3::Hasher,
    bytes: &[u8],
) -> Result<(), ErrCtx> {
    writer.write_all(bytes)?;
    checksum.update(bytes);
    Ok(())
}

fn prune_page_hash_cache(directory: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut indexes = entries
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with("pages-v") && name.ends_with(".bin")
            })
        })
        .collect::<Vec<_>>();
    indexes.sort_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH)
    });
    let remove_count = indexes.len().saturating_sub(MAX_PAGE_HASH_CACHE_ENTRIES);
    for path in indexes.into_iter().take(remove_count) {
        if path != keep {
            if let Some(state_hash) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix("pages-v4-"))
                .and_then(|name| name.strip_suffix(".bin"))
            {
                for cache_name in [
                    format!("worktree-probe-v2-{state_hash}.json"),
                    format!("worktree-probe-v3-{state_hash}.json"),
                    format!("worktree-probe-v4-{state_hash}.json"),
                    format!("page-ownership-v1-{state_hash}.json"),
                ] {
                    let _ = std::fs::remove_file(directory.join(cache_name));
                }
            }
            let _ = std::fs::remove_file(path);
        }
    }
}

fn page_hash_chunk_count(page_count: usize) -> usize {
    page_count.div_ceil(PAGE_HASH_CHUNK_PAGES)
}

/// Hash `SQLite` pages using stable database-content semantics.
///
/// `SQLite`'s online backup rewrites cache-invalidation counters and the version number of the
/// `SQLite` library that last wrote page 1. They are not database content, so excluding them keeps
/// page indexes portable between rollback-journal and online-backup snapshots created on different
/// platforms.
fn sqlite_page_chunk_hash(first_page: u32, bytes: &[u8]) -> blake3::Hash {
    if first_page != 1 {
        return blake3::hash(bytes);
    }

    let change_counter_end = SQLITE_FILE_CHANGE_COUNTER_OFFSET + SQLITE_VOLATILE_HEADER_FIELD_BYTES;
    let library_version_end = SQLITE_LIBRARY_VERSION_OFFSET + SQLITE_VOLATILE_HEADER_FIELD_BYTES;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&bytes[..SQLITE_FILE_CHANGE_COUNTER_OFFSET]);
    hasher.update(&bytes[change_counter_end..SQLITE_VERSION_VALID_FOR_OFFSET]);
    hasher.update(&bytes[library_version_end..]);
    hasher.finalize()
}

fn sqlite_page_bytes_equal(page_number: u32, left: &[u8], right: &[u8]) -> bool {
    if page_number != 1 {
        return left == right;
    }

    let change_counter_end = SQLITE_FILE_CHANGE_COUNTER_OFFSET + SQLITE_VOLATILE_HEADER_FIELD_BYTES;
    let library_version_end = SQLITE_LIBRARY_VERSION_OFFSET + SQLITE_VOLATILE_HEADER_FIELD_BYTES;
    left[..SQLITE_FILE_CHANGE_COUNTER_OFFSET] == right[..SQLITE_FILE_CHANGE_COUNTER_OFFSET]
        && left[change_counter_end..SQLITE_VERSION_VALID_FOR_OFFSET]
            == right[change_counter_end..SQLITE_VERSION_VALID_FOR_OFFSET]
        && left[library_version_end..] == right[library_version_end..]
}

/// A stable page reader for a physical `SQLite` worktree file.
///
/// This module is the data-plane boundary between repository operations and `SQLite`. Repository
/// commands must not read physical `SQLite` pages or manipulate Graft volumes directly.
pub(crate) struct PhysicalSqliteReader {
    input: Mutex<File>,
    path: PathBuf,
    snapshot_path: PathBuf,
    snapshot: graft::snapshot::Snapshot,
    _snapshot_dir: Option<TempDir>,
}

struct LockedPhysicalSqliteReader {
    physical: PhysicalSqliteReader,
    _guard: Connection,
}

impl LockedPhysicalSqliteReader {
    /// Reads the live rollback-journal database under one `SQLite` shared lock.
    ///
    /// This path is only used after an exact page-index hit, so the bounded sequential scan and
    /// table mapping finish quickly. Cache misses retain the cloned snapshot path and never hold a
    /// worktree lock during their authoritative full comparison.
    fn open(path: &Path) -> Result<Option<Self>, ErrCtx> {
        validate_sqlite_source(path)?;
        let source = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        source.busy_timeout(Duration::from_secs(5))?;
        let journal_mode: String =
            source.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("delete") {
            return Ok(None);
        }
        source.execute_batch("BEGIN")?;
        source.query_row("SELECT count(*) FROM sqlite_schema", [], |_| Ok(()))?;
        let physical = PhysicalSqliteReader::open_snapshot(path, path, None)?;
        Ok(Some(Self { physical, _guard: source }))
    }
}

/// Runs a bounded read against a consistent physical `SQLite` image.
///
/// Rollback-journal databases can be read directly under a shared transaction, which avoids
/// copying an entire large worktree merely to inspect one table. WAL databases retain the online
/// backup fallback because their committed image may span the database and WAL sidecar.
pub(super) fn with_consistent_physical_sqlite_reader<T>(
    path: &Path,
    read: impl FnOnce(&PhysicalSqliteReader) -> Result<T, ErrCtx>,
) -> Result<T, ErrCtx> {
    if let Some(locked) = LockedPhysicalSqliteReader::open(path)? {
        return read(&locked.physical);
    }
    let snapshot = PhysicalSqliteReader::open(path)?;
    read(&snapshot)
}

/// The stable `SQLite` image and page-set produced by one staging operation.
///
/// Keeping this alive until commit mirrors Git's index semantics: the staged snapshot, not the
/// possibly newer worktree, is the source of truth for the commit. The captured changed-page set
/// also avoids repeating the full physical/staged/previous comparison during summary generation.
pub(crate) struct PreparedSqliteStage {
    physical: Option<PhysicalSqliteReader>,
    prepared_table_candidates: Option<Option<BTreeSet<String>>>,
    previous: Option<CommitFileState>,
    staged: CommitFileState,
    changed_pages: BTreeSet<u32>,
    page_hash_cache_hit: bool,
}

impl PreparedSqliteStage {
    pub(crate) fn matches(&self, previous: &CommitFileState, staged: &CommitFileState) -> bool {
        self.previous.as_ref() == Some(previous) && &self.staged == staged
    }

    pub(crate) fn table_candidates(&self) -> Result<Option<BTreeSet<String>>, ErrCtx> {
        if let Some(candidates) = &self.prepared_table_candidates {
            return Ok(candidates.clone());
        }
        self.physical
            .as_ref()
            .expect("deferred table candidates retain their stable SQLite snapshot")
            .table_candidates_for_changed_pages(&self.changed_pages, None, None)
    }

    pub(crate) fn page_hash_cache_hit(&self) -> bool {
        self.page_hash_cache_hit
    }

    pub(crate) fn changed_page_count(&self) -> usize {
        self.changed_pages.len()
    }
}

impl PhysicalSqliteReader {
    pub(crate) fn open(path: &Path) -> Result<Self, ErrCtx> {
        validate_sqlite_source(path)?;

        let snapshot_dir = tempfile::Builder::new()
            .prefix("graft-sqlite-snapshot-")
            .tempdir()?;
        let snapshot_path = snapshot_dir.path().join("snapshot.sqlite");
        backup_sqlite_source(path, &snapshot_path)?;

        Self::open_snapshot(path, &snapshot_path, Some(snapshot_dir))
    }

    /// Opens a database whose writer has already been closed and whose bytes are therefore stable.
    ///
    /// Unlike `SQLite`'s online backup, this preserves page-1 change counters. That matters when an
    /// internally generated merge result is rebound to an already-open VFS connection.
    fn open_stable(path: &Path) -> Result<Self, ErrCtx> {
        validate_sqlite_source(path)?;
        Self::open_snapshot(path, path, None)
    }

    fn open_snapshot(
        path: &Path,
        snapshot_path: &Path,
        snapshot_dir: Option<TempDir>,
    ) -> Result<Self, ErrCtx> {
        let metadata = std::fs::symlink_metadata(snapshot_path)?;
        let mut input = File::open(snapshot_path)?;
        let mut header = [0_u8; 100];
        input.read_exact(&mut header)?;
        validate_sqlite_header(path, &header)?;

        let page_size = PAGESIZE.as_usize();
        if metadata.len() % page_size as u64 != 0 {
            return Err(ErrCtx::InvalidCommand(
                format!(
                    "SQLite database `{}` is not an even multiple of {page_size} bytes",
                    path.display()
                )
                .into(),
            ));
        }

        let page_count = metadata.len() / page_size as u64;
        let page_count = u32::try_from(page_count).map_err(|_| {
            ErrCtx::InvalidCommand(
                format!("SQLite database `{}` has too many pages", path.display()).into(),
            )
        })?;
        let mut snapshot = graft::snapshot::Snapshot::empty();
        snapshot.page_count = PageCount::new(page_count);
        Ok(Self {
            input: Mutex::new(input),
            path: path.to_path_buf(),
            snapshot_path: snapshot_path.to_path_buf(),
            snapshot,
            _snapshot_dir: snapshot_dir,
        })
    }

    pub(super) fn worktree_state(&self) -> RepoWorktreeFileState {
        RepoWorktreeFileState { page_count: self.page_count() }
    }

    pub(crate) fn matches_reader(&self, other: &dyn VolumeRead) -> Result<bool, ErrCtx> {
        if self.page_count() != other.page_count() {
            return Ok(false);
        }
        for page_number in 1..=self.page_count().to_u32() {
            graft::repo::cancellation_checkpoint()?;
            let pageidx = PageIdx::try_from(page_number).map_err(|error| {
                ErrCtx::InvalidCommand(
                    format!("invalid SQLite page index {page_number}: {error}").into(),
                )
            })?;
            if !sqlite_page_bytes_equal(
                page_number,
                self.read_page(pageidx)?.as_ref(),
                other.read_page(pageidx)?.as_ref(),
            ) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn visit_page_chunks(
        &self,
        mut visitor: impl FnMut(u32, &[u8]) -> Result<(), ErrCtx>,
    ) -> Result<(), ErrCtx> {
        let mut input = self.input.lock();
        input.seek(SeekFrom::Start(0))?;
        let mut reader = BufReader::with_capacity(PAGE_SCAN_BUFFER_BYTES, &mut *input);
        let mut chunk = vec![0_u8; PAGE_HASH_CHUNK_BYTES];
        let mut first_page = 1_u32;
        let page_count = self.page_count().to_u32();
        while first_page <= page_count {
            graft::repo::cancellation_checkpoint()?;
            let pages_remaining = (page_count - first_page + 1) as usize;
            let chunk_pages = pages_remaining.min(PAGE_HASH_CHUNK_PAGES);
            let chunk_bytes = chunk_pages * PAGESIZE.as_usize();
            reader.read_exact(&mut chunk[..chunk_bytes])?;
            visitor(first_page, &chunk[..chunk_bytes])?;
            first_page += chunk_pages as u32;
        }
        Ok(())
    }

    /// Finds changed table candidates using the committed page index when it is available.
    ///
    /// This mirrors Git's index/stat fast path: hash the worktree sequentially in coarse chunks,
    /// then compare individual pages only inside chunks whose content hash changed. Falling back
    /// to the authoritative page-by-page comparison preserves correctness when the index is
    /// missing or unreadable.
    pub(super) fn cached_changed_table_candidates(
        &self,
        runtime: &Runtime,
        repo: &Repository,
        key: &str,
        expected_state: &CommitFileState,
        expected: &dyn VolumeRead,
    ) -> Result<Option<BTreeSet<String>>, ErrCtx> {
        let probe = self.cached_diff_probe(runtime, repo, key, expected_state, expected)?;
        Ok(probe.table_candidates)
    }

    /// Reuses the physical worktree as a fast staged-snapshot reader only when every byte still
    /// matches the staged state. While validating that invariant, collect pages changed from the
    /// previous commit so checkpoint summaries can avoid scanning unrelated large tables.
    pub(super) fn staged_table_candidates(
        &self,
        staged: &dyn VolumeRead,
        previous: &dyn VolumeRead,
    ) -> Result<Option<BTreeSet<String>>, ErrCtx> {
        if self.page_count() != staged.page_count() {
            return Ok(None);
        }

        let max_page_count = self
            .page_count()
            .to_u32()
            .max(previous.page_count().to_u32());
        let mut changed_pages = BTreeSet::new();
        for page_number in 1..=max_page_count {
            if page_number.is_multiple_of(1_024) {
                graft::repo::cancellation_checkpoint()?;
            }
            let pageidx = PageIdx::try_from(page_number).map_err(|error| {
                ErrCtx::InvalidCommand(
                    format!("invalid SQLite page index {page_number}: {error}").into(),
                )
            })?;
            let physical_page = self.read_page(pageidx)?;
            if page_number <= self.page_count().to_u32()
                && !sqlite_page_bytes_equal(
                    page_number,
                    physical_page.as_ref(),
                    staged.read_page(pageidx)?.as_ref(),
                )
            {
                return Ok(None);
            }
            if !sqlite_page_bytes_equal(
                page_number,
                physical_page.as_ref(),
                previous.read_page(pageidx)?.as_ref(),
            ) {
                changed_pages.insert(page_number);
            }
        }
        self.table_candidates_for_changed_pages(&changed_pages, Some(previous), None)
    }

    fn table_candidates_for_changed_pages(
        &self,
        changed_pages: &BTreeSet<u32>,
        expected: Option<&dyn VolumeRead>,
        expected_ownership: Option<&SqlitePageOwnershipIndex>,
    ) -> Result<Option<BTreeSet<String>>, ErrCtx> {
        if changed_pages.is_empty() {
            return Ok(Some(BTreeSet::new()));
        }
        let expected_tables = expected_ownership.map(|index| {
            changed_pages
                .iter()
                .filter_map(|page| index.table_for_page(*page).map(str::to_owned))
                .collect::<BTreeSet<_>>()
        });
        let Some(current_owners) =
            self.dbstat_page_owners(Some(changed_pages), expected_tables.as_ref())?
        else {
            return Ok(None);
        };
        let current_freelist = sqlite_freelist_pages(self)?;
        let expected_freelist = expected.map(sqlite_freelist_pages).transpose()?.flatten();
        let mut tables = BTreeSet::new();
        for page_number in changed_pages {
            let current_owner = current_owners.get(page_number);
            let expected_owner =
                expected_ownership.and_then(|index| index.table_for_page(*page_number));
            if let Some(table) = current_owner {
                tables.insert(table.clone());
            }
            if let Some(table) = expected_owner {
                tables.insert(table.to_string());
            }
            if *page_number == 1 {
                continue;
            }
            let absent_now = *page_number > self.page_count().to_u32();
            let absent_before =
                expected.is_some_and(|reader| *page_number > reader.page_count().to_u32());
            let free_now = absent_now
                || current_freelist
                    .as_ref()
                    .is_some_and(|pages| pages.contains(page_number));
            let free_before = absent_before
                || expected_freelist
                    .as_ref()
                    .is_some_and(|pages| pages.contains(page_number));
            if expected_ownership.is_some() {
                let schema_changed = changed_pages.contains(&1);
                let schema_owned_allocation = schema_changed && free_before;
                if (current_owner.is_none() && !free_now && !schema_owned_allocation)
                    || (expected_owner.is_none() && !free_before)
                {
                    return Ok(None);
                }
                continue;
            }
            let free_on_both_sides = current_freelist
                .as_ref()
                .is_some_and(|pages| pages.contains(page_number))
                && (absent_before
                    || expected_freelist
                        .as_ref()
                        .is_some_and(|pages| pages.contains(page_number)));
            if current_owner.is_none() && !free_on_both_sides {
                return Ok(None);
            }
        }
        Ok(Some(tables))
    }

    fn dbstat_page_owners(
        &self,
        filter: Option<&BTreeSet<u32>>,
        tables: Option<&BTreeSet<String>>,
    ) -> Result<Option<BTreeMap<u32, String>>, ErrCtx> {
        let connection = Connection::open_with_flags(
            &self.snapshot_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        if let Some(tables) = tables {
            return dbstat_page_owners_for_tables(&connection, filter, tables);
        }
        let mut statement = match connection.prepare(
            "SELECT d.pageno, COALESCE(m.tbl_name, d.name) \
             FROM dbstat AS d \
             LEFT JOIN sqlite_schema AS m ON m.name = d.name",
        ) {
            Ok(statement) => statement,
            Err(_) => return Ok(None),
        };
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut owners = BTreeMap::new();
        for row in rows {
            let (page_number, table_name) = row?;
            if filter.is_some_and(|pages| !pages.contains(&page_number))
                || table_name.starts_with("sqlite_")
            {
                continue;
            }
            owners.insert(page_number, table_name);
        }
        Ok(Some(owners))
    }

    fn page_ownership_index(&self) -> Result<Option<SqlitePageOwnershipIndex>, ErrCtx> {
        let Some(owners) = self.dbstat_page_owners(None, None)? else {
            return Ok(None);
        };
        Ok(Some(SqlitePageOwnershipIndex {
            page_count: self.page_count().to_u32(),
            ranges: compress_page_ownership_ranges(owners),
        }))
    }

    pub(super) fn matches_state(
        &self,
        runtime: &Runtime,
        expected: &CommitFileState,
    ) -> Result<bool, ErrCtx> {
        if self.page_count() != expected.snapshot.page_count {
            return Ok(false);
        }

        let stored = runtime.snapshot_reader(expected.snapshot.to_snapshot());
        for page_number in 1..=self.page_count().to_u32() {
            graft::repo::cancellation_checkpoint()?;
            let pageidx = PageIdx::try_from(page_number).map_err(|err| {
                ErrCtx::InvalidCommand(
                    format!("invalid SQLite page index {page_number}: {err}").into(),
                )
            })?;
            if !sqlite_page_bytes_equal(
                page_number,
                self.read_page(pageidx)?.as_ref(),
                stored.read_page(pageidx)?.as_ref(),
            ) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Verifies the worktree against an exact committed state using its persisted page index.
    ///
    /// A cache hit turns the comparison into one sequential physical read and avoids both a
    /// worktree clone and random reads from the immutable Graft snapshot. The page hashes are
    /// content-addressed by `expected`; any cache miss or validation failure uses the authoritative
    /// page comparison.
    pub(super) fn matches_cached_state(
        &self,
        runtime: &Runtime,
        repo: &Repository,
        key: &str,
        expected: &CommitFileState,
    ) -> Result<bool, ErrCtx> {
        let expected_reader = runtime.snapshot_reader(expected.snapshot.to_snapshot());
        self.cached_diff_probe(runtime, repo, key, expected, &expected_reader)
            .map(|probe| probe.matches)
    }

    /// Compares against an exact persisted page index without falling back to random snapshot
    /// reads. `None` means the disposable index is unavailable and the caller should use its
    /// authoritative non-cache path.
    fn matches_indexed_state(
        &self,
        repo: &Repository,
        key: &str,
        expected: &CommitFileState,
    ) -> Result<Option<bool>, ErrCtx> {
        if self.page_count() != expected.snapshot.page_count {
            return Ok(Some(false));
        }
        let cache = SqlitePageHashCache::new(repo, key);
        let Some(expected_hashes) = cache.load(expected)? else {
            return Ok(None);
        };
        let mut matches = true;
        self.visit_page_chunks(|first_page, chunk_bytes| {
            let chunk_index = (first_page - 1) as usize / PAGE_HASH_CHUNK_PAGES;
            matches &= expected_hashes
                .get(chunk_index)
                .is_some_and(|expected_hash| {
                    expected_hash == sqlite_page_chunk_hash(first_page, chunk_bytes).as_bytes()
                });
            Ok(())
        })?;
        Ok(Some(matches))
    }

    fn cached_diff_probe(
        &self,
        runtime: &Runtime,
        repo: &Repository,
        key: &str,
        expected_state: &CommitFileState,
        expected: &dyn VolumeRead,
    ) -> Result<WorktreeDiffProbe, ErrCtx> {
        if self.snapshot_path != self.path {
            return Ok(WorktreeDiffProbe {
                matches: self.matches_state(runtime, expected_state)?,
                table_candidates: None,
            });
        }
        let cache = SqlitePageHashCache::new(repo, key);
        let identity = worktree_diff_probe_identity(&self.path, &cache, expected_state)?;
        if let Some(probe) = load_worktree_diff_probe(&identity) {
            return Ok(probe);
        }
        match cache.load_probe(expected_state, &identity.fingerprint) {
            Ok(Some(probe)) => {
                store_worktree_diff_probe(identity, probe.clone());
                return Ok(probe);
            }
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(
                    ?error,
                    "ignoring unreadable persisted SQLite worktree probe"
                );
            }
        }
        let cached_hashes = match cache.load(expected_state) {
            Ok(hashes) => hashes,
            Err(error) => {
                tracing::debug!(?error, "ignoring unreadable SQLite page-index cache");
                None
            }
        };

        let mut changed_pages = BTreeSet::new();
        self.visit_page_chunks(|first_page, chunk_bytes| {
            let chunk_index = (first_page - 1) as usize / PAGE_HASH_CHUNK_PAGES;
            let current_hash = sqlite_page_chunk_hash(first_page, chunk_bytes);
            if cached_hashes
                .as_ref()
                .and_then(|hashes| hashes.get(chunk_index))
                .is_some_and(|expected_hash| expected_hash == current_hash.as_bytes())
            {
                return Ok(());
            }

            for (page_offset, page_bytes) in
                chunk_bytes.chunks_exact(PAGESIZE.as_usize()).enumerate()
            {
                let page_number = first_page + page_offset as u32;
                let pageidx = PageIdx::try_from(page_number).map_err(|error| {
                    ErrCtx::InvalidCommand(
                        format!("invalid SQLite page index {page_number}: {error}").into(),
                    )
                })?;
                let unchanged = expected.page_count().contains(pageidx)
                    && sqlite_page_bytes_equal(
                        page_number,
                        expected.read_page(pageidx)?.as_ref(),
                        page_bytes,
                    );
                if !unchanged {
                    changed_pages.insert(page_number);
                }
            }
            Ok(())
        })?;
        for page_number in (self.page_count().to_u32() + 1)..=expected.page_count().to_u32() {
            changed_pages.insert(page_number);
        }
        let expected_ownership = match cache.load_ownership(expected_state) {
            Ok(index) => index,
            Err(error) => {
                tracing::debug!(?error, "ignoring unreadable SQLite page-ownership cache");
                None
            }
        };
        let table_candidates = self.table_candidates_for_changed_pages(
            &changed_pages,
            Some(expected),
            expected_ownership.as_ref(),
        )?;
        tracing::debug!(
            changed_pages = changed_pages.len(),
            page_ownership_cache_hit = expected_ownership.is_some(),
            candidate_resolution = if table_candidates.is_some() {
                "exact"
            } else {
                "fallback"
            },
            candidate_tables = ?table_candidates,
            "classified SQLite worktree page changes"
        );
        let probe = WorktreeDiffProbe {
            matches: changed_pages.is_empty(),
            table_candidates,
        };
        if let Err(error) = cache.store_probe(expected_state, &identity.fingerprint, &probe) {
            tracing::debug!(?error, "failed to persist SQLite worktree probe");
        }
        store_worktree_diff_probe(identity, probe.clone());
        Ok(probe)
    }
}

fn dbstat_page_owners_for_tables(
    connection: &Connection,
    filter: Option<&BTreeSet<u32>>,
    tables: &BTreeSet<String>,
) -> Result<Option<BTreeMap<u32, String>>, ErrCtx> {
    if tables.is_empty() {
        return Ok(Some(BTreeMap::new()));
    }
    let mut schema = connection.prepare(
        "SELECT name, tbl_name FROM sqlite_schema WHERE name IS NOT NULL AND tbl_name IS NOT NULL",
    )?;
    let schema_rows = schema.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut objects = Vec::new();
    for row in schema_rows {
        let (object_name, table_name) = row?;
        if tables.contains(&table_name) {
            objects.push((object_name, table_name));
        }
    }
    let mut dbstat = match connection.prepare("SELECT pageno FROM dbstat WHERE name = ?1") {
        Ok(statement) => statement,
        Err(_) => return Ok(None),
    };
    let mut owners = BTreeMap::new();
    for (object_name, table_name) in objects {
        let pages = dbstat.query_map([object_name], |row| row.get::<_, u32>(0))?;
        for page in pages {
            let page_number = page?;
            if filter.is_none_or(|changed| changed.contains(&page_number)) {
                owners.insert(page_number, table_name.clone());
            }
        }
    }
    Ok(Some(owners))
}

fn compress_page_ownership_ranges(owners: BTreeMap<u32, String>) -> Vec<SqlitePageOwnershipRange> {
    let mut ranges: Vec<SqlitePageOwnershipRange> = Vec::new();
    for (page_number, table) in owners {
        if let Some(last) = ranges.last_mut()
            && last.last_page.checked_add(1) == Some(page_number)
            && last.table == table
        {
            last.last_page = page_number;
            continue;
        }
        ranges.push(SqlitePageOwnershipRange {
            first_page: page_number,
            last_page: page_number,
            table,
        });
    }
    ranges
}

fn sqlite_freelist_pages(reader: &dyn VolumeRead) -> Result<Option<BTreeSet<u32>>, ErrCtx> {
    if reader.page_count().to_u32() == 0 {
        return Ok(Some(BTreeSet::new()));
    }
    let header = reader.read_page(PageIdx::try_from(1_u32).map_err(|error| {
        ErrCtx::InvalidCommand(format!("invalid SQLite header page: {error}").into())
    })?)?;
    let raw_page_size = u16::from_be_bytes([header.as_ref()[16], header.as_ref()[17]]);
    let page_size = if raw_page_size == 1 {
        65_536
    } else {
        raw_page_size as u32
    };
    if page_size != PAGESIZE.as_u32() {
        return Ok(None);
    }
    let mut trunk_page = sqlite_u32(header.as_ref(), 32);
    let declared_count = sqlite_u32(header.as_ref(), 36) as usize;
    if (trunk_page == 0) != (declared_count == 0) {
        return Ok(None);
    }
    let mut pages = BTreeSet::new();
    while trunk_page != 0 {
        if trunk_page > reader.page_count().to_u32() || !pages.insert(trunk_page) {
            return Ok(None);
        }
        let page = reader.read_page(PageIdx::try_from(trunk_page).map_err(|error| {
            ErrCtx::InvalidCommand(
                format!("invalid SQLite freelist page {trunk_page}: {error}").into(),
            )
        })?)?;
        let leaf_count = sqlite_u32(page.as_ref(), 4) as usize;
        if leaf_count > (PAGESIZE.as_usize() - 8) / 4 {
            return Ok(None);
        }
        for leaf_index in 0..leaf_count {
            let leaf_page = sqlite_u32(page.as_ref(), 8 + leaf_index * 4);
            if leaf_page == 0
                || leaf_page > reader.page_count().to_u32()
                || !pages.insert(leaf_page)
            {
                return Ok(None);
            }
        }
        trunk_page = sqlite_u32(page.as_ref(), 0);
    }
    Ok((pages.len() == declared_count).then_some(pages))
}

fn sqlite_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("validated SQLite page offset"),
    )
}

fn validate_sqlite_source(path: &Path) -> Result<(), ErrCtx> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(ErrCtx::InvalidCommand(
            format!(
                "path `{}` is not a regular SQLite database file",
                path.display()
            )
            .into(),
        ));
    }

    if metadata.len() < 100 {
        return Err(ErrCtx::InvalidCommand(
            format!("path `{}` is not a SQLite database", path.display()).into(),
        ));
    }

    let mut input = File::open(path)?;
    let mut header = [0_u8; 100];
    input.read_exact(&mut header)?;
    validate_sqlite_header(path, &header)
}

fn validate_sqlite_header(path: &Path, header: &[u8; 100]) -> Result<(), ErrCtx> {
    if &header[..SQLITE_DATABASE_MAGIC.len()] != SQLITE_DATABASE_MAGIC {
        return Err(ErrCtx::InvalidCommand(
            format!("path `{}` is not a SQLite database", path.display()).into(),
        ));
    }

    let sqlite_page_size = sqlite_page_size_from_header(header);
    if !(512..=65_536).contains(&sqlite_page_size) || !sqlite_page_size.is_power_of_two() {
        return Err(ErrCtx::InvalidCommand(
            format!(
                "SQLite database `{}` declares invalid page size {sqlite_page_size}",
                path.display()
            )
            .into(),
        ));
    }
    Ok(())
}

fn backup_sqlite_source(path: &Path, snapshot_path: &Path) -> Result<(), ErrCtx> {
    const BACKUP_TIMEOUT: Duration = Duration::from_secs(30);

    #[cfg(target_os = "macos")]
    if clone_rollback_journal_snapshot(path, snapshot_path, BACKUP_TIMEOUT)? {
        return Ok(());
    }

    backup_sqlite_source_online(path, snapshot_path, BACKUP_TIMEOUT)
}

fn backup_sqlite_source_online(
    path: &Path,
    snapshot_path: &Path,
    backup_timeout: Duration,
) -> Result<(), ErrCtx> {
    const BACKUP_RETRY_DELAY: Duration = Duration::from_millis(10);
    const PAGES_PER_STEP: i32 = 256;

    let source = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    source.busy_timeout(backup_timeout)?;
    let mut destination = Connection::open_with_flags(
        snapshot_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let backup = Backup::new(&source, &mut destination)?;
    let deadline = Instant::now() + backup_timeout;
    loop {
        match backup.step(PAGES_PER_STEP)? {
            rusqlite::backup::StepResult::Done => break,
            // Keep copying immediately while progress is being made. Sleeping after every batch
            // turns a large, uncontended database into an artificial multi-second import.
            rusqlite::backup::StepResult::More => continue,
            rusqlite::backup::StepResult::Busy | rusqlite::backup::StepResult::Locked => {
                if Instant::now() >= deadline {
                    return Err(ErrCtx::InvalidCommand(
                        format!(
                            "timed out waiting for a consistent SQLite snapshot of `{}`",
                            path.display()
                        )
                        .into(),
                    ));
                }
                std::thread::sleep(BACKUP_RETRY_DELAY);
            }
            _ => unreachable!("unknown SQLite backup step result"),
        }
    }
    drop(backup);

    // A backup of a WAL database retains WAL mode in page 1 even though the destination has no
    // WAL or shared-memory sidecars. Normalize the private snapshot so checkout produces a
    // standalone database that can also be opened read-only.
    let journal_mode: String =
        destination.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("delete") {
        destination.query_row("PRAGMA journal_mode=DELETE", [], |_| Ok(()))?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn clone_rollback_journal_snapshot(
    path: &Path,
    snapshot_path: &Path,
    timeout: Duration,
) -> Result<bool, ErrCtx> {
    let source = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    source.busy_timeout(timeout)?;
    let journal_mode: String = source.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("delete") {
        return Ok(false);
    }

    // A read transaction holds SQLite's shared lock while APFS creates the clone. Writers may
    // continue preparing a transaction, but cannot publish an in-place rollback-journal commit
    // until the clone has captured one coherent database image.
    source.execute_batch("BEGIN")?;
    source.query_row("SELECT count(*) FROM sqlite_schema", [], |_| Ok(()))?;
    let clone_result = clone_file(path, snapshot_path);
    source.execute_batch("ROLLBACK")?;
    match clone_result {
        Ok(()) => Ok(true),
        Err(_) => {
            let _ = std::fs::remove_file(snapshot_path);
            Ok(false)
        }
    }
}

#[cfg(target_os = "macos")]
fn clone_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    unsafe extern "C" {
        fn clonefile(source: *const c_char, destination: *const c_char, flags: c_int) -> c_int;
    }

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: both pointers come from live NUL-terminated `CString`s and flags=0 is the documented
    // clonefile mode. The destination does not exist inside our private temporary directory.
    let result = unsafe { clonefile(source.as_ptr(), destination.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

impl VolumeRead for PhysicalSqliteReader {
    fn snapshot(&self) -> &graft::snapshot::Snapshot {
        &self.snapshot
    }

    fn page_count(&self) -> PageCount {
        self.snapshot.page_count
    }

    fn read_page(&self, pageidx: PageIdx) -> Result<Page, graft::err::GraftErr> {
        if pageidx.to_u32() > self.page_count().to_u32() {
            return Ok(Page::EMPTY);
        }
        let offset = u64::from(pageidx.to_u32() - 1) * PAGESIZE.as_u64();
        let mut page_bytes = vec![0_u8; PAGESIZE.as_usize()];
        let mut input = self.input.lock();
        input.seek(SeekFrom::Start(offset)).map_err(|err| {
            graft::err::LogicalErr::Other(format!(
                "failed to seek SQLite database `{}`: {err}",
                self.path.display()
            ))
        })?;
        input.read_exact(&mut page_bytes).map_err(|err| {
            graft::err::LogicalErr::Other(format!(
                "failed to read SQLite database `{}`: {err}",
                self.path.display()
            ))
        })?;
        Page::try_from(page_bytes.as_slice()).map_err(|err| {
            graft::err::LogicalErr::Other(format!(
                "invalid SQLite page in `{}`: {err}",
                self.path.display()
            ))
            .into()
        })
    }
}

pub(crate) fn physical_sqlite_file_matches_state(
    runtime: &Runtime,
    path: &Path,
    expected: &CommitFileState,
) -> Result<bool, ErrCtx> {
    let physical = PhysicalSqliteReader::open(path)?;
    physical.matches_state(runtime, expected)
}

/// Verifies an internally copied, stable worktree candidate against an immutable Graft state.
///
/// The content-addressed page index turns the common path into one sequential candidate read.
/// Missing or invalid index data returns `None`, so the caller can discard the copy and retain
/// authoritative snapshot materialization without making this cache a correctness dependency.
pub(super) fn stable_physical_sqlite_matches_indexed_state(
    repo: &Repository,
    key: &str,
    path: &Path,
    expected: &CommitFileState,
) -> Result<Option<bool>, ErrCtx> {
    let physical = PhysicalSqliteReader::open_stable(path)?;
    physical.matches_indexed_state(repo, key, expected)
}

/// Returns whether the disposable exact page index needed to verify a speculative worktree seed
/// is present and valid. A miss keeps candidate construction on the ordinary post-plan path so a
/// rejected merge plan never starts a full snapshot materialization in the background.
pub(super) fn sqlite_page_index_available(
    repo: &Repository,
    key: &str,
    expected: &CommitFileState,
) -> Result<bool, ErrCtx> {
    SqlitePageHashCache::new(repo, key)
        .load(expected)
        .map(|index| index.is_some())
}

/// Prepares an existing worktree database for atomic replacement.
///
/// Physical `SQLite` files are outside Graft's VFS lock manager. We therefore ask `SQLite` for an
/// exclusive lock, fold a stale WAL into the main database, switch the old file back to rollback
/// journal mode, and remove sidecars before rename. A live writer causes the checkout to fail
/// instead of replacing the main file underneath it.
pub(super) struct SqliteReplacementGuard {
    connection: Option<Connection>,
}

impl SqliteReplacementGuard {
    /// Releases Graft's own `SQLite` handle before changing the directory entry on Windows.
    ///
    /// `SQLite`'s Windows VFS does not request delete sharing for database handles, so keeping this
    /// guard open would make `rename` and `remove_file` fail with `ERROR_ACCESS_DENIED`. The
    /// exclusive-lock preflight still rejects an active external transaction. On Unix we retain
    /// the handle through the filesystem operation, preserving the stronger no-writer race guard.
    pub(super) fn release_for_filesystem_change(&mut self) {
        #[cfg(target_os = "windows")]
        if let Some(connection) = self.connection.take() {
            let _ = connection.execute_batch("ROLLBACK");
        }
    }
}

impl Drop for SqliteReplacementGuard {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            let _ = connection.execute_batch("ROLLBACK");
        }
    }
}

pub(super) fn prepare_sqlite_path_for_replacement(
    path: &Path,
) -> Result<SqliteReplacementGuard, ErrCtx> {
    const LOCK_TIMEOUT: Duration = Duration::from_secs(1);

    if !path.exists() {
        remove_sqlite_sidecars(path)?;
        return Ok(SqliteReplacementGuard { connection: None });
    }
    if !is_sqlite_database_path(path)? {
        return Ok(SqliteReplacementGuard { connection: None });
    }

    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(LOCK_TIMEOUT)?;
    match connection.execute_batch("BEGIN EXCLUSIVE; ROLLBACK;") {
        Ok(()) => {}
        Err(rusqlite::Error::SqliteFailure(error, _))
            if matches!(
                error.code,
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked
            ) =>
        {
            return Err(ErrCtx::InvalidCommand(
                format!(
                    "cannot replace SQLite database `{}` while another transaction is active",
                    path.display()
                )
                .into(),
            ));
        }
        Err(error) => return Err(error.into()),
    }

    let journal_mode: String =
        connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if journal_mode.eq_ignore_ascii_case("wal") {
        let (busy, log_pages, checkpointed_pages): (i64, i64, i64) =
            connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
        if busy != 0 || log_pages != checkpointed_pages {
            return Err(ErrCtx::InvalidCommand(
                format!(
                    "cannot replace SQLite database `{}` while its WAL is in use",
                    path.display()
                )
                .into(),
            ));
        }
        let normalized: String =
            connection.query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))?;
        if !normalized.eq_ignore_ascii_case("delete") {
            return Err(ErrCtx::InvalidCommand(
                format!(
                    "could not detach WAL before replacing SQLite database `{}`",
                    path.display()
                )
                .into(),
            ));
        }
    }
    match connection.execute_batch("BEGIN EXCLUSIVE") {
        Ok(()) => {}
        Err(rusqlite::Error::SqliteFailure(error, _))
            if matches!(
                error.code,
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked
            ) =>
        {
            return Err(ErrCtx::InvalidCommand(
                format!(
                    "cannot replace SQLite database `{}` while another transaction is active",
                    path.display()
                )
                .into(),
            ));
        }
        Err(error) => return Err(error.into()),
    }
    // OPFS is driven by a single browser worker, so there is no second SQLite
    // process to exclude. WasmFS cannot safely rename an OPFS file while a
    // connection still owns its sync access handle: closing that old handle
    // after the rename can overwrite the replacement. Finish preparation and
    // release the handle before checkout moves the file.
    #[cfg(all(target_arch = "wasm32", target_os = "emscripten"))]
    {
        connection.execute_batch("ROLLBACK")?;
        drop(connection);
        remove_sqlite_sidecars(path)?;
        return Ok(SqliteReplacementGuard { connection: None });
    }

    #[cfg(not(all(target_arch = "wasm32", target_os = "emscripten")))]
    {
        remove_sqlite_sidecars(path)?;
        Ok(SqliteReplacementGuard { connection: Some(connection) })
    }
}

pub(super) fn remove_sqlite_sidecars(path: &Path) -> Result<(), ErrCtx> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let sidecar = PathBuf::from(sidecar);
        match std::fs::symlink_metadata(&sidecar) {
            Ok(metadata) if metadata.file_type().is_file() => std::fs::remove_file(sidecar)?,
            Ok(_) => {
                return Err(ErrCtx::InvalidCommand(
                    format!(
                        "SQLite sidecar `{}` is not a regular file",
                        sidecar.display()
                    )
                    .into(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Imports a physical `SQLite` worktree file into Graft storage.
///
/// When a staged or committed base exists, unchanged pages remain referenced by the base snapshot
/// and only changed pages produce a new storage commit. The committed repository representation is
/// unchanged: callers still receive a `CommitFileState` containing a Volume and immutable snapshot.
pub(super) fn import_physical_sqlite_file_state(
    runtime: &Runtime,
    path: &Path,
    base: Option<&CommitFileState>,
) -> Result<CommitFileState, ErrCtx> {
    let physical = PhysicalSqliteReader::open(path)?;
    import_sqlite_reader_state(runtime, path, base, &physical, None, None)
        .map(|(state, _, _)| state)
}

#[cfg(test)]
pub(super) fn prepare_physical_sqlite_file_state(
    runtime: &Runtime,
    path: &Path,
    base: Option<&CommitFileState>,
) -> Result<(CommitFileState, PreparedSqliteStage), ErrCtx> {
    prepare_physical_sqlite_file_state_with_cache(runtime, path, base, None)
}

pub(crate) fn prepare_cached_physical_sqlite_file_state(
    runtime: &Runtime,
    repo: &Repository,
    key: &str,
    path: &Path,
    base: Option<&CommitFileState>,
) -> Result<(CommitFileState, PreparedSqliteStage), ErrCtx> {
    if std::fs::metadata(path)?.len() < MIN_PAGE_HASH_CACHE_FILE_BYTES {
        return prepare_physical_sqlite_file_state_with_cache(runtime, path, base, None);
    }
    let cache = SqlitePageHashCache::new(repo, key);
    prepare_physical_sqlite_file_state_with_cache(runtime, path, base, Some(&cache))
}

fn prepare_physical_sqlite_file_state_with_cache(
    runtime: &Runtime,
    path: &Path,
    base: Option<&CommitFileState>,
    cache: Option<&SqlitePageHashCache>,
) -> Result<(CommitFileState, PreparedSqliteStage), ErrCtx> {
    let expected_ownership = match (base, cache) {
        (Some(base), Some(cache)) => match cache.load_ownership(base) {
            Ok(index) => index,
            Err(error) => {
                tracing::debug!(?error, "ignoring unreadable SQLite page-ownership cache");
                None
            }
        },
        _ => None,
    };
    let cached_hashes = match (base, cache) {
        (Some(base), Some(cache)) => match cache.load(base) {
            Ok(hashes) => hashes,
            Err(error) => {
                tracing::debug!(?error, "ignoring unreadable SQLite page-index cache");
                None
            }
        },
        _ => None,
    };
    if cached_hashes.is_some()
        && let Some(locked) = LockedPhysicalSqliteReader::open(path)?
    {
        let (state, changed_pages, page_hash_cache_hit) = import_sqlite_reader_state(
            runtime,
            path,
            base,
            &locked.physical,
            cache,
            cached_hashes,
        )?;
        let previous_reader =
            base.map(|state| runtime.snapshot_reader(state.snapshot.to_snapshot()));
        let prepared_table_candidates = Some(
            locked.physical.table_candidates_for_changed_pages(
                &changed_pages,
                previous_reader
                    .as_ref()
                    .map(|reader| reader as &dyn VolumeRead),
                expected_ownership.as_ref(),
            )?,
        );
        let prepared = PreparedSqliteStage {
            physical: None,
            prepared_table_candidates,
            previous: base.cloned(),
            staged: state.clone(),
            changed_pages,
            page_hash_cache_hit,
        };
        return Ok((state, prepared));
    }

    let physical = PhysicalSqliteReader::open(path)?;
    let (state, changed_pages, page_hash_cache_hit) =
        import_sqlite_reader_state(runtime, path, base, &physical, cache, cached_hashes)?;
    let prepared = PreparedSqliteStage {
        physical: Some(physical),
        prepared_table_candidates: None,
        previous: base.cloned(),
        staged: state.clone(),
        changed_pages,
        page_hash_cache_hit,
    };
    Ok((state, prepared))
}

pub(super) fn import_stable_sqlite_file_state(
    runtime: &Runtime,
    path: &Path,
    base: Option<&CommitFileState>,
) -> Result<CommitFileState, ErrCtx> {
    let physical = PhysicalSqliteReader::open_stable(path)?;
    import_sqlite_reader_state(runtime, path, base, &physical, None, None)
        .map(|(state, _, _)| state)
}

/// Imports a stable `SQLite` candidate from a conservative set of physically changed pages.
///
/// Callers must derive `changed_pages` from `SQLite`'s committed WAL frames. Every listed page is
/// still compared with the immutable base before writing, so false positives are harmless. WAL
/// validation failure must use [`import_stable_sqlite_file_state`] instead of calling this helper.
pub(super) fn import_stable_sqlite_file_state_from_changed_pages(
    runtime: &Runtime,
    path: &Path,
    base: &CommitFileState,
    changed_pages: &BTreeSet<u32>,
) -> Result<CommitFileState, ErrCtx> {
    let physical = PhysicalSqliteReader::open_stable(path)?;
    let base_reader = runtime.snapshot_reader(base.snapshot.to_snapshot());
    let mut target = None;
    for &page_number in changed_pages {
        graft::repo::cancellation_checkpoint()?;
        if page_number == 0 || page_number > physical.page_count().to_u32() {
            continue;
        }
        let pageidx = PageIdx::try_from(page_number).map_err(|error| {
            ErrCtx::InvalidCommand(
                format!(
                    "invalid SQLite WAL page index in `{}`: {error}",
                    path.display()
                )
                .into(),
            )
        })?;
        let page = physical.read_page(pageidx)?;
        let unchanged = base_reader.page_count().contains(pageidx)
            && sqlite_page_bytes_equal(
                page_number,
                base_reader.read_page(pageidx)?.as_ref(),
                page.as_ref(),
            );
        if unchanged {
            continue;
        }
        ensure_import_target(runtime, Some(base), &mut target)?
            .writer
            .write_page(pageidx, page)?;
    }

    if base.snapshot.page_count != physical.page_count() {
        ensure_import_target(runtime, Some(base), &mut target)?
            .writer
            .soft_truncate(physical.page_count())?;
    }

    match target {
        Some(target) => target.commit(runtime),
        None => Ok(base.clone()),
    }
}

fn import_sqlite_reader_state(
    runtime: &Runtime,
    path: &Path,
    base: Option<&CommitFileState>,
    physical: &PhysicalSqliteReader,
    cache: Option<&SqlitePageHashCache>,
    cached_hashes: Option<Vec<[u8; PAGE_HASH_BYTES]>>,
) -> Result<(CommitFileState, BTreeSet<u32>, bool), ErrCtx> {
    let page_hash_cache_hit = cached_hashes.is_some();
    let base_reader = base.map(|state| runtime.snapshot_reader(state.snapshot.to_snapshot()));
    let mut target = None;
    let mut changed_pages = BTreeSet::new();
    let page_count = physical.page_count().to_u32() as usize;
    let mut current_hashes = Vec::with_capacity(page_hash_chunk_count(page_count));

    physical.visit_page_chunks(|first_page, chunk_bytes| {
        let current_hash = *sqlite_page_chunk_hash(first_page, chunk_bytes).as_bytes();
        current_hashes.push(current_hash);
        let chunk_index = (first_page - 1) as usize / PAGE_HASH_CHUNK_PAGES;
        if cached_hashes
            .as_ref()
            .and_then(|hashes| hashes.get(chunk_index))
            .is_some_and(|expected| expected == &current_hash)
        {
            return Ok(());
        }

        for (page_offset, page_bytes) in chunk_bytes.chunks_exact(PAGESIZE.as_usize()).enumerate() {
            let page_number = first_page + page_offset as u32;
            let pageidx = PageIdx::try_from(page_number).map_err(|error| {
                ErrCtx::InvalidCommand(
                    format!("invalid SQLite page index in `{}`: {error}", path.display()).into(),
                )
            })?;
            let unchanged = match &base_reader {
                Some(reader) if reader.page_count().contains(pageidx) => sqlite_page_bytes_equal(
                    page_number,
                    reader.read_page(pageidx)?.as_ref(),
                    page_bytes,
                ),
                _ => false,
            };
            if unchanged {
                continue;
            }
            changed_pages.insert(page_number);
            let target = ensure_import_target(runtime, base, &mut target)?;
            let page = Page::try_from(page_bytes).map_err(|error| {
                ErrCtx::InvalidCommand(
                    format!("invalid SQLite page in `{}`: {error}", path.display()).into(),
                )
            })?;
            target.writer.write_page(pageidx, page)?;
        }
        Ok(())
    })?;

    if base.is_none_or(|state| state.snapshot.page_count != physical.page_count()) {
        if let Some(base) = base {
            for page_number in
                (physical.page_count().to_u32() + 1)..=base.snapshot.page_count.to_u32()
            {
                changed_pages.insert(page_number);
            }
        }
        let target = ensure_import_target(runtime, base, &mut target)?;
        target.writer.soft_truncate(physical.page_count())?;
    }

    let state = match target {
        Some(target) => target.commit(runtime)?,
        None => base
            .cloned()
            .expect("an unchanged import must have a base snapshot"),
    };
    if let Some(cache) = cache
        && let Err(error) = cache.store(&state, &current_hashes)
    {
        tracing::debug!(?error, "failed to persist SQLite page-index cache");
    }
    if let Some(cache) = cache {
        match physical.page_ownership_index() {
            Ok(Some(index)) => {
                if let Err(error) = cache.store_ownership(&state, index) {
                    tracing::debug!(?error, "failed to persist SQLite page ownership");
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(?error, "failed to derive SQLite page ownership");
            }
        }
    }
    tracing::debug!(
        path = %path.display(),
        page_count,
        chunks_hashed = current_hashes.len(),
        changed_pages = changed_pages.len(),
        page_hash_cache_hit,
        "prepared physical SQLite state"
    );
    Ok((state, changed_pages, page_hash_cache_hit))
}

struct ImportTarget {
    vid: VolumeId,
    writer: VolumeWriter,
    cleanup_on_error: bool,
}

impl ImportTarget {
    fn open(runtime: &Runtime, base: Option<&CommitFileState>) -> Result<Self, ErrCtx> {
        if let Some(base) = base {
            let snapshot = base.snapshot.to_snapshot();
            if runtime.volume_exists(&base.volume)?
                && runtime.snapshot_is_latest(&base.volume, &snapshot)?
            {
                return Ok(Self {
                    vid: base.volume.clone(),
                    writer: runtime.volume_writer(base.volume.clone())?,
                    cleanup_on_error: false,
                });
            }

            let volume = runtime.volume_from_snapshot(&snapshot)?;
            return Ok(Self {
                writer: runtime.volume_writer(volume.vid.clone())?,
                vid: volume.vid,
                cleanup_on_error: true,
            });
        }

        let volume = runtime.volume_open(None, None, None)?;
        Ok(Self {
            writer: runtime.volume_writer(volume.vid.clone())?,
            vid: volume.vid,
            cleanup_on_error: true,
        })
    }

    fn commit(mut self, runtime: &Runtime) -> Result<CommitFileState, ErrCtx> {
        let writer = self.writer;
        let reader = match writer.commit() {
            Ok(reader) => reader,
            Err(err) => {
                if self.cleanup_on_error {
                    let _ = runtime.volume_delete(&self.vid);
                }
                return Err(err.into());
            }
        };
        self.cleanup_on_error = false;
        Ok(CommitFileState {
            volume: self.vid,
            snapshot: repo_snapshot_with_commit_hashes(runtime, reader.snapshot())?,
        })
    }
}

fn ensure_import_target<'a>(
    runtime: &Runtime,
    base: Option<&CommitFileState>,
    target: &'a mut Option<ImportTarget>,
) -> Result<&'a mut ImportTarget, ErrCtx> {
    if target.is_none() {
        *target = Some(ImportTarget::open(runtime, base)?);
    }
    Ok(target.as_mut().expect("import target was initialized"))
}

pub(super) fn sqlite_page_size_from_header(header: &[u8; 100]) -> u32 {
    let raw = u16::from_be_bytes([header[16], header[17]]);
    if raw == 1 { 65_536 } else { raw as u32 }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use graft::setup::setup_graft_temporary;
    use rusqlite::params;
    use std::io::Write;

    use super::*;

    fn test_runtime() -> Runtime {
        setup_graft_temporary(RemoteConfig::Memory, None).unwrap()
    }

    fn create_database(path: &Path, journal_mode: &str) -> Connection {
        let mut connection = Connection::open(path).unwrap();
        connection
            .pragma_update(None, "page_size", PAGESIZE.as_u32())
            .unwrap();
        let actual_mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        if !actual_mode.eq_ignore_ascii_case(journal_mode) {
            let sql = format!("PRAGMA journal_mode={journal_mode}");
            connection.query_row(&sql, [], |_| Ok(())).unwrap();
        }
        if journal_mode.eq_ignore_ascii_case("wal") {
            connection
                .pragma_update(None, "wal_autocheckpoint", 0)
                .unwrap();
        }
        connection
            .execute_batch("CREATE TABLE records(id INTEGER PRIMARY KEY, payload BLOB NOT NULL);")
            .unwrap();
        let transaction = connection.transaction().unwrap();
        for id in 1..=64_i64 {
            transaction
                .execute(
                    "INSERT INTO records(id, payload) VALUES (?1, ?2)",
                    params![id, vec![id as u8; 3_000]],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        connection
    }

    fn create_freelist_database(path: &Path) -> Connection {
        let mut connection = Connection::open(path).unwrap();
        connection
            .pragma_update(None, "page_size", PAGESIZE.as_u32())
            .unwrap();
        connection
            .execute_batch(
                "CREATE TABLE views(id TEXT PRIMARY KEY, layout_json TEXT NOT NULL) WITHOUT ROWID;
                 CREATE TABLE unrelated(id INTEGER PRIMARY KEY, payload BLOB NOT NULL);
                 INSERT INTO views(id, layout_json) VALUES ('grid', '{}');",
            )
            .unwrap();
        let transaction = connection.transaction().unwrap();
        for id in 1..=2_048_i64 {
            transaction
                .execute(
                    "INSERT INTO unrelated(id, payload) VALUES (?1, ?2)",
                    params![id, vec![id as u8; 3_000]],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        connection
            .execute("DELETE FROM unrelated WHERE id <= 1024", [])
            .unwrap();
        let freelist_count: u32 = connection
            .pragma_query_value(None, "freelist_count", |row| row.get(0))
            .unwrap();
        assert!(freelist_count > 1);
        connection
    }

    fn next_random(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    fn create_randomized_without_rowid_database(path: &Path) -> Connection {
        let mut connection = Connection::open(path).unwrap();
        connection
            .pragma_update(None, "page_size", PAGESIZE.as_u32())
            .unwrap();
        connection
            .execute_batch(
                "CREATE TABLE records(
                    id TEXT PRIMARY KEY COLLATE BINARY,
                    payload BLOB NOT NULL,
                    marker INTEGER NOT NULL
                 ) STRICT, WITHOUT ROWID;",
            )
            .unwrap();

        let mut seed = 0x5EED_CAFE_F00D_BAAD_u64;
        let transaction = connection.transaction().unwrap();
        for index in 0..768_u32 {
            let payload_len = if index % 41 == 0 {
                9_000
            } else {
                128 + (next_random(&mut seed) % 1_700) as usize
            };
            transaction
                .execute(
                    "INSERT INTO records(id, payload, marker) VALUES (?1, ?2, ?3)",
                    params![
                        format!("base-{index:04}"),
                        vec![(next_random(&mut seed) & 0xff) as u8; payload_len],
                        next_random(&mut seed) as i64,
                    ],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        connection
    }

    fn prepare_with_forced_page_cache(
        runtime: &Runtime,
        repo: &Repository,
        key: &str,
        path: &Path,
        base: Option<&CommitFileState>,
    ) -> Result<(CommitFileState, PreparedSqliteStage), ErrCtx> {
        let cache = SqlitePageHashCache::new(repo, key);
        prepare_physical_sqlite_file_state_with_cache(runtime, path, base, Some(&cache))
    }

    #[test]
    fn page_index_ignores_sqlite_volatile_header_fields() {
        let original = vec![0_u8; PAGESIZE.as_usize()];
        let mut header_changed = original.clone();
        header_changed[SQLITE_FILE_CHANGE_COUNTER_OFFSET..SQLITE_FILE_CHANGE_COUNTER_OFFSET + 4]
            .copy_from_slice(&17_u32.to_be_bytes());
        header_changed[SQLITE_VERSION_VALID_FOR_OFFSET..SQLITE_VERSION_VALID_FOR_OFFSET + 4]
            .copy_from_slice(&23_u32.to_be_bytes());
        header_changed[SQLITE_LIBRARY_VERSION_OFFSET..SQLITE_LIBRARY_VERSION_OFFSET + 4]
            .copy_from_slice(&3_053_001_u32.to_be_bytes());

        assert!(sqlite_page_bytes_equal(1, &original, &header_changed));
        assert_eq!(
            sqlite_page_chunk_hash(1, &original),
            sqlite_page_chunk_hash(1, &header_changed)
        );

        let mut content_changed = header_changed;
        content_changed[40] = 1;
        assert!(!sqlite_page_bytes_equal(1, &original, &content_changed));
        assert_ne!(
            sqlite_page_chunk_hash(1, &original),
            sqlite_page_chunk_hash(1, &content_changed)
        );
    }

    #[test]
    fn online_backup_library_version_change_does_not_dirty_database() {
        let temp = tempfile::tempdir().unwrap();
        let source_path = temp.path().join("source.sqlite");
        let snapshot_path = temp.path().join("snapshot.sqlite");
        drop(create_database(&source_path, "delete"));

        let mut source = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&source_path)
            .unwrap();
        source
            .seek(SeekFrom::Start(SQLITE_LIBRARY_VERSION_OFFSET as u64))
            .unwrap();
        source.write_all(&1_u32.to_be_bytes()).unwrap();
        source.sync_all().unwrap();
        drop(source);

        backup_sqlite_source_online(&source_path, &snapshot_path, Duration::from_secs(5)).unwrap();

        let source = std::fs::read(&source_path).unwrap();
        let snapshot = std::fs::read(&snapshot_path).unwrap();
        assert_ne!(
            &source[SQLITE_LIBRARY_VERSION_OFFSET
                ..SQLITE_LIBRARY_VERSION_OFFSET + SQLITE_VOLATILE_HEADER_FIELD_BYTES],
            &snapshot[SQLITE_LIBRARY_VERSION_OFFSET
                ..SQLITE_LIBRARY_VERSION_OFFSET + SQLITE_VOLATILE_HEADER_FIELD_BYTES]
        );
        assert!(sqlite_page_bytes_equal(
            1,
            &source[..PAGESIZE.as_usize()],
            &snapshot[..PAGESIZE.as_usize()]
        ));
    }

    #[test]
    fn unchanged_import_reuses_snapshot_and_changed_import_is_incremental() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("app.sqlite");
        let connection = create_database(&path, "delete");
        let runtime = test_runtime();

        let initial = import_physical_sqlite_file_state(&runtime, &path, None).unwrap();
        let commits_before = runtime.volume_log(&initial.volume).unwrap().len();
        let unchanged = import_physical_sqlite_file_state(&runtime, &path, Some(&initial)).unwrap();
        assert_eq!(unchanged, initial);
        assert_eq!(
            runtime.volume_log(&initial.volume).unwrap().len(),
            commits_before
        );

        connection
            .execute(
                "UPDATE records SET payload = ?1 WHERE id = 32",
                [vec![0xA5_u8; 3_000]],
            )
            .unwrap();
        let updated = import_physical_sqlite_file_state(&runtime, &path, Some(&initial)).unwrap();
        assert_eq!(updated.volume, initial.volume);
        assert_ne!(updated.snapshot, initial.snapshot);

        let latest = runtime.volume_log(&updated.volume).unwrap().remove(0);
        assert!(latest.changed_pages > 0);
        assert!(latest.changed_pages < updated.snapshot.page_count.to_u32() as usize);
    }

    #[test]
    fn sparse_page_candidates_match_authoritative_diff_across_randomized_btree_changes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("randomized-without-rowid.sqlite");
        let mut connection = create_randomized_without_rowid_database(&path);
        let runtime = test_runtime();
        let mut previous = import_physical_sqlite_file_state(&runtime, &path, None).unwrap();
        let mut seed = 0xA11C_E5E1_5A7E_2026_u64;

        for round in 0..6_u32 {
            let transaction = connection.transaction().unwrap();

            // Update stable keys with both inline and overflow payloads.
            for update in 0..48_u32 {
                let index = 256 + (next_random(&mut seed) % 384) as u32;
                let payload_len = if update % 7 == 0 {
                    12_000 + (next_random(&mut seed) % 4_000) as usize
                } else {
                    64 + (next_random(&mut seed) % 2_400) as usize
                };
                transaction
                    .execute(
                        "UPDATE records SET payload = ?1, marker = ?2 WHERE id = ?3",
                        params![
                            vec![(next_random(&mut seed) & 0xff) as u8; payload_len],
                            next_random(&mut seed) as i64,
                            format!("base-{index:04}"),
                        ],
                    )
                    .unwrap();
            }

            // Delete a disjoint key range while inserts force repeated page splits.
            for deletion in 0..20_u32 {
                let index = round * 20 + deletion;
                transaction
                    .execute(
                        "DELETE FROM records WHERE id = ?1",
                        [format!("base-{index:04}")],
                    )
                    .unwrap();
            }
            for insertion in 0..96_u32 {
                let payload_len = if insertion % 11 == 0 {
                    10_000
                } else {
                    96 + (next_random(&mut seed) % 2_000) as usize
                };
                transaction
                    .execute(
                        "INSERT INTO records(id, payload, marker) VALUES (?1, ?2, ?3)",
                        params![
                            format!("round-{round:02}-{insertion:04}"),
                            vec![(next_random(&mut seed) & 0xff) as u8; payload_len],
                            next_random(&mut seed) as i64,
                        ],
                    )
                    .unwrap();
            }
            transaction.commit().unwrap();

            let current =
                import_physical_sqlite_file_state(&runtime, &path, Some(&previous)).unwrap();
            let from = previous.snapshot.to_snapshot();
            let to = current.snapshot.to_snapshot();
            let candidates = runtime
                .snapshot_changed_page_candidates(&from, &to)
                .unwrap();
            assert!(
                !candidates.is_empty(),
                "round {round} produced no candidates"
            );

            let authoritative =
                crate::row_level_diff::row_level_diff_snapshots(&runtime, &from, &to).unwrap();
            let sparse = crate::row_level_diff::row_level_diff_snapshots_with_page_candidates(
                &runtime,
                &from,
                &to,
                &candidates,
            )
            .unwrap();

            assert_eq!(sparse.analysis, authoritative.analysis, "round {round}");
            assert_eq!(
                sparse.schema_changes, authoritative.schema_changes,
                "round {round}"
            );
            assert_eq!(
                sparse.table_changes, authoritative.table_changes,
                "round {round}"
            );
            assert_eq!(
                sparse.opaque_changes, authoritative.opaque_changes,
                "round {round}"
            );
            previous = current;
        }
    }

    #[test]
    fn staged_candidates_require_an_exact_worktree_match() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("app.sqlite");
        let connection = create_database(&path, "delete");
        let runtime = test_runtime();
        let initial = import_physical_sqlite_file_state(&runtime, &path, None).unwrap();

        connection
            .execute(
                "UPDATE records SET payload = ?1 WHERE id = 32",
                [vec![0xA5_u8; 3_000]],
            )
            .unwrap();
        let updated = import_physical_sqlite_file_state(&runtime, &path, Some(&initial)).unwrap();
        let previous_reader = runtime.snapshot_reader(initial.snapshot.to_snapshot());
        let staged_reader = runtime.snapshot_reader(updated.snapshot.to_snapshot());
        let physical = PhysicalSqliteReader::open(&path).unwrap();
        assert_eq!(
            physical
                .staged_table_candidates(&staged_reader, &previous_reader)
                .unwrap(),
            Some(BTreeSet::from(["records".to_string()]))
        );

        connection
            .execute(
                "UPDATE records SET payload = ?1 WHERE id = 48",
                [vec![0x5A_u8; 3_000]],
            )
            .unwrap();
        let changed_after_stage = PhysicalSqliteReader::open(&path).unwrap();
        assert_eq!(
            changed_after_stage
                .staged_table_candidates(&staged_reader, &previous_reader)
                .unwrap(),
            None
        );
    }

    #[test]
    fn prepared_stage_keeps_candidates_after_worktree_moves_on() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("app.sqlite");
        let connection = create_database(&path, "delete");
        let runtime = test_runtime();
        let initial = import_physical_sqlite_file_state(&runtime, &path, None).unwrap();

        connection
            .execute(
                "UPDATE records SET payload = ?1 WHERE id = 32",
                [vec![0xA5_u8; 3_000]],
            )
            .unwrap();
        let (staged, prepared) =
            prepare_physical_sqlite_file_state(&runtime, &path, Some(&initial)).unwrap();

        connection
            .execute(
                "UPDATE records SET payload = ?1 WHERE id = 48",
                [vec![0x5A_u8; 3_000]],
            )
            .unwrap();

        assert!(prepared.matches(&initial, &staged));
        assert_eq!(
            prepared.table_candidates().unwrap(),
            Some(BTreeSet::from(["records".to_string()]))
        );
    }

    #[test]
    fn cached_stage_reuses_persisted_page_hashes_for_the_exact_base() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let path = temp.path().join("app.sqlite");
        let connection = create_database(&path, "delete");
        let runtime = test_runtime();
        let initial = import_physical_sqlite_file_state(&runtime, &path, None).unwrap();

        connection
            .execute(
                "UPDATE records SET payload = ?1 WHERE id = 32",
                [vec![0xA5_u8; 3_000]],
            )
            .unwrap();
        let (first, first_prepared) =
            prepare_with_forced_page_cache(&runtime, &repo, "app.sqlite", &path, Some(&initial))
                .unwrap();
        assert!(!first_prepared.page_hash_cache_hit());

        connection
            .execute(
                "UPDATE records SET payload = ?1 WHERE id = 48",
                [vec![0x5A_u8; 3_000]],
            )
            .unwrap();
        let (second, second_prepared) =
            prepare_with_forced_page_cache(&runtime, &repo, "app.sqlite", &path, Some(&first))
                .unwrap();
        assert!(second_prepared.page_hash_cache_hit());
        assert!(second_prepared.physical.is_none());
        assert!(second_prepared.prepared_table_candidates.is_some());
        assert_ne!(second.snapshot, first.snapshot);

        let unchanged =
            prepare_with_forced_page_cache(&runtime, &repo, "app.sqlite", &path, Some(&second))
                .unwrap();
        assert!(unchanged.1.page_hash_cache_hit());
        assert_eq!(unchanged.0, second);
        connection
            .execute(
                "UPDATE records SET payload = ?1 WHERE id = 1",
                [vec![0xC3_u8; 3_000]],
            )
            .expect("the indexed read lock must be released before stage returns");
    }

    #[test]
    fn cached_stage_detects_repeated_updates_to_the_same_pages() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let path = temp.path().join("app.sqlite");
        let connection = create_database(&path, "delete");
        let runtime = test_runtime();
        let initial = import_physical_sqlite_file_state(&runtime, &path, None).unwrap();

        connection
            .execute(
                "UPDATE records SET payload = ?1 WHERE id = 32",
                [vec![0xA5_u8; 3_000]],
            )
            .unwrap();
        let (first, _) =
            prepare_with_forced_page_cache(&runtime, &repo, "app.sqlite", &path, Some(&initial))
                .unwrap();
        assert_ne!(first.snapshot, initial.snapshot);

        connection
            .execute(
                "UPDATE records SET payload = ?1 WHERE id = 32",
                [vec![0x5A_u8; 3_000]],
            )
            .unwrap();
        let (second, prepared) =
            prepare_with_forced_page_cache(&runtime, &repo, "app.sqlite", &path, Some(&first))
                .unwrap();

        assert!(prepared.page_hash_cache_hit());
        assert_ne!(second.snapshot, first.snapshot);
    }

    #[test]
    fn small_database_skips_the_persistent_page_hash_cache() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let path = temp.path().join("small.sqlite");
        let connection = create_database(&path, "delete");
        assert!(std::fs::metadata(&path).unwrap().len() < MIN_PAGE_HASH_CACHE_FILE_BYTES);
        let runtime = test_runtime();
        let initial = import_physical_sqlite_file_state(&runtime, &path, None).unwrap();

        connection
            .execute(
                "UPDATE records SET payload = ?1 WHERE id = 32",
                [vec![0xA5_u8; 3_000]],
            )
            .unwrap();
        let (_, prepared) = prepare_cached_physical_sqlite_file_state(
            &runtime,
            &repo,
            "small.sqlite",
            &path,
            Some(&initial),
        )
        .unwrap();

        assert!(!prepared.page_hash_cache_hit());
        assert!(!repo.graft_dir().join("cache").join("sqlite-pages").exists());
    }

    #[test]
    fn cached_changed_table_candidates_reuses_the_committed_page_index() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let path = temp.path().join("app.sqlite");
        let connection = create_database(&path, "delete");
        let runtime = test_runtime();
        let initial = import_physical_sqlite_file_state(&runtime, &path, None).unwrap();

        connection
            .execute(
                "UPDATE records SET payload = ?1 WHERE id = 32",
                [vec![0xA5_u8; 3_000]],
            )
            .unwrap();
        let (committed, _) =
            prepare_with_forced_page_cache(&runtime, &repo, "app.sqlite", &path, Some(&initial))
                .unwrap();

        let matches_committed = with_consistent_physical_sqlite_reader(&path, |physical| {
            physical.matches_cached_state(&runtime, &repo, "app.sqlite", &committed)
        })
        .unwrap();
        assert!(matches_committed);

        connection
            .execute(
                "UPDATE records SET payload = ?1 WHERE id = 48",
                [vec![0x5A_u8; 3_000]],
            )
            .unwrap();
        let expected = runtime.snapshot_reader(committed.snapshot.to_snapshot());
        let candidates = with_consistent_physical_sqlite_reader(&path, |physical| {
            physical.cached_changed_table_candidates(
                &runtime,
                &repo,
                "app.sqlite",
                &committed,
                &expected,
            )
        })
        .unwrap();

        assert_eq!(candidates, Some(BTreeSet::from(["records".to_string()])));
        let cache = SqlitePageHashCache::new(&repo, "app.sqlite");
        assert!(cache.probe_path_for_state(&committed).unwrap().is_file());
        WORKTREE_DIFF_PROBES
            .get_or_init(|| Mutex::new(VecDeque::new()))
            .lock()
            .clear();
        let matches_changed = with_consistent_physical_sqlite_reader(&path, |physical| {
            physical.matches_cached_state(&runtime, &repo, "app.sqlite", &committed)
        })
        .unwrap();
        assert!(!matches_changed);
    }

    #[test]
    fn overflow_allocation_from_freelist_keeps_exact_table_candidates() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let path = temp.path().join("app.sqlite");
        let connection = create_freelist_database(&path);
        let runtime = test_runtime();
        let (baseline, _) =
            prepare_with_forced_page_cache(&runtime, &repo, "app.sqlite", &path, None).unwrap();
        let cache = SqlitePageHashCache::new(&repo, "app.sqlite");
        let ownership = cache.load_ownership(&baseline).unwrap().unwrap();
        assert_eq!(ownership.table_for_page(1), None);
        assert!(cache.ownership_path_for_state(&baseline).unwrap().is_file());

        connection
            .execute(
                "UPDATE views SET layout_json = ?1 WHERE id = 'grid'",
                ["x".repeat(3_000)],
            )
            .unwrap();
        let overflow_pages: u32 = connection
            .query_row(
                "SELECT count(*) FROM dbstat WHERE name = 'views' AND pagetype = 'overflow'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(overflow_pages > 0);

        let expected = runtime.snapshot_reader(baseline.snapshot.to_snapshot());
        let candidates = with_consistent_physical_sqlite_reader(&path, |physical| {
            physical.cached_changed_table_candidates(
                &runtime,
                &repo,
                "app.sqlite",
                &baseline,
                &expected,
            )
        })
        .unwrap();
        assert_eq!(candidates, Some(BTreeSet::from(["views".to_string()])));
    }

    #[test]
    fn overflow_release_to_freelist_uses_baseline_page_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let path = temp.path().join("app.sqlite");
        let connection = create_freelist_database(&path);
        connection
            .execute(
                "UPDATE views SET layout_json = ?1 WHERE id = 'grid'",
                ["x".repeat(3_000)],
            )
            .unwrap();
        let runtime = test_runtime();
        let (baseline, _) =
            prepare_with_forced_page_cache(&runtime, &repo, "app.sqlite", &path, None).unwrap();

        connection
            .execute("UPDATE views SET layout_json = '{}' WHERE id = 'grid'", [])
            .unwrap();
        let expected = runtime.snapshot_reader(baseline.snapshot.to_snapshot());
        let candidates = with_consistent_physical_sqlite_reader(&path, |physical| {
            physical.cached_changed_table_candidates(
                &runtime,
                &repo,
                "app.sqlite",
                &baseline,
                &expected,
            )
        })
        .unwrap();
        assert_eq!(candidates, Some(BTreeSet::from(["views".to_string()])));
    }

    #[test]
    fn bounded_consistent_reader_uses_the_live_rollback_journal_image() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("app.sqlite");
        let connection = create_database(&path, "delete");

        let read_live_image =
            with_consistent_physical_sqlite_reader(
                &path,
                |reader| Ok(reader.snapshot_path == path),
            )
            .unwrap();

        assert!(read_live_image);
        connection
            .execute(
                "UPDATE records SET payload = ?1 WHERE id = 1",
                [vec![0xC3_u8; 3_000]],
            )
            .expect("the bounded read lock must be released after the callback");
    }

    #[test]
    fn corrupted_page_hash_cache_falls_back_to_authoritative_comparison() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let path = temp.path().join("app.sqlite");
        let connection = create_database(&path, "delete");
        let runtime = test_runtime();
        let initial = import_physical_sqlite_file_state(&runtime, &path, None).unwrap();

        connection
            .execute(
                "UPDATE records SET payload = ?1 WHERE id = 32",
                [vec![0xA5_u8; 3_000]],
            )
            .unwrap();
        let (first, _) =
            prepare_with_forced_page_cache(&runtime, &repo, "app.sqlite", &path, Some(&initial))
                .unwrap();
        let cache = SqlitePageHashCache::new(&repo, "app.sqlite");
        std::fs::write(cache.path_for_state(&first).unwrap(), b"corrupted").unwrap();

        connection
            .execute(
                "UPDATE records SET payload = ?1 WHERE id = 48",
                [vec![0x5A_u8; 3_000]],
            )
            .unwrap();
        let (second, prepared) =
            prepare_with_forced_page_cache(&runtime, &repo, "app.sqlite", &path, Some(&first))
                .unwrap();
        assert!(!prepared.page_hash_cache_hit());
        assert_ne!(second.snapshot, first.snapshot);
    }

    #[test]
    fn cached_wal_stage_retains_the_online_backup_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let path = temp.path().join("app.sqlite");
        let connection = create_database(&path, "wal");
        let runtime = test_runtime();
        let initial = import_physical_sqlite_file_state(&runtime, &path, None).unwrap();

        connection
            .execute(
                "UPDATE records SET payload = ?1 WHERE id = 32",
                [vec![0xA5_u8; 3_000]],
            )
            .unwrap();
        let (first, _) =
            prepare_with_forced_page_cache(&runtime, &repo, "app.sqlite", &path, Some(&initial))
                .unwrap();
        connection
            .execute(
                "UPDATE records SET payload = ?1 WHERE id = 48",
                [vec![0x5A_u8; 3_000]],
            )
            .unwrap();
        let (second, prepared) =
            prepare_with_forced_page_cache(&runtime, &repo, "app.sqlite", &path, Some(&first))
                .unwrap();

        assert!(prepared.page_hash_cache_hit());
        assert!(prepared.physical.is_some());
        assert!(prepared.prepared_table_candidates.is_none());
        assert_ne!(second.snapshot, first.snapshot);
    }

    #[test]
    fn wal_import_reads_committed_state_without_checkpointing_source() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("app.sqlite");
        let connection = create_database(&path, "wal");
        let wal_path = PathBuf::from(format!("{}-wal", path.display()));
        assert!(wal_path.exists());
        let runtime = test_runtime();

        let initial = import_physical_sqlite_file_state(&runtime, &path, None).unwrap();
        connection
            .execute(
                "UPDATE records SET payload = ?1 WHERE id = 17",
                [vec![0x5A_u8; 3_000]],
            )
            .unwrap();
        let updated = import_physical_sqlite_file_state(&runtime, &path, Some(&initial)).unwrap();
        assert!(
            wal_path.exists(),
            "import must not checkpoint or remove the source WAL"
        );

        let materialized = temp.path().join("materialized.sqlite");
        write_repo_file_state_to_path(&runtime, &updated, &materialized).unwrap();
        let restored =
            Connection::open_with_flags(&materialized, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let payload: Vec<u8> = restored
            .query_row("SELECT payload FROM records WHERE id = 17", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(payload, vec![0x5A_u8; 3_000]);
    }

    #[test]
    fn materialization_refuses_a_live_writer_and_cleans_stale_sidecars() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("app.sqlite");
        let connection = create_database(&path, "wal");
        let runtime = test_runtime();
        let state = import_physical_sqlite_file_state(&runtime, &path, None).unwrap();

        connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        let error = write_repo_file_state_to_path(&runtime, &state, &path).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("while another transaction is active"),
            "{error}"
        );
        connection.execute_batch("ROLLBACK").unwrap();
        drop(connection);

        write_repo_file_state_to_path(&runtime, &state, &path).unwrap();
        for suffix in ["-wal", "-shm", "-journal"] {
            assert!(!PathBuf::from(format!("{}{}", path.display(), suffix)).exists());
        }
        Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    }

    #[test]
    fn replacement_guard_blocks_new_writers_until_it_is_dropped() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("app.sqlite");
        drop(create_database(&path, "delete"));

        let guard = prepare_sqlite_path_for_replacement(&path).unwrap();
        let contender = Connection::open(&path).unwrap();
        contender.busy_timeout(Duration::ZERO).unwrap();
        let error = contender.execute_batch("BEGIN IMMEDIATE").unwrap_err();
        assert!(matches!(
            error,
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked,
                    ..
                },
                _
            )
        ));

        drop(guard);
        contender
            .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
            .unwrap();
    }
}
