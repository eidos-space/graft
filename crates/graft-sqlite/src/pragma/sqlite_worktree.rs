use graft::volume_writer::VolumeWriter;
use rusqlite::{Connection, ErrorCode, OpenFlags, backup::Backup};
use std::time::{Duration, Instant};
#[cfg(target_os = "macos")]
use std::{
    ffi::{CString, c_char, c_int},
    os::unix::ffi::OsStrExt,
};
use tempfile::TempDir;

use super::*;

/// A stable page reader for a physical `SQLite` worktree file.
///
/// This module is the data-plane boundary between repository operations and `SQLite`. Repository
/// commands must not read physical `SQLite` pages or manipulate Graft volumes directly.
pub(super) struct PhysicalSqliteReader {
    input: Mutex<File>,
    path: PathBuf,
    snapshot_path: PathBuf,
    snapshot: graft::snapshot::Snapshot,
    _snapshot_dir: Option<TempDir>,
}

impl PhysicalSqliteReader {
    pub(super) fn open(path: &Path) -> Result<Self, ErrCtx> {
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
            return Err(ErrCtx::PragmaErr(
                format!(
                    "SQLite database `{}` is not an even multiple of {page_size} bytes",
                    path.display()
                )
                .into(),
            ));
        }

        let page_count = metadata.len() / page_size as u64;
        let page_count = u32::try_from(page_count).map_err(|_| {
            ErrCtx::PragmaErr(
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

    /// Finds tables that own pages changed from `expected` to this stable worktree snapshot.
    ///
    /// `dbstat` gives us `SQLite`'s page ownership without decoding every row. Returning `None`
    /// deliberately falls back to the full logical scan whenever a changed page cannot be mapped,
    /// preserving exact diff semantics for unusual freelist or extension layouts.
    pub(super) fn changed_table_candidates(
        &self,
        expected: &dyn VolumeRead,
    ) -> Result<Option<BTreeSet<String>>, ErrCtx> {
        let max_page_count = self
            .page_count()
            .to_u32()
            .max(expected.page_count().to_u32());
        let mut changed_pages = BTreeSet::new();
        for page_number in 1..=max_page_count {
            if page_number.is_multiple_of(1_024) {
                graft::repo::cancellation_checkpoint()?;
            }
            let pageidx = PageIdx::try_from(page_number).map_err(|error| {
                ErrCtx::PragmaErr(
                    format!("invalid SQLite page index {page_number}: {error}").into(),
                )
            })?;
            if self.read_page(pageidx)? != expected.read_page(pageidx)? {
                changed_pages.insert(page_number);
            }
        }
        self.table_candidates_for_changed_pages(&changed_pages)
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
                ErrCtx::PragmaErr(
                    format!("invalid SQLite page index {page_number}: {error}").into(),
                )
            })?;
            let physical_page = self.read_page(pageidx)?;
            if page_number <= self.page_count().to_u32()
                && physical_page != staged.read_page(pageidx)?
            {
                return Ok(None);
            }
            if physical_page != previous.read_page(pageidx)? {
                changed_pages.insert(page_number);
            }
        }
        self.table_candidates_for_changed_pages(&changed_pages)
    }

