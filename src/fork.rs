// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Fork detection.
//!
//! libdaos and DFS mount state do not survive `fork`: a read-write mount keeps
//! its object ID allocator in the `dfs_t`, so a parent and a child would hand
//! out the same object IDs, and locks held by parent threads stay locked in
//! the child. The crate therefore refuses to use inherited state in a child.
//!
//! # Token
//!
//! The identity of a process generation is a token: a nonzero `u64` in one
//! atomic cell, 0 meaning "not armed in this process". A process arms the
//! cell once and keeps its token; every runtime, connection and mount
//! records the token (and the pid) of the process that made it in a
//! [`Stamp`].
//!
//! Page mode: on Linux the cell lives in a private anonymous page marked
//! `MADV_WIPEONFORK` (Linux 4.14 and newer) before its address is published.
//! Every child with its own copy of the address space reads 0 there, however
//! it was made (`fork`, `clone`, `clone3`) and whatever its pid. The hot
//! check compares the token only. No atfork handler is installed.
//!
//! Plain mode: where the page cannot be made, marked or trusted (`mmap` or
//! `madvise` fails, or an invalid advice is accepted, which a layer that
//! ignores advice would do; or a target other than Linux) the cell is an
//! ordinary static. Every check then also compares the kernel pid, and a
//! `pthread_atfork` child handler clears the cell. A process whose pid
//! cannot be read is refused in this mode.
//!
//! Pid policy, one function for every check ([`pids_agree`]): plain mode
//! accepts only when both pids are positive and equal; page mode rejects
//! only when both pids are positive and unequal. A record is accepted when
//! its token matches the cell and the pids agree. Pids come from the kernel
//! (`SYS_getpid`): the C library's cached value can be the parent's in a
//! child made with a raw `clone`. 0 stands for an unknown pid.
//!
//! # Owner
//!
//! The process-global entry points (`init`, `library_info`, the runtime
//! lookup) first claim [`OWNER`], before they take any lock. The owner is one
//! 64-bit value: the pid in the low half, the low 32 bits of the token in the
//! high half. When the owner was initialising, had initialised, or had a
//! thread inside the library load, the locks those steps hold may be held for
//! ever in a child, so a child gets [`Error::Forked`] instead of waiting.
//! Otherwise the owner had at most loaded the library, and the child takes
//! the ownership over. A child forked after a parent thread released such a
//! lock but before it lowered its counter is refused as well; the check errs
//! on the safe side.
//!
//! A refusal is decided from loads and one pid read and never waits. A
//! process about to take the ownership proceeds in this order: choose the
//! cell, install the child handler (plain mode only; `pthread_atfork` takes a
//! lock of the C library), arm the token, store the owner. A claim without a
//! readable pid in plain mode refuses before the handler and the token.
//!
//! Reading order of a claim: the owner first, then the cell, its token, the
//! mode and the pid, once per attempt. A nonzero owner stored by this
//! process was stored after its cell was published and armed, so a thread
//! that reads that owner also sees the cell and the token, and a token never
//! changes in a process once armed: the owner never refuses itself. In page
//! mode a child reads 0 or a token it armed itself, whose low half differs
//! from the owner's high half, so it never passes as the owner.
//!
//! Assumption: the child's copy of the page holding [`OWNER`] is the content
//! of that page at one instant. The kernel provides this for ordinary pages;
//! not for pages pinned for direct I/O, which the crate's statics are not.
//!
//! # What is recognised
//!
//! - Page mode: any process with its own copy of the address space other
//!   than the maker of a record, on every call, also when its pid cannot be
//!   read.
//! - Plain mode: a process is taken for the maker of a record when its
//!   kernel pid is known and equals the recorded pid and no child handler of
//!   the crate ran on the way from the maker to it. A descendant with another
//!   pid is recognised on every call; one with the recorded pid (possible with
//!   PID namespaces) only when a `fork` of the C library lies between, after
//!   the handler was installed. The first setup with a readable pid in a raw
//!   `clone` child of a multithreaded process can wait for ever inside
//!   `pthread_atfork`; refusals never call it.
//! - Both modes: a child that shares the address space (`vfork`, `CLONE_VM`,
//!   `posix_spawn`) must not use the crate before `exec`. Calling the crate
//!   from another atfork child handler is not supported.

use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use crate::error::{Error, Result};

/// A process as the owner records it: pid in the low 32 bits, the low 32
/// bits of its token in the high 32 bits. A stored owner never has a high
/// half of 0 (see [`Guard::arm`]).
type Ident = u64;

fn ident_of(pid: u32, token: u64) -> Ident {
    ((token & u64::from(u32::MAX)) << 32) | u64::from(pid)
}

fn pid_of(id: Ident) -> u32 {
    (id & u64::from(u32::MAX)) as u32
}

/// The chosen cell and its mode.
#[derive(Clone, Copy)]
struct Chosen {
    cell: &'static AtomicU64,
    /// Plain mode: checks compare the pid as well.
    by_pid: bool,
}

/// What a process reads about itself in one attempt.
#[derive(Clone, Copy)]
struct Seen {
    chosen: Option<Chosen>,
    token: u64,
    pid: u32,
}

/// The pid policy. `recorded` and `now` are 0 when unknown.
fn pids_agree(by_pid: bool, recorded: u32, now: u32) -> bool {
    if by_pid {
        recorded != 0 && recorded == now
    } else {
        recorded == 0 || now == 0 || recorded == now
    }
}

/// True when `owner` names the process that read `now`.
fn is_own(owner: Ident, now: Seen) -> bool {
    match now.chosen {
        Some(c) => {
            now.token != 0
                && (owner >> 32) as u32 == now.token as u32
                && pids_agree(c.by_pid, pid_of(owner), now.pid)
        }
        None => false,
    }
}

/// A mapped page, unmapped on drop unless it was published.
struct Page {
    ptr: *mut AtomicU64,
    len: usize,
}

impl Page {
    fn as_ptr(&self) -> *mut AtomicU64 {
        self.ptr
    }

