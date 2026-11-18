// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Fork guard.
//!
//! libdaos and DFS mount state do not survive `fork`: a read-write mount keeps
//! its object ID allocator in the `dfs_t`, so a parent and a child would hand
//! out the same object IDs, and locks held by parent threads stay locked in
//! the child. The crate therefore refuses every use in a fork child.
//!
//! A `pthread_atfork` child handler increments [`FORK_EPOCH`] and does nothing
//! else. Each runtime and mount records the epoch and pid it was created in;
//! every entry point compares the epoch first (one atomic load, before any
//! lock), and cold paths also compare `getpid()`, which catches children made
//! with a raw `clone` that skips atfork handlers.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;

use crate::error::{Error, Result};

static FORK_EPOCH: AtomicU64 = AtomicU64::new(0);
static REGISTER: Once = Once::new();

extern "C" fn on_fork_child() {
    FORK_EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// Install the atfork child handler once per process.
pub(crate) fn ensure_registered() {
    REGISTER.call_once(|| {
        // SAFETY: the handler is an extern "C" function that performs one
        // atomic increment; it is async-signal-safe and never unwinds.
        let rc = unsafe { libc::pthread_atfork(None, None, Some(on_fork_child)) };
        // pthread_atfork fails only with ENOMEM. Without the handler the
        // getpid() comparison on cold paths still catches children.
        let _ = rc;
    });
}

/// Current fork epoch of this process.
pub(crate) fn epoch() -> u64 {
    FORK_EPOCH.load(Ordering::Relaxed)
}

/// Current process id.
pub(crate) fn pid() -> u32 {
    // SAFETY: getpid has no preconditions.
    unsafe { libc::getpid() as u32 }
}

/// Where a runtime or mount was created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stamp {
    pub(crate) epoch: u64,
    pub(crate) pid: u32,
}

impl Stamp {
    /// Stamp for "now". Registers the atfork handler first so that a later
    /// fork is always counted.
    pub(crate) fn now() -> Stamp {
        ensure_registered();
        Stamp {
            epoch: epoch(),
            pid: pid(),
        }
    }

    /// Hot-path check: one atomic load.
    #[inline]
    pub(crate) fn check(&self) -> Result<()> {
        if FORK_EPOCH.load(Ordering::Relaxed) == self.epoch {
            Ok(())
        } else {
            Err(self.forked())
        }
    }

    /// Cold-path check: epoch and pid.
    pub(crate) fn check_cold(&self) -> Result<()> {
        self.check()?;
        if pid() == self.pid {
            Ok(())
        } else {
            Err(self.forked())
        }
    }

    /// True when this process is not the one the stamp was made in.
    pub(crate) fn is_foreign(&self) -> bool {
        self.check_cold().is_err()
    }

    fn forked(&self) -> Error {
        Error::Forked {
            created_pid: self.pid,
            current_pid: pid(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_matches_in_same_process() {
        let s = Stamp::now();
        assert!(s.check().is_ok());
        assert!(s.check_cold().is_ok());
        assert!(!s.is_foreign());
    }

    #[test]
    fn foreign_pid_is_detected() {
        let s = Stamp {
            epoch: epoch(),
            pid: pid().wrapping_add(1),
        };
        assert!(s.check().is_ok(), "hot path looks at the epoch only");
        assert!(matches!(s.check_cold(), Err(Error::Forked { .. })));
    }

    #[test]
    fn epoch_increments_in_a_real_fork_child() {
        ensure_registered();
        let before = epoch();
        // SAFETY: the child only reads an atomic and calls _exit.
        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            let ok = epoch() == before + 1;
            unsafe { libc::_exit(if ok { 0 } else { 1 }) };
        }
        let mut status = 0;
        // SAFETY: waiting for our own child.
        unsafe { libc::waitpid(child, &mut status, 0) };
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
        assert_eq!(epoch(), before, "the parent epoch does not change");
    }
}
