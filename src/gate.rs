// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The close gate shared by DFS mounts and container connections.
//!
//! Every operation that passes a handle to libdaos or libdfs holds the gate
//! shared and checks the closed flag. A close takes the gate exclusively
//! (polling with backoff up to a deadline, never blocking on it), marks it
//! closed and runs the teardown while it holds it. A handle therefore never
//! reaches the library after the teardown has released it.
//!
//! While a close waits for the gate, new operations are refused with
//! [`Error::Closed`]; a close that ends with an error leaves the gate open
//! again once the last waiting close has returned.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

thread_local! {
    /// Gates this thread holds shared. A nested entry (a handle released
    /// while an operation of the same thread runs) must not take the read
    /// lock a second time.
    static HELD: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// The current time, as deadlines see it.
fn now() -> Instant {
    #[cfg(all(test, feature = "mock"))]
    if let Some(t) = testing::clock() {
        return t;
    }
    Instant::now()
}

/// A point in time after which a close stops waiting. `None` inside means
/// never: the timeout did not fit an `Instant`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Deadline {
    at: Option<Instant>,
    #[cfg(all(test, feature = "mock"))]
    pub(crate) id: u64,
}

impl Deadline {
    fn make(at: Option<Instant>) -> Deadline {
        Deadline {
            at,
            #[cfg(all(test, feature = "mock"))]
            id: testing::next_id(),
        }
    }

    /// `timeout` from now; never, when that does not fit an `Instant`.
    pub(crate) fn after(timeout: Duration) -> Deadline {
        Deadline::make(now().checked_add(timeout))
    }

    /// A deadline that has passed already: one try, no waiting.
    pub(crate) fn passed() -> Deadline {
        Deadline::make(Some(now()))
    }

    /// True once the deadline has been reached.
    pub(crate) fn expired(&self) -> bool {
        match self.at {
            None => false,
            Some(at) => now() >= at,
        }
    }

    /// Time left, at most `cap`.
    fn left(&self, cap: Duration) -> Duration {
        match self.at {
            None => cap,
            Some(at) => cap.min(at.saturating_duration_since(now())),
        }
    }
}

/// Outcome of the check a closing call runs with the gate held exclusively.
#[derive(Debug)]
pub(crate) enum Check {
    /// Close now.
    Ready,
    /// Not yet: release the gate, wait and check again until the deadline.
    Wait(Error),
    /// Give up at once with this error; nothing is closed.
    Fail(Error),
}

/// How the last round before the deadline ended.
#[derive(Debug)]
pub(crate) enum Last {
    /// The gate was held by others; the gate's own `Busy` with its running
    /// calls.
    Contended(Error),
    /// The gate was taken and the check asked to wait, with this reason.
    Waiting(Error),
}

impl Last {
    /// The error this round stands for.
    pub(crate) fn reason(self) -> Error {
        match self {
            Last::Contended(e) | Last::Waiting(e) => e,
        }
    }
}

/// What a close returns at its deadline.
pub(crate) enum Final {
    /// Return this error.
    Error(Error),
    /// Nothing stands in the way now: try once more, without sleeping.
    Retry,
}

/// Close gate.
pub(crate) struct Gate {
    /// `true` once closed (the same value as `closed`, written under the
    /// exclusive lock).
    lock: RwLock<bool>,
    /// The closed flag for readers that must not wait for a running close
    /// (registry lookups, `is_closed`).
    closed: AtomicBool,
    /// Number of closes waiting for the gate, and of holds that refuse new
    /// operations for a close (see [`Gate::raise_closers`]); while non-zero,
    /// new operations are refused with `Closed`.
    closers: AtomicUsize,
    in_flight: AtomicUsize,
    /// Sleeps of closes on this gate.
    #[cfg(all(test, feature = "mock"))]
    pub(crate) waits: AtomicUsize,
    /// Raises of `closers`.
    #[cfg(all(test, feature = "mock"))]
    pub(crate) raises: AtomicUsize,
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

/// Lowers the count of waiting closes when dropped, also on a panic.
struct Closer<'a>(&'a Gate);

