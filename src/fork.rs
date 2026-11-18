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
//!
//! The process-global entry points (`init`, `library_info`, the runtime
//! lookup) first claim [`OWNER`], before they take any lock. A fork child
//! sees the parent's pid there. When the parent was initialising, had
//! initialised, or had a thread inside the library load at the moment of the
//! fork, the locks those steps hold may be held for ever in the child, so the
//! child gets [`Error::Forked`] instead of waiting. Otherwise the parent had at
//! most loaded the library, and the child takes the ownership over. A child
//! forked after a parent thread released such a lock but before it lowered
//! its counter is refused as well; the check errs on the safe side.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::error::{Error, Result};

static FORK_EPOCH: AtomicU64 = AtomicU64::new(0);
/// The atfork handler is installed (in this process or in an ancestor whose
/// memory this process inherited).
static REGISTERED: AtomicBool = AtomicBool::new(false);
/// Pid of the process whose thread is installing the handler.
static REGISTER_CLAIM: AtomicU32 = AtomicU32::new(0);

extern "C" fn on_fork_child() {
    FORK_EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// Install the atfork child handler once per process.
///
/// No `Once`: a child forked while a parent thread was inside the
/// registration would wait for ever on the inherited `Once`. The claim is
/// keyed by pid instead, so such a child registers again. A handler installed
/// twice only raises the epoch by two at each later fork, which every check
/// treats like one.
pub(crate) fn ensure_registered() {
    register_with(&REGISTERED, &REGISTER_CLAIM, MAX_REGISTER_SPINS, || {
        // SAFETY: the handler is an extern "C" function that performs one
        // atomic increment; it is async-signal-safe and never unwinds.
        let rc = unsafe { libc::pthread_atfork(None, None, Some(on_fork_child)) };
        // pthread_atfork fails only with ENOMEM. Without the handler the
        // getpid() comparison on cold paths still catches children.
        let _ = rc;
    });
}

/// How often a thread yields while another thread of its process holds the
/// registration claim before it installs the handler itself.
const MAX_REGISTER_SPINS: u32 = 1_000;

/// The registration steps over explicit cells (unit tests use their own).
fn register_with(done: &AtomicBool, claim: &AtomicU32, max_spins: u32, install: impl FnOnce()) {
    if done.load(Ordering::Acquire) {
        return;
    }
    let me = pid();
    let mut install = Some(install);
    let mut spins = 0;
    loop {
        if done.load(Ordering::Acquire) {
            return;
        }
        let current = claim.load(Ordering::Acquire);
        if current == me && spins < max_spins {
            // Another thread of this process is installing it right now; the
            // call is short and cannot block.
            spins += 1;
            std::thread::yield_now();
            continue;
        }
        // Past the bound the claim may also be stale: a pid reused by this
        // process after an ancestor with the same pid forked while inside
        // the registration. Install anyway; a doubled handler is harmless.
        // 0 (nobody) or another pid (a parent whose thread was inside the
        // registration at the fork): claim it for this process.
        if claim
            .compare_exchange(current, me, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            if let Some(f) = install.take() {
                f();
            }
            done.store(true, Ordering::Release);
            return;
        }
    }
}

/// Which process owns the crate's process-global state, and whether that
/// process was busy with it. See the module docs.
pub(crate) struct Owner {
    pid: AtomicU32,
    /// Threads between "about to take the init lock" and "released it".
    init_active: AtomicUsize,
    /// `init` completed or failed for good (the runtime or the fatal error is
    /// published).
    init_done: AtomicBool,
    /// Threads between "about to take the loader lock" and "released it".
    loader_active: AtomicUsize,
}

/// The owner of this process.
pub(crate) static OWNER: Owner = Owner::new();

/// Counts a thread inside a locked step of [`Owner`] until dropped.
pub(crate) struct Active<'a>(&'a AtomicUsize);

impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Owner {
    pub(crate) const fn new() -> Owner {
        Owner {
            pid: AtomicU32::new(0),
            init_active: AtomicUsize::new(0),
            init_done: AtomicBool::new(false),
            loader_active: AtomicUsize::new(0),
        }
    }

    /// Claim the process-global state for this process. Takes no lock and
    /// never waits. Returns [`Error::Forked`] in a fork child of a process
    /// that was initialising, had initialised, or was loading the library.
    pub(crate) fn claim(&self) -> Result<()> {
        self.claim_as(pid())
    }

    fn claim_as(&self, me: u32) -> Result<()> {
        loop {
            let owner = self.pid.load(Ordering::SeqCst);
            if owner == me {
                return Ok(());
            }
            if owner != 0 && self.busy() {
                // A thread raises a busy flag only after its own claim
                // succeeded. When the owner changed since the load above, a
                // thread of this process may have won the claim and raised
                // the flag already: look again instead of refusing.
                if self.pid.load(Ordering::SeqCst) != owner {
                    continue;
                }
                return Err(Error::Forked {
                    created_pid: owner,
                    current_pid: me,
                });
            }
            if self
                .pid
                .compare_exchange(owner, me, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    fn busy(&self) -> bool {
        self.init_active.load(Ordering::SeqCst) != 0
            || self.init_done.load(Ordering::SeqCst)
            || self.loader_active.load(Ordering::SeqCst) != 0
    }

    /// Call before taking the init lock; drop after releasing it.
    pub(crate) fn enter_init(&self) -> Active<'_> {
        self.init_active.fetch_add(1, Ordering::SeqCst);
        Active(&self.init_active)
    }

    /// Call before taking the loader lock; drop after releasing it.
    pub(crate) fn enter_loader(&self) -> Active<'_> {
        self.loader_active.fetch_add(1, Ordering::SeqCst);
        Active(&self.loader_active)
    }

    /// The runtime or the fatal init error was published.
    pub(crate) fn mark_init_done(&self) {
        self.init_done.store(true, Ordering::SeqCst);
    }
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
        // Ownership first. A refusal is not reported here. Callers that use
        // process-global state (`runtime()`, `init`) claim before they get
        // here and return the refusal there. A stamp made in a refused child
        // would carry that child's own epoch and pid and pass its own checks,
        // so the refusal must come from those callers, not from the stamp.
        let _ = OWNER.claim();
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

    #[test]
    fn owner_claim_rules() {
        let o = Owner::new();
        assert!(o.claim_as(100).is_ok(), "first claim");
        assert!(o.claim_as(100).is_ok(), "same process");
        // A child of an idle owner (nothing, or only a finished library
        // load) takes the ownership over.
        assert!(o.claim_as(200).is_ok());
        assert_eq!(o.pid.load(Ordering::SeqCst), 200);
        let forked = |r: Result<()>, created: u32, current: u32| {
            matches!(r, Err(Error::Forked { created_pid, current_pid })
                if created_pid == created && current_pid == current)
        };
        // Fork while a thread is inside the library load.
        {
            let _l = o.enter_loader();
            assert!(forked(o.claim_as(300), 200, 300));
            assert!(o.claim_as(200).is_ok(), "the owner itself is never refused");
        }
        assert!(o.claim_as(300).is_ok());
        // Fork while a thread is inside init (also the second of two).
        {
            let _a = o.enter_init();
            let _b = o.enter_init();
            assert!(forked(o.claim_as(400), 300, 400));
        }
        // A retryable failure leaves nothing behind.
        assert!(o.claim_as(400).is_ok());
        // Fork after init completed or failed for good.
        o.mark_init_done();
        assert!(forked(o.claim_as(500), 400, 500));
        assert_eq!(o.pid.load(Ordering::SeqCst), 400);
    }

    /// Two threads of a child whose inherited owner is the parent make their
    /// first claim at once. The loser of the claim must not be refused
    /// because the winner raised a busy counter in the meantime.
    #[test]
    fn concurrent_first_claims_in_a_takeover_child() {
        use std::sync::{Arc, Barrier};
        const ROUNDS: usize = 20_000;
        const PARENT: u32 = 99;
        const CHILD: u32 = 100;
        let owners: Arc<Vec<Owner>> = Arc::new((0..ROUNDS).map(|_| Owner::new()).collect());
        for o in owners.iter() {
            o.pid.store(PARENT, Ordering::SeqCst);
        }
        let barrier = Arc::new(Barrier::new(2));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let owners = Arc::clone(&owners);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let mut refused = 0;
                    for o in owners.iter() {
                        barrier.wait();
                        if o.claim_as(CHILD).is_err() {
                            refused += 1;
                        }
                        let _loader = o.enter_loader();
                        // Hold the counter until both threads have claimed.
                        barrier.wait();
                    }
                    refused
                })
            })
            .collect();
        let refused: usize = workers.into_iter().map(|w| w.join().unwrap()).sum();
        assert_eq!(refused, 0, "spurious Forked in the owning process");
        assert!(owners.iter().all(|o| o.pid.load(Ordering::SeqCst) == CHILD));
    }

    /// A claim that names this process but that no thread of it holds (a
    /// reused pid) does not make the caller wait for ever.
    #[test]
    fn stale_claim_of_own_pid_is_taken_after_the_bound() {
        let done = AtomicBool::new(false);
        let claim = AtomicU32::new(pid());
        let mut installs = 0;
        register_with(&done, &claim, MAX_REGISTER_SPINS, || installs += 1);
        assert_eq!(installs, 1);
        assert!(done.load(Ordering::SeqCst));
    }

    /// A child forked while a parent thread is inside the atfork
    /// registration registers again and does not wait (a `Once` would).
    #[test]
    fn fork_during_registration_does_not_wait() {
        use std::time::{Duration, Instant};
        static DONE: AtomicBool = AtomicBool::new(false);
        static CLAIM: AtomicU32 = AtomicU32::new(0);
        static ENTERED: AtomicBool = AtomicBool::new(false);
        static RELEASE: AtomicBool = AtomicBool::new(false);
        static INSTALLS: AtomicU32 = AtomicU32::new(0);
        let t = std::thread::spawn(|| {
            register_with(&DONE, &CLAIM, u32::MAX, || {
                INSTALLS.fetch_add(1, Ordering::SeqCst);
                ENTERED.store(true, Ordering::SeqCst);
                while !RELEASE.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ENTERED.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "registration never entered");
            std::thread::sleep(Duration::from_millis(1));
        }
        // SAFETY: the child uses atomics, getpid, clock_gettime and _exit.
        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            // A hanging child ends by SIGALRM instead of blocking waitpid.
            // SAFETY: alarm has no preconditions.
            unsafe { libc::alarm(10) };
            let start = Instant::now();
            register_with(&DONE, &CLAIM, u32::MAX, || {
                INSTALLS.fetch_add(1, Ordering::SeqCst);
            });
            let ok = DONE.load(Ordering::SeqCst)
                && INSTALLS.load(Ordering::SeqCst) == 2
                && start.elapsed() < Duration::from_secs(2);
            // SAFETY: terminate the child without running anything else.
            unsafe { libc::_exit(if ok { 0 } else { 1 }) };
        }
        // A second thread of this process waits for the first one instead
        // of installing again.
        let t2 = std::thread::spawn(|| {
            register_with(&DONE, &CLAIM, u32::MAX, || {
                INSTALLS.fetch_add(1, Ordering::SeqCst);
            })
        });
        let mut status = 0;
        // SAFETY: waiting for our own child.
        unsafe { libc::waitpid(child, &mut status, 0) };
        RELEASE.store(true, Ordering::SeqCst);
        t.join().unwrap();
        t2.join().unwrap();
        assert!(libc::WIFEXITED(status));
        assert_eq!(
            libc::WEXITSTATUS(status),
            0,
            "child waited or registered wrongly"
        );
        assert!(DONE.load(Ordering::SeqCst));
        assert_eq!(INSTALLS.load(Ordering::SeqCst), 1, "parent installed once");
    }
}
