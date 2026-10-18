#![allow(unsafe_code)]
use anyhow::{Result, bail};
use std::{
    fs::OpenOptions,
    io::Write,
    os::windows::io::AsRawHandle,
    sync::atomic::{AtomicBool, Ordering},
};
use windows_sys::Win32::System::Console::*;
use zeroize::{Zeroize, Zeroizing};

static ACTIVE: AtomicBool = AtomicBool::new(false);
struct ConsoleGuard {
    input: std::fs::File,
    mode: u32,
}
impl Drop for ConsoleGuard {
    fn drop(&mut self) {
        // Clear secret/typeahead input before restoring the console's original mode.
        unsafe {
            FlushConsoleInputBuffer(self.input.as_raw_handle());
            SetConsoleMode(self.input.as_raw_handle(), self.mode);
        }
        ACTIVE.store(false, Ordering::Release);
    }
}
pub(super) fn read() -> Result<Zeroizing<Vec<u8>>> {
    read_with_prompt("Sync password: ")
}
pub(super) fn read_with_prompt(prompt: &'static str) -> Result<Zeroizing<Vec<u8>>> {
    if ACTIVE.swap(true, Ordering::AcqRel) {
        bail!("another protected terminal input is active");
    }
    let input = match OpenOptions::new().read(true).write(true).open("CONIN$") {
        Ok(input) => input,
        Err(_) => {
            ACTIVE.store(false, Ordering::Release);
            bail!("interactive unlock requires a console; use --password-stdin");
        }
    };
    let mut mode = 0;
    if unsafe { GetConsoleMode(input.as_raw_handle(), &mut mode) } == 0 {
        ACTIVE.store(false, Ordering::Release);
        bail!("protected console input unavailable");
    }
    let guard = ConsoleGuard { input, mode };
    if unsafe {
        SetConsoleMode(
            guard.input.as_raw_handle(),
            mode & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT),
        )
    } == 0
    {
        bail!("cannot disable console echo");
    }
    let mut output = OpenOptions::new().write(true).open("CONOUT$")?;
    output.write_all(prompt.as_bytes())?;
    output.flush()?;
    let mut units = Zeroizing::new(Vec::<u16>::with_capacity(1024));
    let mut unit = Zeroizing::new([0_u16; 1]);
    loop {
        let mut count = 0;
        if unsafe {
            ReadConsoleW(
                guard.input.as_raw_handle(),
                unit.as_mut_ptr().cast(),
                1,
                &mut count,
                std::ptr::null(),
            )
        } == 0
        {
            bail!("protected console read failed");
        }
        if count == 0 {
            continue;
        }
        match unit[0] {
            10 | 13 => break,
            3 | 4 | 26 | 27 => return Err(graft::e2ee::EncryptionError::Cancelled.into()),
            8 | 127 => {
                if let Some(last) = units.last_mut() {
                    last.zeroize();
                    units.pop();
                }
            }
            value if value < 32 => return Err(graft::e2ee::EncryptionError::InvalidPassword.into()),
            value => {
                if units.len() == 1024 {
                    return Err(graft::e2ee::EncryptionError::InvalidPassword.into());
                }
                units.push(value);
            }
        }
    }
    output.write_all(b"\r\n")?;
    let mut bytes = Zeroizing::new(Vec::with_capacity(1024));
    let mut encoded = Zeroizing::new([0_u8; 4]);
    for character in char::decode_utf16(units.iter().copied()) {
        let character = character.map_err(|_| graft::e2ee::EncryptionError::InvalidPassword)?;
        let text = character.encode_utf8(encoded.as_mut());
        if bytes.len() + text.len() > 1024 {
            return Err(graft::e2ee::EncryptionError::InvalidPassword.into());
        }
        bytes.extend_from_slice(text.as_bytes());
    }
    if bytes.is_empty() {
        return Err(graft::e2ee::EncryptionError::InvalidPassword.into());
    }
    Ok(bytes)
}
