//! Native exclusion for supported repository metadata operations.
//!
//! The canonical metadata directory inode is the lock object. No extra lock
//! pathname can be removed/recreated to split participating writers. Reentrancy
//! is thread-scoped; the guard cannot move to another thread or a JavaScript host.
use super::{RepoErr, Repository, Result};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::{
    cell::RefCell,
    collections::HashMap,
    fs::File,
    path::PathBuf,
    rc::{Rc, Weak},
};

thread_local! {
    static HELD: RefCell<HashMap<PathBuf, Weak<Gate>>> = RefCell::new(HashMap::new());
}

struct Gate {
    creator_pid: u32,
    directory: PathBuf,
    file: File,
    device: u64,
    inode: u64,
}

impl Gate {
    fn check(&self) -> Result<()> {
        if self.creator_pid != std::process::id() {
            return Err(RepoErr::MutationFenceStale);
        }
        #[cfg(unix)]
        let path = std::fs::metadata(&self.directory)?;
        #[cfg(unix)]
        let opened = self.file.metadata()?;
        #[cfg(windows)]
        let path = crate::native_fs::symlink_metadata(&self.directory)?;
        #[cfg(windows)]
        let opened = crate::native_fs::file_metadata(&crate::native_fs::open(&self.directory)?)?;
        if !path.is_dir()
            || !opened.is_dir()
            || path.dev() != self.device
            || path.ino() != self.inode
            || opened.dev() != self.device
            || opened.ino() != self.inode
        {
            return Err(RepoErr::MutationFenceStale);
        }
        Ok(())
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        // A fork child inherits the same open-file-description lock. It must
        // only close its descriptor, never release its parent's live lock.
        if self.creator_pid == std::process::id() {
            let _ = self.file.unlock();
        }
    }
}

/// Non-Send native capability excluding participating local metadata writers.
/// Does not pause ordinary external files, `SQLite` connections, legacy binaries,
/// direct `.graft` edits or writers on other devices. Repository history remains
/// immutable while canonical commands are excluded.
/// An inherited guard after `fork` is stale and cannot release the parent's lock.
pub struct RepositoryMutationGuard {
    gate: Rc<Gate>,
}

impl RepositoryMutationGuard {
    pub fn check_held(&self) -> Result<()> {
        self.gate.check()
    }
}

impl Drop for RepositoryMutationGuard {
    fn drop(&mut self) {
        if Rc::strong_count(&self.gate) == 1 {
            let _ = HELD.try_with(|held| {
                held.borrow_mut().remove(&self.gate.directory);
            });
        }
    }
}

