// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pool and container connections owned by the crate, and the process-wide
//! registry that shares them.
//!
//! A [`PoolConn`] is one `daos_pool_connect` handle, a [`ContConn`] one
//! `daos_cont_open` handle on top of a pool connection, and a DFS mount
//! (`crate::dfs::Mount`) one `dfs_mount` on top of a container connection.
//! Each layer holds an `Arc` of the layer below (a lease). When the last
//! lease of a connection goes, the connection is closed: the container
//! before its pool, which is the order libdaos requires (`daos_cont_close`
//! returns `-DER_BUSY` while objects of the container are open,
//! `daos_pool_disconnect` while containers of the pool are open).
//!
//! Each connection also has a close gate (see `crate::gate`): every call that
//! passes its handle to libdaos holds the gate shared, and an explicit
//! `close` takes it exclusively. After an explicit close the handle is gone
//! for every view, and later calls return [`Error::Closed`].
//!
//! # Container connections and their mounts
//!
//! A container connection lists every DFS mount of it that is not
//! unmounted, oldest first, and holds each listed mount strongly; a listed
//! mount holds a lease of the connection. The cycle is cut when `dfs_umount`
//! of the mount succeeds: only then does the thread that ran it take the
//! mount out of the list and give up the lease. A mount whose unmount
//! failed, or that was dropped
//! while the process could not be confirmed, stays listed with its lease for
//! the rest of the process. So does the connection of a transaction that
//! was not closed: it keeps one reference of the connection for good.
//!
//! Each mount has one atomic state (`state`): mounted, unmounting (one
//! thread is inside its teardown), unmounted, failed (with the errno of
//! `dfs_umount`) or orphaned (with the recorded process id). Mounted leaves
//! by one exchange, so one thread runs the teardown: the drop of the last
//! user of the mount, or an explicit close (of the mount or of the
//! container) that holds the mount gate exclusively. The connection's
//! native handle has a state of its own (`life`): open, closing, closed, or
//! kept open for the rest of the process.
//!
//! Users rule: a call into libdfs for a mount is made by a thread that holds
//! a user handle of it (`crate::dfs::MountRef`), with two exceptions that
//! rest on the exchange instead: the teardown run by the drop of the last
//! user, and the teardown started by a container close. So when no user is
//! left, no call of the mount runs or can start, and the one teardown has
//! the mount to itself.
//!
//! Lease rule: a container connection whose last reference goes has no
//! listed mount and no transaction left open, so its drop or release never
//! closes the native handle under a mount or a transaction. Holding a mount
//! never starts or delays its unmount.
//!
//! A container close takes its gate exclusively and decides from the mount
//! states, the open transactions and the transaction fault, read so that
//! the verdict drawn from them held at one instant: an error that keeps the
//! connection open is
//! returned at once; open transactions or mounts being unmounted make it
//! wait; otherwise it unmounts the mounts that are still mounted, oldest
//! first, trying each mount gate once, and closes the handle when every
//! mount is unmounted.
//!
//! Lock order, outermost first: the container gate, a mount gate (tried,
//! never blocked on, under the container gate), the mount list of the
//! connection (tried by a close), then the leaf mutexes (the container lease
//! of a mount, its handle table and directory cache, the pool lease of a
//! connection), none of which is held across another lock, a DAOS call or
//! the drop of a handle. The registry lock is never held across a gate, a
//! list lock or a DAOS call.
//!
//! The registry keys pools by `(pool string, access mode)` and containers and
//! mounts by `(pool string, container string, access mode)`. A request for a
//! key with a live entry returns that entry. Label and UUID strings of the
//! same pool or container are different keys. The registry lock is never held
//! during a DAOS call: a connect in progress reserves its key, and a second
//! request for the same key waits for it. A connection upgraded from the
//! registry under the lock is dropped only after the lock is released, since
//! dropping the last reference closes it.
//!
//! One DAOS system per process: the first successful connect fixes it.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError, Weak};
use std::time::Duration;

use crate::dfs::{CloseHold, Mount, MountRef, Unmount};
use crate::error::{Error, Result};
use crate::fork::Stamp;
use crate::gate::{Check, Deadline, Final, Gate, Last, Pass};
use crate::sys::api::{Group, Raw};
use crate::sys::{self, daos_handle_t};
use crate::uuid::Uuid;

/// Name DAOS uses when no system name is given (`DAOS_DEFAULT_SYS_NAME`,
/// `src/include/daos/mgmt.h`). `None` and this name are the same system.
pub(crate) const DEFAULT_SYSTEM_NAME: &str = "daos_server";

/// Access mode of a pool or container connection.
///
/// A read-only container connection always sits on a read-only pool
/// connection and a read-write one on a read-write pool connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Access {
    /// Read-only (`DAOS_PC_RO`, `DAOS_COO_RO`, `O_RDONLY`).
    ReadOnly,
    /// Read-write (`DAOS_PC_RW`, `DAOS_COO_RW`, `O_RDWR`).
    #[default]
    ReadWrite,
}

impl Access {
    pub(crate) fn pool_flags(self) -> u32 {
        match self {
            Access::ReadOnly => sys::DAOS_PC_RO,
            Access::ReadWrite => sys::DAOS_PC_RW,
        }
    }

    pub(crate) fn cont_flags(self) -> u32 {
        match self {
            Access::ReadOnly => sys::DAOS_COO_RO,
            Access::ReadWrite => sys::DAOS_COO_RW,
        }
    }

    pub(crate) fn open_flags(self) -> i32 {
        match self {
            Access::ReadOnly => libc::O_RDONLY,
            Access::ReadWrite => libc::O_RDWR,
        }
    }
}

pub(crate) fn cstring(s: &str, what: &'static str) -> Result<CString> {
    CString::new(s).map_err(|_| Error::InvalidArgument(what))
}

