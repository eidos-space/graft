#![allow(unsafe_code)]
//! Win32 calls are isolated here. Handles are non-inheritable; records cannot be
//! opened through reparse points. No password or key reaches this module.
pub use std::fs::{
    File, canonicalize, create_dir, create_dir_all, read, read_dir, remove_dir, remove_file, write,
};
use std::{
    ffi::c_void,
    io, mem,
    os::windows::{
        ffi::OsStrExt,
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::Path,
    ptr,
};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, *},
    Storage::FileSystem::*,
    System::Threading::*,
};

fn wide(path: &Path) -> io::Result<Vec<u16>> {
    let text: Vec<u16> = path.as_os_str().encode_wide().collect();
    if text.contains(&0) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(text.into_iter().chain(Some(0)).collect())
}
fn denied<T>() -> io::Result<T> {
    Err(io::ErrorKind::PermissionDenied.into())
}
struct Local(*mut c_void);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

fn user() -> io::Result<Vec<u8>> {
    unsafe {
        let mut token = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut size = 0;
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut size);
        // u64 allocation supplies alignment for TOKEN_USER.
        let mut buffer = vec![0_u64; (size as usize).div_ceil(8)];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        );
        CloseHandle(token);
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let sid = (*(buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid;
        let length = GetLengthSid(sid);
        let mut result = vec![0_u8; length as usize];
        if CopySid(length, result.as_mut_ptr().cast(), sid) == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(result)
    }
}

fn descriptor() -> io::Result<Local> {
    unsafe {
        let sid = user()?;
        let mut text = ptr::null_mut();
        if ConvertSidToStringSidW(sid.as_ptr() as _, &mut text) == 0 {
            return Err(io::Error::last_os_error());
        }
        let allocation = Local(text.cast());
        let mut length = 0;
        while *text.add(length) != 0 {
            length += 1;
        }
        let sid_text = String::from_utf16(std::slice::from_raw_parts(text, length))
            .map_err(|_| io::ErrorKind::InvalidData)?;
        drop(allocation);
        let sddl: Vec<u16> = format!("O:{sid_text}D:P(A;OICI;FA;;;{sid_text})(A;OICI;FA;;;SY)")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut value = ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut value,
            ptr::null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Local(value))
    }
}

fn is_private(file: &File) -> io::Result<bool> {
    unsafe {
        let mut owner = ptr::null_mut();
        let mut dacl = ptr::null_mut();
        let mut sd = ptr::null_mut();
        let status = GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut sd,
        );
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let _allocation = Local(sd);
        let sid = user()?;
        if owner.is_null() || dacl.is_null() || EqualSid(owner, sid.as_ptr() as _) == 0 {
            return Ok(false);
        }
        let mut system = [0_u64; 9];
        let mut system_size = mem::size_of_val(&system) as u32;
        if CreateWellKnownSid(
            WinLocalSystemSid,
            ptr::null_mut(),
            system.as_mut_ptr().cast(),
            &mut system_size,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut owner_allowed = false;
        for index in 0..(*dacl).AceCount {
            let mut entry = ptr::null_mut();
            if GetAce(dacl, index as u32, &mut entry) == 0 {
                return Err(io::Error::last_os_error());
            }
            let ace = &*(entry.cast::<ACCESS_ALLOWED_ACE>());
            if ace.Header.AceType != 0 {
                return Ok(false);
            }
            let trustee = ptr::addr_of!(ace.SidStart) as _;
            if EqualSid(trustee, sid.as_ptr() as _) != 0 {
                owner_allowed = true;
            } else if EqualSid(trustee, system.as_ptr() as _) == 0 {
                return Ok(false);
            }
        }
        Ok(owner_allowed)
    }
}