impl Drop for Closer<'_> {
    fn drop(&mut self) {
        self.0.lower_closers();
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
            #[cfg(all(test, feature = "mock"))]
            waits: AtomicUsize::new(0),
            #[cfg(all(test, feature = "mock"))]
            raises: AtomicUsize::new(0),
        }
    }

    pub(crate) fn key(&self) -> usize {
        self as *const Gate as usize
    }

    /// Enter an operation. `for_release`: releasing a handle is still allowed
    /// while a close waits (it only shortens the work of that close).
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
        #[cfg(all(test, feature = "mock"))]
        testing::run("BeforeSharedLock", key, &());
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

    /// True while a close waits for the gate.
    pub(crate) fn closing(&self) -> bool {
        self.closers.load(Ordering::SeqCst) > 0
    }

    /// The number of waiting closes and holds.
    #[cfg(all(test, feature = "mock"))]
    pub(crate) fn closers(&self) -> usize {
        self.closers.load(Ordering::SeqCst)
    }

    /// Hold the gate exclusively until the guard drops.
    #[cfg(all(test, feature = "mock"))]
    pub(crate) fn hold(&self) -> RwLockWriteGuard<'_, bool> {
        self.lock.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Refuse new operations (as a waiting close does) until
    /// [`Gate::lower_closers`] is called once for this call. Takes no lock.
    pub(crate) fn raise_closers(&self) {
        self.closers.fetch_add(1, Ordering::SeqCst);
        #[cfg(all(test, feature = "mock"))]
        self.raises.fetch_add(1, Ordering::SeqCst);
    }

    /// Undo one [`Gate::raise_closers`].
    pub(crate) fn lower_closers(&self) {
        self.closers.fetch_sub(1, Ordering::SeqCst);
    }

    /// Wait until no close waits for the gate any more. Every close has a
    /// deadline, so this ends.
    pub(crate) fn wait_closers(&self) {
        let mut backoff = Duration::from_micros(200);
        while self.closing() {
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_millis(20));
        }
    }

    /// Close the gate when `precheck` returns [`Check::Ready`] with the gate
    /// held exclusively, and only then. The precheck gets the deadline. On
    /// [`Check::Wait`] the gate is released again and the check repeated
    /// until the deadline; then that error is returned. On [`Check::Fail`]
    /// its error is returned at once. In both cases the gate is not marked
    /// closed.
    ///
    /// Returns `Ok(None)` when the gate was already closed and `Ok(Some(_))`
    /// with the result of `teardown` otherwise.
    pub(crate) fn close_when<T>(
        &self,
        timeout: Duration,
        precheck: impl FnMut(Deadline) -> Check,
        teardown: impl FnOnce() -> T,
    ) -> Result<Option<T>> {
        self.close_by(
            Deadline::after(timeout),
            || None,
            precheck,
            |last, _| Final::Error(last.reason()),
            teardown,
        )
    }

    /// The exclusive guard, if no one holds the gate. A poisoned lock is
    /// taken as it is: the flag it guards is a plain `bool`.
    fn try_take(&self) -> Option<RwLockWriteGuard<'_, bool>> {
        match self.lock.try_write() {
            Ok(g) => Some(g),
            Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    /// The gate's own `Busy`: the operations that hold it now.
    fn busy(&self) -> Error {
        Error::busy(self.in_flight.load(Ordering::SeqCst), 0)
    }

    fn expired(&self, deadline: &Deadline) -> bool {
        let expired = deadline.expired();
        #[cfg(all(test, feature = "mock"))]
        testing::note_check(self.key(), deadline.id, expired);
        expired
    }

    /// Refuse new operations, wait until `deadline` for the gate, then mark
    /// it closed and run `teardown` while holding it exclusively.
    ///
    /// Each round tries the gate once. A round that cannot take it asks
    /// `standing()`, and an error from it ends the call. A round that takes
    /// it runs `precheck`: [`Check::Ready`] closes, [`Check::Fail`] ends the
    /// call, [`Check::Wait`] ends the round. When the deadline has passed
    /// after a round, `at_deadline(last, false)` decides, with no guard of
    /// this gate held: [`Final::Error`] is returned; [`Final::Retry`] tries
    /// the gate once more at once (taken: the precheck decides, and a wait
    /// returns its reason; not taken: `at_deadline(Contended, true)` gives
    /// the error). Nothing sleeps after the deadline.
    ///
    /// Returns `Ok(None)` when the gate was already closed and `Ok(Some(_))`
    /// with the result of `teardown` otherwise.
    pub(crate) fn close_by<T>(
        &self,
        deadline: Deadline,
        standing: impl Fn() -> Option<Error>,
        mut precheck: impl FnMut(Deadline) -> Check,
        mut at_deadline: impl FnMut(Last, bool) -> Final,
        teardown: impl FnOnce() -> T,
    ) -> Result<Option<T>> {
        #[cfg(all(test, feature = "mock"))]
        testing::run("InnerGateEntered", 0, &deadline);
        self.raise_closers();
        let _closer = Closer(self);
        let mut backoff = Duration::from_micros(200);
        loop {
            let last = match self.try_take() {
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
                        Check::Wait(e) => Last::Waiting(e),
                        Check::Fail(e) => return Err(e),
                    }
                }
                None => {
                    if let Some(e) = standing() {
                        return Err(e);
                    }
                    #[cfg(all(test, feature = "mock"))]
                    testing::run("GateRoundBlocked", self.key(), &());
                    Last::Contended(self.busy())
                }
            };
            if self.expired(&deadline) {
                #[cfg(all(test, feature = "mock"))]
                testing::run("BeforeDeadlineVerdict", self.key(), &());
                match at_deadline(last, false) {
                    Final::Error(e) => return Err(e),
                    Final::Retry => {}
                }
                #[cfg(all(test, feature = "mock"))]
                testing::run("BeforeLastAcquire", self.key(), &());
                return match self.try_take() {
                    Some(mut guard) => {
                        if *guard {
                            return Ok(None);
                        }
                        match precheck(deadline) {
                            Check::Ready => {
                                *guard = true;
                                self.closed.store(true, Ordering::SeqCst);
                                Ok(Some(teardown()))
                            }
                            Check::Wait(e) | Check::Fail(e) => Err(e),
                        }
                    }
                    None => {
                        let busy = self.busy();
                        match at_deadline(Last::Contended(busy.clone()), true) {
                            Final::Error(e) => Err(e),
                            Final::Retry => Err(busy),
                        }
                    }
                };
            }
            #[cfg(all(test, feature = "mock"))]
            {
                testing::run("BeforeSleep", self.key(), &());
                self.waits.fetch_add(1, Ordering::SeqCst);
            }
            std::thread::sleep(deadline.left(backoff));
            backoff = (backoff * 2).min(Duration::from_millis(20));
        }
    }
}