/// Check that a symbol group is available on `backend`.
pub(crate) fn require(backend: &dyn Raw, group: Group) -> Result<()> {
    match backend.missing(group) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Pool connection

/// One pool handle. Disconnected when the last `Arc` goes (quietly), or by
/// an explicit close or release (with an error report).
pub(crate) struct PoolConn {
    pub(crate) backend: Arc<dyn Raw>,
    pub(crate) poh: daos_handle_t,
    pub(crate) name: String,
    pub(crate) sys_name: Option<String>,
    pub(crate) access: Access,
    pub(crate) uuid: Uuid,
    pub(crate) stamp: Stamp,
    pub(crate) gate: Gate,
    /// Container connections open on this pool.
    pub(crate) conts: AtomicUsize,
    /// Set once the handle was disconnected (or must not be touched).
    done: AtomicBool,
}

impl PoolConn {
    fn connect(
        backend: Arc<dyn Raw>,
        name: &str,
        sys_name: Option<&str>,
        access: Access,
        stamp: Stamp,
    ) -> Result<PoolConn> {
        let c_name = cstring(name, "pool names must not contain NUL")?;
        let c_sys = sys_name
            .map(|s| cstring(s, "system names must not contain NUL"))
            .transpose()?;
        let mut poh = daos_handle_t::default();
        // pi_bits 0: no space or rebuild query, only the identity.
        let mut info = sys::daos_pool_info_t::default();
        // SAFETY: NUL-terminated strings that outlive the call; sys may be
        // NULL (default system); info points to a local of the header's
        // layout; ev NULL (blocking); out-pointers to locals.
        let rc = unsafe {
            backend.daos_pool_connect2(
                c_name.as_ptr(),
                c_sys.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                access.pool_flags(),
                &mut poh,
                (&mut info as *mut sys::daos_pool_info_t).cast(),
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            return Err(Error::daos_at("daos_pool_connect", rc, name));
        }
        Ok(PoolConn {
            backend,
            poh,
            name: name.to_string(),
            sys_name: sys_name.map(str::to_string),
            access,
            uuid: Uuid::from_bytes(info.pi_uuid),
            stamp,
            gate: Gate::new(),
            conts: AtomicUsize::new(0),
            done: AtomicBool::new(false),
        })
    }

    /// For places without an error to return: a failed process check reads
    /// as "not usable".
    pub(crate) fn is_usable(&self) -> bool {
        !self.done.load(Ordering::SeqCst) && !self.stamp.is_foreign() && !self.gate.is_closed()
    }

    /// Whether this connection can serve a new user. An error when this
    /// process is not, or cannot be shown to be, the one that opened it.
    pub(crate) fn usable(&self) -> Result<bool> {
        self.stamp.check_cold()?;
        Ok(!self.done.load(Ordering::SeqCst) && !self.gate.is_closed())
    }

    /// Enter a pool operation.
    pub(crate) fn enter(&self) -> Result<Pass<'_>> {
        self.stamp.check()?;
        self.gate.enter(false)
    }

    fn disconnect(&self) -> Result<()> {
        if self.done.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        // SAFETY: a handle from daos_pool_connect, disconnected once (guarded
        // by `done`); every container of this pool released its lease first.
        let rc = unsafe {
            self.backend
                .daos_pool_disconnect(self.poh, std::ptr::null_mut())
        };
        if rc != 0 {
            return Err(Error::daos_at("daos_pool_disconnect", rc, &self.name));
        }
        Ok(())
    }

    /// Explicit close: wait up to `timeout` until no operation runs and no
    /// container connection of this pool is open, then disconnect.
    pub(crate) fn close(&self, timeout: Duration) -> Result<()> {
        self.stamp.check_cold()?;
        let precheck = |_| match self.conts.load(Ordering::SeqCst) {
            0 => Check::Ready,
            n => Check::Wait(Error::busy_open(n)),
        };
        match self
            .gate
            .close_when(timeout, precheck, || self.disconnect())?
        {
            None => Ok(()),
            Some(r) => r,
        }
    }

    /// Give up one lease and disconnect when it was the last one, reporting
    /// the error of `daos_pool_disconnect`.
    ///
    /// When this process cannot be confirmed as the one that connected, the
    /// handle is marked done (its drop makes no DAOS call) and the error of
    /// the check is returned.
    ///
    /// Of two releases of the last two leases at once, exactly one gets the
    /// connection and reports.
    pub(crate) fn release(this: Arc<PoolConn>) -> Result<()> {
        let Some(p) = Arc::into_inner(this) else {
            return Ok(());
        };
        #[cfg(test)]
        crate::fork::testing::fault_point("pool_release");
        if let Err(e) = p.stamp.check_cold() {
            p.done.store(true, Ordering::SeqCst);
            return Err(e);
        }
        p.disconnect()
    }
}

impl Drop for PoolConn {
    fn drop(&mut self) {
        if self.stamp.is_foreign() {
            return; // fork child: leak, never call libdaos
        }
        let _ = self.disconnect();
    }
}

// ---------------------------------------------------------------------------
// States

/// Values of `Mount::state` and `ContConn::tx_fault`: a tag in the low 8
/// bits, a payload (an errno, a DER code or a process id) in the high 32.
pub(crate) mod state {
    /// The `dfs_t` is usable.
    pub(crate) const MOUNTED: u8 = 0;
    /// One thread is inside the teardown (`dfs_release`, `dfs_umount`).
    pub(crate) const UNMOUNTING: u8 = 1;
    /// `dfs_umount` returned 0.
    pub(crate) const UNMOUNTED: u8 = 2;
    /// `dfs_umount` failed with the errno in the payload.
    pub(crate) const FAILED: u8 = 3;
    /// The last user dropped the mount while the process could not be
    /// confirmed; the payload is the recorded process id.
    pub(crate) const ORPHANED: u8 = 4;

    /// `daos_tx_close` failed with the DER code in the payload.
    pub(crate) const TX_FAILED: u8 = 1;
    /// A transaction close was skipped because the process could not be
    /// confirmed; the payload is the recorded process id.
    pub(crate) const TX_ORPHANED: u8 = 2;

    pub(crate) fn pack(tag: u8, payload: u32) -> u64 {
        u64::from(payload) << 32 | u64::from(tag)
    }

    pub(crate) fn tag(v: u64) -> u8 {
        v as u8
    }

    pub(crate) fn payload(v: u64) -> u32 {
        (v >> 32) as u32
    }
}

/// Values of `ContConn::life`.
pub(crate) mod life {
    /// The native handle is open and usable.
    pub(crate) const OPEN: u8 = 0;
    /// `daos_cont_close` is running.
    pub(crate) const CLOSING: u8 = 1;
    /// The native handle is closed.
    pub(crate) const CLOSED: u8 = 2;
    /// The native handle stays open for the rest of the process.
    pub(crate) const KEPT: u8 = 3;
}

use state::{FAILED, MOUNTED, ORPHANED, TX_FAILED, UNMOUNTED, UNMOUNTING};

/// What stands in the way of a container close at one instant.
#[derive(Debug)]
pub(crate) enum Verdict {
    /// An error that stays for the life of the process.
    Standing(Error),
    /// A reason to wait.
    Busy(Error),
}

/// The error of a failed `dfs_umount` on `target` ("pool/container").
pub(crate) fn umount_error(target: &str, errno: i32) -> Error {
    Error::Dfs {
        op: "dfs_umount",
        errno,
        path: Some(target.to_string()),
    }
}

/// The verdict on mount states (oldest first), open transactions and the
/// transaction fault of a container `target`. The first that applies: a
/// failed unmount (oldest), an orphaned mount (oldest), the transaction
/// fault, then open transactions, then mounts being unmounted.
pub(crate) fn verdict(
    target: &str,
    states: &[u64],
    transactions: usize,
    tx_fault: u64,
) -> Option<Verdict> {
    if let Some(s) = states.iter().find(|s| state::tag(**s) == FAILED) {
        let errno = state::payload(*s) as i32;
        return Some(Verdict::Standing(umount_error(target, errno)));
    }
    if let Some(s) = states.iter().find(|s| state::tag(**s) == ORPHANED) {
        return Some(Verdict::Standing(Error::Forked {
            created_pid: state::payload(*s),
            current_pid: 0,
        }));
    }
    if tx_fault != 0 {
        let p = state::payload(tx_fault);
        return Some(Verdict::Standing(if state::tag(tx_fault) == TX_FAILED {
            Error::daos("daos_tx_close", -(p as i32))
        } else {
            Error::Forked {
                created_pid: p,
                current_pid: 0,
            }
        }));
    }
    if transactions > 0 {
        return Some(Verdict::Busy(Error::busy(0, transactions)));
    }
    let departing = states
        .iter()
        .filter(|s| state::tag(**s) == UNMOUNTING)
        .count();
    if departing > 0 {
        return Some(Verdict::Busy(Error::busy_open(departing)));
    }
    None
}

/// The mounts of a container with their states, the open transactions and
/// the transaction fault, read so that the verdict drawn from them was true
/// at one instant.
pub(crate) struct Observed {
    pub(crate) mounts: Vec<Arc<Mount>>,
    pub(crate) states: Vec<u64>,
    pub(crate) transactions: usize,
    pub(crate) tx_fault: u64,
}

#[cfg(all(test, feature = "mock"))]
pub(crate) static CONT_DROPS: AtomicUsize = AtomicUsize::new(0);

// ---------------------------------------------------------------------------
// Container connection

/// One container handle on a pool connection. Closed when the last `Arc`
/// goes (quietly), or by an explicit close or release (with an error report).
pub(crate) struct ContConn {
    /// Dropped first among the fields, after `Drop::drop` returned.
    #[cfg(all(test, feature = "mock"))]
    _dropped: crate::gate::testing::DropPoint,
    pub(crate) backend: Arc<dyn Raw>,
    pub(crate) coh: daos_handle_t,
    pub(crate) name: String,
    pub(crate) uuid: Uuid,
    pub(crate) access: Access,
    pub(crate) stamp: Stamp,
    pub(crate) gate: Gate,
    /// Every DFS mount of this container that is not unmounted, oldest
    /// first. A listed mount holds a lease of this connection, so the list
    /// and the leases form a cycle that a successful `dfs_umount` of each
    /// mount cuts.
    pub(crate) mounts: Mutex<Vec<Arc<Mount>>>,
    /// Raised under the `mounts` lock at every push and every removal.
    pub(crate) mounts_gen: AtomicU64,
    /// Object, KV and array handles open on this container.
    pub(crate) handles: crate::handles::Table,
    /// Transactions open on this container.
    pub(crate) transactions: AtomicUsize,
    /// The pool lease; `None` only after a close or release took it.
    pool: Mutex<Option<Arc<PoolConn>>>,
    /// Pool handle, kept for calls (valid while the lease is held).
    pub(crate) poh: daos_handle_t,
    /// Pool string of the lease (for errors and registry keys).
    pub(crate) pool_name: String,
    /// Whether the native handle is open, closing, closed, or left open for
    /// good (see [`life`]). Shared with the registry
    /// (`RegistryInner::open_conts`), so a destroy sees the native handle
    /// also when the connection is gone or being dropped.
    pub(crate) life: Arc<AtomicU8>,
    /// The first native transaction of this container that was not closed
    /// (0: none). Set once.
    pub(crate) tx_fault: AtomicU64,
}

impl ContConn {
    fn open(pool: Arc<PoolConn>, name: &str, stamp: Stamp) -> Result<ContConn> {
        let c_name = cstring(name, "container names must not contain NUL")?;
        let mut coh = daos_handle_t::default();
        let mut info = sys::daos_cont_info_t::default();
        let backend = pool.backend.clone();
        {
            let _pass = pool.enter()?;
            // SAFETY: live pool handle (gate held); NUL-terminated name; info
            // points to a local of the header's layout; ev NULL (blocking);
            // out-pointers to locals.
            let rc = unsafe {
                backend.daos_cont_open2(
                    pool.poh,
                    c_name.as_ptr(),
                    pool.access.cont_flags(),
                    &mut coh,
                    (&mut info as *mut sys::daos_cont_info_t).cast(),
                    std::ptr::null_mut(),
                )
            };
            if rc != 0 {
                return Err(Error::daos_at(
                    "daos_cont_open",
                    rc,
                    format!("{}/{}", pool.name, name),
                ));
            }
            pool.conts.fetch_add(1, Ordering::SeqCst);
        }
        Ok(ContConn {
            #[cfg(all(test, feature = "mock"))]
            _dropped: crate::gate::testing::DropPoint("ContDropReturned"),
            backend,
            coh,
            name: name.to_string(),
            uuid: Uuid::from_bytes(info.ci_uuid),
            access: pool.access,
            stamp,
            gate: Gate::new(),
            mounts: Mutex::new(Vec::new()),
            mounts_gen: AtomicU64::new(0),
            handles: crate::handles::Table::default(),
            transactions: AtomicUsize::new(0),
            poh: pool.poh,
            pool_name: pool.name.clone(),
            pool: Mutex::new(Some(pool)),
            life: Arc::new(AtomicU8::new(life::OPEN)),
            tx_fault: AtomicU64::new(0),
        })
    }

    /// The pool connection, or `Closed` after the container was closed.
    pub(crate) fn pool(&self) -> Result<Arc<PoolConn>> {
        self.pool
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or(Error::Closed)
    }

    /// "pool/container" for errors.
    pub(crate) fn target(&self) -> String {
        format!("{}/{}", self.pool_name, self.name)
    }

    fn is_open(&self) -> bool {
        self.life.load(Ordering::SeqCst) == life::OPEN
    }

    /// For places without an error to return: a failed process check reads
    /// as "not usable".
    pub(crate) fn is_usable(&self) -> bool {
        self.is_open() && !self.stamp.is_foreign() && !self.gate.is_closed()
    }

    /// Whether this connection can serve a new user. An error when this
    /// process is not, or cannot be shown to be, the one that opened it.
    pub(crate) fn usable(&self) -> Result<bool> {
        self.stamp.check_cold()?;
        Ok(self.is_open() && !self.gate.is_closed())
    }

    /// Enter a container operation.
    pub(crate) fn enter(&self) -> Result<Pass<'_>> {
        self.stamp.check()?;
        self.gate.enter(false)
    }

    /// Record the first native transaction of this connection that was not
    /// closed, and keep this connection alive for good: one strong reference
    /// is never given back. Atomic operations only.
    pub(crate) fn keep_tx(this: &Arc<ContConn>, fault: u64) {
        if this
            .tx_fault
            .compare_exchange(0, fault, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            std::mem::forget(Arc::clone(this));
        }
    }

    /// The error that keeps this connection open for good, if any: a listed
    /// mount whose unmount failed or that was orphaned, or a transaction
    /// fault. The states and the fault are read as in
    /// [`ContConn::observe_again`], so the error returned was the first that
    /// applied at one instant. Takes the list lock to copy the list.
    pub(crate) fn standing_error(&self) -> Option<Error> {
        let mounts: Vec<Arc<Mount>> = self
            .mounts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let (states, _, fault) = self.observe_again(&mounts);
        match verdict(&self.target(), &states, 0, fault) {
            Some(Verdict::Standing(e)) => Some(e),
            _ => None,
        }
    }

    /// Close the handle once and give up the pool lease. Returns the first
    /// error.
    fn finish(&self) -> Result<()> {
        if self
            .life
            .compare_exchange(
                life::OPEN,
                life::CLOSING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            return Ok(());
        }
        // SAFETY: a handle from daos_cont_open, closed once (guarded by the
        // exchange above); every mount of it that is still listed is
        // unmounted, no transaction of it is open or was left open, and the
        // release of each of its object handles was attempted first.
        let rc = unsafe { self.backend.daos_cont_close(self.coh, std::ptr::null_mut()) };
        let closed = if rc != 0 {
            // The handle may still be open in libdaos.
            self.life.store(life::KEPT, Ordering::SeqCst);
            Err(Error::daos_at("daos_cont_close", rc, self.target()))
        } else {
            self.life.store(life::CLOSED, Ordering::SeqCst);
            Ok(())
        };
        let pool = self.pool.lock().unwrap_or_else(|e| e.into_inner()).take();
        let released = match pool {
            Some(p) => {
                if closed.is_ok() {
                    p.conts.fetch_sub(1, Ordering::SeqCst);
                }
                PoolConn::release(p)
            }
            None => Ok(()),
        };
        closed.and(released)
    }

    /// Explicit close for every view: refuse new operations, wait until the
    /// deadline for running ones, for open transactions and for mounts that
    /// other threads are unmounting; unmount the DFS mounts of this
    /// container, then close object handles, close the handle and give up
    /// the pool lease.
    ///
    /// A standing error (a mount that could not be unmounted or was
    /// orphaned, a transaction that was not closed) is returned at once. A
    /// `Busy` leaves the container, its objects and the mount in use open.
    pub(crate) fn close(&self, timeout: Duration) -> Result<()> {
        // Before any lock: this may be a fork child.
        self.stamp.check_cold()?;
        let deadline = Deadline::after(timeout);
        #[cfg(all(test, feature = "mock"))]
        crate::gate::testing::note(crate::gate::testing::Note::Made("cont", deadline.id));
        // A standing error this call has seen or made.
        let known: RefCell<Option<Error>> = RefCell::new(None);
        // An error of a teardown this call ran to an unmounted end.
        let cleanup: RefCell<Option<Error>> = RefCell::new(None);
        // The hold that refuses new calls of the mount in use.
        let hold: RefCell<Option<CloseHold>> = RefCell::new(None);
        let done = self.gate.close_by(
            deadline,
            || self.standing(deadline, &known),
            |_| self.check(deadline, &known, &cleanup, &hold),
            |last, last_try| self.at_deadline(deadline, last, last_try, &known),
            || self.close_handle(&cleanup),
        );
        // New calls of the mount in use are accepted again from here on.
        drop(hold);
        done?.unwrap_or(Ok(()))
    }

    /// The mounts with their states, the open transactions and the
    /// transaction fault, read so that the verdict drawn from them was true
    /// at one instant; `None` when that could not be had by the deadline
    /// (the list lock was held by others, or the list kept changing).
    pub(crate) fn observe(&self, deadline: Deadline) -> Option<Observed> {
        loop {
            let (gen, mounts) = loop {
                let list = match self.mounts.try_lock() {
                    Ok(list) => list,
                    Err(TryLockError::Poisoned(p)) => p.into_inner(),
                    Err(TryLockError::WouldBlock) => {
                        if deadline.expired() {
                            return None;
                        }
                        std::thread::sleep(Duration::from_micros(50));
                        continue;
                    }
                };
                break (self.mounts_gen.load(Ordering::SeqCst), list.clone());
            };
            #[cfg(all(test, feature = "mock"))]
            crate::gate::testing::run("AfterListClone", 0, &());
            let (states, transactions, tx_fault) = self.observe_again(&mounts);
            if self.mounts_gen.load(Ordering::SeqCst) == gen {
                return Some(Observed {
                    mounts,
                    states,
                    transactions,
                    tx_fault,
                });
            }
            if deadline.expired() {
                return None;
            }
        }
    }

    /// The states of `mounts`, the open transactions and the transaction
    /// fault, for mounts already in hand. The states are read before and
    /// after the two counters until both readings agree: every state then
    /// held while the counters were read. A mount changes state at most
    /// twice, so this ends. Takes no lock.
    pub(crate) fn observe_again(&self, mounts: &[Arc<Mount>]) -> (Vec<u64>, usize, u64) {
        loop {
            #[cfg(not(all(test, feature = "mock")))]
            let first: Vec<u64> = mounts.iter().map(|m| m.state()).collect();
            #[cfg(all(test, feature = "mock"))]
            let first: Vec<u64> = mounts
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let s = m.state();
                    crate::gate::testing::run("AfterStateRead", i, &());
                    s
                })
                .collect();
            #[cfg(all(test, feature = "mock"))]
            crate::gate::testing::run("BeforeTxRead", 0, &());
            // The count before the fault: every writer records its fault
            // before it lowers the count.
            let transactions = self.transactions.load(Ordering::SeqCst);
            #[cfg(all(test, feature = "mock"))]
            crate::gate::testing::run("AfterTxRead", 0, &());
            let tx_fault = self.tx_fault.load(Ordering::SeqCst);
            #[cfg(all(test, feature = "mock"))]
            crate::gate::testing::run("AfterTxRead", 0, &());
            let second: Vec<u64> = mounts.iter().map(|m| m.state()).collect();
            if first == second {
                return (second, transactions, tx_fault);
            }
        }
    }

    /// For a round that could not take the gate: a standing error, if one
    /// is seen now or was seen before by this call.
    fn standing(&self, deadline: Deadline, known: &RefCell<Option<Error>>) -> Option<Error> {
        match self.observe(deadline) {
            Some(o) => match verdict(&self.target(), &o.states, o.transactions, o.tx_fault) {
                Some(Verdict::Standing(e)) => {
                    *known.borrow_mut() = Some(e.clone());
                    Some(e)
                }
                _ => None,
            },
            None => known.borrow().clone(),
        }
    }

    /// The result at the deadline: what stands in the way now, or one more
    /// try when nothing does.
    pub(crate) fn at_deadline(
        &self,
        deadline: Deadline,
        last: Last,
        last_try: bool,
        known: &RefCell<Option<Error>>,
    ) -> Final {
        match self.observe(deadline) {
            Some(o) => match verdict(&self.target(), &o.states, o.transactions, o.tx_fault) {
                Some(Verdict::Standing(e)) | Some(Verdict::Busy(e)) => Final::Error(e),
                None if !last_try => Final::Retry,
                None => Final::Error(last.reason()),
            },
            None => Final::Error(match (known.borrow().clone(), last) {
                (Some(e), _) => e,
                (None, Last::Contended(busy)) => busy,
                (None, Last::Waiting(_)) => Error::busy(0, 0),
            }),
        }
    }

    /// The check of a close, with the container gate held exclusively: no
    /// call of the container runs and no mount can be added.
    fn check(
        &self,
        deadline: Deadline,
        known: &RefCell<Option<Error>>,
        cleanup: &RefCell<Option<Error>>,
        hold: &RefCell<Option<CloseHold>>,
    ) -> Check {
        let c = self.check_mounts(deadline, known, cleanup, hold);
        #[cfg(all(test, feature = "mock"))]
        crate::gate::testing::run("CheckReturned", 0, &c);
        c
    }

    fn check_mounts(
        &self,
        deadline: Deadline,
        known: &RefCell<Option<Error>>,
        cleanup: &RefCell<Option<Error>>,
        hold: &RefCell<Option<CloseHold>>,
    ) -> Check {
        let target = self.target();
        let Some(mut o) = self.observe(deadline) else {
            return Check::Wait(Error::busy(0, 0));
        };
        loop {
            let v = verdict(&target, &o.states, o.transactions, o.tx_fault);
            #[cfg(all(test, feature = "mock"))]
            crate::gate::testing::run("AfterVerdict", 0, &v);
            match v {
                Some(Verdict::Standing(e)) => {
                    *known.borrow_mut() = Some(e.clone());
                    return Check::Fail(e);
                }
                Some(Verdict::Busy(e)) => return Check::Wait(e),
                None => {}
            }
            // With no verdict, every mount is unmounted or mounted.
            let Some(i) = o.states.iter().position(|s| state::tag(*s) == MOUNTED) else {
                debug_assert!(o.states.iter().all(|s| state::tag(*s) == UNMOUNTED));
                return Check::Ready;
            };
            // Oldest first: only the newest mount can have a user.
            let m = &o.mounts[i];
            let newest = i + 1 == o.mounts.len();
            let u = {
                let mut h = hold.borrow_mut();
                Mount::close_now(m, if newest { Some(&mut *h) } else { None })
            };
            #[cfg(all(test, feature = "mock"))]
            crate::gate::testing::run("AfterMountClose", 0, &());
            match u {
                Unmount::Refused(e) => return Check::Fail(e),
                Unmount::Done(Err(e)) if state::tag(m.state()) == FAILED => {
                    // This call's own dfs_umount failed.
                    *known.borrow_mut() = Some(e.clone());
                    return Check::Fail(e);
                }
                Unmount::Done(Err(e)) => {
                    cleanup.borrow_mut().get_or_insert(e);
                }
                Unmount::Done(Ok(())) | Unmount::Elsewhere => {}
                Unmount::Busy(e) => {
                    if state::tag(m.state()) == MOUNTED {
                        return Check::Wait(e);
                    }
                    // Another close of the mount won: look at the states.
                }
            }
            let (states, transactions, tx_fault) = self.observe_again(&o.mounts);
            o.states = states;
            o.transactions = transactions;
            o.tx_fault = tx_fault;
        }
    }

    /// The teardown of a close: every listed mount is unmounted, no
    /// transaction is open or was left open. Close the object handles, then
    /// the native handle, and give up the pool lease.
    fn close_handle(&self, cleanup: &RefCell<Option<Error>>) -> Result<()> {
        let handles = self.handles.close_all(self.backend.as_ref()).err();
        let first = handles.or(cleanup.borrow_mut().take());
        match (self.finish(), first) {
            (Err(e), _) | (Ok(()), Some(e)) => Err(e),
            (Ok(()), None) => Ok(()),
        }
    }

    /// Give up one lease. When it was the last one, close the container and
    /// release its pool lease; the first error is reported.
    ///
    /// When this process cannot be confirmed as the one that opened the
    /// container, the handle is left open for good (its drop makes no DAOS
    /// call for it) and the error of the check is returned. The pool lease
    /// goes with the connection as usual.
    ///
    /// Of two releases of the last two leases at once, exactly one gets the
    /// connection and reports.
    pub(crate) fn release(this: Arc<ContConn>) -> Result<()> {
        let Some(c) = Arc::into_inner(this) else {
            return Ok(());
        };
        #[cfg(test)]
        crate::fork::testing::fault_point("cont_release");
        if let Err(e) = c.stamp.check_cold() {
            // No close: a destroy of this container stays busy in this
            // process.
            let _ =
                c.life
                    .compare_exchange(life::OPEN, life::KEPT, Ordering::SeqCst, Ordering::SeqCst);
            return Err(e);
        }
        c.finish()
    }
}