    /// Keep the mapping for the life of the process.
    fn keep(self) -> *mut AtomicU64 {
        let p = self.ptr;
        std::mem::forget(self);
        p
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        // SAFETY: `ptr` and `len` are a mapping made by this process that was
        // never published, so nothing else refers to it. Should munmap fail,
        // the page stays mapped and unused.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

/// Where the token of this process lives.
struct Guard {
    /// The chosen cell: a wiped page or `plain`. Null until chosen; never
    /// replaced or unmapped once published.
    cell: AtomicPtr<AtomicU64>,
    /// The cell of plain mode.
    plain: AtomicU64,
    /// The child handler is installed (in this process or an ancestor whose
    /// memory this process inherited).
    handler: AtomicBool,
    /// Last token drawn.
    next: AtomicU64,
}

static GUARD: Guard = Guard::new();

impl Guard {
    const fn new() -> Guard {
        Guard {
            cell: AtomicPtr::new(ptr::null_mut()),
            plain: AtomicU64::new(0),
            handler: AtomicBool::new(false),
            next: AtomicU64::new(0),
        }
    }

    fn chosen_at(&'static self, p: *mut AtomicU64) -> Option<Chosen> {
        if p.is_null() {
            return None;
        }
        // SAFETY: a published cell is either `self.plain` or a page that is
        // never unmapped, so it lives for the rest of the process.
        let cell: &'static AtomicU64 = unsafe { &*p };
        Some(Chosen {
            cell,
            by_pid: ptr::eq(cell, &self.plain),
        })
    }

    /// Loads and one `pid()`. Chooses, installs and arms nothing.
    fn seen(&'static self) -> Seen {
        let chosen = self.chosen_at(self.cell.load(Ordering::Acquire));
        let token = chosen.map_or(0, |c| c.cell.load(Ordering::Acquire));
        Seen {
            chosen,
            token,
            pid: pid(),
        }
    }

    /// Choose the cell. Never waits, installs nothing, writes no token.
    ///
    /// The first successful exchange wins for the life of the process, also
    /// when it is the plain cell. A loser's page is unmapped; should
    /// `munmap` fail it stays mapped and unused, and a child forked while a
    /// loser held its page inherits one unused page. The loser's pointer
    /// never left its thread.
    fn choose_with(&'static self, map: impl FnOnce() -> Option<Page>) -> Chosen {
        if let Some(c) = self.chosen_at(self.cell.load(Ordering::Acquire)) {
            return c;
        }
        let page = map();
        let plain = &self.plain as *const AtomicU64 as *mut AtomicU64;
        let candidate = page.as_ref().map_or(plain, Page::as_ptr);
        match self.cell.compare_exchange(
            ptr::null_mut(),
            candidate,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                if let Some(p) = page {
                    p.keep();
                }
                self.chosen_at(candidate).expect("candidate is not null")
            }
            Err(winner) => {
                drop(page);
                self.chosen_at(winner)
                    .expect("a published cell is not null")
            }
        }
    }

    /// Plain mode: install the child handler unless it is installed. The
    /// flag is set only when the installation succeeded. Two threads may
    /// both install; the handler only stores 0.
    fn handler_with(&self, install: impl FnOnce() -> libc::c_int) -> bool {
        if self.handler.load(Ordering::Acquire) {
            return true;
        }
        if install() == 0 {
            self.handler.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// The token of this process; arms one when the cell reads 0. A drawn
    /// token is never 0 in its low 32 bits and never equals `avoid` there
    /// (the high half of the owner it may replace).
    fn arm(&self, cell: &AtomicU64, avoid: u32) -> u64 {
        loop {
            let t = cell.load(Ordering::Acquire);
            if t != 0 {
                return t;
            }
            let new = self.next.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            let low = new as u32;
            if low == 0 || low == avoid {
                continue;
            }
            match cell.compare_exchange(0, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return new,
                Err(cur) if cur != 0 => return cur,
                Err(_) => {}
            }
        }
    }

    /// Install (plain mode, when `install` is given) and arm on a chosen
    /// cell. `install` is `None` for a process that does not own the state.
    fn finish_with(
        &self,
        chosen: Chosen,
        install: Option<impl FnOnce() -> libc::c_int>,
        avoid: u32,
    ) -> u64 {
        if chosen.by_pid {
            if let Some(f) = install {
                self.handler_with(f);
            }
        }
        self.arm(chosen.cell, avoid)
    }

    fn clear(&self) {
        self.plain.store(0, Ordering::Relaxed);
    }
}

/// `MADV_WIPEONFORK` of the Linux uapi headers; not in every libc 0.2
/// release.
#[cfg(target_os = "linux")]
const MADV_WIPEONFORK: libc::c_int = 18;

/// A private page the kernel clears in every child, or `None`.
fn map_wiped_page() -> Option<Page> {
    #[cfg(test)]
    if testing::MAP_DISABLED.load(Ordering::SeqCst) {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        map_wiped_page_with(|p, len, advice| {
            // SAFETY: `p` and `len` are the mapping just made by
            // map_wiped_page_with; madvise does not touch other memory.
            unsafe { libc::madvise(p, len, advice) }
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Map a page and mark it with `advise`. None of these calls takes a lock of
/// the C library.
#[cfg(target_os = "linux")]
fn map_wiped_page_with(
    advise: impl Fn(*mut libc::c_void, usize, libc::c_int) -> libc::c_int,
) -> Option<Page> {
    // SAFETY: sysconf has no preconditions.
    let len = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let len = usize::try_from(len).ok().filter(|&n| n > 0)?;
    // SAFETY: a new private anonymous mapping; no existing memory is named.
    let p = unsafe {
        libc::mmap(
            ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return None;
    }
    // Zero-filled: the cell reads 0. Unmapped on every early return.
    let page = Page { ptr: p.cast(), len };
    // A layer that accepts unknown advice cannot be trusted to clear the
    // page.
    if advise(p, len, -1) == 0 {
        return None;
    }
    if advise(p, len, MADV_WIPEONFORK) != 0 {
        return None;
    }
    Some(page)
}

/// Install the plain-mode child handler. Returns the result of
/// `pthread_atfork`.
fn install_handler() -> libc::c_int {
    #[cfg(test)]
    {
        testing::INSTALL_CALLS.fetch_add(1, Ordering::SeqCst);
        let f = testing::INSTALL_OVERRIDE.load(Ordering::SeqCst);
        if f != 0 {
            // SAFETY: the override holds a `fn() -> c_int` stored by
            // `testing::set_install_override`.
            let f: fn() -> libc::c_int = unsafe { std::mem::transmute(f) };
            return f();
        }
    }
    // SAFETY: the handler is an extern "C" function that performs one atomic
    // store; it is async-signal-safe and never unwinds.
    unsafe { libc::pthread_atfork(None, None, Some(on_fork_child)) }
}

extern "C" fn on_fork_child() {
    GUARD.clear();
}

/// The kernel's id of this process, 0 when it cannot be read.
pub(crate) fn pid() -> u32 {
    pid_from(raw_pid())
}

fn raw_pid() -> libc::c_long {
    #[cfg(test)]
    if let Some(v) = testing::injected_raw_pid() {
        return v;
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: SYS_getpid has no arguments and no preconditions.
        unsafe { libc::syscall(libc::SYS_getpid) }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: getpid has no preconditions.
        libc::c_long::from(unsafe { libc::getpid() })
    }
}

/// A positive `u32`, or 0 (unknown).
fn pid_from(raw: libc::c_long) -> u32 {
    u32::try_from(raw).unwrap_or(0)
}

/// Which process owns the crate's process-global state, and whether that
/// process was busy with it. See the module docs.
#[repr(align(64))]
pub(crate) struct Owner {
    /// [`Ident`] of the owner, 0 when nobody claimed the state.
    id: AtomicU64,
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
            id: AtomicU64::new(0),
            init_active: AtomicUsize::new(0),
            init_done: AtomicBool::new(false),
            loader_active: AtomicUsize::new(0),
        }
    }

    /// The owner as stored now. The only first load of an attempt.
    fn load_id(&self) -> Ident {
        #[cfg(test)]
        testing::run_before_owner_load();
        self.id.load(Ordering::SeqCst)
    }

    /// Claim the process-global state for this process. Takes no lock of the
    /// crate and never waits on a refusal. Returns [`Error::Forked`] in a
    /// child of a process that was initialising, had initialised, or was
    /// loading the library, and in plain mode where the pid cannot be read.
    /// Returns the stamp of this process as the successful attempt saw it.
    pub(crate) fn claim(&self) -> Result<Stamp> {
        self.claim_with(
            || GUARD.seen(),
            || GUARD.choose_with(map_wiped_page),
            |chosen, owner| GUARD.finish_with(chosen, Some(install_handler), (owner >> 32) as u32),
        )
    }

    fn claim_with(
        &self,
        seen: impl Fn() -> Seen,
        choose: impl Fn() -> Chosen,
        finish: impl Fn(Chosen, Ident) -> u64,
    ) -> Result<Stamp> {
        loop {
            let owner = self.load_id();
            // The one sample of this attempt: compared, reported, published
            // and returned.
            let now = seen();
            if owner != 0 && is_own(owner, now) {
                let c = now.chosen.expect("own implies a chosen cell");
                return Ok(Stamp {
                    cell: c.cell,
                    token: now.token,
                    pid: now.pid,
                    by_pid: c.by_pid,
                });
            }
            let refused = Error::Forked {
                created_pid: pid_of(owner),
                current_pid: now.pid,
            };
            if self.busy() {
                // A thread raises a busy flag only after its own claim
                // succeeded. When the owner changed since the load above, a
                // thread of this process may have won the claim and raised
                // the flag already: look again instead of refusing.
                if self.id.load(Ordering::SeqCst) != owner {
                    continue;
                }
                return Err(refused);
            }
            // Plain mode needs a pid. Decide before anything is installed or
            // armed.
            if now.pid == 0 && matches!(now.chosen, Some(c) if c.by_pid) {
                return Err(refused);
            }
            let chosen = choose();
            if now.pid == 0 && chosen.by_pid {
                return Err(refused);
            }
            let token = finish(chosen, owner);
            let me = ident_of(now.pid, token);
            if self
                .id
                .compare_exchange(owner, me, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Ok(Stamp {
                    cell: chosen.cell,
                    token,
                    pid: now.pid,
                    by_pid: chosen.by_pid,
                });
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

/// The process a runtime, connection or mount was made in.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Stamp {
    cell: &'static AtomicU64,
    token: u64,
    pid: u32,
    by_pid: bool,
}

impl Stamp {
    /// A stamp for this process. Never fails. In a refused process it is a
    /// stamp of that process (for a backend made there) and passes its
    /// checks there. Where the pid cannot be read in plain mode it fails its
    /// own checks.
    pub(crate) fn now() -> Stamp {
        match OWNER.claim() {
            Ok(s) => {
                if s.by_pid {
                    // Retry of an installation that failed earlier.
                    GUARD.handler_with(install_handler);
                }
                s
            }
            Err(_) => {
                // A refusal is not reported here. Callers that use
                // process-global state (`runtime()`, `init`) claim before
                // they get here and return the refusal there. A stamp made
                // in a refused child carries that child's own token and pid
                // and passes its own checks, so the refusal must come from
                // those callers, not from the stamp.
                let chosen = GUARD.choose_with(map_wiped_page);
                let avoid = (OWNER.id.load(Ordering::SeqCst) >> 32) as u32;
                let token = GUARD.finish_with(chosen, None::<fn() -> libc::c_int>, avoid);
                Stamp {
                    cell: chosen.cell,
                    token,
                    pid: pid(),
                    by_pid: chosen.by_pid,
                }
            }
        }
    }

    /// A stamp that passed its own cold check, for storing in a runtime,
    /// connection or mount.
    pub(crate) fn fresh() -> Result<Stamp> {
        let s = Stamp::now();
        s.check_cold()?;
        Ok(s)
    }

    /// Hot-path check: one atomic load in page mode.
    #[inline]
    pub(crate) fn check(&self) -> Result<()> {
        if self.by_pid {
            return self.check_cold();
        }
        if self.cell.load(Ordering::Relaxed) == self.token {
            Ok(())
        } else {
            Err(Error::Forked {
                created_pid: self.pid,
                current_pid: pid(),
            })
        }
    }

    /// Cold-path check: token and pid.
    pub(crate) fn check_cold(&self) -> Result<()> {
        let now = pid();
        if self.cell.load(Ordering::Relaxed) == self.token && pids_agree(self.by_pid, self.pid, now)
        {
            Ok(())
        } else {
            Err(Error::Forked {
                created_pid: self.pid,
                current_pid: now,
            })
        }
    }

    /// The recorded pid (0: unknown).
    #[cfg(test)]
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    /// True when this process is not, or cannot be shown to be, the one the
    /// stamp was made in.
    pub(crate) fn is_foreign(&self) -> bool {
        self.check_cold().is_err()
    }
}

/// Test seams shared with the tests of other modules.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::time::{Duration, Instant};

    thread_local! {
        /// Run once by the next `Owner::load_id` of this thread.
        static BEFORE_OWNER_LOAD: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
        /// (skip, count, value): after `skip` real reads, the next `count`
        /// reads of `raw_pid()` return `value`.
        static RAW_PID: Cell<Option<(usize, usize, libc::c_long)>> = const { Cell::new(None) };
        /// Reads of `raw_pid()` counted by `count_raw_pid_reads`.
        static READS: Cell<Option<usize>> = const { Cell::new(None) };
        /// The label whose `fault_point` arms one failed read.
        static PID_FAULT_AT: Cell<Option<&'static str>> = const { Cell::new(None) };
        /// The next read of `raw_pid()` returns -1.
        static PID_FAULT_ARMED: Cell<bool> = const { Cell::new(false) };
    }

    pub(crate) static MAP_DISABLED: AtomicBool = AtomicBool::new(false);
    pub(crate) static INSTALL_OVERRIDE: AtomicUsize = AtomicUsize::new(0);
    pub(crate) static INSTALL_CALLS: AtomicUsize = AtomicUsize::new(0);

    pub(crate) fn set_before_owner_load(f: Box<dyn FnMut()>) {
        BEFORE_OWNER_LOAD.with(|h| *h.borrow_mut() = Some(f));
    }

    pub(super) fn run_before_owner_load() {
        let f = BEFORE_OWNER_LOAD.with(|h| h.borrow_mut().take());
        if let Some(mut f) = f {
            f();
        }
    }

    /// Replace `install_handler` with `f` (`None`: the real one).
    pub(crate) fn set_install_override(f: Option<fn() -> libc::c_int>) {
        INSTALL_OVERRIDE.store(f.map_or(0, |f| f as usize), Ordering::SeqCst);
    }

    pub(super) fn injected_raw_pid() -> Option<libc::c_long> {
        if let Some(n) = READS.with(Cell::get) {
            READS.with(|r| r.set(Some(n + 1)));
        }
        if PID_FAULT_ARMED.with(|a| a.replace(false)) {
            return Some(-1);
        }
        match RAW_PID.with(Cell::get) {
            Some((skip, count, v)) if skip > 0 => {
                RAW_PID.with(|c| c.set(Some((skip - 1, count, v))));
                None
            }
            Some((0, count, v)) if count > 0 => {
                RAW_PID.with(|c| c.set(Some((0, count - 1, v))));
                Some(v)
            }
            _ => None,
        }
    }

    /// Restores a thread-local cell when dropped, also on a panic.
    struct Restore<T: Copy + 'static> {
        key: &'static std::thread::LocalKey<Cell<T>>,
        old: T,
    }

    impl<T: Copy + 'static> Drop for Restore<T> {
        fn drop(&mut self) {
            let old = self.old;
            self.key.with(|c| c.set(old));
        }
    }

    fn set<T: Copy + 'static>(key: &'static std::thread::LocalKey<Cell<T>>, v: T) -> Restore<T> {
        let old = key.with(|c| c.replace(v));
        Restore { key, old }
    }

    fn with_raw_pid_spec<R>(spec: (usize, usize, libc::c_long), f: impl FnOnce() -> R) -> R {
        let _restore = set(&RAW_PID, Some(spec));
        f()
    }

    /// Every read of `raw_pid()` during `f` returns `v`.
    pub(crate) fn with_raw_pid<R>(v: libc::c_long, f: impl FnOnce() -> R) -> R {
        with_raw_pid_spec((0, usize::MAX, v), f)
    }

    /// Every read after the first `n` returns `v`.
    pub(crate) fn with_raw_pid_after<R>(n: usize, v: libc::c_long, f: impl FnOnce() -> R) -> R {
        with_raw_pid_spec((n, usize::MAX, v), f)
    }

    // Used by the tests over the in-memory backend.
    #[cfg(feature = "mock")]
    /// Exactly the read after the first `n` returns `v`.
    pub(crate) fn with_raw_pid_once_after<R>(
        n: usize,
        v: libc::c_long,
        f: impl FnOnce() -> R,
    ) -> R {
        with_raw_pid_spec((n, 1, v), f)
    }

    // Used by the tests over the in-memory backend.
    #[cfg(feature = "mock")]
    /// The number of `raw_pid()` calls during `f`, a read that returned an
    /// injected value included. An active injection is left as it is.
    pub(crate) fn count_raw_pid_reads(f: impl FnOnce()) -> usize {
        let restore = set(&READS, Some(0));
        f();
        let n = READS.with(Cell::get).expect("set above");
        drop(restore);
        n
    }

    // Used by the tests over the in-memory backend.
    #[cfg(feature = "mock")]
    /// When `fault_point(label)` is reached during `f`, the next single read
    /// of `raw_pid()` returns -1. Panics when the label was not reached or
    /// its read was not made.
    pub(crate) fn with_pid_fault_at<R>(label: &'static str, f: impl FnOnce() -> R) -> R {
        let at = set(&PID_FAULT_AT, Some(label));
        let armed = set(&PID_FAULT_ARMED, false);
        let r = f();
        let reached = PID_FAULT_AT.with(Cell::get).is_none();
        let consumed = !PID_FAULT_ARMED.with(Cell::get);
        drop(armed);
        drop(at);
        assert!(reached, "fault point {label} was not reached");
        assert!(consumed, "no pid read followed fault point {label}");
        r
    }

    /// A place where `with_pid_fault_at` may fail the next pid read. The
    /// label is consumed once.
    pub(crate) fn fault_point(label: &'static str) {
        if PID_FAULT_AT.with(Cell::get) == Some(label) {
            PID_FAULT_AT.with(|c| c.set(None));
            PID_FAULT_ARMED.with(|a| a.set(true));
        }
    }

    /// Wait for `pid` at most `limit`: the exit status, or minus the signal.
    /// Past the deadline the child is killed and reaped.
    pub(crate) fn wait_child(pid: libc::pid_t, limit: Duration) -> i32 {
        assert!(pid > 0, "no child to wait for");
        let deadline = Instant::now() + limit;
        let mut killed = false;
        loop {
            let mut status = 0;
            // SAFETY: waiting for our own child.
            let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if r == pid {
                if killed {
                    return -libc::SIGKILL;
                }
                return if libc::WIFEXITED(status) {
                    libc::WEXITSTATUS(status)
                } else {
                    -libc::WTERMSIG(status)
                };
            }
            if r < 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                panic!("waitpid: {e}");
            }
            if !killed && Instant::now() >= deadline {
                // SAFETY: our own child, not reaped yet.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                killed = true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Fork; the child runs `f` under `alarm(10)` and leaves with its code.
    pub(crate) fn fork_child(f: impl FnOnce() -> i32) -> libc::pid_t {
        // SAFETY: the child runs `f`, which uses atomics and system calls
        // only, then _exit.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            // SAFETY: alarm has no preconditions.
            unsafe { libc::alarm(10) };
            let code = f();
            // SAFETY: leave without running anything inherited.
            unsafe { libc::_exit(code) };
        }
        pid
    }

    /// Plain mode for this process: no page is mapped and the plain cell is
    /// chosen. Must run before anything chose a cell.
    pub(crate) fn force_plain() {
        MAP_DISABLED.store(true, Ordering::SeqCst);
        let c = GUARD.choose_with(map_wiped_page);
        assert!(c.by_pid, "a page was chosen before force_plain");
    }

    /// Unless this process runs exactly `name` (env `RUST_DAOS_FORK_UNIT`),
    /// run the test binary again with only that test, wait at most 60 s,
    /// require success and return true. Returns false in the process that
    /// should run the test body.
    pub(crate) fn rerun_alone(name: &str) -> bool {
        use std::io::Read;
        use std::process::{Command, Stdio};
        if std::env::var("RUST_DAOS_FORK_UNIT").as_deref() == Ok(name) {
            return false;
        }
        let exe = std::env::current_exe().expect("current_exe");
        let mut child = Command::new(exe)
            .args([name, "--exact", "--nocapture", "--test-threads=1"])
            .env("RUST_DAOS_FORK_UNIT", name)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the test binary");
        let mut so = child.stdout.take().expect("piped");
        let mut se = child.stderr.take().expect("piped");
        let to = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = so.read_to_string(&mut s);
            s
        });
        let te = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = se.read_to_string(&mut s);
            s
        });
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(st) = child.try_wait().expect("wait") {
                break Some(st);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let out = to.join().expect("stdout reader");
        let err = te.join().expect("stderr reader");
        let ok = matches!(status, Some(st) if st.success()) && out.contains("1 passed");
        assert!(ok, "{name} alone: {status:?}\n{out}\n{err}");
        true
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    /// An identity with token 1, for the cases that only vary the pid.
    fn id(pid: u32) -> Ident {
        ident_of(pid, 1)
    }

    impl Owner {
        /// Claim as `me` over a test-local cell in plain mode.
        fn claim_as(&self, me: Ident) -> Result<()> {
            static CELL: AtomicU64 = AtomicU64::new(0);
            let token = me >> 32;
            let chosen = Chosen {
                cell: &CELL,
                by_pid: true,
            };
            self.claim_with(
                || Seen {
                    chosen: Some(chosen),
                    token,
                    pid: pid_of(me),
                },
                || chosen,
                |_, _| token,
            )
            .map(|_| ())
        }
    }

    fn forked<T>(r: Result<T>, created: u32, current: u32) -> bool {
        matches!(r, Err(Error::Forked { created_pid, current_pid })
            if created_pid == created && current_pid == current)
    }

    fn is_forked<T>(r: Result<T>) -> bool {
        matches!(r, Err(Error::Forked { .. }))
    }

    fn leaked_guard() -> &'static Guard {
        Box::leak(Box::new(Guard::new()))
    }

    fn no_install() -> Option<fn() -> libc::c_int> {
        None
    }

    fn wipe_required() -> bool {
        std::env::var("RUST_DAOS_REQUIRE_WIPE").as_deref() == Ok("1")
    }

    /// Plain mode where page mode is needed: fail when the wipe is required,
    /// otherwise say so and return true (the caller returns).
    fn no_page(what: &str) -> bool {
        assert!(
            !wipe_required(),
            "{what}: the kernel does not clear the page in children"
        );
        println!("{what}: no wiped page here, case not run");
        true
    }

    /// A plain private page, not marked.
    fn anon_page() -> Page {
        // SAFETY: sysconf has no preconditions.
        let len = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        // SAFETY: a new private anonymous mapping.
        let p = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(p, libc::MAP_FAILED);
        Page { ptr: p.cast(), len }
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    fn raw_clone() -> libc::pid_t {
        // SAFETY: a clone with only SIGCHLD: a copy of the address space, no
        // shared state, no C library atfork handlers. The children below use
        // atomics and system calls only.
        let r = unsafe {
            libc::syscall(
                libc::SYS_clone,
                libc::SIGCHLD as libc::c_long,
                0 as libc::c_long,
                0 as libc::c_long,
                0 as libc::c_long,
                0 as libc::c_long,
            )
        };
        if r < 0 {
            panic!("clone: {}", std::io::Error::last_os_error());
        }
        r as libc::pid_t
    }

    /// Raw clone; the child runs `f` under `alarm(10)` and leaves with its
    /// code.
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    fn clone_child(f: impl FnOnce() -> i32) -> libc::pid_t {
        let pid = raw_clone();
        if pid == 0 {
            // SAFETY: alarm has no preconditions.
            unsafe { libc::alarm(10) };
            let code = f();
            // SAFETY: leave without running anything inherited.
            unsafe { libc::_exit(code) };
        }
        pid
    }

    #[test]
    fn ident_layout() {
        assert_eq!(
            ident_of(0x89ab_cdef, 0x0123_4567_7654_3210),
            0x7654_3210_89ab_cdef
        );
        assert_eq!(pid_of(0x7654_3210_89ab_cdef), 0x89ab_cdef);
        assert_eq!(ident_of(5, 1 << 32), ident_of(5, 0));
        assert_ne!(ident_of(1, 0), 0);
    }

    #[test]
    fn stamp_matches_in_same_process() {
        let s = Stamp::now();
        assert!(s.check().is_ok());
        assert!(s.check_cold().is_ok());
        assert!(!s.is_foreign());
    }

    #[test]
    fn foreign_pid_is_detected() {
        let s = Stamp::now();
        let other = Stamp {
            pid: s.pid.wrapping_add(1).max(1),
            by_pid: false,
            ..s
        };
        assert!(
            other.check().is_ok(),
            "the hot path looks at the token only"
        );
        assert!(is_forked(other.check_cold()));
        let other = Stamp {
            by_pid: true,
            ..other
        };
        assert!(is_forked(other.check()));
    }

    #[test]
    fn pids_agree_by_mode() {
        assert!(pids_agree(true, 5, 5));
        for (a, b) in [(5, 6), (0, 5), (5, 0), (0, 0)] {
            assert!(!pids_agree(true, a, b), "plain ({a}, {b})");
        }
        for (a, b) in [(5, 5), (0, 5), (5, 0), (0, 0)] {
            assert!(pids_agree(false, a, b), "page ({a}, {b})");
        }
        assert!(!pids_agree(false, 5, 6));
        assert_eq!(pid_from(-1), 0);
        assert_eq!(pid_from(0), 0);
        assert_eq!(pid_from(7), 7);
    }

    #[test]
    fn owner_claim_rules() {
        let o = Owner::new();
        assert!(o.claim_as(id(100)).is_ok(), "first claim");
        assert!(o.claim_as(id(100)).is_ok(), "same process");
        // A child of an idle owner (nothing, or only a finished library
        // load) takes the ownership over.
        assert!(o.claim_as(id(200)).is_ok());
        assert_eq!(o.id.load(Ordering::SeqCst), id(200));
        // Fork while a thread is inside the library load.
        {
            let _l = o.enter_loader();
            assert!(forked(o.claim_as(id(300)), 200, 300));
            assert!(
                o.claim_as(id(200)).is_ok(),
                "the owner itself is never refused"
            );
        }
        assert!(o.claim_as(id(300)).is_ok());
        // Fork while a thread is inside init (also the second of two).
        {
            let _a = o.enter_init();
            let _b = o.enter_init();
            assert!(forked(o.claim_as(id(400)), 300, 400));
        }
        // A retryable failure leaves nothing behind.
        assert!(o.claim_as(id(400)).is_ok());
        // Fork after init completed or failed for good.
        o.mark_init_done();
        assert!(forked(o.claim_as(id(500)), 400, 500));
        assert_eq!(o.id.load(Ordering::SeqCst), id(400));
    }

    /// A parent that is PID 1 of its namespace and its first child in a new
    /// PID namespace both see pid 1; only the token tells them apart.
    #[test]
    fn equal_pids_with_different_tokens() {
        let parent = ident_of(1, 7);
        let child = ident_of(1, 8);
        assert_ne!(parent, child);

        // Idle owner: the child takes it over.
        let o = Owner::new();
        assert!(o.claim_as(parent).is_ok());
        assert!(o.claim_as(child).is_ok());
        assert_eq!(o.id.load(Ordering::SeqCst), child);

        // A thread of the parent inside the library load or inside init.
        for loader in [true, false] {
            let o = Owner::new();
            assert!(o.claim_as(parent).is_ok());
            let _active = if loader {
                o.enter_loader()
            } else {
                o.enter_init()
            };
            assert!(forked(o.claim_as(child), 1, 1), "loader {loader}");
            assert!(o.claim_as(parent).is_ok(), "loader {loader}");
            assert_eq!(o.id.load(Ordering::SeqCst), parent);
        }

        // The parent completed init or failed for good.
        let o = Owner::new();
        assert!(o.claim_as(parent).is_ok());
        o.mark_init_done();
        assert!(forked(o.claim_as(child), 1, 1));
        assert!(o.claim_as(parent).is_ok());
        assert_eq!(o.id.load(Ordering::SeqCst), parent);
    }

    /// Two threads of a child whose inherited owner is the parent make their
    /// first claim at once. The loser of the claim must not be refused
    /// because the winner raised a busy counter in the meantime.
    #[test]
    fn concurrent_first_claims_in_a_takeover_child() {
        use std::sync::{Arc, Barrier};
        const ROUNDS: usize = 20_000;
        let parent = id(99);
        let child = id(100);
        let owners: Arc<Vec<Owner>> = Arc::new((0..ROUNDS).map(|_| Owner::new()).collect());
        for o in owners.iter() {
            o.id.store(parent, Ordering::SeqCst);
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
                        if o.claim_as(child).is_err() {
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
        assert!(owners.iter().all(|o| o.id.load(Ordering::SeqCst) == child));
    }

    /// Same token, another known pid: refused in page mode by the pid.
    #[cfg(target_os = "linux")]
    #[test]
    fn other_known_pid_is_refused_with_an_equal_token() {
        if rerun_alone("fork::tests::other_known_pid_is_refused_with_an_equal_token") {
            return;
        }
        let first = OWNER.claim().unwrap();
        if first.by_pid && no_page("other_known_pid_is_refused_with_an_equal_token") {
            return;
        }
        let s = Stamp::now();
        OWNER.mark_init_done();
        let real = pid();
        with_raw_pid(real as libc::c_long + 1, || {
            assert!(forked(OWNER.claim(), real, real + 1));
            assert!(s.check().is_ok());
            assert!(is_forked(s.check_cold()));
        });
        assert!(OWNER.claim().is_ok());
    }

    #[test]
    fn unknown_pid_in_plain_mode() {
        if rerun_alone("fork::tests::unknown_pid_in_plain_mode") {
            return;
        }
        force_plain();
        with_raw_pid(-1, || {
            assert!(forked(OWNER.claim(), 0, 0));
        });
        assert_eq!(OWNER.id.load(Ordering::SeqCst), 0);
        assert_eq!(INSTALL_CALLS.load(Ordering::SeqCst), 0);
        assert!(!GUARD.handler.load(Ordering::SeqCst));
        assert_eq!(GUARD.plain.load(Ordering::SeqCst), 0);
        assert_eq!(GUARD.next.load(Ordering::SeqCst), 0);

        assert!(OWNER.claim().is_ok());
        let s = Stamp::now();
        let real = pid();
        let made_without_pid = with_raw_pid(-1, || {
            assert!(forked(s.check(), real, 0));
            assert!(forked(s.check_cold(), real, 0));
            assert!(forked(OWNER.claim(), real, 0));
            assert!(is_forked(Stamp::fresh()));
            let c = Stamp::now();
            assert!(is_forked(c.check()));
            c
        });
        assert!(is_forked(made_without_pid.check()));
        OWNER.mark_init_done();
        with_raw_pid(real as libc::c_long + 1, || {
            assert!(forked(OWNER.claim(), real, real + 1));
            assert!(is_forked(s.check()));
        });
    }

    #[test]
    fn unknown_pid_before_a_cell_is_chosen() {
        if rerun_alone("fork::tests::unknown_pid_before_a_cell_is_chosen") {
            return;
        }
        MAP_DISABLED.store(true, Ordering::SeqCst);
        assert!(GUARD.cell.load(Ordering::SeqCst).is_null());
        with_raw_pid(-1, || {
            assert!(forked(OWNER.claim(), 0, 0));
        });
        assert!(ptr::eq(GUARD.cell.load(Ordering::SeqCst), &GUARD.plain));
        assert_eq!(INSTALL_CALLS.load(Ordering::SeqCst), 0);
        assert!(!GUARD.handler.load(Ordering::SeqCst));
        assert_eq!(GUARD.plain.load(Ordering::SeqCst), 0);
        assert_eq!(GUARD.next.load(Ordering::SeqCst), 0);
        assert_eq!(OWNER.id.load(Ordering::SeqCst), 0);
        assert!(OWNER.claim().is_ok());
        assert_eq!(INSTALL_CALLS.load(Ordering::SeqCst), 1);
    }

    /// The pid of the sample is the one published, even when later reads
    /// fail.
    #[test]
    fn pid_lost_between_claim_and_publication() {
        let g = leaked_guard();
        g.choose_with(|| None);
        let o = Owner::new();
        with_raw_pid(-1, || {
            let s = o
                .claim_with(
                    || Seen { pid: 7, ..g.seen() },
                    || g.choose_with(|| None),
                    |c, owner| g.finish_with(c, Some(|| 0), (owner >> 32) as u32),
                )
                .unwrap();
            assert_eq!(pid_of(o.id.load(Ordering::SeqCst)), 7);
            assert_eq!(s.pid, 7);
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unknown_pid_in_page_mode() {
        if rerun_alone("fork::tests::unknown_pid_in_page_mode") {
            return;
        }
        let first = OWNER.claim().unwrap();
        if first.by_pid && no_page("unknown_pid_in_page_mode") {
            return;
        }
        let s = Stamp::now();
        OWNER.mark_init_done();
        let real = pid();
        with_raw_pid(-1, || {
            assert!(OWNER.claim().is_ok());
            assert!(s.check().is_ok());
            assert!(s.check_cold().is_ok());
            assert!(!s.is_foreign());
            assert!(Stamp::fresh().is_ok(), "the armed token still matches");
        });
        let child = fork_child(|| {
            with_raw_pid(-1, || {
                if !forked(OWNER.claim(), real, 0) {
                    return 1;
                }
                if !is_forked(s.check()) {
                    return 2;
                }
                if !is_forked(s.check_cold()) {
                    return 3;
                }
                0
            })
        });
        assert_eq!(wait_child(child, Duration::from_secs(30)), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn first_claim_without_a_pid_in_page_mode() {
        let g = leaked_guard();
        let c = g.choose_with(map_wiped_page);
        if c.by_pid && no_page("first_claim_without_a_pid_in_page_mode") {
            return;
        }
        let o = Owner::new();
        let s = o
            .claim_with(
                || Seen { pid: 0, ..g.seen() },
                || g.choose_with(map_wiped_page),
                |c, owner| {
                    g.finish_with(
                        c,
                        Some(|| -> libc::c_int { panic!("no handler in page mode") }),
                        (owner >> 32) as u32,
                    )
                },
            )
            .unwrap();
        let stored = o.id.load(Ordering::SeqCst);
        assert_eq!(pid_of(stored), 0);
        assert_ne!(stored >> 32, 0);
        assert_eq!(s.pid, 0);
        o.mark_init_done();
        let token = g.seen().token;
        let no_choose = || -> Chosen { panic!("no setup for the owner") };
        let no_finish = |_: Chosen, _: Ident| -> u64 { panic!("no setup for the owner") };
        assert!(o
            .claim_with(
                || Seen {
                    pid: 1234,
                    ..g.seen()
                },
                no_choose,
                no_finish
            )
            .is_ok());
        assert!(is_forked(o.claim_with(
            || Seen {
                token: token + 1,
                ..g.seen()
            },
            no_choose,
            no_finish
        )));
    }

    /// On a C library that does not cache the pid this cannot tell
    /// `getpid()` apart; it guards the value.
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn pid_matches_the_kernel() {
        // SAFETY: SYS_getpid has no preconditions.
        let kernel = unsafe { libc::syscall(libc::SYS_getpid) };
        assert_eq!(libc::c_long::from(pid()), kernel);
        let parent = pid();
        let child = clone_child(|| i32::from(pid() == parent));
        assert_eq!(wait_child(child, Duration::from_secs(30)), 0);
    }

    #[test]
    fn token_is_armed_once() {
        let g = leaked_guard();
        let c = g.choose_with(|| None);
        let a = g.finish_with(c, no_install(), 0);
        let b = g.finish_with(c, no_install(), 0);
        assert_ne!(a, 0);
        assert_eq!(a, b);
        assert_eq!(g.next.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn token_skips_zero_and_the_replaced_owner() {
        let arm = |next: u64, avoid: u32| {
            let g = Guard::new();
            g.next.store(next, Ordering::SeqCst);
            let cell = AtomicU64::new(0);
            g.arm(&cell, avoid)
        };
        assert_eq!(arm(0xffff_ffff, 0), 0x1_0000_0001);
        assert_eq!(arm(0xffff_ffff, 1), 0x1_0000_0002);
        assert_eq!(arm(6, 7), 8);
        assert_eq!(arm(u64::MAX, 0), 1);
    }

    #[test]
    fn reserved_draw_is_not_reused() {
        let g = Guard::new();
        g.next.store(5, Ordering::SeqCst);
        let cell = AtomicU64::new(0);
        assert_eq!(g.arm(&cell, 0), 6);
    }

    #[test]
    fn cleared_cell_gets_a_larger_token() {
        let g = leaked_guard();
        let c = g.choose_with(|| None);
        let old = g.finish_with(c, no_install(), 0);
        g.clear();
        let new = g.finish_with(c, no_install(), 0);
        assert!(new > old);
        let s = Stamp {
            cell: c.cell,
            token: old,
            pid: pid(),
            by_pid: false,
        };
        assert!(is_forked(s.check()));
    }

    #[test]
    fn concurrent_arming_agrees() {
        use std::sync::{Arc, Barrier};
        const ROUNDS: usize = 2_000;
        let guards: Arc<Vec<&'static Guard>> =
            Arc::new((0..ROUNDS).map(|_| leaked_guard()).collect());
        let barrier = Arc::new(Barrier::new(2));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let guards = Arc::clone(&guards);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    guards
                        .iter()
                        .map(|g| {
                            barrier.wait();
                            let c = g.choose_with(|| None);
                            g.finish_with(c, no_install(), 0)
                        })
                        .collect::<Vec<u64>>()
                })
            })
            .collect();
        let tokens: Vec<Vec<u64>> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        assert_eq!(tokens[0], tokens[1]);
        assert!(tokens[0].iter().all(|&t| t != 0));
    }

    #[test]
    fn seen_changes_nothing() {
        let g = leaked_guard();
        let s = g.seen();
        assert!(s.chosen.is_none());
        assert_eq!(s.token, 0);
        assert!(g.cell.load(Ordering::SeqCst).is_null());
        assert!(!g.handler.load(Ordering::SeqCst));
        assert_eq!(g.next.load(Ordering::SeqCst), 0);
        let c = g.choose_with(|| None);
        let t = g.finish_with(c, no_install(), 0);
        let s = g.seen();
        assert_eq!(s.token, t);
        assert!(s.chosen.is_some_and(|c| c.by_pid));
    }

    #[test]
    fn choosing_writes_no_token() {
        let g = leaked_guard();
        let c = g.choose_with(|| None);
        assert!(c.by_pid);
        assert!(ptr::eq(c.cell, &g.plain));
        assert_eq!(g.plain.load(Ordering::SeqCst), 0);
        assert_eq!(g.next.load(Ordering::SeqCst), 0);
        assert!(!g.handler.load(Ordering::SeqCst));
        let again = g.choose_with(|| -> Option<Page> { panic!("chosen already") });
        assert!(ptr::eq(again.cell, c.cell));
    }

    #[test]
    fn plain_mode_decides_before_it_installs() {
        let g = leaked_guard();
        let c = g.choose_with(|| None);
        let calls = Cell::new(0);
        g.finish_with(
            c,
            Some(|| {
                assert!(ptr::eq(g.cell.load(Ordering::SeqCst), &g.plain));
                calls.set(calls.get() + 1);
                0
            }),
            0,
        );
        assert_eq!(calls.get(), 1);
        g.finish_with(
            c,
            Some(|| {
                calls.set(calls.get() + 1);
                0
            }),
            0,
        );
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn failed_handler_install_is_retried() {
        let g = leaked_guard();
        assert!(!g.handler_with(|| libc::ENOMEM));
        assert!(!g.handler.load(Ordering::SeqCst));
        assert!(g.handler_with(|| 0));
        assert!(g.handler_with(|| -> libc::c_int { panic!("installed already") }));
        let g = leaked_guard();
        let c = g.choose_with(|| None);
        g.finish_with(c, no_install(), 0);
        assert!(!g.handler.load(Ordering::SeqCst));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn page_mode_never_installs_a_handler() {
        let g = leaked_guard();
        let c = g.choose_with(map_wiped_page);
        if c.by_pid && no_page("page_mode_never_installs_a_handler") {
            return;
        }
        let calls = Cell::new(0);
        g.finish_with(
            c,
            Some(|| {
                calls.set(calls.get() + 1);
                0
            }),
            0,
        );
        assert_eq!(calls.get(), 0);
        assert!(!g.handler.load(Ordering::SeqCst));
    }

    /// The production claim and stamp in a process in page mode install no
    /// handler.
    #[cfg(target_os = "linux")]
    #[test]
    fn page_mode_claim_and_stamp_install_no_handler() {
        if rerun_alone("fork::tests::page_mode_claim_and_stamp_install_no_handler") {
            return;
        }
        let s = OWNER.claim().unwrap();
        if s.by_pid && no_page("page_mode_claim_and_stamp_install_no_handler") {
            return;
        }
        let t = Stamp::now();
        assert!(!t.by_pid);
        assert_eq!(INSTALL_CALLS.load(Ordering::SeqCst), 0);
        assert!(!GUARD.handler.load(Ordering::SeqCst));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mapper_rejects_a_layer_that_accepts_any_advice() {
        assert!(map_wiped_page_with(|_, _, _| 0).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mapper_rejects_a_failed_wipe_advice() {
        assert!(map_wiped_page_with(|_, _, _| -1).is_none());
    }

    /// A completes its choice inside B's map closure. Returns (winner cell,
    /// A's page, B's page).
    fn race(g: &'static Guard) -> (Chosen, usize, usize, usize) {
        let mut a_addr = 0;
        let mut b_addr = 0;
        let mut len = 0;
        let b = g.choose_with(|| {
            let pb = anon_page();
            b_addr = pb.as_ptr() as usize;
            len = pb.len;
            let a = g.choose_with(|| {
                let pa = anon_page();
                a_addr = pa.as_ptr() as usize;
                Some(pa)
            });
            assert_eq!(a.cell as *const AtomicU64 as usize, a_addr);
            Some(pb)
        });
        (b, a_addr, b_addr, len)
    }

    #[test]
    fn racing_cell_choice_keeps_the_first() {
        let g = leaked_guard();
        let (b, a_addr, b_addr, _) = race(g);
        assert_eq!(b.cell as *const AtomicU64 as usize, a_addr);
        assert_ne!(a_addr, b_addr);
        assert!(!b.by_pid);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn losing_page_is_unmapped() {
        if rerun_alone("fork::tests::losing_page_is_unmapped") {
            return;
        }
        let g = leaked_guard();
        let (_, _, b_addr, len) = race(g);
        // The process maps nothing between the race and this call.
        // SAFETY: msync only reports whether the range is mapped.
        let rc = unsafe { libc::msync(b_addr as *mut libc::c_void, len, libc::MS_ASYNC) };
        let errno = std::io::Error::last_os_error().raw_os_error();
        assert_eq!(rc, -1);
        assert_eq!(errno, Some(libc::ENOMEM));
    }

    #[test]
    fn refusal_runs_no_setup() {
        let g = leaked_guard();
        let c = g.choose_with(|| None);
        let o = Owner::new();
        let me = pid();
        o.id.store(ident_of(me, 9), Ordering::SeqCst);
        let _busy = o.enter_init();
        let r = o.claim_with(
            || Seen {
                chosen: Some(c),
                token: 0,
                pid: me,
            },
            || -> Chosen { panic!("no setup on a refusal") },
            |_, _| -> u64 { panic!("no setup on a refusal") },
        );
        assert!(forked(r, me, me));
        assert_eq!(o.id.load(Ordering::SeqCst), ident_of(me, 9));
    }

    #[test]
    fn busy_flags_without_an_owner_refuse() {
        let o = Owner::new();
        let _busy = o.enter_init();
        let me = pid();
        let r = o.claim_with(
            || Seen {
                chosen: None,
                token: 0,
                pid: me,
            },
            || -> Chosen { panic!("no setup on a refusal") },
            |_, _| -> u64 { panic!("no setup on a refusal") },
        );
        assert!(forked(r, 0, me));
        assert_eq!(o.id.load(Ordering::SeqCst), 0);
    }

    /// A thread of this process makes the first claim between another
    /// thread's start and its owner load: that thread must see itself as the
    /// owner. Reading the token before the owner would refuse it.
    #[test]
    fn first_claim_right_before_the_owner_load() {
        if rerun_alone("fork::tests::first_claim_right_before_the_owner_load") {
            return;
        }
        set_before_owner_load(Box::new(|| {
            let c = GUARD.choose_with(map_wiped_page);
            let t = GUARD.finish_with(c, Some(install_handler), 0);
            OWNER.id.store(ident_of(pid(), t), Ordering::SeqCst);
            std::mem::forget(OWNER.enter_init());
        }));
        assert!(OWNER.claim().is_ok());

        let g = leaked_guard();
        let o: &'static Owner = Box::leak(Box::new(Owner::new()));
        let early = g.seen();
        set_before_owner_load(Box::new(move || {
            let c = g.choose_with(|| None);
            let t = g.finish_with(c, Some(|| 0), 0);
            o.id.store(ident_of(pid(), t), Ordering::SeqCst);
            std::mem::forget(o.enter_init());
        }));
        let r = o.claim_with(
            || early,
            || g.choose_with(|| None),
            |c, owner| g.finish_with(c, Some(|| 0), (owner >> 32) as u32),
        );
        assert!(is_forked(r), "a sample taken before the owner load");
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn wiped_page_reads_zero_in_a_child() {
        let Some(page) = map_wiped_page() else {
            no_page("wiped_page_reads_zero_in_a_child");
            return;
        };
        // SAFETY: the page is mapped, zeroed and aligned for an AtomicU64.
        let cell = unsafe { &*page.as_ptr() };
        cell.store(7, Ordering::SeqCst);
        let child = clone_child(|| i32::from(cell.load(Ordering::SeqCst) != 0));
        assert_eq!(wait_child(child, Duration::from_secs(30)), 0);
        assert_eq!(cell.load(Ordering::SeqCst), 7);
    }

    #[test]
    fn handler_clears_the_plain_cell_in_a_fork_child() {
        static G: Guard = Guard::new();
        extern "C" fn clear_g() {
            G.clear();
        }
        // SAFETY: the handler performs one atomic store and never unwinds.
        assert_eq!(
            unsafe { libc::pthread_atfork(None, None, Some(clear_g)) },
            0
        );
        let c = G.choose_with(|| None);
        let t = G.finish_with(c, no_install(), 0);
        let s = Stamp {
            cell: c.cell,
            token: t,
            pid: pid(),
            by_pid: true,
        };
        let child = fork_child(|| {
            if G.plain.load(Ordering::SeqCst) != 0 {
                return 1;
            }
            if G.finish_with(c, no_install(), 0) <= t {
                return 2;
            }
            let forged = Stamp {
                pid: pid(),
                by_pid: false,
                ..s
            };
            if !is_forked(forged.check()) {
                return 3;
            }
            0
        });
        assert_eq!(wait_child(child, Duration::from_secs(30)), 0);
        assert_eq!(G.plain.load(Ordering::SeqCst), t);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fork_while_the_page_is_unpublished() {
        let g = leaked_guard();
        let mut published = 0;
        let mut ran = false;
        let c = g.choose_with(|| {
            let page = map_wiped_page()?;
            published = page.as_ptr() as usize;
            ran = true;
            let child = fork_child(|| {
                if !g.cell.load(Ordering::SeqCst).is_null() {
                    return 1;
                }
                let start = Instant::now();
                let c = g.choose_with(map_wiped_page);
                let t = g.finish_with(c, no_install(), 0);
                if t == 0 {
                    return 2;
                }
                if start.elapsed() > Duration::from_secs(2) {
                    return 3;
                }
                0
            });
            assert_eq!(wait_child(child, Duration::from_secs(30)), 0);
            Some(page)
        });
        if !ran {
            no_page("fork_while_the_page_is_unpublished");
            return;
        }
        assert!(!c.by_pid);
        assert_eq!(g.cell.load(Ordering::SeqCst) as usize, published);
    }

    /// The checks of a child of a process that claimed with token 1, then
    /// set `next` to 0xffff_ffff, and completed init.
    fn fresh_process_child_checks(s: Stamp, installs: usize) -> i32 {
        if !is_forked(OWNER.claim()) {
            return 1;
        }
        if GUARD.seen().token != 0 {
            return 2;
        }
        if GUARD.next.load(Ordering::SeqCst) != 0xffff_ffff {
            return 3;
        }
        if GUARD.handler.load(Ordering::SeqCst) || INSTALL_CALLS.load(Ordering::SeqCst) != installs
        {
            return 4;
        }
        let c = Stamp::now();
        if c.token != 0x1_0000_0002 {
            return 5;
        }
        if c.check().is_err() {
            return 6;
        }
        let me = pid();
        OWNER.id.store(ident_of(me, 1), Ordering::SeqCst);
        if !forked(OWNER.claim(), me, me) {
            return 7;
        }
        if !is_forked(Stamp { pid: me, ..s }.check_cold()) {
            return 8;
        }
        0
    }

    /// Claims in a fresh process; `None` when plain mode was chosen.
    fn fresh_process_claim() -> Option<Stamp> {
        assert!(GUARD.cell.load(Ordering::SeqCst).is_null());
        assert_eq!(OWNER.id.load(Ordering::SeqCst), 0);
        let s = OWNER.claim().unwrap();
        if s.by_pid {
            return None;
        }
        assert_eq!(s.token, 1);
        assert_eq!(OWNER.id.load(Ordering::SeqCst), ident_of(pid(), 1));
        assert_eq!(s.pid, pid());
        OWNER.mark_init_done();
        GUARD.next.store(0xffff_ffff, Ordering::SeqCst);
        Some(s)
    }

    fn fresh_process_parent_checks(s: Stamp) {
        assert_eq!(GUARD.seen().token, 1);
        assert!(OWNER.claim().is_ok());
        assert!(s.check_cold().is_ok());
    }

    #[test]
    fn first_claim_in_a_fresh_process_then_fork() {
        if rerun_alone("fork::tests::first_claim_in_a_fresh_process_then_fork") {
            return;
        }
        let Some(s) = fresh_process_claim() else {
            no_page("first_claim_in_a_fresh_process_then_fork");
            return;
        };
        let installs = INSTALL_CALLS.load(Ordering::SeqCst);
        let child = fork_child(|| fresh_process_child_checks(s, installs));
        assert_eq!(wait_child(child, Duration::from_secs(30)), 0);
        fresh_process_parent_checks(s);
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn first_claim_in_a_fresh_process_then_raw_clone() {
        if rerun_alone("fork::tests::first_claim_in_a_fresh_process_then_raw_clone") {
            return;
        }
        let Some(s) = fresh_process_claim() else {
            assert!(
                !wipe_required(),
                "the kernel does not clear the page in children"
            );
            // Plain mode: the raw clone child is told apart by its pid.
            let s = Stamp::now();
            OWNER.mark_init_done();
            let child = clone_child(|| {
                if !is_forked(s.check()) {
                    return 1;
                }
                i32::from(!is_forked(OWNER.claim()))
            });
            assert_eq!(wait_child(child, Duration::from_secs(30)), 0);
            return;
        };
        let installs = INSTALL_CALLS.load(Ordering::SeqCst);
        let child = clone_child(|| fresh_process_child_checks(s, installs));
        assert_eq!(wait_child(child, Duration::from_secs(30)), 0);
        fresh_process_parent_checks(s);
    }

    static PARKED: AtomicBool = AtomicBool::new(false);
    static UNPARK: AtomicBool = AtomicBool::new(false);

    fn enomem() -> libc::c_int {
        libc::ENOMEM
    }

    fn park_then_enomem() -> libc::c_int {
        PARKED.store(true, Ordering::SeqCst);
        while !UNPARK.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        libc::ENOMEM
    }

    /// The checks of a child made while a parent thread is inside the
    /// handler installation.
    fn parked_child_checks(s: Stamp, installs: usize) -> i32 {
        let start = Instant::now();
        if !is_forked(OWNER.claim()) {
            return 1;
        }
        let _ = Stamp::now();
        if start.elapsed() > Duration::from_secs(2) {
            return 2;
        }
        if INSTALL_CALLS.load(Ordering::SeqCst) != installs {
            return 3;
        }
        if !is_forked(s.check()) {
            return 4;
        }
        0
    }

    #[test]
    fn plain_mode_install_retry_and_refusal() {
        if rerun_alone("fork::tests::plain_mode_install_retry_and_refusal") {
            return;
        }
        force_plain();
        set_install_override(Some(enomem));
        assert!(OWNER.claim().is_ok());
        assert_eq!(INSTALL_CALLS.load(Ordering::SeqCst), 1);
        assert!(!GUARD.handler.load(Ordering::SeqCst));
        let s = Stamp::now();
        assert_eq!(INSTALL_CALLS.load(Ordering::SeqCst), 2);
        assert!(s.by_pid);
        OWNER.mark_init_done();

        set_install_override(Some(park_then_enomem));
        let t = std::thread::spawn(Stamp::now);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !PARKED.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "installation never entered");
            std::thread::sleep(Duration::from_millis(1));
        }
        let installs = INSTALL_CALLS.load(Ordering::SeqCst);
        let child = fork_child(|| parked_child_checks(s, installs));
        assert_eq!(wait_child(child, Duration::from_secs(30)), 0);
        #[cfg(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        {
            let token = GUARD.plain.load(Ordering::SeqCst);
            let child = clone_child(|| {
                if GUARD.plain.load(Ordering::SeqCst) != token {
                    return 10;
                }
                parked_child_checks(s, installs)
            });
            assert_eq!(wait_child(child, Duration::from_secs(30)), 0);
        }
        UNPARK.store(true, Ordering::SeqCst);
        t.join().unwrap();
        assert!(!GUARD.handler.load(Ordering::SeqCst));

        set_install_override(None);
        let before = INSTALL_CALLS.load(Ordering::SeqCst);
        let _ = Stamp::now();
        assert_eq!(INSTALL_CALLS.load(Ordering::SeqCst), before + 1);
        assert!(GUARD.handler.load(Ordering::SeqCst));
        let _ = Stamp::now();
        assert_eq!(INSTALL_CALLS.load(Ordering::SeqCst), before + 1);
        let child = fork_child(|| {
            if GUARD.plain.load(Ordering::SeqCst) != 0 {
                return 1;
            }
            i32::from(!is_forked(s.check()))
        });
        assert_eq!(wait_child(child, Duration::from_secs(30)), 0);
    }
}