/// Test instrumentation: hooks run at named points, a clock that stands
/// still, deadline ids and a recorder of expiry checks.
#[cfg(all(test, feature = "mock"))]
pub(crate) mod testing {
    use std::any::Any;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    type HookFn = Rc<RefCell<dyn FnMut(&dyn Any)>>;

    struct Hook {
        name: &'static str,
        key: Option<usize>,
        once: bool,
        f: HookFn,
    }

    /// One note of the deadline recorder.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Note {
        /// A public close made the deadline with this id.
        Made(&'static str, u64),
        /// A gate (its key) checked the deadline with this id.
        Check(usize, u64, bool),
        /// A container close handed the deadline with this id to the gate of
        /// a mount.
        Handed(u64),
    }

    thread_local! {
        static HOOKS: RefCell<Vec<Hook>> = const { RefCell::new(Vec::new()) };
        static CLOCK: Cell<Option<Instant>> = const { Cell::new(None) };
        static NOTES: RefCell<Option<Vec<Note>>> = const { RefCell::new(None) };
        /// A gate whose expiry check must find the deadline passed.
        static MUST_EXPIRE: Cell<Option<usize>> = const { Cell::new(None) };
    }

    static IDS: AtomicU64 = AtomicU64::new(1);

    pub(crate) fn next_id() -> u64 {
        IDS.fetch_add(1, Ordering::SeqCst)
    }