impl Drop for ContConn {
    fn drop(&mut self) {
        #[cfg(all(test, feature = "mock"))]
        CONT_DROPS.fetch_add(1, Ordering::SeqCst);
        // Before any lock: this may be a fork child.
        match self.stamp.check_cold() {
            Ok(()) => {}
            Err(Error::Forked { current_pid: 0, .. }) => {
                // The owner cannot be confirmed: never call libdaos, and
                // record that the handle stays open.
                let _ = self.life.compare_exchange(
                    life::OPEN,
                    life::KEPT,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
                return;
            }
            // Fork child: leak, never call libdaos.
            Err(_) => return,
        }
        // The last reference: no mount is listed (a listed mount holds a
        // lease), no transaction is open (it holds a reference) or was left
        // open (that keeps a reference), and object handles hold leases.
        let _ = self.finish();
    }
}

// ---------------------------------------------------------------------------
// Registry

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Key {
    Pool(String, Access),
    Cont(String, String, Access),
    Mount(String, String, Access),
}

/// Registry state. Entries are weak: the registry never keeps a connection
/// alive.
#[derive(Default)]
pub(crate) struct RegistryInner {
    /// System name fixed by the first successful connect.
    system: Option<String>,
    /// System name reserved by connects in progress, with their count.
    pending_system: Option<(String, usize)>,
    pending: HashSet<Key>,
    pools: HashMap<(String, Access), Weak<PoolConn>>,
    conts: HashMap<(String, String, Access), Weak<ContConn>>,
    pub(crate) mounts: HashMap<(String, String, Access), Weak<MountRef>>,
    /// `(pool, container, life)` of every container connection made through
    /// this registry whose native handle was not closed yet (see
    /// `ContConn::life`). Entries that read closed are dropped when a
    /// connection is stored.
    open_conts: Vec<(String, String, Arc<AtomicU8>)>,
}

impl RegistryInner {
    fn check_system(&self, sys_key: &str) -> Result<()> {
        let sys = self
            .system
            .as_deref()
            .or(self.pending_system.as_ref().map(|p| p.0.as_str()));
        match sys {
            Some(existing) if existing != sys_key => Err(Error::Conflict {
                what: "system",
                existing: existing.to_string(),
                requested: sys_key.to_string(),
            }),
            _ => Ok(()),
        }
    }

    fn reserve(&mut self, key: &Key, sys_key: &str) {
        self.pending.insert(key.clone());
        match &mut self.pending_system {
            Some((_, n)) => *n += 1,
            None => self.pending_system = Some((sys_key.to_string(), 1)),
        }
    }

    fn unreserve(&mut self, key: &Key) {
        self.pending.remove(key);
        if let Some((_, n)) = &mut self.pending_system {
            *n -= 1;
            if *n == 0 {
                self.pending_system = None;
            }
        }
    }

    /// True when a container or mount of `(pool, cont)` is live or being
    /// opened, in any access mode, or when a native handle of it is not
    /// closed: still open, being closed by a connection that the registry
    /// can no longer reach, or left open for good (see `open_conts`). An
    /// error when a live entry cannot be shown to belong to this process. A
    /// live mount holds its container connection, so the connections are
    /// enough to find it.
    ///
    /// Every connection upgraded here is moved into `upgraded`, for the
    /// caller to drop after it released the registry lock.
    pub(crate) fn container_in_use(
        &self,
        pool: &str,
        cont: &str,
        upgraded: &mut Upgraded,
    ) -> Result<bool> {
        let pending = self.pending.iter().any(|k| match k {
            Key::Cont(p, c, _) | Key::Mount(p, c, _) => p == pool && c == cont,
            Key::Pool(..) => false,
        });
        if pending {
            return Ok(true);
        }
        for ((p, c, _), w) in &self.conts {
            if p == pool && c == cont {
                if let Some(x) = w.upgrade() {
                    #[cfg(test)]
                    run_after_upgrade();
                    let usable = x.usable();
                    upgraded.conts.push(x);
                    if usable? {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(self
            .open_conts
            .iter()
            .any(|(p, c, l)| p == pool && c == cont && l.load(Ordering::SeqCst) != life::CLOSED))
    }
}

/// Connections upgraded under the registry lock, dropped by the owner after
/// the lock was released.
#[derive(Default)]
pub(crate) struct Upgraded {
    conts: Vec<Arc<ContConn>>,
}

#[cfg(test)]
thread_local! {
    /// Run once right after `container_in_use` upgraded a container.
    static AFTER_UPGRADE: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
}

#[cfg(test)]
fn run_after_upgrade() {
    if let Some(f) = AFTER_UPGRADE.with(|h| h.borrow_mut().take()) {
        f();
    }
}

/// Keys reserved by a container destroy; released on drop.
pub(crate) struct DestroyGuard<'a> {
    registry: &'a Registry,
    keys: Vec<Key>,
}

impl Drop for DestroyGuard<'_> {
    fn drop(&mut self) {
        let mut inner = self.registry.lock();
        for k in &self.keys {
            inner.pending.remove(k);
        }
        drop(inner);
        self.registry.changed.notify_all();
    }
}

/// A key reserved by a connect in progress; unreserved on drop unless taken.
struct Reservation<'a> {
    registry: &'a Registry,
    key: Option<Key>,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if let Some(k) = self.key.take() {
            let mut inner = self.registry.lock();
            inner.unreserve(&k);
            drop(inner);
            self.registry.changed.notify_all();
        }
    }
}

/// Connection registry of one backend (the loaded library, or one in-memory
/// backend).
#[derive(Default)]
pub(crate) struct Registry {
    inner: Mutex<RegistryInner>,
    changed: Condvar,
}

/// System key of an optional system name.
pub(crate) fn system_key(sys_name: Option<&str>) -> String {
    sys_name.unwrap_or(DEFAULT_SYSTEM_NAME).to_string()
}

impl Registry {
    pub(crate) fn lock(&self) -> MutexGuard<'_, RegistryInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Return the live entry for `key` (found with `lookup`) or create it with
    /// `create` without holding the lock, then record it with `store`.
    ///
    /// An error of `lookup` (a cached entry whose process check fails) is
    /// returned before anything is reserved: such an entry is neither used
    /// nor replaced by a second connection.
    pub(crate) fn acquire<T>(
        &self,
        key: Key,
        sys_key: &str,
        lookup: impl Fn(&RegistryInner) -> Option<Arc<T>>,
        usable: impl Fn(&T) -> Result<bool>,
        create: impl FnOnce() -> Result<Arc<T>>,
        store: impl FnOnce(&mut RegistryInner, &Arc<T>),
    ) -> Result<Arc<T>> {
        // Entries found but not used: dropped only after the lock is
        // released (dropping the last reference closes the connection).
        let mut unused: Vec<Arc<T>> = Vec::new();
        let mut inner = self.lock();
        loop {
            if let Err(e) = inner.check_system(sys_key) {
                drop(inner);
                drop(unused);
                return Err(e);
            }
            if let Some(found) = lookup(&inner) {
                match usable(&found) {
                    Ok(true) => {
                        drop(inner);
                        drop(unused);
                        return Ok(found);
                    }
                    Ok(false) => unused.push(found),
                    Err(e) => {
                        drop(inner);
                        unused.push(found);
                        drop(unused);
                        return Err(e);
                    }
                }
            }
            if !inner.pending.contains(&key) {
                break;
            }
            // Look again only once the reservation of this key is gone, so
            // an entry is set aside at most once per reservation, not once
            // per notification of the shared condition variable.
            inner = self
                .changed
                .wait_while(inner, |i| i.pending.contains(&key))
                .unwrap_or_else(|e| e.into_inner());
        }
        inner.reserve(&key, sys_key);
        drop(inner);
        drop(unused);
        // Unreserves the key if `create` panics, so later requests for it
        // do not wait forever.
        let mut reservation = Reservation {
            registry: self,
            key: Some(key),
        };
        let result = create();
        let key = reservation.key.take().expect("set above");
        let mut inner = self.lock();
        inner.unreserve(&key);
        if let Ok(v) = &result {
            inner.system.get_or_insert_with(|| sys_key.to_string());
            store(&mut inner, v);
        }
        drop(inner);
        self.changed.notify_all();
        result
    }

    /// Reserve the container keys of `(pool, cont)` in both access modes for
    /// a destroy: [`Error::Busy`] when the container is live or being opened
    /// under these strings; otherwise opens of these keys wait until the
    /// guard drops. The lock is not held during the destroy call.
    pub(crate) fn reserve_destroy(&self, pool: &str, cont: &str) -> Result<DestroyGuard<'_>> {
        let mut upgraded = Upgraded::default();
        let r = self.reserve_destroy_with(pool, cont, &mut upgraded);
        // The lock is released: a last reference may close its connection
        // now.
        drop(upgraded);
        r
    }