    fn table_candidates_for_changed_pages(
        &self,
        changed_pages: &BTreeSet<u32>,
    ) -> Result<Option<BTreeSet<String>>, ErrCtx> {
        if changed_pages.is_empty() {
            return Ok(Some(BTreeSet::new()));
        }

        let connection = Connection::open_with_flags(
            &self.snapshot_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let mut statement = match connection.prepare(
            "SELECT d.pageno, COALESCE(m.tbl_name, d.name) \
             FROM dbstat AS d \
             LEFT JOIN sqlite_schema AS m ON m.name = d.name",
        ) {
            Ok(statement) => statement,
            Err(_) => return Ok(None),
        };
        let mut mapped_pages = BTreeSet::new();
        let mut tables = BTreeSet::new();
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (page_number, table_name) = row?;
            if !changed_pages.contains(&page_number) {
                continue;
            }
            mapped_pages.insert(page_number);
            if !table_name.starts_with("sqlite_") {
                tables.insert(table_name);
            }
        }

        // Page 1 always carries SQLite's change counter and schema cookie. Schema changes are
        // compared separately, so it is the only unmapped page that is safe to ignore here.
        if changed_pages
            .iter()
            .any(|page_number| *page_number != 1 && !mapped_pages.contains(page_number))
        {
            return Ok(None);
        }
        Ok(Some(tables))
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
                ErrCtx::PragmaErr(format!("invalid SQLite page index {page_number}: {err}").into())
            })?;
            if self.read_page(pageidx)? != stored.read_page(pageidx)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn validate_sqlite_source(path: &Path) -> Result<(), ErrCtx> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(ErrCtx::PragmaErr(
            format!(
                "path `{}` is not a regular SQLite database file",
                path.display()
            )
            .into(),
        ));
    }

    if metadata.len() < 100 {
        return Err(ErrCtx::PragmaErr(
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
        return Err(ErrCtx::PragmaErr(
            format!("path `{}` is not a SQLite database", path.display()).into(),
        ));
    }

    let sqlite_page_size = sqlite_page_size_from_header(header);
    if !(512..=65_536).contains(&sqlite_page_size) || !sqlite_page_size.is_power_of_two() {
        return Err(ErrCtx::PragmaErr(
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
    const BACKUP_RETRY_DELAY: Duration = Duration::from_millis(10);
    const PAGES_PER_STEP: i32 = 256;

    #[cfg(target_os = "macos")]
    if clone_rollback_journal_snapshot(path, snapshot_path, BACKUP_TIMEOUT)? {
        return Ok(());
    }

    let source = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    source.busy_timeout(BACKUP_TIMEOUT)?;
    let mut destination = Connection::open_with_flags(
        snapshot_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let backup = Backup::new(&source, &mut destination)?;
    let deadline = Instant::now() + BACKUP_TIMEOUT;
    loop {
        match backup.step(PAGES_PER_STEP)? {
            rusqlite::backup::StepResult::Done => break,
            // Keep copying immediately while progress is being made. Sleeping after every batch
            // turns a large, uncontended database into an artificial multi-second import.
            rusqlite::backup::StepResult::More => continue,
            rusqlite::backup::StepResult::Busy | rusqlite::backup::StepResult::Locked => {
                if Instant::now() >= deadline {
                    return Err(ErrCtx::PragmaErr(
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
            return Err(ErrCtx::PragmaErr(
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
            return Err(ErrCtx::PragmaErr(
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
            return Err(ErrCtx::PragmaErr(
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
            return Err(ErrCtx::PragmaErr(
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

fn remove_sqlite_sidecars(path: &Path) -> Result<(), ErrCtx> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let sidecar = PathBuf::from(sidecar);
        match std::fs::symlink_metadata(&sidecar) {
            Ok(metadata) if metadata.file_type().is_file() => std::fs::remove_file(sidecar)?,
            Ok(_) => {
                return Err(ErrCtx::PragmaErr(
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
    import_sqlite_reader_state(runtime, path, base, physical)
}

pub(super) fn import_stable_sqlite_file_state(
    runtime: &Runtime,
    path: &Path,
) -> Result<CommitFileState, ErrCtx> {
    let physical = PhysicalSqliteReader::open_stable(path)?;
    import_sqlite_reader_state(runtime, path, None, physical)
}

fn import_sqlite_reader_state(
    runtime: &Runtime,
    path: &Path,
    base: Option<&CommitFileState>,
    physical: PhysicalSqliteReader,
) -> Result<CommitFileState, ErrCtx> {
    let base_reader = base.map(|state| runtime.snapshot_reader(state.snapshot.to_snapshot()));
    let mut target = None;

    for page_number in 1..=physical.page_count().to_u32() {
        graft::repo::cancellation_checkpoint()?;
        let pageidx = PageIdx::try_from(page_number).map_err(|err| {
            ErrCtx::PragmaErr(
                format!("invalid SQLite page index in `{}`: {err}", path.display()).into(),
            )
        })?;
        let page = physical.read_page(pageidx)?;
        let unchanged = match &base_reader {
            Some(reader) if reader.page_count().contains(pageidx) => {
                reader.read_page(pageidx)? == page
            }
            _ => false,
        };
        if unchanged {
            continue;
        }

        let target = ensure_import_target(runtime, base, &mut target)?;
        target.writer.write_page(pageidx, page)?;
    }

    if base.is_none_or(|state| state.snapshot.page_count != physical.page_count()) {
        let target = ensure_import_target(runtime, base, &mut target)?;
        target.writer.soft_truncate(physical.page_count())?;
    }

    let Some(target) = target else {
        return Ok(base
            .cloned()
            .expect("an unchanged import must have a base snapshot"));
    };
    target.commit(runtime)
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
