//! Bounded foreground terminal input. No key-owning work starts before restoration.
use graft::e2ee::EncryptionError;
use rustix::termios::{
    OptionalActions, SpecialCodeIndex, Termios, tcgetattr, tcgetpgrp, tcsetattr,
};
use std::{
    fs::{File, OpenOptions},
    io::{self, IsTerminal, Read, Write},
    sync::atomic::{AtomicBool, Ordering},
};
use zeroize::{Zeroize, Zeroizing};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static CANCELLED: AtomicBool = AtomicBool::new(false);
extern "C" fn cancel(_: libc::c_int) {
    CANCELLED.store(true, Ordering::Relaxed);
}

struct TerminalGuard {
    tty: File,
    original: Termios,
    handlers: Vec<(libc::c_int, libc::sigaction)>,
    restored: bool,
}
impl TerminalGuard {
    fn open() -> io::Result<Self> {
        let tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        if !tty.is_terminal() || tcgetpgrp(&tty)? != rustix::process::getpgrp() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "foreground terminal required",
            ));
        }
        let original = tcgetattr(&tty)?;
        if ACTIVE.swap(true, Ordering::AcqRel) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "terminal input already active",
            ));
        }
        CANCELLED.store(false, Ordering::Relaxed);
        let mut guard = Self {
            tty,
            original,
            handlers: Vec::new(),
            restored: false,
        };
        for signal in [
            libc::SIGINT,
            libc::SIGTERM,
            libc::SIGHUP,
            libc::SIGQUIT,
            libc::SIGTSTP,
        ] {
            // SAFETY: sigaction is POD; sigemptyset initializes the mask. The
            // handler only stores to a lock-free atomic, never allocates or locks.
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = cancel as *const () as usize;
            let mut previous = std::mem::MaybeUninit::<libc::sigaction>::uninit();
            // SAFETY: action and previous are valid pointers for sigaction, and
            // each installed action is retained for restoration before threads start.
            unsafe {
                if libc::sigemptyset(&mut action.sa_mask) != 0
                    || libc::sigaction(signal, &action, previous.as_mut_ptr()) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                guard.handlers.push((signal, previous.assume_init()));
            }
        }
        let mut hidden = guard.original.clone();
        hidden.make_raw();
        hidden.special_codes[SpecialCodeIndex::VMIN] = 0;
        hidden.special_codes[SpecialCodeIndex::VTIME] = 1;
        tcsetattr(&guard.tty, OptionalActions::Flush, &hidden)?;
        Ok(guard)
    }
    fn restore(&mut self) -> io::Result<()> {
        // Flush queued secret/typeahead bytes before restoring shell echo.
        let terminal = tcsetattr(&self.tty, OptionalActions::Flush, &self.original);
        while let Some((signal, previous)) = self.handlers.last() {
            // SAFETY: previous was returned by sigaction for this exact signal.
            if unsafe { libc::sigaction(*signal, previous, std::ptr::null_mut()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            self.handlers.pop();
        }
        self.restored = terminal.is_ok();
        ACTIVE.store(false, Ordering::Release);
        terminal.map_err(io::Error::from)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if !self.restored {
            let _ = self.restore();
        }
    }
}

fn erase_last(bytes: &mut Vec<u8>) {
    let end = std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| s.char_indices().last().map(|(i, _)| i))
        .unwrap_or_else(|| bytes.len().saturating_sub(1));
    bytes[end..].zeroize();
    bytes.truncate(end);
}

fn input(
    reader: &mut impl Read,
    cancelled: impl Fn() -> bool,
) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    let mut bytes = Zeroizing::new(Vec::with_capacity(1024));
    let mut byte = Zeroizing::new([0; 1]);
    loop {
        if cancelled() {
            return Err(EncryptionError::Cancelled.into());
        }
        match reader.read(&mut *byte) {
            Ok(0) => continue, // VTIME timeout; check signals again without unbounded blocking.
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(graft::e2ee::EncryptedHttpError::LocalIo(error.kind()).into()),
        }
        match byte[0] {
            b'\r' | b'\n' => break,
            3 | 4 | 26 | 27 => return Err(EncryptionError::Cancelled.into()),
            8 | 127 => erase_last(&mut bytes),
            21 => bytes.zeroize(),
            value if value < 32 => return Err(EncryptionError::InvalidPassword.into()),
            value => {
                if bytes.len() == 1024 {
                    return Err(EncryptionError::InvalidPassword.into());
                }
                bytes.push(value);
            }
        }
    }
    if cancelled() {
        return Err(EncryptionError::Cancelled.into());
    }
    if bytes.is_empty() || std::str::from_utf8(&bytes).is_err() {
        return Err(EncryptionError::InvalidPassword.into());
    }
    Ok(bytes)
}

pub(super) fn read() -> anyhow::Result<Zeroizing<Vec<u8>>> {
    read_with_prompt("Sync password: ")
}

pub(super) fn read_with_prompt(prompt: &'static str) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    let mut guard=TerminalGuard::open().map_err(|_|anyhow::anyhow!(
        "interactive unlock requires a usable foreground terminal; use --password-stdin for protected automation input"))?;
    let result = (|| {
        guard.tty.write_all(prompt.as_bytes())?;
        guard.tty.flush()?;
        let status = guard.tty.try_clone()?;
        input(&mut guard.tty, || {
            CANCELLED.load(Ordering::Relaxed) || tcgetpgrp(&status).is_err()
        })
    })();
    guard
        .restore()
        .map_err(|_| anyhow::anyhow!("failed to restore password terminal; unlock stopped"))?;
    guard.tty.write_all(b"\n")?;
    if CANCELLED.load(Ordering::Relaxed) {
        return Err(EncryptionError::Cancelled.into());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interactive_input_preserves_spaces_and_unicode_and_edits_without_secret_output() {
        assert_eq!(
            &*input(&mut " 密码 \n".as_bytes(), || false).unwrap(),
            " 密码 ".as_bytes()
        );
        assert_eq!(
            &*input(&mut "密码\x7f字\n".as_bytes(), || false).unwrap(),
            "密字".as_bytes()
        );
        assert_eq!(
            &*input(&mut b"discard\x15valid\n".as_slice(), || false).unwrap(),
            b"valid"
        );
    }
    #[test]
    fn interactive_input_is_bounded_and_cancelled_before_unlock() {
        let mut maximum = vec![b'x'; 1024];
        maximum.push(b'\n');
        assert_eq!(
            input(&mut maximum.as_slice(), || false).unwrap().len(),
            1024
        );
        let over = vec![b'x'; 1025];
        assert_eq!(
            input(&mut over.as_slice(), || false)
                .unwrap_err()
                .downcast::<EncryptionError>()
                .unwrap(),
            EncryptionError::InvalidPassword
        );
        for mut invalid in [b"\n".as_slice(), b"\xff\n", b"\0\n"] {
            assert_eq!(
                input(&mut invalid, || false)
                    .unwrap_err()
                    .downcast::<EncryptionError>()
                    .unwrap(),
                EncryptionError::InvalidPassword
            );
        }
        for mut cancel in [b"secret\x03".as_slice(), b"\x04", b"\x1a", b"\x1b"] {
            assert_eq!(
                input(&mut cancel, || false)
                    .unwrap_err()
                    .downcast::<EncryptionError>()
                    .unwrap(),
                EncryptionError::Cancelled
            );
        }
        assert_eq!(
            input(&mut b"secret\n".as_slice(), || true)
                .unwrap_err()
                .downcast::<EncryptionError>()
                .unwrap(),
            EncryptionError::Cancelled
        );
    }
}
