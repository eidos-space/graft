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

/// Install the owner and DACL before any recovery bytes are written. Elevated
/// Windows tokens can otherwise assign Administrators as a temporary file owner.
pub fn private_named_tempfile(
    directory: &std::path::Path,
) -> std::io::Result<tempfile::NamedTempFile> {
    let file = tempfile::NamedTempFile::new_in(directory)?;
    #[cfg(windows)]
    set_permissions(file.path(), Permissions::from_mode(0o600))?;
    Ok(file)
}

/// Protect temporary verification state before creating files or admitting it
/// as private storage, including with an elevated Windows token.
pub fn private_tempdir(
    prefix: &str,
    directory: &std::path::Path,
) -> std::io::Result<tempfile::TempDir> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    validate_private_directory(directory)?;
    let temporary = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(directory)?;
    set_permissions(temporary.path(), Permissions::from_mode(0o700))?;
    validate_private_directory(temporary.path())?;
    Ok(temporary)
}