    fn reserve_destroy_with(
        &self,
        pool: &str,
        cont: &str,
        upgraded: &mut Upgraded,
    ) -> Result<DestroyGuard<'_>> {
        let mut inner = self.lock();
        if inner.container_in_use(pool, cont, upgraded)? {
            return Err(Error::busy_open(1));
        }
        let mut keys = Vec::new();
        for access in [Access::ReadOnly, Access::ReadWrite] {
            for k in [
                Key::Cont(pool.to_string(), cont.to_string(), access),
                Key::Mount(pool.to_string(), cont.to_string(), access),
            ] {
                inner.pending.insert(k.clone());
                keys.push(k);
            }
        }
        Ok(DestroyGuard {
            registry: self,
            keys,
        })
    }

    /// The shared pool connection for `(name, access)`.
    pub(crate) fn pool(
        &self,
        backend: &Arc<dyn Raw>,
        name: &str,
        sys_name: Option<&str>,
        access: Access,
    ) -> Result<Arc<PoolConn>> {
        if name.is_empty() {
            return Err(Error::InvalidArgument("pool is empty"));
        }
        let sys_key = system_key(sys_name);
        let map_key = (name.to_string(), access);
        self.acquire(
            Key::Pool(name.to_string(), access),
            &sys_key,
            |r| r.pools.get(&map_key).and_then(Weak::upgrade),
            PoolConn::usable,
            || {
                let stamp = Stamp::fresh()?;
                PoolConn::connect(backend.clone(), name, sys_name, access, stamp).map(Arc::new)
            },
            |r, p| {
                r.pools.retain(|_, w| w.strong_count() > 0);
                r.pools.insert(map_key.clone(), Arc::downgrade(p));
            },
        )
    }

    /// The shared container connection for `(pool, cont, access)`; connects
    /// the pool first when needed.
    pub(crate) fn container(
        &self,
        backend: &Arc<dyn Raw>,
        pool: &str,
        cont: &str,
        sys_name: Option<&str>,
        access: Access,
    ) -> Result<Arc<ContConn>> {
        let p = self.pool(backend, pool, sys_name, access)?;
        self.container_on(&p, cont)
    }

    /// The shared container connection for `cont` on an existing pool
    /// connection (whose name and access mode make the key).
    pub(crate) fn container_on(&self, pool: &Arc<PoolConn>, cont: &str) -> Result<Arc<ContConn>> {
        if cont.is_empty() {
            return Err(Error::InvalidArgument("container is empty"));
        }
        let sys_key = system_key(pool.sys_name.as_deref());
        let map_key = (pool.name.clone(), cont.to_string(), pool.access);
        self.acquire(
            Key::Cont(pool.name.clone(), cont.to_string(), pool.access),
            &sys_key,
            |r| r.conts.get(&map_key).and_then(Weak::upgrade),
            ContConn::usable,
            || {
                let stamp = Stamp::fresh()?;
                ContConn::open(pool.clone(), cont, stamp).map(Arc::new)
            },
            |r, c| {
                r.conts.retain(|_, w| w.strong_count() > 0);
                r.conts.insert(map_key.clone(), Arc::downgrade(c));
                r.open_conts
                    .retain(|e| e.2.load(Ordering::SeqCst) != life::CLOSED);
                r.open_conts
                    .push((c.pool_name.clone(), c.name.clone(), Arc::clone(&c.life)));
            },
        )
    }
}

/// Shared setup of the tests of container and mount closes.
#[cfg(all(test, feature = "mock"))]
pub(crate) mod support {
    use super::*;
    use crate::cont::{Container, ContainerOptions};
    use crate::gate::testing;
    use crate::mock::{MemDfs, PauseHandle};
    use crate::Dfs;
    use std::thread::JoinHandle;

    /// Bounds a failing run only.
    pub(crate) const LIMIT: Duration = Duration::from_secs(10);
    pub(crate) const ZERO: Duration = Duration::ZERO;

    pub(crate) struct Fixture {
        pub(crate) mem: Arc<MemDfs>,
        pub(crate) cont: Container,
    }

    impl Fixture {
        pub(crate) fn conn(&self) -> &Arc<ContConn> {
            &self.cont.conn
        }

        /// A new user of the shared mount, or a new mount.
        pub(crate) fn mount(&self) -> Dfs {
            self.cont.mount_dfs().unwrap()
        }

        pub(crate) fn calls(&self, op: &str) -> u64 {
            self.mem.calls(op)
        }

        /// Sleeps of closes on the container gate so far.
        pub(crate) fn waits(&self) -> usize {
            self.cont.conn.gate.waits.load(Ordering::SeqCst)
        }

        /// Drop `dfs`, its last user, in a thread, and wait until that
        /// thread is inside `dfs_umount`.
        pub(crate) fn park_drop(&self, dfs: Dfs) -> Parked {
            self.park_drop_with(dfs, self.mem.pause_next("dfs_umount"))
        }

        /// As [`Fixture::park_drop`], with that `dfs_umount` failing with
        /// `errno` once released.
        pub(crate) fn park_failing_drop(&self, dfs: Dfs, errno: i32) -> Parked {
            self.park_drop_with(dfs, self.mem.pause_failing("dfs_umount", errno))
        }

        fn park_drop_with(&self, dfs: Dfs, pause: PauseHandle) -> Parked {
            let t = std::thread::spawn(move || drop(dfs));
            assert!(pause.wait_entered(LIMIT), "the unmount never started");
            Parked {
                pause,
                thread: Some(t),
            }
        }

        /// Close `dfs` in a thread, and wait until that thread is inside
        /// `dfs_umount`.
        pub(crate) fn park_close(&self, dfs: Dfs) -> ParkedClose {
            let pause = self.mem.pause_next("dfs_umount");
            let t = std::thread::spawn(move || dfs.close(Duration::from_secs(30)));
            assert!(pause.wait_entered(LIMIT), "the unmount never started");
            ParkedClose {
                pause,
                thread: Some(t),
            }
        }

        /// Run `cont.query()` in a thread, and wait until it is inside
        /// `daos_cont_query` (it holds a pass of the container gate).
        pub(crate) fn park_query(&self) -> ParkedClose {
            let pause = self.mem.pause_next("daos_cont_query");
            let c = self.cont.clone();
            let t = std::thread::spawn(move || c.query().map(|_| ()));
            assert!(pause.wait_entered(LIMIT), "the query never started");
            ParkedClose {
                pause,
                thread: Some(t),
            }
        }
    }

    /// A thread stopped inside a mock call; released and joined on drop.
    pub(crate) struct Parked {
        pub(crate) pause: PauseHandle,
        thread: Option<JoinHandle<()>>,
    }

    impl Parked {
        /// Let the call continue and join the thread.
        pub(crate) fn finish(mut self) {
            self.pause.release();
            if let Some(t) = self.thread.take() {
                t.join().unwrap();
            }
        }
    }