    fn add(name: &'static str, key: Option<usize>, once: bool, f: HookFn) {
        HOOKS.with(|h| {
            h.borrow_mut().push(Hook { name, key, once, f });
        });
    }

    /// Run `f` once at the next point `name` (for `key`, when given) of this
    /// thread.
    pub(crate) fn set(name: &'static str, key: Option<usize>, f: impl FnMut(&dyn Any) + 'static) {
        add(name, key, true, Rc::new(RefCell::new(f)));
    }

    /// Run `f` at every point `name` (for `key`, when given) of this thread.
    pub(crate) fn record(
        name: &'static str,
        key: Option<usize>,
        f: impl FnMut(&dyn Any) + 'static,
    ) {
        add(name, key, false, Rc::new(RefCell::new(f)));
    }

    /// Remove every hook of this thread.
    pub(crate) fn clear() {
        HOOKS.with(|h| h.borrow_mut().clear());
        CLOCK.with(|c| c.set(None));
        NOTES.with(|n| *n.borrow_mut() = None);
        MUST_EXPIRE.with(|m| m.set(None));
    }

    /// The point `name` with `key`: run the first matching hook.
    pub(crate) fn run(name: &'static str, key: usize, arg: &dyn Any) {
        let f = HOOKS.with(|h| {
            let mut h = h.borrow_mut();
            let i = h
                .iter()
                .position(|x| x.name == name && x.key.map_or(true, |k| k == key))?;
            if h[i].once {
                Some(h.remove(i).f)
            } else {
                Some(h[i].f.clone())
            }
        });
        if let Some(f) = f {
            (f.borrow_mut())(arg);
        }
    }

    /// Runs the point named by it when dropped. As the first field of a
    /// struct it runs once the struct's own `Drop::drop` has returned, before
    /// any other field is dropped.
    pub(crate) struct DropPoint(pub(crate) &'static str);

    impl Drop for DropPoint {
        fn drop(&mut self) {
            // Not while the hooks of this thread are being torn down.
            if HOOKS.try_with(|_| ()).is_ok() {
                run(self.0, 0, &());
            }
        }
    }

    /// Stand the clock of this thread still at `t` (`None`: the real one).
    pub(crate) fn set_clock(t: Option<Instant>) {
        CLOCK.with(|c| c.set(t));
    }

    pub(crate) fn clock() -> Option<Instant> {
        CLOCK.with(Cell::get)
    }

    /// Start recording deadline notes on this thread.
    pub(crate) fn start_notes() {
        NOTES.with(|n| *n.borrow_mut() = Some(Vec::new()));
    }

    /// The notes recorded since [`start_notes`]; recording stops.
    pub(crate) fn take_notes() -> Vec<Note> {
        NOTES.with(|n| n.borrow_mut().take().unwrap_or_default())
    }

    /// Panic at once when gate `key` finds a deadline not passed.
    pub(crate) fn must_expire(key: Option<usize>) {
        MUST_EXPIRE.with(|m| m.set(key));
    }

    pub(crate) fn note(n: Note) {
        NOTES.with(|v| {
            if let Some(v) = v.borrow_mut().as_mut() {
                v.push(n);
            }
        });
    }

    pub(crate) fn note_check(gate: usize, id: u64, expired: bool) {
        if !expired && MUST_EXPIRE.with(Cell::get) == Some(gate) {
            panic!("gate {gate:#x} found deadline {id} not passed");
        }
        note(Note::Check(gate, id, expired));
    }
}

#[cfg(all(test, feature = "mock"))]
mod tests {
    use super::testing::{self, Note};
    use super::*;
    use crate::conn::support::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::Arc;

    /// The checks of gate `key`, as (deadline id, expired).
    fn checks(notes: &[Note], key: usize) -> Vec<(u64, bool)> {
        notes
            .iter()
            .filter_map(|n| match *n {
                Note::Check(g, id, expired) if g == key => Some((id, expired)),
                _ => None,
            })
            .collect()
    }

