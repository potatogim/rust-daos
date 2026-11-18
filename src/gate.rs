// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The close gate shared by DFS mounts and container connections.
//!
//! Every operation that passes a handle to libdaos or libdfs holds the gate
//! shared and checks the closed flag. `close` takes the gate exclusively
//! (polling with backoff up to a timeout), marks it closed and runs the
//! teardown while it holds it. A handle therefore never reaches the library
//! after the teardown has released it.
//!
//! While a `close` waits for the gate, new operations are refused with
//! [`Error::Closed`]; a close that times out returns [`Error::Busy`] and
//! leaves the gate open again.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{RwLock, RwLockReadGuard, TryLockError};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

thread_local! {
    /// Gates this thread holds shared. A nested entry (a handle released
    /// while an operation of the same thread runs) must not take the read
    /// lock a second time.
    static HELD: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// Outcome of the check a closing call runs with the gate held exclusively.
pub(crate) enum Check {
    /// Close now.
    Ready,
    /// Not yet: release the gate, wait and check again until the deadline;
    /// then return this error.
    Wait(Error),
    /// Give up at once with this error; nothing is closed.
    Fail(Error),
}

/// Close gate.
pub(crate) struct Gate {
    /// `true` once closed (the same value as `closed`, written under the
    /// exclusive lock).
    lock: RwLock<bool>,
    /// The closed flag for readers that must not wait for a running close
    /// (registry lookups, `is_closed`).
    closed: AtomicBool,
    /// Number of `close` calls waiting for the gate; while non-zero, new
    /// operations are refused with `Closed`.
    closers: AtomicUsize,
    in_flight: AtomicUsize,
}

/// Permission to use the handles behind a gate; held for one operation.
pub(crate) struct Pass<'a> {
    gate: &'a Gate,
    guard: Option<RwLockReadGuard<'a, bool>>,
}

impl Drop for Pass<'_> {
    fn drop(&mut self) {
        if self.guard.take().is_some() {
            self.gate.in_flight.fetch_sub(1, Ordering::SeqCst);
            let key = self.gate.key();
            // try_with: during thread teardown the TLS slot may be gone.
            let _ = HELD.try_with(|h| {
                let mut h = h.borrow_mut();
                if let Some(i) = h.iter().rposition(|k| *k == key) {
                    h.remove(i);
                }
            });
        }
    }
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    pub(crate) fn new() -> Gate {
        Gate {
            lock: RwLock::new(false),
            closed: AtomicBool::new(false),
            closers: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
        }
    }

    fn key(&self) -> usize {
        self as *const Gate as usize
    }

    /// Enter an operation. `for_release`: releasing a handle is still allowed
    /// while a `close` waits (it only shortens the work of that close).
    pub(crate) fn enter(&self, for_release: bool) -> Result<Pass<'_>> {
        let key = self.key();
        // try_with: during thread teardown the TLS slot may be gone; treat
        // that as "not nested".
        let nested = HELD
            .try_with(|h| h.borrow().contains(&key))
            .unwrap_or(false);
        if nested {
            // This thread already holds the gate, so it cannot close
            // underneath; the outer operation checked the flag.
            return Ok(Pass {
                gate: self,
                guard: None,
            });
        }
        let closing = || !for_release && self.closers.load(Ordering::SeqCst) > 0;
        if closing() {
            return Err(Error::Closed);
        }
        let guard = self.lock.read().unwrap_or_else(|e| e.into_inner());
        if *guard || closing() {
            return Err(Error::Closed);
        }
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        let _ = HELD.try_with(|h| h.borrow_mut().push(key));
        Ok(Pass {
            gate: self,
            guard: Some(guard),
        })
    }

    /// True once a close has marked the gate closed. Never waits: a close
    /// that is still waiting or tearing down does not block the caller.
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Closed flag through exclusive access (for `Drop`).
    pub(crate) fn is_closed_mut(&mut self) -> bool {
        *self.closed.get_mut()
    }

    /// True while a `close` waits for the gate.
    pub(crate) fn closing(&self) -> bool {
        self.closers.load(Ordering::SeqCst) > 0
    }

    /// Wait until no `close` waits for the gate any more. Every close has a
    /// deadline, so this ends.
    pub(crate) fn wait_closers(&self) {
        let mut backoff = Duration::from_micros(200);
        while self.closing() {
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_millis(20));
        }
    }

    /// Refuse new operations, wait up to `timeout` for running ones, then mark
    /// the gate closed and run `teardown` while holding it exclusively.
    ///
    /// Returns `Ok(None)` when the gate was already closed, `Ok(Some(_))` with
    /// the result of `teardown` otherwise, and [`Error::Busy`] when operations
    /// still run at the deadline (the gate then accepts operations again).
    pub(crate) fn close<T>(
        &self,
        timeout: Duration,
        teardown: impl FnOnce() -> T,
    ) -> Result<Option<T>> {
        self.close_when(timeout, |_| Check::Ready, teardown)
    }

    /// As [`Gate::close`], but the gate is only closed when `precheck`
    /// returns [`Check::Ready`] with the gate held exclusively. `precheck`
    /// gets the deadline. On [`Check::Wait`] the gate is released again and
    /// the check repeated until the deadline; then that error is returned.
    /// On [`Check::Fail`] its error is returned at once. In both cases the
    /// gate is not marked closed.
    pub(crate) fn close_when<T>(
        &self,
        timeout: Duration,
        precheck: impl FnMut(Instant) -> Check,
        teardown: impl FnOnce() -> T,
    ) -> Result<Option<T>> {
        self.closers.fetch_add(1, Ordering::SeqCst);
        let result = self.close_wait(timeout, precheck, teardown);
        self.closers.fetch_sub(1, Ordering::SeqCst);
        result
    }

    fn close_wait<T>(
        &self,
        timeout: Duration,
        mut precheck: impl FnMut(Instant) -> Check,
        teardown: impl FnOnce() -> T,
    ) -> Result<Option<T>> {
        let deadline = Instant::now() + timeout;
        let mut backoff = Duration::from_micros(200);
        loop {
            let guard = match self.lock.try_write() {
                Ok(g) => Some(g),
                Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
                Err(TryLockError::WouldBlock) => None,
            };
            let refused = match guard {
                Some(mut guard) => {
                    if *guard {
                        return Ok(None);
                    }
                    match precheck(deadline) {
                        Check::Ready => {
                            *guard = true;
                            self.closed.store(true, Ordering::SeqCst);
                            return Ok(Some(teardown()));
                        }
                        Check::Wait(e) => e,
                        Check::Fail(e) => return Err(e),
                    }
                }
                None => Error::busy(self.in_flight.load(Ordering::SeqCst), 0),
            };
            let now = Instant::now();
            if now >= deadline {
                return Err(refused);
            }
            std::thread::sleep(backoff.min(deadline - now));
            backoff = (backoff * 2).min(Duration::from_millis(20));
        }
    }
}