    impl Drop for Parked {
        fn drop(&mut self) {
            self.pause.release();
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    /// As [`Parked`], for a thread with a result.
    pub(crate) struct ParkedClose {
        pub(crate) pause: PauseHandle,
        thread: Option<JoinHandle<Result<()>>>,
    }

    impl ParkedClose {
        /// Let the call continue, join the thread and return its result.
        pub(crate) fn finish(mut self) -> Result<()> {
            self.pause.release();
            self.thread.take().expect("joined once").join().unwrap()
        }
    }

    impl Drop for ParkedClose {
        fn drop(&mut self) {
            self.pause.release();
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    /// A container "mock" on pool "mock" of a new backend.
    pub(crate) fn fixture() -> Fixture {
        testing::clear();
        let mem = Arc::new(MemDfs::new());
        let cont =
            Container::open_mock(mem.clone(), &ContainerOptions::new("mock", "mock")).unwrap();
        Fixture { mem, cont }
    }

    /// Plain mode first, then [`fixture`]. Only in a process of its own.
    pub(crate) fn fixture_plain() -> Fixture {
        crate::fork::testing::force_plain();
        fixture()
    }

    /// The counts of a `Busy`.
    pub(crate) fn busy<T: std::fmt::Debug>(r: &Result<T>) -> (usize, usize, usize) {
        match r {
            Err(Error::Busy {
                in_flight,
                transactions,
                open_connections,
            }) => (*in_flight, *transactions, *open_connections),
            other => panic!("not Busy: {other:?}"),
        }
    }

    /// The errno of a failed `dfs_umount`.
    pub(crate) fn umount_errno<T: std::fmt::Debug>(r: &Result<T>) -> i32 {
        match r {
            Err(Error::Dfs {
                op: "dfs_umount",
                errno,
                ..
            }) => *errno,
            other => panic!("not a dfs_umount error: {other:?}"),
        }
    }

    /// The process ids of a `Forked`.
    pub(crate) fn forked<T: std::fmt::Debug>(r: &Result<T>) -> (u32, u32) {
        match r {
            Err(Error::Forked {
                created_pid,
                current_pid,
            }) => (*created_pid, *current_pid),
            other => panic!("not Forked: {other:?}"),
        }
    }

    /// The tags of the states of `mounts`.
    pub(crate) fn tags(states: &[u64]) -> Vec<u8> {
        states.iter().map(|s| state::tag(*s)).collect()
    }

    /// The mount behind `dfs`.
    pub(crate) fn core(dfs: &Dfs) -> Arc<Mount> {
        dfs.mount.core()
    }

    /// The listed mounts of `conn`.
    pub(crate) fn listed(conn: &ContConn) -> Vec<Arc<Mount>> {
        conn.mounts.lock().unwrap().clone()
    }

    /// A one-shot signal between threads.
    pub(crate) fn signal() -> (
        std::sync::mpsc::SyncSender<()>,
        std::sync::mpsc::Receiver<()>,
    ) {
        std::sync::mpsc::sync_channel(1)
    }

    /// Wait for a signal, bounded by [`LIMIT`].
    pub(crate) fn wait(rx: &std::sync::mpsc::Receiver<()>) {
        rx.recv_timeout(LIMIT).expect("no signal");
    }
}

#[cfg(all(test, feature = "mock"))]
mod tests {
    use super::*;
    use crate::fork::testing::{
        count_raw_pid_reads, force_plain, rerun_alone, with_pid_fault_at, with_raw_pid_once_after,
    };
    use crate::mock::MemDfs;
    use crate::pool::PoolOptions;
    use crate::{Dfs, Pool};

    const RW: Access = Access::ReadWrite;
    const SECS: Duration = Duration::from_secs(5);

    /// Plain mode first, then the backend, then its init.
    fn plain_mock() -> (Arc<MemDfs>, Arc<dyn Raw>) {
        force_plain();
        let mem = Arc::new(MemDfs::new());
        mem.ensure_init().unwrap();
        let backend: Arc<dyn Raw> = mem.clone();
        (mem, backend)
    }

    fn is_forked<T>(r: &Result<T>) -> bool {
        matches!(r, Err(Error::Forked { .. }))
    }

    /// A lookup that filters with a boolean takes the failed check for a
    /// closed entry, drops it and connects again (a second
    /// `daos_pool_connect2` and a second handle), wherever the stamp for the
    /// new connection is taken.
    #[test]
    fn failed_pid_read_keeps_the_cached_pool() {
        if rerun_alone("conn::tests::failed_pid_read_keeps_the_cached_pool") {
            return;
        }
        let (mem, backend) = plain_mock();
        let registry = backend.registry();
        let p1 = registry.pool(&backend, "p", None, RW).unwrap();
        assert_eq!(mem.calls("daos_pool_connect2"), 1);
        assert_eq!(mem.open_pool_handles(), 1);
        // A healthy cached hit of the same call; the entry's check is its
        // last pid read.
        let call = || registry.pool(&backend, "p", None, RW);
        let mut hit = None;
        let n = count_raw_pid_reads(|| hit = Some(call()));
        assert!(n >= 1);
        assert!(Arc::ptr_eq(&hit.unwrap().unwrap(), &p1));
        let r = with_raw_pid_once_after(n - 1, -1, call);
        assert!(is_forked(&r), "{:?}", r.err());
        assert_eq!(mem.calls("daos_pool_connect2"), 1);
        assert_eq!(mem.open_pool_handles(), 1);
        assert!(Arc::ptr_eq(&call().unwrap(), &p1));
        assert_eq!(mem.calls("daos_pool_connect2"), 1);
        assert_eq!(mem.open_pool_handles(), 1);
    }

    /// A lookup that filters with a boolean opens the container a second
    /// time.
    #[test]
    fn failed_pid_read_keeps_the_cached_container() {
        if rerun_alone("conn::tests::failed_pid_read_keeps_the_cached_container") {
            return;
        }
        let (mem, backend) = plain_mock();
        let registry = backend.registry();
        let p = registry.pool(&backend, "p", None, RW).unwrap();
        let c1 = registry.container_on(&p, "c").unwrap();
        assert_eq!(mem.calls("daos_cont_open2"), 1);
        let call = || registry.container_on(&p, "c");
        let mut hit = None;
        let n = count_raw_pid_reads(|| hit = Some(call()));
        assert!(n >= 1);
        assert!(Arc::ptr_eq(&hit.unwrap().unwrap(), &c1));
        let r = with_raw_pid_once_after(n - 1, -1, call);
        assert!(is_forked(&r), "{:?}", r.err());
        assert_eq!(mem.calls("daos_cont_open2"), 1);
        assert!(Arc::ptr_eq(&call().unwrap(), &c1));
        assert_eq!(mem.calls("daos_cont_open2"), 1);
    }

    /// Positive control: an entry that is really closed is replaced.
    #[test]
    fn closed_entry_is_still_replaced() {
        if rerun_alone("conn::tests::closed_entry_is_still_replaced") {
            return;
        }
        let (mem, backend) = plain_mock();
        let registry = backend.registry();
        let p1 = registry.pool(&backend, "p", None, RW).unwrap();
        p1.close(SECS).unwrap();
        let p2 = registry.pool(&backend, "p", None, RW).unwrap();
        assert_eq!(mem.calls("daos_pool_connect2"), 2);
        assert!(!Arc::ptr_eq(&p1, &p2));

        let c = registry.container_on(&p2, "c").unwrap();
        let options = crate::dfs::MountOptions::default();
        let m1 = crate::dfs::mount_on(&c, None, &options).unwrap();
        m1.close(SECS).unwrap();
        let m2 = crate::dfs::mount_on(&c, None, &options).unwrap();
        assert_eq!(mem.calls("dfs_mount"), 2);
        assert!(!Arc::ptr_eq(&m1, &m2));
    }

    /// With a boolean check the container looks unused, the reservation
    /// succeeds and the forced destroy reaches DAOS (`daos_cont_destroy2`
    /// becomes 1).
    #[test]
    fn failed_pid_read_does_not_free_a_container_for_destroy() {
        if rerun_alone("conn::tests::failed_pid_read_does_not_free_a_container_for_destroy") {
            return;
        }
        let (mem, backend) = plain_mock();
        let registry = backend.registry();
        let pool = Pool::connect_mock(mem.clone(), &PoolOptions::new("p")).unwrap();
        let _cont = pool.open_container("c").unwrap();
        let mut first = None;
        let n = count_raw_pid_reads(|| first = Some(pool.destroy_container("c", true)));
        assert!(matches!(first, Some(Err(Error::Busy { .. }))));
        let k = count_raw_pid_reads(|| {
            assert!(matches!(
                registry.reserve_destroy("p", "c"),
                Err(Error::Busy { .. })
            ));
        });
        assert!(k >= 1 && n >= k);
        // The first read inside the reservation fails, all others are
        // healthy.
        let r = with_raw_pid_once_after(n - k, -1, || pool.destroy_container("c", true));
        assert!(is_forked(&r), "{:?}", r.err());
        assert_eq!(mem.calls("daos_cont_destroy2"), 0);
        let r = with_raw_pid_once_after(0, -1, || registry.reserve_destroy("p", "c").map(|_| ()));
        assert!(is_forked(&r), "{:?}", r.err());
        assert!(matches!(
            pool.destroy_container("c", true),
            Err(Error::Busy { .. })
        ));
        assert_eq!(mem.calls("daos_cont_destroy2"), 0);
    }

    /// The process check inside `ContConn::release` fails after the entry
    /// check of `close` passed. An implementation that answers `Ok` here
    /// fails.
    #[test]
    fn container_release_reports_a_failed_process_check() {
        if rerun_alone("conn::tests::container_release_reports_a_failed_process_check") {
            return;
        }
        let (mem, _) = plain_mock();
        // A separate view of the pool of the mount: the pool reference of the
        // container is not the last one. Were it the last, dropping it later,
        // once the pid reads again, would attempt a disconnect, which the
        // mock refuses with -DER_BUSY while the skipped container handle is
        // open; marking the container done does not prevent that attempt.
        let pool = Pool::connect_mock(mem.clone(), &PoolOptions::new("mock")).unwrap();
        let dfs = Dfs::connect_mock(mem.clone()).unwrap();
        let umount = mem.calls("dfs_umount");
        let cont_close = mem.calls("daos_cont_close");
        let disconnect = mem.calls("daos_pool_disconnect");
        let r = with_pid_fault_at("cont_release", || dfs.close(SECS));
        assert!(is_forked(&r), "{:?}", r.err());
        assert_eq!(mem.calls("dfs_umount"), umount + 1);
        assert_eq!(mem.calls("daos_cont_close"), cont_close);
        assert_eq!(mem.calls("daos_pool_disconnect"), disconnect);
        drop(dfs);
        assert_eq!(mem.calls("daos_pool_disconnect"), disconnect);
        // The native container handle is still open: a forced destroy is
        // refused although no connection of the container is left.
        assert!(matches!(
            pool.destroy_container("mock", true),
            Err(Error::Busy { .. })
        ));
        assert_eq!(mem.calls("daos_cont_destroy2"), 0);
        drop(pool);
        // The container was marked done: no close for it, also now.
        assert_eq!(mem.calls("daos_cont_close"), cont_close);
    }

    /// The last reference of a container drops while a destroy scans the
    /// registry: the scan's own reference is the last one. Its close runs
    /// only after the registry lock was released, so while that close is
    /// held in the mock a registry call for another key completes. A close
    /// that fails keeps the container busy for later destroys.
    #[test]
    fn last_reference_dropped_during_the_destroy_scan() {
        if rerun_alone("conn::tests::last_reference_dropped_during_the_destroy_scan") {
            return;
        }
        let (mem, backend) = plain_mock();
        let registry = backend.registry();
        let p = registry.pool(&backend, "p", None, RW).unwrap();
        let c = registry.container_on(&p, "c").unwrap();
        mem.fail_next("daos_cont_close", -1012);
        let pause = mem.pause_next("daos_cont_close");
        let scan = {
            let backend = backend.clone();
            std::thread::spawn(move || {
                // After the scan upgraded the container, the only other
                // reference goes.
                AFTER_UPGRADE.with(|h| *h.borrow_mut() = Some(Box::new(move || drop(c))));
                let r = backend.registry().reserve_destroy("p", "c").map(|_| ());
                let hook_ran = AFTER_UPGRADE.with(|h| h.borrow().is_none());
                (r, hook_ran)
            })
        };
        assert!(pause.wait_entered(SECS), "the last close never started");
        // The scan thread is inside daos_cont_close; the registry is free.
        let other = registry.pool(&backend, "q", None, RW).unwrap();
        drop(other);
        pause.release();
        let (r, hook_ran) = scan.join().unwrap();
        assert!(hook_ran, "hook not run");
        assert!(matches!(r, Err(Error::Busy { .. })), "{r:?}");
        assert_eq!(mem.calls("daos_cont_close"), 1);
        // No connection is left; the failed close keeps the destroy busy.
        assert!(registry
            .lock()
            .conts
            .values()
            .all(|w| w.upgrade().is_none()));
        assert!(matches!(
            registry.reserve_destroy("p", "c"),
            Err(Error::Busy { .. })
        ));
        let pool = Pool::connect_mock(mem.clone(), &PoolOptions::new("p")).unwrap();
        assert!(matches!(
            pool.destroy_container("c", true),
            Err(Error::Busy { .. })
        ));
        assert_eq!(mem.calls("daos_cont_destroy2"), 0);
    }

    /// The last reference of a container is dropped and its
    /// `daos_cont_close` is still running: a forced destroy is busy and makes
    /// no DAOS call. Once the close succeeded the container can be destroyed.
    #[test]
    fn destroy_during_the_last_close_is_busy() {
        if rerun_alone("conn::tests::destroy_during_the_last_close_is_busy") {
            return;
        }
        let mem = Arc::new(MemDfs::new());
        let pool = Pool::connect_mock(mem.clone(), &PoolOptions::new("p")).unwrap();
        let cont = pool.open_container("c").unwrap();
        let pause = mem.pause_next("daos_cont_close");
        let t = std::thread::spawn(move || drop(cont));
        assert!(pause.wait_entered(SECS));
        let registry = mem.registry();
        assert!(registry
            .lock()
            .conts
            .values()
            .all(|w| w.upgrade().is_none()));
        assert!(matches!(
            pool.destroy_container("c", true),
            Err(Error::Busy { .. })
        ));
        assert_eq!(mem.calls("daos_cont_destroy2"), 0);
        pause.release();
        t.join().unwrap();
        assert_eq!(mem.calls("daos_cont_close"), 1);
        // The record went with the successful close.
        pool.destroy_container("c", true).unwrap();
        assert_eq!(mem.calls("daos_cont_destroy2"), 1);
        // Later connections under one name leave nothing behind once their
        // closes succeeded: storing a connection drops the entries of
        // handles that were closed.
        let first = pool.open_container("e").unwrap();
        first.close(SECS).unwrap();
        let again = pool.open_container("e").unwrap();
        {
            let inner = registry.lock();
            assert_eq!(inner.open_conts.len(), 1, "closed entries were kept");
            assert_ne!(inner.open_conts[0].2.load(Ordering::SeqCst), life::CLOSED);
        }
        again.close(SECS).unwrap();
        assert!(registry
            .lock()
            .open_conts
            .iter()
            .all(|(_, _, l)| l.load(Ordering::SeqCst) == life::CLOSED));
        pool.destroy_container("e", true).unwrap();
        assert_eq!(mem.calls("daos_cont_destroy2"), 2);
    }

    /// The last two leases of a pool are released by two threads at once:
    /// exactly one of them gets the connection and reports the failed
    /// disconnect. With "try to take it, else drop the lease" both can miss
    /// it, and the last drop disconnects without a report.
    #[test]
    fn concurrent_last_releases_report_once() {
        use std::sync::Barrier;
        const ROUNDS: usize = 5_000;
        let mem = Arc::new(MemDfs::new());
        mem.ensure_init().unwrap();
        let backend: Arc<dyn Raw> = mem.clone();
        let registry = backend.registry();
        let barrier = Arc::new(Barrier::new(2));
        for round in 0..ROUNDS {
            let p = registry.pool(&backend, "p", None, RW).unwrap();
            mem.fail_next("daos_pool_disconnect", -1003);
            let other = Arc::clone(&p);
            let b = Arc::clone(&barrier);
            let t = std::thread::spawn(move || {
                b.wait();
                PoolConn::release(other)
            });
            barrier.wait();
            let mine = PoolConn::release(p);
            let theirs = t.join().unwrap();
            let errors = usize::from(mine.is_err()) + usize::from(theirs.is_err());
            assert_eq!(errors, 1, "round {round}: {mine:?} {theirs:?}");
        }
    }

    /// The process check inside `PoolConn::release` fails after the entry
    /// check of `close` passed.
    #[test]
    fn pool_release_reports_a_failed_process_check() {
        if rerun_alone("conn::tests::pool_release_reports_a_failed_process_check") {
            return;
        }
        let (mem, _) = plain_mock();
        let pool = Pool::connect_mock(mem.clone(), &PoolOptions::new("p")).unwrap();
        let cont = pool.open_container("c").unwrap();
        // The container's connection now holds the last pool reference.
        drop(pool);
        let cont_close = mem.calls("daos_cont_close");
        let disconnect = mem.calls("daos_pool_disconnect");
        let r = with_pid_fault_at("pool_release", || cont.close(SECS));
        assert!(is_forked(&r), "{:?}", r.err());
        assert_eq!(mem.calls("daos_cont_close"), cont_close + 1);
        assert_eq!(mem.calls("daos_pool_disconnect"), disconnect);
        drop(cont);
        assert_eq!(mem.calls("daos_pool_disconnect"), disconnect);
    }
}

/// Container close against mounts being unmounted, transactions and calls.
#[cfg(all(test, feature = "mock"))]
mod close_tests {
    use super::support::*;
    use super::*;
    use crate::gate::testing;
    use crate::tx::TxMode;
    use std::cell::RefCell as Cell2;
    use std::rc::Rc;

    const T30: Duration = Duration::from_secs(30);
    const T1: Duration = Duration::from_secs(1);

    fn closed_count(f: &Fixture) -> u64 {
        f.calls("daos_cont_close")
    }

    /// Record every `Check` the close returns.
    fn record_checks() -> Rc<Cell2<Vec<&'static str>>> {
        let seen = Rc::new(Cell2::new(Vec::new()));
        let s = seen.clone();
        testing::record("CheckReturned", None, move |c| {
            let c = c.downcast_ref::<Check>().unwrap();
            s.borrow_mut().push(match c {
                Check::Ready => "ready",
                Check::Wait(_) => "wait",
                Check::Fail(_) => "fail",
            });
        });
        seen
    }

    #[test]
    fn close_waits_for_a_mount_being_dropped() {
        let f = fixture();
        let parked = f.park_drop(f.mount());
        let r = f.cont.close(ZERO);
        assert_eq!(busy(&r), (0, 0, 1));
        assert!(!f.cont.is_closed());
        assert_eq!(closed_count(&f), 0);
        parked.finish();
        f.cont.close(T1).unwrap();
        assert_eq!(closed_count(&f), 1);

        // The unmount ends while the close waits for it.
        let f = fixture();
        let parked = RefCell::new(Some(f.park_drop(f.mount())));
        testing::set("AfterVerdict", None, move |_| {
            parked.borrow_mut().take().unwrap().finish();
        });
        f.cont.close(T30).unwrap();
        assert_eq!(closed_count(&f), 1);
    }

    #[test]
    fn busy_close_leaves_the_new_mount_usable() {
        let f = fixture();
        let parked = f.park_drop(f.mount());
        let b = f.mount();
        assert_eq!(f.calls("dfs_mount"), 2);
        let r = f.cont.close(ZERO);
        assert_eq!(busy(&r), (0, 0, 1));
        b.write("/x", b"1").unwrap();
        assert_eq!(b.read("/x").unwrap(), b"1");
        assert_eq!(f.calls("dfs_umount"), 1);
        assert_eq!(closed_count(&f), 0);
        parked.finish();
    }

    #[test]
    fn mount_states_are_taken_at_one_instant() {
        let f = fixture();
        let parked_a = f.park_drop(f.mount());
        let b = f.mount();
        let mounts = listed(f.conn());
        assert_eq!(mounts.len(), 2);
        let closing = Rc::new(Cell2::new(None));
        {
            let closing = closing.clone();
            let mem = f.mem.clone();
            let b = b.clone();
            let parked_a = RefCell::new(Some(parked_a));
            testing::set("AfterStateRead", Some(0), move |_| {
                parked_a.borrow_mut().take().unwrap().finish();
                let pause = mem.pause_next("dfs_umount");
                let b = b.clone();
                let t = std::thread::spawn(move || b.close(T30));
                assert!(pause.wait_entered(LIMIT));
                *closing.borrow_mut() = Some((pause, t));
            });
        }
        let (states, _, _) = f.conn().observe_again(&mounts);
        assert_eq!(tags(&states), [state::UNMOUNTED, state::UNMOUNTING]);
        let (pause, t) = closing.borrow_mut().take().unwrap();
        pause.release();
        t.join().unwrap().unwrap();
    }

    #[test]
    fn two_unmounts_at_once_are_two() {
        let f = fixture();
        let parked_a = f.park_drop(f.mount());
        let b = f.mount();
        let parked_b = f.park_close(b);
        let r = f.cont.close(ZERO);
        assert_eq!(busy(&r), (0, 0, 2));
        assert_eq!(closed_count(&f), 0);
        parked_a.finish();
        parked_b.finish().unwrap();
    }

    #[test]
    fn list_members_are_checked_with_the_states() {
        // A mount added after the list was read.
        let f = fixture();
        let parked_a = f.park_drop(f.mount());
        let made = Rc::new(Cell2::new(None));
        {
            let made = made.clone();
            let cont = f.cont.clone();
            testing::set("AfterListClone", None, move |_| {
                *made.borrow_mut() = Some(cont.mount_dfs().unwrap());
            });
        }
        let o = f.conn().observe(Deadline::after(LIMIT)).unwrap();
        assert_eq!(o.mounts.len(), 2);
        assert!(made.borrow().is_some());
        parked_a.finish();

        // A mount added and one removed after the list was read.
        let f = fixture();
        let parked_a = RefCell::new(Some(f.park_drop(f.mount())));
        let closing = Rc::new(Cell2::new(None));
        {
            let closing = closing.clone();
            let cont = f.cont.clone();
            let mem = f.mem.clone();
            testing::set("AfterListClone", None, move |_| {
                let b = cont.mount_dfs().unwrap();
                let pause = mem.pause_next("dfs_umount");
                let t = std::thread::spawn(move || b.close(T30));
                assert!(pause.wait_entered(LIMIT));
                parked_a.borrow_mut().take().unwrap().finish();
                *closing.borrow_mut() = Some((pause, t));
            });
        }
        let o = f.conn().observe(Deadline::after(LIMIT)).unwrap();
        assert_eq!(tags(&o.states), [state::UNMOUNTING]);
        let (pause, t) = closing.borrow_mut().take().unwrap();
        pause.release();
        t.join().unwrap().unwrap();
    }

    #[test]
    fn mount_closed_after_the_verdict_alone_is_no_reason_for_busy() {
        let f = fixture();
        let b = RefCell::new(Some(f.mount()));
        testing::set("AfterVerdict", None, move |_| {
            let b = b.borrow_mut().take().unwrap();
            std::thread::spawn(move || b.close(T1))
                .join()
                .unwrap()
                .unwrap();
        });
        let seen = record_checks();
        f.cont.close(ZERO).unwrap();
        assert_eq!(closed_count(&f), 1);
        assert_eq!(*seen.borrow(), ["ready"]);
    }

    #[test]
    fn mount_without_a_user_is_unmounted_once() {
        // A's last user is dropped; its drop has not taken the exchange.
        fn park_view_drop(f: &Fixture) -> (std::sync::mpsc::SyncSender<()>, JoinHandle<()>) {
            let a = f.mount();
            let (at_tx, at_rx) = signal();
            let (go_tx, go_rx) = signal();
            let t = std::thread::spawn(move || {
                testing::set("ViewDropBeforeUnmount", None, move |_| {
                    at_tx.send(()).unwrap();
                    wait(&go_rx);
                });
                drop(a);
            });
            wait(&at_rx);
            (go_tx, t)
        }
        use std::thread::JoinHandle;

        let f = fixture();
        let (go, t) = park_view_drop(&f);
        let _b = f.mount();
        f.cont.close(T1).unwrap();
        assert_eq!(f.calls("dfs_umount"), 2);
        go.send(()).unwrap();
        t.join().unwrap();
        assert_eq!(f.calls("dfs_umount"), 2);
        assert_eq!(closed_count(&f), 1);

        // The mount in use has a call running: only the mount without a
        // user is unmounted.
        let f = fixture();
        let (go, t) = park_view_drop(&f);
        let b = f.mount();
        let file = Arc::new(b.create("/f").unwrap());
        let pause = f.mem.pause_next("dfs_get_size");
        let size = {
            let file = file.clone();
            std::thread::spawn(move || file.size())
        };
        assert!(pause.wait_entered(LIMIT));
        let r = f.cont.close(ZERO);
        assert_eq!(busy(&r), (1, 0, 0));
        assert_eq!(f.calls("dfs_umount"), 1);
        pause.release();
        size.join().unwrap().unwrap();
        b.write("/g", b"2").unwrap();
        assert_eq!(b.read("/g").unwrap(), b"2");
        go.send(()).unwrap();
        t.join().unwrap();
        assert_eq!(f.calls("dfs_umount"), 1);
    }

    #[test]
    fn mount_is_unmounting_before_its_gate_reads_closed() {
        let f = fixture();
        let b = f.mount();
        let m = core(&b);
        let seen = Rc::new(Cell2::new(None));
        {
            let seen = seen.clone();
            let cont = f.cont.clone();
            testing::set("MountGateClosed", None, move |_| {
                let cont = cont.clone();
                let m = m.clone();
                let r = std::thread::spawn(move || (state::tag(m.state()), cont.close(ZERO)))
                    .join()
                    .unwrap();
                *seen.borrow_mut() = Some(r);
            });
        }
        b.close(T1).unwrap();
        let (tag, r) = seen.borrow_mut().take().unwrap();
        assert_eq!(tag, state::UNMOUNTING);
        assert_eq!(busy(&r), (0, 0, 1));
    }

    #[test]
    fn unmount_running_in_another_close_is_counted() {
        let f = fixture();
        let parked = f.park_close(f.mount());
        let r = f.cont.close(ZERO);
        assert_eq!(busy(&r), (0, 0, 1));
        assert_eq!(closed_count(&f), 0);
        parked.finish().unwrap();
        f.cont.close(T1).unwrap();
        assert_eq!(closed_count(&f), 1);
    }

    #[test]
    fn close_overtaken_by_another_close_is_not_done() {
        let f = fixture();
        let parked = Rc::new(Cell2::new(None));
        {
            let parked = parked.clone();
            let b = RefCell::new(Some(f.mount()));
            let mem = f.mem.clone();
            testing::set("AfterVerdict", None, move |_| {
                let b = b.borrow_mut().take().unwrap();
                let pause = mem.pause_next("dfs_umount");
                let t = std::thread::spawn(move || b.close(T30));
                assert!(pause.wait_entered(LIMIT));
                *parked.borrow_mut() = Some((pause, t));
            });
        }
        let r = f.cont.close(ZERO);
        assert_eq!(busy(&r), (0, 0, 1));
        assert_eq!(closed_count(&f), 0);
        let (pause, t) = parked.borrow_mut().take().unwrap();
        pause.release();
        t.join().unwrap().unwrap();
    }

    #[test]
    fn mount_finished_elsewhere_after_a_busy_is_done() {
        let f = fixture();
        let b = f.mount();
        let file = b.create("/f").unwrap();
        let pause = f.mem.pause_next("dfs_get_size");
        let size = std::thread::spawn(move || file.size());
        assert!(pause.wait_entered(LIMIT));
        {
            let size = RefCell::new(Some(size));
            let b = RefCell::new(Some(b));
            testing::set("AfterMountClose", None, move |_| {
                pause.release();
                size.borrow_mut().take().unwrap().join().unwrap().unwrap();
                let b = b.borrow_mut().take().unwrap();
                std::thread::spawn(move || b.close(T1))
                    .join()
                    .unwrap()
                    .unwrap();
            });
        }
        let seen = record_checks();
        f.cont.close(ZERO).unwrap();
        assert_eq!(closed_count(&f), 1);
        assert_eq!(seen.borrow().first(), Some(&"ready"));
    }

    #[test]
    fn refusal_of_the_mount_check_is_returned() {
        if crate::fork::testing::rerun_alone(
            "conn::close_tests::refusal_of_the_mount_check_is_returned",
        ) {
            return;
        }
        // The mount is closed by another thread to its end meanwhile.
        let f = fixture_plain();
        let b = RefCell::new(Some(f.mount()));
        testing::set("AfterMountClose", None, move |_| {
            let b = b.borrow_mut().take().unwrap();
            std::thread::spawn(move || b.close(T1))
                .join()
                .unwrap()
                .unwrap();
        });
        let r = crate::fork::testing::with_pid_fault_at("mount_close", || f.cont.close(T1));
        assert_eq!(forked(&r).1, 0);
        assert_eq!(closed_count(&f), 0);
        assert!(!f.cont.is_closed());

        // The mount is being unmounted by another thread meanwhile.
        let f = fixture();
        let parked = Rc::new(Cell2::new(None));
        {
            let parked = parked.clone();
            let b = RefCell::new(Some(f.mount()));
            let mem = f.mem.clone();
            testing::set("AfterMountClose", None, move |_| {
                let b = b.borrow_mut().take().unwrap();
                let pause = mem.pause_next("dfs_umount");
                let t = std::thread::spawn(move || b.close(T30));
                assert!(pause.wait_entered(LIMIT));
                *parked.borrow_mut() = Some((pause, t));
            });
        }
        let r = crate::fork::testing::with_pid_fault_at("mount_close", || f.cont.close(T1));
        assert_eq!(forked(&r).1, 0);
        assert_eq!(closed_count(&f), 0);
        assert!(!f.cont.is_closed());
        let (pause, t) = parked.borrow_mut().take().unwrap();
        pause.release();
        t.join().unwrap().unwrap();
    }

    #[test]
    fn failed_unmount_of_a_dropped_mount_is_returned_at_once() {
        let f = fixture();
        f.mem.fail_next("dfs_umount", libc::EIO);
        drop(f.mount());
        for _ in 0..2 {
            let r = f.cont.close(T30);
            assert_eq!(umount_errno(&r), libc::EIO);
        }
        assert_eq!(f.waits(), 0);
        assert_eq!(closed_count(&f), 0);
        let Fixture { mem, cont } = f;
        drop(cont);
        assert_eq!(mem.calls("daos_cont_close"), 0);
        assert_eq!(mem.open_container_handles(), 1);
        let pool =
            crate::Pool::connect_mock(mem.clone(), &crate::pool::PoolOptions::new("mock")).unwrap();
        assert!(matches!(
            pool.destroy_container("mock", true),
            Err(Error::Busy { .. })
        ));
        assert_eq!(mem.calls("daos_cont_destroy2"), 0);
        assert_eq!(busy(&pool.close(ZERO)), (0, 0, 1));
        assert_eq!(mem.calls("daos_pool_disconnect"), 0);
    }

    #[test]
    fn open_transaction_does_not_hide_a_failed_unmount() {
        let f = fixture();
        let tx = f.cont.tx(TxMode::ReadWrite).unwrap();
        f.mem.fail_next("dfs_umount", libc::EIO);
        drop(f.mount());
        let r = f.cont.close(T30);
        assert_eq!(umount_errno(&r), libc::EIO);
        assert_eq!(f.waits(), 0);
        tx.close().unwrap();
    }

    #[test]
    fn drop_without_a_pid_is_returned_by_the_close() {
        if crate::fork::testing::rerun_alone(
            "conn::close_tests::drop_without_a_pid_is_returned_by_the_close",
        ) {
            return;
        }
        let f = fixture_plain();
        let own = f.conn().stamp.pid();
        let dfs = f.mount();
        crate::fork::testing::with_raw_pid(-1, || drop(dfs));
        assert_eq!(f.calls("dfs_umount"), 0);
        let r = f.cont.close(T30);
        assert_eq!(forked(&r), (own, 0));
        assert_eq!(f.waits(), 0);
        let mounts = f.calls("dfs_mount");
        assert_eq!(forked(&f.cont.mount_dfs()), (own, 0));
        assert_eq!(f.calls("dfs_mount"), mounts);
        let Fixture { mem, cont } = f;
        drop(cont);
        assert_eq!(mem.calls("daos_cont_close"), 0);
        let pool =
            crate::Pool::connect_mock(mem.clone(), &crate::pool::PoolOptions::new("mock")).unwrap();
        assert!(matches!(
            pool.destroy_container("mock", true),
            Err(Error::Busy { .. })
        ));
        assert_eq!(busy(&pool.close(ZERO)), (0, 0, 1));
    }

    #[test]
    fn failed_unmount_survives_a_drop_without_a_pid() {
        if crate::fork::testing::rerun_alone(
            "conn::close_tests::failed_unmount_survives_a_drop_without_a_pid",
        ) {
            return;
        }
        let f = fixture_plain();
        let dfs = f.mount();
        f.mem.fail_next("dfs_umount", libc::EIO);
        assert_eq!(umount_errno(&dfs.close(T1)), libc::EIO);
        crate::fork::testing::with_raw_pid(-1, || drop(dfs));
        assert_eq!(umount_errno(&f.cont.close(T30)), libc::EIO);
    }

    #[test]
    fn standing_errors_are_ordered() {
        use state::{pack, FAILED, ORPHANED, TX_FAILED, TX_ORPHANED, UNMOUNTING};
        let fault = pack(TX_FAILED, 1011);
        let standing = |states: &[u64], n: usize, fault: u64| match verdict("p/c", states, n, fault)
        {
            Some(Verdict::Standing(e)) => e,
            other => panic!("not standing: {other:?}"),
        };
        let errno = |e: Error| umount_errno::<()>(&Err(e));
        let pid = |e: Error| forked::<()>(&Err(e));
        let tx = |e: Error| match e {
            Error::Daos {
                op: "daos_tx_close",
                code,
                ..
            } => code.value(),
            other => panic!("not a tx fault: {other:?}"),
        };
        let eio = pack(FAILED, libc::EIO as u32);
        let enospc = pack(FAILED, libc::ENOSPC as u32);
        assert_eq!(errno(standing(&[pack(ORPHANED, 7), eio], 0, 0)), libc::EIO);
        assert_eq!(errno(standing(&[enospc, eio], 0, 0)), libc::ENOSPC);
        assert_eq!(
            pid(standing(&[pack(ORPHANED, 7), pack(ORPHANED, 9)], 0, 0)),
            (7, 0)
        );
        assert_eq!(pid(standing(&[pack(ORPHANED, 7)], 0, fault)), (7, 0));
        assert_eq!(tx(standing(&[], 1, fault)), 1011);
        assert_eq!(tx(standing(&[], 0, fault)), 1011);
        assert_eq!(pid(standing(&[], 0, pack(TX_ORPHANED, 5))), (5, 0));
        assert_eq!(errno(standing(&[eio], 0, fault)), libc::EIO);
        let reason = |states: &[u64], n: usize| match verdict("p/c", states, n, 0) {
            Some(Verdict::Busy(e)) => busy::<()>(&Err(e)),
            other => panic!("not busy: {other:?}"),
        };
        let unmounting = pack(UNMOUNTING, 0);
        assert_eq!(reason(&[unmounting], 1), (0, 1, 0));
        assert_eq!(reason(&[unmounting], 0), (0, 0, 1));
        assert!(verdict("p/c", &[pack(state::MOUNTED, 0)], 0, 0).is_none());
    }

    #[test]
    fn older_orphan_and_newer_failure() {
        if crate::fork::testing::rerun_alone("conn::close_tests::older_orphan_and_newer_failure") {
            return;
        }
        // A view drop of A stopped before its exchange.
        fn park_view_drop(
            f: &Fixture,
            hook: &'static str,
            no_pid: bool,
        ) -> (std::sync::mpsc::SyncSender<()>, std::thread::JoinHandle<()>) {
            let a = f.mount();
            let (at_tx, at_rx) = signal();
            let (go_tx, go_rx) = signal();
            let t = std::thread::spawn(move || {
                testing::set(hook, None, move |_| {
                    at_tx.send(()).unwrap();
                    wait(&go_rx);
                });
                if no_pid {
                    crate::fork::testing::with_raw_pid(-1, || drop(a));
                } else {
                    drop(a);
                }
            });
            wait(&at_rx);
            (go_tx, t)
        }

        let f = fixture_plain();
        let (go, t) = park_view_drop(&f, "ViewDropBeforeOrphan", true);
        let b = f.mount();
        go.send(()).unwrap();
        t.join().unwrap();
        assert_eq!(state::tag(listed(f.conn())[0].state()), state::ORPHANED);
        f.mem.fail_next("dfs_umount", libc::EIO);
        assert_eq!(umount_errno(&b.close(T1)), libc::EIO);
        assert_eq!(umount_errno(&f.cont.close(T1)), libc::EIO);
        let mounts = f.calls("dfs_mount");
        assert_eq!(umount_errno(&f.cont.mount_dfs()), libc::EIO);
        assert_eq!(f.calls("dfs_mount"), mounts);

        // Two failures: the older mount fails last.
        let f = fixture();
        let (go, t) = park_view_drop(&f, "ViewDropBeforeUnmount", false);
        let b = f.mount();
        f.mem.fail_next("dfs_umount", libc::EIO);
        assert_eq!(umount_errno(&b.close(T1)), libc::EIO);
        f.mem.fail_next("dfs_umount", libc::ENOSPC);
        go.send(()).unwrap();
        t.join().unwrap();
        assert_eq!(umount_errno(&f.cont.close(T1)), libc::ENOSPC);
    }

    #[test]
    fn error_recorded_while_the_close_waits_for_the_gate() {
        let f = fixture();
        let dfs = f.mount();
        let query = f.park_query();
        let (blocked_tx, blocked_rx) = signal();
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let key = f.conn().gate.key();
        let cont = f.cont.clone();
        let closer = std::thread::spawn(move || {
            testing::set("GateRoundBlocked", Some(key), move |_| {
                blocked_tx.send(()).unwrap();
            });
            done_tx.send(cont.close(Duration::from_secs(60))).unwrap();
        });
        wait(&blocked_rx);
        f.mem.fail_next("dfs_umount", libc::EIO);
        drop(dfs);
        let r = done_rx.recv_timeout(LIMIT).expect("the close kept waiting");
        assert_eq!(umount_errno(&r), libc::EIO);
        closer.join().unwrap();
        query.finish().unwrap();
    }

    #[test]
    fn error_recorded_after_the_last_verdict_wins_at_the_deadline() {
        let f = fixture();
        let parked = RefCell::new(Some(f.park_failing_drop(f.mount(), libc::EIO)));
        testing::set("AfterVerdict", None, move |_| {
            parked.borrow_mut().take().unwrap().finish();
        });
        assert_eq!(umount_errno(&f.cont.close(ZERO)), libc::EIO);
    }

    #[test]
    fn obstacle_gone_at_the_deadline_closes() {
        let at_deadline = |f: &Fixture, action: Box<dyn FnOnce()>| {
            let action = RefCell::new(Some(action));
            testing::set(
                "BeforeDeadlineVerdict",
                Some(f.conn().gate.key()),
                move |_| (action.borrow_mut().take().unwrap())(),
            );
            f.cont.close(ZERO).unwrap();
            assert_eq!(closed_count(f), 1);
            assert_eq!(f.waits(), 0);
        };

        // A mount being dropped.
        let f = fixture();
        let parked = f.park_drop(f.mount());
        at_deadline(&f, Box::new(move || parked.finish()));

        // An open transaction.
        let f = fixture();
        let (open_tx, open_rx) = signal();
        let (close_tx, close_rx) = signal();
        let (closed_tx, closed_rx) = signal();
        let cont = f.cont.clone();
        let worker = std::thread::spawn(move || {
            let tx = cont.tx(TxMode::ReadWrite).unwrap();
            open_tx.send(()).unwrap();
            wait(&close_rx);
            tx.close().unwrap();
            closed_tx.send(()).unwrap();
        });
        wait(&open_rx);
        at_deadline(
            &f,
            Box::new(move || {
                close_tx.send(()).unwrap();
                wait(&closed_rx);
            }),
        );
        worker.join().unwrap();

        // A call of the container: the only round was contended.
        let f = fixture();
        let query = f.park_query();
        at_deadline(&f, Box::new(move || query.finish().unwrap()));
    }

    #[test]
    fn busy_reason_is_the_same_without_the_gate() {
        let f = fixture();
        let tx = f.cont.tx(TxMode::ReadWrite).unwrap();
        let query = f.park_query();
        assert_eq!(busy(&f.cont.close(ZERO)), (0, 1, 0));
        query.finish().unwrap();
        tx.close().unwrap();

        let f = fixture();
        let parked = f.park_drop(f.mount());
        let query = f.park_query();
        assert_eq!(busy(&f.cont.close(ZERO)), (0, 0, 1));
        query.finish().unwrap();
        parked.finish();

        let f = fixture();
        let query = f.park_query();
        assert_eq!(busy(&f.cont.close(ZERO)), (1, 0, 0));
        query.finish().unwrap();
    }

    #[test]
    fn locked_list_falls_back_without_losing_a_known_error() {
        type Ack = std::sync::mpsc::SyncSender<()>;
        enum Cmd {
            Lock(Ack),
            Unlock(Ack),
        }
        fn send(cmd: &std::sync::mpsc::Sender<Cmd>, make: fn(Ack) -> Cmd) {
            let (tx, rx) = signal();
            cmd.send(make(tx)).unwrap();
            wait(&rx);
        }
        let f = fixture();
        let conn = f.conn().clone();
        std::thread::scope(|s| {
            let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
            let list = &conn.mounts;
            s.spawn(move || {
                let mut guard = None;
                // Ends when every sender is gone.
                while let Ok(c) = cmd_rx.recv() {
                    match c {
                        Cmd::Lock(ack) => {
                            guard = Some(list.lock().unwrap());
                            ack.send(()).unwrap();
                        }
                        Cmd::Unlock(ack) => {
                            guard = None;
                            ack.send(()).unwrap();
                        }
                    }
                }
                drop(guard);
            });

            // Locked from the start, a call of the container running.
            let query = f.park_query();
            send(&cmd_tx, Cmd::Lock);
            let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
            let cont = f.cont.clone();
            let t = std::thread::spawn(move || done_tx.send(cont.close(ZERO)).unwrap());
            let r = done_rx
                .recv_timeout(LIMIT)
                .expect("the close waited for the list");
            assert_eq!(busy(&r), (1, 0, 0));
            t.join().unwrap();
            send(&cmd_tx, Cmd::Unlock);
            query.finish().unwrap();

            // Locked after this call's own unmount failed.
            let b = f.mount();
            f.mem.fail_next("dfs_umount", libc::EIO);
            {
                let cmd = cmd_tx.clone();
                testing::set("AfterMountClose", None, move |_| send(&cmd, Cmd::Lock));
            }
            assert_eq!(umount_errno(&f.cont.close(ZERO)), libc::EIO);
            drop(b);

            // Directly at the deadline, with the list still locked.
            let passed = Deadline::passed();
            let known = RefCell::new(Some(umount_error("mock/mock", libc::EIO)));
            let waiting = Last::Waiting(Error::busy(0, 3));
            match conn.at_deadline(passed, waiting, false, &known) {
                Final::Error(e) => assert_eq!(umount_errno::<()>(&Err(e)), libc::EIO),
                Final::Retry => panic!("retry"),
            }
            let none = RefCell::new(None);
            let contended = Last::Contended(Error::busy(2, 0));
            match conn.at_deadline(passed, contended, false, &none) {
                Final::Error(e) => assert_eq!(busy::<()>(&Err(e)), (2, 0, 0)),
                Final::Retry => panic!("retry"),
            }
            let waiting = Last::Waiting(Error::busy(0, 3));
            match conn.at_deadline(passed, waiting, false, &none) {
                Final::Error(e) => assert_eq!(busy::<()>(&Err(e)), (0, 0, 0)),
                Final::Retry => panic!("retry"),
            }
            send(&cmd_tx, Cmd::Unlock);
            drop(cmd_tx);
        });
    }

    #[test]
    fn dropped_user_unmounts_at_once_while_a_close_waits() {
        let f = fixture();
        let parked_a = f.park_drop(f.mount());
        let b = f.mount();
        let (handed_tx, handed_rx) = std::sync::mpsc::sync_channel(1);
        let mem = f.mem.clone();
        let cont = f.cont.clone();
        let closer = std::thread::spawn(move || {
            let b = RefCell::new(Some(b));
            testing::set("AfterVerdict", None, move |_| {
                let pause = mem.pause_next("dfs_umount");
                let b = b.borrow_mut().take().unwrap();
                let dropper = std::thread::spawn(move || drop(b));
                assert!(pause.wait_entered(LIMIT));
                assert_eq!(pause.thread(), Some(dropper.thread().id()));
                handed_tx.send((pause, dropper)).unwrap();
            });
            cont.close(Duration::from_secs(60))
        });
        let (pause_b, dropper) = handed_rx.recv_timeout(LIMIT).unwrap();
        pause_b.release();
        dropper.join().unwrap();
        parked_a.finish();
        closer.join().unwrap().unwrap();
        assert_eq!(f.calls("dfs_umount"), 2);
        assert_eq!(closed_count(&f), 1);
    }

    #[test]
    fn fault_between_the_two_reads_is_seen() {
        if crate::fork::testing::rerun_alone(
            "conn::close_tests::fault_between_the_two_reads_is_seen",
        ) {
            return;
        }
        let f = fixture_plain();
        let cont: &'static crate::Container = Box::leak(Box::new(f.cont.clone()));
        let oid = cont
            .oid_allocator()
            .next(
                crate::obj::ObjectType::KvHashed,
                crate::obj::ObjectClass::UNKNOWN,
                crate::obj::ClassHints::default(),
            )
            .unwrap();
        let kv = cont.kv(oid, crate::obj::OpenMode::ReadWrite).unwrap();
        let tx = RefCell::new(Some(cont.tx(TxMode::ReadWrite).unwrap()));
        testing::set("AfterTxRead", None, move |_| {
            let tx = tx.borrow_mut().take().unwrap();
            crate::fork::testing::with_raw_pid(-1, || drop(tx));
        });
        let first = Rc::new(Cell2::new(None));
        {
            let first = first.clone();
            testing::record("AfterVerdict", None, move |v| {
                let v = v.downcast_ref::<Option<Verdict>>().unwrap();
                first
                    .borrow_mut()
                    .get_or_insert(matches!(v, Some(Verdict::Standing(_))));
            });
        }
        let r = cont.close(T1);
        assert_eq!(forked(&r).1, 0);
        assert_eq!(*first.borrow(), Some(true));
        cont.query().unwrap();
        kv.put(None, "k", b"v").unwrap();
        assert_eq!(f.calls("daos_cont_close"), 0);
    }

    #[test]
    fn destroy_is_refused_while_closing_and_when_kept() {
        if crate::fork::testing::rerun_alone(
            "conn::close_tests::destroy_is_refused_while_closing_and_when_kept",
        ) {
            return;
        }
        crate::fork::testing::force_plain();
        use crate::pool::PoolOptions;
        use crate::Pool;
        let setup = || {
            testing::clear();
            let mem = Arc::new(crate::mock::MemDfs::new());
            let pool = Pool::connect_mock(mem.clone(), &PoolOptions::new("p")).unwrap();
            let cont = pool.open_container("c").unwrap();
            (mem, pool, cont)
        };
        let refused = |pool: &Pool, mem: &crate::mock::MemDfs| {
            assert!(matches!(
                pool.destroy_container("c", true),
                Err(Error::Busy { .. })
            ));
            assert_eq!(mem.calls("daos_cont_destroy2"), 0);
        };

        // The last view is dropped and its close is running.
        let (mem, pool, cont) = setup();
        let pause = mem.pause_next("daos_cont_close");
        let t = std::thread::spawn(move || drop(cont));
        assert!(pause.wait_entered(LIMIT));
        refused(&pool, &mem);
        pause.release();
        t.join().unwrap();

        // The close failed.
        let (mem, pool, cont) = setup();
        mem.fail_next("daos_cont_close", -1012);
        drop(cont);
        assert_eq!(mem.calls("daos_cont_close"), 1);
        refused(&pool, &mem);

        // The last view dropped while the process could not be confirmed.
        let (mem, pool, cont) = setup();
        let life = Arc::clone(&cont.conn.life);
        crate::fork::testing::with_raw_pid(-1, || drop(cont));
        assert_eq!(life.load(Ordering::SeqCst), life::KEPT);
        assert_eq!(mem.calls("daos_cont_close"), 0);
        refused(&pool, &mem);

        // Control: a close that succeeded frees the destroy, and its entry
        // goes when the next connection is stored.
        let (mem, pool, cont) = setup();
        cont.close(T1).unwrap();
        pool.destroy_container("c", true).unwrap();
        assert_eq!(mem.calls("daos_cont_destroy2"), 1);
        let _next = pool.open_container("e").unwrap();
        assert_eq!(mem.registry().lock().open_conts.len(), 1);
    }

    #[test]
    fn fork_child_drop_of_the_last_user_takes_no_lock() {
        use crate::fork::testing::wait_child;
        let f = fixture();
        let dfs = f.mount();
        let m = core(&dfs);
        let conn = f.conn().clone();
        assert_eq!(Arc::strong_count(&dfs.mount), 1);
        std::thread::scope(|s| {
            let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
            let mut release = Vec::new();
            // Each thread holds one lock until told to let go.
            macro_rules! hold {
                ($take:expr) => {{
                    let (go_tx, go_rx) = signal();
                    let held = held_tx.clone();
                    s.spawn(move || {
                        let guard = $take;
                        held.send(()).unwrap();
                        wait(&go_rx);
                        drop(guard);
                    });
                    release.push(go_tx);
                }};
            }
            let (cg, mg, list) = (&conn.gate, m.gate(), &conn.mounts);
            hold!(cg.hold());
            hold!(mg.hold());
            hold!(list.lock().unwrap());
            for _ in 0..3 {
                held_rx.recv_timeout(LIMIT).unwrap();
            }
            let calls = f.mem.calls_total();
            let cont = f.cont.clone();
            // SAFETY: the child uses atomics and system calls only, and
            // leaves with _exit.
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0, "fork failed");
            if pid == 0 {
                // SAFETY: alarm has no preconditions.
                unsafe { libc::alarm(10) };
                let code = if !matches!(dfs.close(ZERO), Err(Error::Forked { .. })) {
                    1
                } else if !matches!(cont.close(ZERO), Err(Error::Forked { .. })) {
                    2
                } else {
                    // Read here: the child has one thread, while other
                    // tests of the parent drop mounts at any time.
                    let drops = crate::dfs::VIEW_DROPS.load(Ordering::SeqCst);
                    drop(dfs);
                    if crate::dfs::VIEW_DROPS.load(Ordering::SeqCst) != drops + 1 {
                        3
                    } else if f.mem.calls_total() != calls {
                        4
                    } else {
                        0
                    }
                };
                // SAFETY: leave without running anything inherited.
                unsafe { libc::_exit(code) };
            }
            assert_eq!(wait_child(pid, Duration::from_secs(30)), 0);
            for go in release {
                go.send(()).unwrap();
            }
            drop(dfs);
        });
    }

    #[test]
    fn fork_child_drop_of_the_last_connection_takes_no_lock() {
        use crate::fork::testing::wait_child;
        let f = fixture();
        let Fixture { mem, cont } = f;
        assert_eq!(Arc::strong_count(&cont.conn), 1);
        let other = mem.clone();
        let registry = other.hold_registry_lock();
        let pool = cont.conn.pool.lock().unwrap();
        let calls = mem.calls_total();
        // The drop counter of the child before its drop.
        let drops = Rc::new(std::cell::Cell::new(0));
        {
            let drops = drops.clone();
            let mem = mem.clone();
            // The child leaves once `Drop::drop` of the connection has
            // returned, when its first field is dropped: before the other
            // fields are dropped and its memory is freed.
            testing::set("ContDropReturned", None, move |_| {
                // Fields are also dropped while a panic unwinds out of the
                // drop body: that must not read as a clean return.
                let code = if std::thread::panicking() {
                    4
                } else if CONT_DROPS.load(Ordering::SeqCst) != drops.get() + 1 {
                    1
                } else if mem.calls_total() != calls {
                    2
                } else {
                    0
                };
                // SAFETY: leave without running anything inherited.
                unsafe { libc::_exit(code) };
            });
        }
        // SAFETY: the child uses atomics, a hook of this thread set before
        // the fork, and system calls only; it frees no memory and leaves
        // with _exit when the first field of the connection is dropped.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // SAFETY: alarm has no preconditions.
            unsafe { libc::alarm(10) };
            std::mem::forget(pool);
            std::mem::forget(registry);
            // Read here: the child has one thread, while other tests of the
            // parent drop connections at any time.
            drops.set(CONT_DROPS.load(Ordering::SeqCst));
            drop(cont);
            // Reached only when the drop did not leave through the hook.
            // SAFETY: leave without running anything inherited.
            unsafe { libc::_exit(3) };
        }
        // Before any assertion: the hook would end this process on a panic.
        testing::clear();
        drop(pool);
        drop(registry);
        assert!(pid > 0, "fork failed");
        assert_eq!(wait_child(pid, Duration::from_secs(30)), 0);
    }

