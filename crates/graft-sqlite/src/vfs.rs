use std::{
    borrow::Cow,
    collections::HashMap,
    fmt::Debug,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

use graft::{
    GraftErr, LogicalErr,
    core::{
        PageIdx,
        page::{PAGESIZE, Page},
    },
    repo::RepoErr,
    rt::runtime::Runtime,
    volume_writer::VolumeWrite,
};
use parking_lot::Mutex;
use sqlite_plugin::{
    flags::{AccessFlags, CreateMode, LockLevel, OpenKind, OpenMode, OpenOpts},
    vars::{
        self, SQLITE_BUSY, SQLITE_BUSY_SNAPSHOT, SQLITE_CANTOPEN, SQLITE_INTERNAL, SQLITE_IOERR,
        SQLITE_NOTFOUND,
    },
    vfs::{Pragma, PragmaErr, SqliteErr, Vfs, VfsResult},
};
use thiserror::Error;

use crate::{
    file::{FileHandle, VfsFile, mem_file::MemFile, vol_file::VolFile},
    pragma::GraftPragma,
};

const SQLITE_DATABASE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

#[derive(Debug, Error)]
pub enum ErrCtx {
    #[error("Graft error: {0}")]
    Graft(#[from] GraftErr),

    #[error("Unknown Pragma")]
    UnknownPragma,

    #[error("Pragma error: {0}")]
    PragmaErr(Cow<'static, str>),

    #[error("Tag not found")]
    TagNotFound,

    #[error("Transaction is busy")]
    Busy,

    #[error("The transaction snapshot is no longer current")]
    BusySnapshot,

    #[error("Invalid lock transition")]
    InvalidLockTransition,

    #[error("Invalid volume state")]
    InvalidVolumeState,

    #[error("Graft repository error: {0}")]
    Repo(#[from] RepoErr),

    #[error(transparent)]
    IoErr(#[from] std::io::Error),

    #[error(transparent)]
    FmtErr(#[from] std::fmt::Error),
}

impl ErrCtx {
    #[inline]
    fn wrap<T>(cb: impl FnOnce() -> Result<T, ErrCtx>) -> VfsResult<T> {
        match cb() {
            Ok(t) => Ok(t),
            Err(err) => Err(err.sqlite_err()),
        }
    }

    fn sqlite_err(&self) -> SqliteErr {
        match self {
            ErrCtx::UnknownPragma => SQLITE_NOTFOUND,
            ErrCtx::TagNotFound => SQLITE_CANTOPEN,
            ErrCtx::Busy => SQLITE_BUSY,
            ErrCtx::BusySnapshot => SQLITE_BUSY_SNAPSHOT,
            ErrCtx::Graft(err) => Self::map_graft_err(err),
            _ => SQLITE_INTERNAL,
        }
    }

    fn map_graft_err(err: &GraftErr) -> SqliteErr {
        match err {
            GraftErr::Storage(_) => SQLITE_IOERR,
            GraftErr::Remote(_) => SQLITE_IOERR,
            GraftErr::Logical(err) => match err {
                LogicalErr::VolumeNotFound(_) => SQLITE_IOERR,
                LogicalErr::VolumeConcurrentWrite(_) => SQLITE_BUSY_SNAPSHOT,
                LogicalErr::VolumeNeedsRecovery(_)
                | LogicalErr::VolumeDiverged(_)
                | LogicalErr::VolumeRemoteMismatch { .. }
                | LogicalErr::Other(_) => SQLITE_INTERNAL,
            },
        }
    }
}

pub struct GraftVfs {
    runtime: Runtime,
    repo_runtimes: Arc<RepoRuntimeRegistry>,
    // VolFile locks keyed by tag
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

#[derive(Debug)]
pub struct RepoRuntimeRegistry {
    base: Runtime,
    runtimes: Mutex<HashMap<PathBuf, Runtime>>,
}

impl RepoRuntimeRegistry {
    fn new(base: Runtime) -> Self {
        Self { base, runtimes: Default::default() }
    }

    pub fn runtime_for(&self, repo: &graft::repo::Repository) -> Result<Runtime, ErrCtx> {
        let key = repo.graft_dir().to_path_buf();
        if let Some(runtime) = self.runtimes.lock().get(&key) {
            return Ok(runtime.clone());
        }

        let runtime = self
            .base
            .fork_with_storage_path(repo.store_dir())
            .map_err(GraftErr::from)?;
        self.runtimes.lock().insert(key, runtime.clone());
        Ok(runtime)
    }
}

impl GraftVfs {
    pub fn new(runtime: Runtime) -> Self {
        Self {
            repo_runtimes: Arc::new(RepoRuntimeRegistry::new(runtime.clone())),
            runtime,
            locks: Default::default(),
        }
    }

    fn runtime_for_tag(
        &self,
        tag: &str,
    ) -> Result<(Runtime, Option<graft::repo::Repository>), ErrCtx> {
        let repo = if should_discover_repo(tag) {
            graft::repo::Repository::discover_for_file(tag).ok()
        } else {
            None
        };
        let runtime = if let Some(repo) = &repo {
            self.repo_runtimes.runtime_for(repo)?
        } else {
            self.runtime.clone()
        };
        Ok((runtime, repo))
    }
}

impl Vfs for GraftVfs {
    type Handle = FileHandle;

    fn device_characteristics(&self, _handle: &mut Self::Handle) -> VfsResult<i32> {
        Ok(
            // writes up to a single page are atomic
            vars::SQLITE_IOCAP_ATOMIC512 |
            vars::SQLITE_IOCAP_ATOMIC1K |
            vars::SQLITE_IOCAP_ATOMIC2K |
            vars::SQLITE_IOCAP_ATOMIC4K |
            // after reboot following a crash or power loss, the only bytes in a file that were written
            // at the application level might have changed and that adjacent bytes, even bytes within
            // the same sector are guaranteed to be unchanged
            vars::SQLITE_IOCAP_POWERSAFE_OVERWRITE |
            // when data is appended to a file, the data is appended first then the size of the file is
            // extended, never the other way around
            vars::SQLITE_IOCAP_SAFE_APPEND |
            // information is written to disk in the same order as calls to xWrite()
            vars::SQLITE_IOCAP_SEQUENTIAL,
        )
    }

    fn access(&self, path: &str, flags: AccessFlags) -> VfsResult<bool> {
        tracing::trace!("access: path={path:?}; flags={flags:?}");
        ErrCtx::wrap(move || {
            let tag = normalize_tag(path)?;
            let (runtime, _) = self.runtime_for_tag(&tag)?;
            Ok(runtime.tag_exists(&tag)? || physical_sqlite_file_exists(&tag)?)
        })
    }

    fn open(&self, path: Option<&str>, opts: OpenOpts) -> VfsResult<Self::Handle> {
        tracing::trace!("open: path={path:?}, opts={opts:?}");
        ErrCtx::wrap(move || {
            // we only open a Volume for main database files
            if opts.kind() == OpenKind::MainDb
                && let Some(tag) = path
            {
                let tag = normalize_tag(tag)?;
                let can_create = matches!(
                    opts.mode(),
                    OpenMode::ReadWrite {
                        create: CreateMode::Create | CreateMode::MustCreate
                    }
                );

                let (runtime, repo) = self.runtime_for_tag(&tag)?;

                let vid = if let Some(vid) = runtime.tag_get(&tag)? {
                    vid
                } else if let Some(vid) = import_physical_sqlite_file_as_volume(&runtime, &tag)? {
                    vid
                } else if can_create {
                    let volume = runtime.volume_open(None, None, None)?;
                    runtime.tag_replace(&tag, volume.vid.clone())?;
                    volume.vid
                } else {
                    return Err(ErrCtx::TagNotFound);
                };

                // get or create a reserved lock for this Volume
                let reserved_lock = self.locks.lock().entry(tag.clone()).or_default().clone();

                return Ok(VolFile::new(
                    runtime,
                    tag,
                    vid,
                    opts,
                    reserved_lock,
                    repo,
                    self.repo_runtimes.clone(),
                )
                .into());
            }

            // all other files use in-memory storage
            Ok(MemFile::default().into())
        })
    }

    fn delete(&self, path: &str) -> VfsResult<()> {
        // nothing to do, SQLite only calls xDelete on secondary
        // files, which in this VFS are in-memory and delete on close
        tracing::trace!("delete: path={path:?}");
        Ok(())
    }

    fn close(&self, handle: Self::Handle) -> VfsResult<()> {
        tracing::trace!("close: file={handle:?}");
        ErrCtx::wrap(move || {
            match handle {
                FileHandle::MemFile(_) => Ok(()),
                FileHandle::VolFile(vol_file) => {
                    if vol_file.opts().delete_on_close() {
                        // TODO: delete volume on close if requested
                        // TODO: do we want to actually delete volumes? or mark them for deletion?
                    }

                    // retrieve a reference to the reserved lock for the volume
                    let mut locks = self.locks.lock();
                    let reserved_lock = locks
                        .get(&vol_file.tag)
                        .expect("reserved lock missing from lock manager");

                    // clean up the lock if this was the last reference
                    // SAFETY: we are holding a lock on the lock manager,
                    // preventing any concurrent opens from incrementing the
                    // reference count
                    if Arc::strong_count(reserved_lock) == 1 {
                        locks.remove(&vol_file.tag);
                    }

                    Ok(())
                }
            }
        })
    }

    fn pragma(
        &self,
        handle: &mut Self::Handle,
        pragma: Pragma<'_>,
    ) -> Result<Option<String>, PragmaErr> {
        tracing::trace!("pragma: file={handle:?}, pragma={pragma:?}");
        if let FileHandle::VolFile(file) = handle {
            match GraftPragma::try_from(&pragma)?.eval(&self.runtime, file) {
                Ok(val) => Ok(val),
                Err(err) => Err(PragmaErr::Fail(err.sqlite_err(), Some(format!("{err}")))),
            }
        } else {
            Err(PragmaErr::NotFound)
        }
    }

    fn lock(&self, handle: &mut Self::Handle, level: LockLevel) -> VfsResult<()> {
        tracing::trace!("lock: file={handle:?}, level={level:?}");
        ErrCtx::wrap(move || handle.lock(level))
    }

    fn unlock(&self, handle: &mut Self::Handle, level: LockLevel) -> VfsResult<()> {
        tracing::trace!("unlock: file={handle:?}, level={level:?}");
        ErrCtx::wrap(move || handle.unlock(level))
    }

    fn check_reserved_lock(&self, handle: &mut Self::Handle) -> VfsResult<bool> {
        tracing::trace!("check_reserved_lock: file={handle:?}");
        ErrCtx::wrap(move || handle.check_reserved_lock())
    }

    fn file_size(&self, handle: &mut Self::Handle) -> VfsResult<usize> {
        tracing::trace!("file_size: handle={handle:?}");
        ErrCtx::wrap(move || handle.file_size())
    }

    fn truncate(&self, handle: &mut Self::Handle, size: usize) -> VfsResult<()> {
        tracing::trace!("truncate: handle={handle:?}, size={size}");
        ErrCtx::wrap(move || handle.truncate(size))
    }

    fn write(&self, handle: &mut Self::Handle, offset: usize, data: &[u8]) -> VfsResult<usize> {
        tracing::trace!(
            "write: handle={handle:?}, offset={offset}, len={}",
            data.len()
        );
        ErrCtx::wrap(move || handle.write(offset, data))
    }

    fn read(&self, handle: &mut Self::Handle, offset: usize, data: &mut [u8]) -> VfsResult<usize> {
        tracing::trace!(
            "read: handle={handle:?}, offset={offset}, len={}",
            data.len()
        );
        ErrCtx::wrap(move || handle.read(offset, data))
    }
}

fn import_physical_sqlite_file_as_volume(
    runtime: &Runtime,
    tag: &str,
) -> Result<Option<graft::core::VolumeId>, ErrCtx> {
    let path = Path::new(tag);
    let Some(header) = physical_sqlite_header(path)? else {
        return Ok(None);
    };

    let sqlite_page_size = sqlite_page_size_from_header(&header);
    let graft_page_size = PAGESIZE.as_u32();
    if sqlite_page_size != graft_page_size {
        return Err(ErrCtx::PragmaErr(
            format!(
                "SQLite database `{}` uses page size {sqlite_page_size}, but Graft requires {graft_page_size}",
                path.display()
            )
            .into(),
        ));
    }

    let metadata = std::fs::metadata(path)?;
    if metadata.len() % graft_page_size as u64 != 0 {
        return Err(ErrCtx::PragmaErr(
            format!(
                "SQLite database `{}` is not an even multiple of {graft_page_size} bytes",
                path.display()
            )
            .into(),
        ));
    }

    let page_count = metadata.len() / graft_page_size as u64;
    let page_count_u32 = u32::try_from(page_count).map_err(|_| {
        ErrCtx::PragmaErr(
            format!(
                "SQLite database `{}` has too many pages to import",
                path.display()
            )
            .into(),
        )
    })?;

    let volume = runtime.volume_open(None, None, None)?;
    let vid = volume.vid;
    let mut writer = runtime.volume_writer(vid.clone())?;
    let mut input = File::open(path)?;
    let mut page_bytes = vec![0_u8; graft_page_size as usize];
    for page_number in 1..=page_count_u32 {
        input.read_exact(&mut page_bytes)?;
        let page = Page::try_from(page_bytes.as_slice()).map_err(|err| {
            ErrCtx::PragmaErr(format!("invalid SQLite page in `{}`: {err}", path.display()).into())
        })?;
        let pageidx = PageIdx::try_from(page_number).map_err(|err| {
            ErrCtx::PragmaErr(
                format!("invalid SQLite page index in `{}`: {err}", path.display()).into(),
            )
        })?;
        writer.write_page(pageidx, page)?;
    }
    writer.commit()?;
    runtime.tag_replace(tag, vid.clone())?;
    Ok(Some(vid))
}

fn physical_sqlite_file_exists(tag: &str) -> Result<bool, ErrCtx> {
    Ok(physical_sqlite_header(Path::new(tag))?.is_some())
}

fn physical_sqlite_header(path: &Path) -> Result<Option<[u8; 100]>, ErrCtx> {
    let mut header = [0_u8; 100];
    let mut input = match File::open(path) {
        Ok(input) => input,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };

    match input.read_exact(&mut header) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    if &header[..SQLITE_DATABASE_MAGIC.len()] != SQLITE_DATABASE_MAGIC {
        return Ok(None);
    }
    Ok(Some(header))
}

fn sqlite_page_size_from_header(header: &[u8; 100]) -> u32 {
    let raw = u16::from_be_bytes([header[16], header[17]]);
    if raw == 1 { 65_536 } else { raw as u32 }
}

pub(crate) fn should_discover_repo(tag: &str) -> bool {
    let path = std::path::Path::new(tag);
    path.is_absolute() || tag.contains('/') || tag.contains('\\') || path.extension().is_some()
}

fn normalize_tag(tag: &str) -> Result<String, ErrCtx> {
    if !should_discover_repo(tag) {
        return Ok(tag.to_string());
    }

    let path = Path::new(tag);
    let Some(file_name) = path.file_name() else {
        return Ok(tag.to_string());
    };

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let Ok(parent) = std::fs::canonicalize(parent) else {
        return Ok(tag.to_string());
    };
    Ok(parent.join(file_name).to_string_lossy().into_owned())
}
