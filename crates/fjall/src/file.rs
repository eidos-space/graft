// Copyright (c) 2024-present, fjall-rs
// This source code is licensed under both the Apache 2.0 and MIT License
// (found in the LICENSE-* files in the repository)

use std::path::Path;

pub const MAGIC_BYTES: &[u8] = &[b'F', b'J', b'L', 3];

pub const KEYSPACES_FOLDER: &str = "keyspaces";

pub const LOCK_FILE: &str = "lock";
pub const VERSION_MARKER: &str = "version";

pub const LSM_CURRENT_VERSION_MARKER: &str = "current";

#[cfg(not(any(target_os = "windows", target_os = "emscripten")))]
pub fn fsync_directory<P: AsRef<Path>>(path: P) -> std::io::Result<()> {
    let path = path.as_ref();

    let file = std::fs::File::open(path).inspect_err(|e| {
        log::error!("Failed to open directory at {}: {e:?}", path.display());
    })?;

    debug_assert!(file.metadata()?.is_dir());

    file.sync_all().inspect_err(|e| {
        log::error!("Failed to fsync directory at {}: {e:?}", path.display());
    })
}

#[cfg(any(target_os = "windows", target_os = "emscripten"))]
pub fn fsync_directory<P: AsRef<Path>>(_path: P) -> std::io::Result<()> {
    // Windows and Emscripten's OPFS backend cannot fsync directory handles.
    Ok(())
}