    #[test]
    fn failure_published_under_a_held_mount_gate_is_returned_at_once() {
        let f = fixture();
        let b = f.mount();
        let m = core(&b);
        let parked = Rc::new(Cell2::new(None));
        {
            let parked = parked.clone();
            let b = RefCell::new(Some(b));
            let mem = f.mem.clone();
            testing::set("AfterVerdict", None, move |_| {
                let b = b.borrow_mut().take().unwrap();
                mem.fail_next("dfs_umount", libc::EIO);
                let (at_tx, at_rx) = signal();
                let (go_tx, go_rx) = signal();
                let t = std::thread::spawn(move || {
                    testing::set("AfterFailedStore", None, move |_| {
                        at_tx.send(()).unwrap();
                        wait(&go_rx);
                    });
                    b.close(T30)
                });
                wait(&at_rx);
                *parked.borrow_mut() = Some((go_tx, t));
            });
        }
        let mount_waits = m.gate().waits.load(Ordering::SeqCst);
        let r = f.cont.close(T30);
        assert_eq!(umount_errno(&r), libc::EIO);
        assert_eq!(f.waits(), 0);
        assert_eq!(m.gate().waits.load(Ordering::SeqCst), mount_waits);
        assert_eq!(closed_count(&f), 0);
        let (go, t) = parked.borrow_mut().take().unwrap();
        go.send(()).unwrap();
        assert_eq!(umount_errno(&t.join().unwrap()), libc::EIO);
    }