pub struct Metadata {
    inner: std::fs::Metadata,
    info: BY_HANDLE_FILE_INFORMATION,
    private: bool,
}
impl Metadata {
    pub fn is_dir(&self) -> bool {
        self.inner.is_dir() && self.info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
    }
    pub fn is_file(&self) -> bool {
        self.inner.is_file() && self.info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
    }
    pub fn len(&self) -> u64 {
        self.inner.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn mode(&self) -> u32 {
        if self.private { 0o700 } else { 0o777 }
    }
    pub fn uid(&self) -> u32 {
        0
    } // Every accepted object must belong to the current token SID.
    pub fn dev(&self) -> u64 {
        self.info.dwVolumeSerialNumber as u64
    }
    pub fn ino(&self) -> u64 {
        ((self.info.nFileIndexHigh as u64) << 32) | self.info.nFileIndexLow as u64
    }
    pub fn nlink(&self) -> u64 {
        self.info.nNumberOfLinks as u64
    }
}
pub fn file_metadata(file: &File) -> io::Result<Metadata> {
    let mut info = unsafe { mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Metadata {
        inner: file.metadata()?,
        info,
        private: is_private(file)?,
    })
}
pub fn symlink_metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    file_metadata(&open(path)?)
}
pub fn open(path: impl AsRef<Path>) -> io::Result<File> {
    let directory = std::fs::symlink_metadata(path.as_ref())?.is_dir();
    std::fs::OpenOptions::new()
        .read(true)
        .write(!directory)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[derive(Default)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    create_new: bool,
}
impl OpenOptions {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn read(&mut self, value: bool) -> &mut Self {
        self.read = value;
        self
    }
    pub fn write(&mut self, value: bool) -> &mut Self {
        self.write = value;
        self
    }
    pub fn create_new(&mut self, value: bool) -> &mut Self {
        self.create_new = value;
        self
    }
    pub fn mode(&mut self, _: u32) -> &mut Self {
        self
    }
    pub fn open(&self, path: impl AsRef<Path>) -> io::Result<File> {
        let sd = descriptor()?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: 0,
        };
        let path = wide(path.as_ref())?;
        let access = READ_CONTROL
            | if self.read { FILE_GENERIC_READ } else { 0 }
            | if self.write { FILE_GENERIC_WRITE } else { 0 };
        // SAFETY: path, descriptor and attributes outlive the synchronous call;
        // a successful handle transfers exactly once to std::fs::File.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                &attributes,
                if self.create_new {
                    CREATE_NEW
                } else {
                    OPEN_EXISTING
                },
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_handle(handle) };
        let metadata = file_metadata(&file)?;
        if !metadata.is_file() || !metadata.private || metadata.nlink() != 1 {
            return denied();
        }
        Ok(file)
    }
}
#[derive(Default)]
pub struct DirBuilder;
impl DirBuilder {
    pub fn new() -> Self {
        Self
    }
    pub fn mode(&mut self, _: u32) -> &mut Self {
        self
    }
    pub fn create(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = wide(path.as_ref())?;
        let sd = descriptor()?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: 0,
        };
        if unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
pub struct Permissions;
impl Permissions {
    pub fn from_mode(_: u32) -> Self {
        Self
    }
}
pub fn set_permissions(path: impl AsRef<Path>, _: Permissions) -> io::Result<()> {
    let path = wide(path.as_ref())?;
    let sd = descriptor()?;
    if unsafe {
        SetFileSecurityW(
            path.as_ptr(),
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            sd.0,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// Windows has no supported fsync(directory). Metadata installation below uses
// write-through same-volume moves; record files are explicitly FlushFileBuffers'd.
pub fn sync_directory(_: &File) -> io::Result<()> {
    Ok(())
}
pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    move_file(from.as_ref(), to.as_ref(), false)
}
fn move_file(from: &Path, to: &Path, replace: bool) -> io::Result<()> {
    let from = wide(from)?;
    let to = wide(to)?;
    let flags = MOVEFILE_WRITE_THROUGH
        | if replace {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), flags) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
pub fn persist(staging: tempfile::NamedTempFile, path: &Path, replace: bool) -> io::Result<()> {
    staging.as_file().sync_all()?;
    move_file(staging.path(), path, replace)?;
    Ok(())
}
