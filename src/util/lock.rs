//! Single-instance lock based on `flock(2)`.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;

/// Holds the lock for as long as the value lives.
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

#[derive(Debug)]
pub enum LockError {
    Held,
    Io(std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Held => write!(f, "another hwstats instance holds the lock"),
            LockError::Io(e) => write!(f, "cannot open lock file: {e}"),
        }
    }
}

pub fn acquire(path: &Path) -> Result<InstanceLock, LockError> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(LockError::Io)?;
    // SAFETY: valid fd owned by `file`.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Err(LockError::Held);
        }
        return Err(LockError::Io(err));
    }
    Ok(InstanceLock { _file: file })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_fails() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.lock");
        let a = acquire(&p).unwrap();
        assert!(matches!(acquire(&p), Err(LockError::Held)));
        drop(a);
        assert!(acquire(&p).is_ok());
    }
}