    #[test]
    fn reason_before_the_last_acquire_is_not_skipped() {
        let before_last = |f: &Fixture, action: Box<dyn FnOnce()>| {
            let action = RefCell::new(Some(action));
            testing::set("BeforeLastAcquire", Some(f.conn().gate.key()), move |_| {
                (action.borrow_mut().take().unwrap())()
            });
            f.cont.close(ZERO)
        };

        // A failed unmount.
        let f = fixture();
        let query = f.park_query();
        let dfs = RefCell::new(Some(f.mount()));
        let mem = f.mem.clone();
        let r = before_last(
            &f,
            Box::new(move || {
                mem.fail_next("dfs_umount", libc::EIO);
                drop(dfs.borrow_mut().take());
            }),
        );
        assert_eq!(umount_errno(&r), libc::EIO);
        query.finish().unwrap();

        // A transaction opened after the deadline verdict.
        let f = fixture();
        let query = f.park_query();
        let pause = f.mem.pause_next("daos_tx_open");
        let (opened_tx, opened_rx) = signal();
        let (end_tx, end_rx) = signal();
        let cont = f.cont.clone();
        let worker = std::thread::spawn(move || {
            let tx = cont.tx(TxMode::ReadWrite).unwrap();
            opened_tx.send(()).unwrap();
            wait(&end_rx);
            drop(tx);
        });
        assert!(pause.wait_entered(LIMIT));
        let r = before_last(
            &f,
            Box::new(move || {
                pause.release();
                wait(&opened_rx);
            }),
        );
        assert_eq!(busy(&r), (0, 1, 0));
        end_tx.send(()).unwrap();
        worker.join().unwrap();
        query.finish().unwrap();

        // A mount whose last user was dropped after the deadline verdict.
        let f = fixture();
        let query = f.park_query();
        let dfs = RefCell::new(Some(f.mount()));
        let parked = Rc::new(Cell2::new(None));
        {
            let parked = parked.clone();
            let mem = f.mem.clone();
            let r = before_last(
                &f,
                Box::new(move || {
                    let pause = mem.pause_next("dfs_umount");
                    let d = dfs.borrow_mut().take().unwrap();
                    let t = std::thread::spawn(move || drop(d));
                    assert!(pause.wait_entered(LIMIT));
                    *parked.borrow_mut() = Some((pause, t));
                }),
            );
            assert_eq!(busy(&r), (0, 0, 1));
        }
        let (pause, t) = parked.borrow_mut().take().unwrap();
        pause.release();
        t.join().unwrap();
        query.finish().unwrap();
    }