impl Repository {
    /// Admit a complete native repository operation, failing busy immediately
    /// if another participating thread/process owns the canonical directory.
    /// Calls nested on this same thread share the existing capability.
    pub fn mutation_guard(&self) -> Result<RepositoryMutationGuard> {
        let directory = std::fs::canonicalize(&self.graft_dir)?;
        if let Some(gate) = HELD.with(|held| held.borrow().get(&directory).and_then(Weak::upgrade))
        {
            gate.check()?;
            return Ok(RepositoryMutationGuard { gate });
        }
        #[cfg(unix)]
        let file = File::open(&directory)?;
        #[cfg(windows)]
        let file = {
            use std::os::windows::fs::OpenOptionsExt;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .share_mode(
                    windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
                        | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE,
                )
                .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT)
                .open(directory.join("mutation.lock"))?;
            let metadata = crate::native_fs::file_metadata(&file)?;
            if !metadata.is_file() || metadata.nlink() != 1 {
                return Err(RepoErr::MutationFenceStale);
            }
            file
        };
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => RepoErr::MutationBusy,
            std::fs::TryLockError::Error(error) => RepoErr::Io(error),
        })?;
        #[cfg(unix)]
        let metadata = file.metadata()?;
        #[cfg(windows)]
        let metadata = crate::native_fs::symlink_metadata(&directory)?;
        let gate = Rc::new(Gate {
            creator_pid: std::process::id(),
            directory: directory.clone(),
            file,
            device: metadata.dev(),
            inode: metadata.ino(),
        });
        gate.check()?;
        HELD.with(|held| {
            held.borrow_mut().insert(directory, Rc::downgrade(&gate));
        });
        Ok(RepositoryMutationGuard { gate })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    #[ignore = "isolated raw-fork helper"]
    fn forked_guard_child_cannot_reenter_or_unlock_parent() {
        unsafe extern "C" {
            fn fork() -> i32;
            fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
            fn _exit(status: i32) -> !;
        }
        let directory = tempfile::tempdir().unwrap();
        let repository = Repository::init(directory.path()).unwrap();
        let guard = repository.mutation_guard().unwrap();
        // SAFETY: This test runs in a dedicated helper process with no background
        // work. The child uses only this thread's gate and exits without running
        // parent process cleanup or the multithreaded test harness.
        let child = unsafe { fork() };
        assert!(child >= 0);
        if child == 0 {
            let rejected = matches!(guard.check_held(), Err(RepoErr::MutationFenceStale))
                && matches!(
                    repository.mutation_guard(),
                    Err(RepoErr::MutationFenceStale)
                );
            drop(guard);
            // SAFETY: Exit only this raw fork child without touching test-harness state.
            unsafe { _exit(if rejected { 0 } else { 1 }) }
        }
        let mut status = 0;
        // SAFETY: `child` is our live child and status is writable for the call.
        assert_eq!(unsafe { waitpid(child, &mut status, 0) }, child);
        assert_eq!(status, 0);
        guard.check_held().unwrap();
        let peer = repository.clone();
        assert!(
            std::thread::spawn(move || matches!(peer.mutation_guard(), Err(RepoErr::MutationBusy)))
                .join()
                .unwrap()
        );
        drop(guard);
        let peer = repository;
        assert!(
            std::thread::spawn(move || peer.mutation_guard().is_ok())
                .join()
                .unwrap()
        );
    }

    #[test]
    fn raw_fork_preserves_parent_gate() {
        assert!(std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "repo::mutation_gate::tests::forked_guard_child_cannot_reenter_or_unlock_parent", "--ignored", "--test-threads=1"])
            .status().unwrap().success());
    }

    #[test]
    #[ignore = "subprocess helper"]
    fn process_gate_child() {
        let directory = std::env::var_os("GRAFT_MUTATION_GATE_TEST_PATH").unwrap();
        let repository = Repository::open(directory).unwrap();
        let blocked = std::env::var_os("GRAFT_MUTATION_GATE_EXPECT_BUSY").is_some();
        let result = repository.branch_create_unborn("child");
        if blocked {
            assert!(matches!(result, Err(RepoErr::MutationBusy)));
        } else {
            result.unwrap();
        }
    }

    #[test]
    fn gate_is_reentrant_and_excludes_other_threads_and_processes() {
        let directory = tempfile::tempdir().unwrap();
        let repository = Repository::init(directory.path()).unwrap();
        let first = repository.mutation_guard().unwrap();
        let second = repository.mutation_guard().unwrap();
        repository.branch_create_unborn("parent").unwrap();
        let child_repository = repository.clone();
        assert!(
            std::thread::spawn(move || matches!(
                child_repository.branch_create_unborn("thread"),
                Err(RepoErr::MutationBusy)
            ))
            .join()
            .unwrap()
        );
        let child = |busy: bool| {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "repo::mutation_gate::tests::process_gate_child",
                    "--ignored",
                ])
                .env("GRAFT_MUTATION_GATE_TEST_PATH", directory.path());
            if busy {
                command.env("GRAFT_MUTATION_GATE_EXPECT_BUSY", "1");
            }
            assert!(command.status().unwrap().success());
        };
        drop(first);
        second.check_held().unwrap();
        child(true);
        assert!(
            !repository
                .branches()
                .unwrap()
                .iter()
                .any(|branch| branch.name == "child" || branch.name == "thread")
        );
        drop(second);
        child(false);
        assert!(
            repository
                .branches()
                .unwrap()
                .iter()
                .any(|branch| branch.name == "child")
        );
    }
}