    fn made(notes: &[Note], what: &str) -> u64 {
        let ids: Vec<u64> = notes
            .iter()
            .filter_map(|n| match *n {
                Note::Made(w, id) if w == what => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 1, "{notes:?}");
        ids[0]
    }

    #[test]
    fn deadline_is_one_and_absolute() {
        // No end.
        let d = Deadline::after(Duration::MAX);
        assert!(!d.expired());
        let f = fixture();
        f.cont.close(Duration::MAX).unwrap();

        // A clock that stands still until the test moves it.
        let t = Duration::from_secs(5);
        let f = fixture();
        let b = f.mount();
        let m = core(&b);
        let file = Arc::new(b.create("/f").unwrap());
        let pause = f.mem.pause_next("dfs_get_size");
        let size = {
            let file = file.clone();
            std::thread::spawn(move || file.size())
        };
        assert!(pause.wait_entered(LIMIT));
        let base = Instant::now();
        let later = base + t + Duration::from_secs(1);
        let (mg, cg) = (m.gate().key(), f.conn().gate.key());
        let waits = |g: &Gate| g.waits.load(Ordering::SeqCst);

        // The close of the mount.
        testing::set_clock(Some(base));
        testing::set("BeforeSleep", Some(mg), move |_| {
            testing::set_clock(Some(later))
        });
        testing::start_notes();
        let before = waits(m.gate());
        assert!(matches!(b.close(t), Err(Error::Busy { .. })));
        let notes = testing::take_notes();
        let id = made(&notes, "mount");
        let seen = checks(&notes, mg);
        assert!(!seen.is_empty());
        assert!(seen.iter().all(|(i, _)| *i == id), "{notes:?}");
        assert_eq!(waits(m.gate()), before + 1);

        // The close of the container: the mount gate is tried once per
        // pass with a deadline that has passed.
        testing::set_clock(Some(base));
        testing::set("BeforeSleep", Some(cg), move |_| {
            testing::set_clock(Some(later))
        });
        testing::must_expire(Some(mg));
        testing::start_notes();
        let (before_c, before_m) = (waits(&f.conn().gate), waits(m.gate()));
        assert!(matches!(f.cont.close(t), Err(Error::Busy { .. })));
        let notes = testing::take_notes();
        testing::must_expire(None);
        testing::set_clock(None);
        let id = made(&notes, "cont");
        let seen = checks(&notes, cg);
        assert!(!seen.is_empty());
        assert!(seen.iter().all(|(i, _)| *i == id), "{notes:?}");
        let mut handed = 0;
        for (i, n) in notes.iter().enumerate() {
            if let Note::Handed(h) = *n {
                handed += 1;
                let next = notes[i + 1..].iter().find_map(|n| match *n {
                    Note::Check(g, id, expired) if g == mg => Some((id, expired)),
                    _ => None,
                });
                assert_eq!(next, Some((h, true)), "{notes:?}");
            }
        }
        assert!(handed >= 2, "{notes:?}");
        assert_eq!(waits(&f.conn().gate), before_c + 1);
        assert_eq!(waits(m.gate()), before_m);
        pause.release();
        size.join().unwrap().unwrap();

        // A teardown that panics.
        let g = Gate::new();
        let r = catch_unwind(AssertUnwindSafe(|| {
            g.close_by(
                Deadline::after(Duration::from_secs(1)),
                || None,
                |_| Check::Ready,
                |last, _| Final::Error(last.reason()),
                || -> () { panic!("teardown") },
            )
        }));
        assert!(r.is_err());
        assert_eq!(g.closers(), 0);
        assert!(g.is_closed());

        // A check that panics.
        let g = Gate::new();
        let r = catch_unwind(AssertUnwindSafe(|| {
            g.close_by(
                Deadline::after(Duration::from_secs(1)),
                || None,
                |_| -> Check { panic!("check") },
                |last, _| Final::Error(last.reason()),
                || (),
            )
        }));
        assert!(r.is_err());
        assert_eq!(g.closers(), 0);
        assert!(g.enter(false).is_ok());
    }
}