    #[test]
    fn mount_in_use_refuses_new_calls_while_the_close_waits() {
        if crate::fork::testing::rerun_alone(
            "conn::close_tests::mount_in_use_refuses_new_calls_while_the_close_waits",
        ) {
            return;
        }
        crate::fork::testing::force_plain();
        struct Setup {
            f: Fixture,
            b: crate::Dfs,
            m: Arc<Mount>,
            file: Arc<crate::File>,
        }
        impl Setup {
            fn new() -> Setup {
                let f = fixture();
                let b = f.mount();
                let m = core(&b);
                let file = Arc::new(b.create("/f").unwrap());
                Setup { f, b, m, file }
            }
            fn closers(&self) -> usize {
                self.m.gate().closers()
            }
            /// A size call of the file, stopped inside the mock.
            fn park_size(
                &self,
            ) -> (
                crate::mock::PauseHandle,
                std::thread::JoinHandle<Result<u64>>,
            ) {
                let pause = self.f.mem.pause_next("dfs_get_size");
                let file = self.file.clone();
                let t = std::thread::spawn(move || file.size());
                assert!(pause.wait_entered(LIMIT));
                (pause, t)
            }
            /// A size call from another thread.
            fn size(&self) -> Result<u64> {
                let file = self.file.clone();
                std::thread::spawn(move || file.size()).join().unwrap()
            }
        }

        // The close waits: new calls of the mount are refused.
        let s = Setup::new();
        let (pause, size) = s.park_size();
        let (seen_tx, seen_rx) = std::sync::mpsc::sync_channel(1);
        let cont = s.f.cont.clone();
        let key = s.f.conn().gate.key();
        let (m, file, mem) = (s.m.clone(), s.file.clone(), s.f.mem.clone());
        let closer = std::thread::spawn(move || {
            testing::set("BeforeSleep", Some(key), move |_| {
                let calls = mem.calls("dfs_get_size");
                let file = file.clone();
                let r = std::thread::spawn(move || file.size()).join().unwrap();
                let seen = (
                    matches!(r, Err(Error::Closed)),
                    mem.calls("dfs_get_size") == calls,
                    m.gate().closers(),
                );
                seen_tx.send(seen).unwrap();
            });
            cont.close(Duration::from_secs(60))
        });
        assert_eq!(seen_rx.recv_timeout(LIMIT).unwrap(), (true, true, 1));
        pause.release();
        size.join().unwrap().unwrap();
        closer.join().unwrap().unwrap();

        // A close that ends with Busy lets the mount go.
        let s = Setup::new();
        let (pause, size) = s.park_size();
        assert_eq!(busy(&s.f.cont.close(ZERO)), (1, 0, 0));
        assert_eq!(s.closers(), 0);
        pause.release();
        size.join().unwrap().unwrap();
        s.size().unwrap();

        // A refused process check of the mount makes no hold.
        let s = Setup::new();
        let (pause, size) = s.park_size();
        let raises = s.m.gate().raises.load(Ordering::SeqCst);
        let r = crate::fork::testing::with_pid_fault_at("mount_close", || s.f.cont.close(ZERO));
        assert_eq!(forked(&r).1, 0);
        assert_eq!(s.m.gate().raises.load(Ordering::SeqCst), raises);
        pause.release();
        size.join().unwrap().unwrap();
        s.size().unwrap();

        // A check that panics after the hold was made.
        let s = Setup::new();
        let (pause, size) = s.park_size();
        testing::set("CheckReturned", None, |_| panic!("check"));
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.f.cont.close(ZERO)));
        assert!(r.is_err());
        assert_eq!(s.closers(), 0);
        pause.release();
        size.join().unwrap().unwrap();

        // Another close of the mount keeps its own count.
        let s = Setup::new();
        let (pause, size) = s.park_size();
        let (at_tx, at_rx) = signal();
        let (go_tx, go_rx) = signal();
        let mount_key = s.m.gate().key();
        let b = s.b.clone();
        let other = std::thread::spawn(move || {
            testing::set("BeforeSleep", Some(mount_key), move |_| {
                at_tx.send(()).unwrap();
                wait(&go_rx);
            });
            b.close(Duration::MAX)
        });
        wait(&at_rx);
        let during = Rc::new(Cell2::new(None));
        {
            let during = during.clone();
            let m = s.m.clone();
            testing::set("AfterMountClose", None, move |_| {
                *during.borrow_mut() = Some(m.gate().closers());
            });
        }
        assert_eq!(busy(&s.f.cont.close(ZERO)), (1, 0, 0));
        assert_eq!(*during.borrow(), Some(2));
        assert_eq!(s.closers(), 1);
        assert!(matches!(s.size(), Err(Error::Closed)));
        pause.release();
        size.join().unwrap().unwrap();
        go_tx.send(()).unwrap();
        other.join().unwrap().unwrap();
        assert_eq!(s.closers(), 0);
    }

    #[test]
    fn mount_states_are_read_around_the_transaction_reads() {
        let f = fixture();
        let parked = RefCell::new(Some(f.park_failing_drop(f.mount(), libc::EIO)));
        let (open_tx, open_rx) = signal();
        let (close_tx, close_rx) = signal();
        let (closed_tx, closed_rx) = signal();
        let cont = f.cont.clone();
        let worker = std::thread::spawn(move || {
            let tx = cont.tx(TxMode::ReadWrite).unwrap();
            open_tx.send(()).unwrap();
            wait(&close_rx);
            tx.close().unwrap();
            closed_tx.send(()).unwrap();
        });
        wait(&open_rx);
        testing::set("BeforeTxRead", None, move |_| {
            parked.borrow_mut().take().unwrap().finish();
            close_tx.send(()).unwrap();
            wait(&closed_rx);
        });
        let conn = f.conn();
        let o = conn.observe(Deadline::after(LIMIT)).unwrap();
        match verdict(&conn.target(), &o.states, o.transactions, o.tx_fault) {
            Some(Verdict::Standing(e)) => assert_eq!(umount_errno::<()>(&Err(e)), libc::EIO),
            other => panic!("{other:?}"),
        }
        worker.join().unwrap();
    }
}
