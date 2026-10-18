//! Private native recovery storage. Unix uses owner mode bits; Windows uses a
//! protected DACL, handle-derived file identity, and rejects reparse points.
#[cfg(unix)]
pub use std::fs::*;
#[cfg(unix)]
pub fn file_metadata(file: &std::fs::File) -> std::io::Result<std::fs::Metadata> {
    file.metadata()
}
#[cfg(unix)]
pub fn open(path: impl AsRef<std::path::Path>) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}
#[cfg(unix)]
pub fn sync_directory(file: &std::fs::File) -> std::io::Result<()> {
    file.sync_all()
}

#[cfg(unix)]
pub fn persist(
    staging: tempfile::NamedTempFile,
    path: &std::path::Path,
    replace: bool,
) -> std::io::Result<()> {
    if replace {
        staging.persist(path)
    } else {
        staging.persist_noclobber(path)
    }
    .map(|_| ())
    .map_err(|error| error.error)
}

#[cfg(windows)]
#[path = "native_fs/windows.rs"]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// Create a directory with the native private policy, never broad inherited ACLs.
pub fn create_private_directory(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::DirBuilderExt;
    DirBuilder::new().mode(0o700).create(path)
}

pub fn validate_private_directory(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    let metadata = symlink_metadata(path)?;
    if !path.is_absolute() || !metadata.is_dir() || metadata.mode() & 0o077 != 0 {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
}
