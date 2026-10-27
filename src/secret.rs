use std::ffi::c_void;
use std::ptr::NonNull;

use nix::sys::mman::{mlock, munlock};
use nix::sys::prctl;
use zeroize::Zeroize;

/// Keeps the process out of core dumps (and ptrace by other processes of
/// the user that aren't its parent).
pub fn harden() {
    let _ = prctl::set_dumpable(false);
}

/// Clipboard content held after the app handed it over: locked in RAM
/// (best effort, RLIMIT_MEMLOCK permitting) and zeroed when dropped.
/// mlockall isn't used, as the daemon's thread stacks would soon exhaust
/// the default 8 MiB limit and make allocations fail.
pub struct Secret {
    bytes: Vec<u8>,
    // How many bytes mlock covered (0 if it failed).
    locked: usize,
}

impl Secret {
    pub fn new(mut bytes: Vec<u8>) -> Self {
        bytes.shrink_to_fit();
        let locked = match NonNull::new(bytes.as_mut_ptr().cast::<c_void>()) {
            Some(ptr) if !bytes.is_empty() && unsafe { mlock(ptr, bytes.len()) }.is_ok() => {
                bytes.len()
            }
            _ => 0,
        };
        Self { bytes, locked }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The bytes from `from` on, to read into.
    pub fn as_mut_bytes(&mut self, from: usize) -> &mut [u8] {
        &mut self.bytes[from..]
    }

    pub fn copy(&self) -> Self {
        Self::new(self.bytes.clone())
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.bytes.zeroize();
        if self.locked > 0
            && let Some(ptr) = NonNull::new(self.bytes.as_mut_ptr().cast::<c_void>())
        {
            let _ = unsafe { munlock(ptr, self.locked) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_bytes() {
        let secret = Secret::new(b"hunter2".to_vec());
        assert_eq!(secret.as_bytes(), b"hunter2");
        assert_eq!(secret.copy().as_bytes(), b"hunter2");
    }

    #[test]
    fn empty_is_fine() {
        assert!(Secret::new(Vec::new()).as_bytes().is_empty());
    }
}
