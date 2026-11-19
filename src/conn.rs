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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::fork::Stamp;
use crate::gate::{Check, Gate, Pass};
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
// Container connection

/// One container handle on a pool connection. Closed when the last `Arc`
/// goes (quietly), or by an explicit close or release (with an error report).
pub(crate) struct ContConn {
    pub(crate) backend: Arc<dyn Raw>,
    pub(crate) coh: daos_handle_t,
    pub(crate) name: String,
    pub(crate) uuid: Uuid,
    pub(crate) access: Access,
    pub(crate) stamp: Stamp,
    pub(crate) gate: Gate,
    /// DFS mounts on this container (closed by an explicit container close).
    pub(crate) mounts: Mutex<Vec<Weak<crate::dfs::Mount>>>,
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
    done: AtomicBool,
    /// True from `daos_cont_open` until a `daos_cont_close` of this handle
    /// succeeded; shared with the registry (`RegistryInner::open_conts`), so
    /// a destroy sees the native handle also when the connection is gone or
    /// being dropped. Stays true for a handle that is never closed (a failed
    /// close, or a close skipped after a failed process check).
    native_open: Arc<AtomicBool>,
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
            backend,
            coh,
            name: name.to_string(),
            uuid: Uuid::from_bytes(info.ci_uuid),
            access: pool.access,
            stamp,
            gate: Gate::new(),
            mounts: Mutex::new(Vec::new()),
            handles: crate::handles::Table::default(),
            transactions: AtomicUsize::new(0),
            poh: pool.poh,
            pool_name: pool.name.clone(),
            pool: Mutex::new(Some(pool)),
            done: AtomicBool::new(false),
            native_open: Arc::new(AtomicBool::new(true)),
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

    /// Enter a container operation.
    pub(crate) fn enter(&self) -> Result<Pass<'_>> {
        self.stamp.check()?;
        self.gate.enter(false)
    }

    /// Close the handle once and give up the pool lease. Returns the first
    /// error.
    fn finish(&self) -> Result<()> {
        if self.done.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        // SAFETY: a handle from daos_cont_open, closed once (guarded by
        // `done`); mounts and objects of this container were released first.
        let rc = unsafe { self.backend.daos_cont_close(self.coh, std::ptr::null_mut()) };
        let closed = if rc != 0 {
            // The handle may still be open in libdaos: `native_open` stays.
            Err(Error::daos_at("daos_cont_close", rc, self.target()))
        } else {
            self.native_open.store(false, Ordering::SeqCst);
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

    /// Explicit close for every view: refuse new operations, wait up to
    /// `timeout` for running ones, for open transactions and for the calls
    /// of DFS mounts of this container, unmount those mounts, then close
    /// object handles, close the handle and give up the pool lease.
    ///
    /// The container is marked closed only after every mount is unmounted;
    /// a `Busy` (calls, transactions, or a mount still in use) leaves the
    /// container open and usable. A container connection has at most one
    /// live mount (mounts are shared per container connection), so a `Busy`
    /// also leaves that mount untouched.
    pub(crate) fn close(&self, timeout: Duration) -> Result<()> {
        self.stamp.check_cold()?;
        // Errors of mounts that were unmounted (a failed dfs_release), to be
        // reported once the container is closed.
        let mount_err: RefCell<Option<Error>> = RefCell::new(None);
        let precheck = |deadline: Instant| {
            let n = self.transactions.load(Ordering::SeqCst);
            if n > 0 {
                return Check::Wait(Error::busy(0, n));
            }
            self.unmount_all(deadline, &mount_err)
        };
        let done = self.gate.close_when(timeout, precheck, || {
            // Every mount of this container is unmounted (precheck).
            self.mounts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            let handles = self.handles.close_all(self.backend.as_ref()).err();
            let first = handles.or(mount_err.borrow_mut().take());
            match (self.finish(), first) {
                (Err(e), _) | (Ok(()), Some(e)) => Err(e),
                (Ok(()), None) => Ok(()),
            }
        })?;
        done.unwrap_or(Ok(()))
    }

    /// Unmount every live DFS mount of this container, waiting for its
    /// calls until `deadline`. Runs with the container gate held
    /// exclusively, so no mount can be added meanwhile (a mount is made
    /// and listed under a container pass).
    fn unmount_all(&self, deadline: Instant, errors: &RefCell<Option<Error>>) -> Check {
        let mounts: Vec<Arc<crate::dfs::Mount>> = self
            .mounts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for m in mounts {
            let left = deadline.saturating_duration_since(Instant::now());
            let r = m.close(left);
            if !m.is_mounted() {
                if let Err(e) = r {
                    errors.borrow_mut().get_or_insert(e);
                }
                continue;
            }
            return match r {
                Err(e @ Error::Busy { .. }) => Check::Wait(e),
                Err(e) => Check::Fail(e),
                // Closed earlier, but dfs_umount failed then: the dfs_t
                // still uses this container handle.
                Ok(()) => Check::Fail(m.umount_error()),
            };
        }
        Check::Ready
    }

    /// Give up one lease. When it was the last one, close the container and
    /// release its pool lease; the first error is reported.
    ///
    /// When this process cannot be confirmed as the one that opened the
    /// container, the handle is marked done (its drop makes no DAOS call for
    /// it) and the error of the check is returned. The pool lease goes with
    /// the connection as usual.
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
            // No close: `native_open` stays true, so a destroy of this
            // container stays busy in this process.
            c.done.store(true, Ordering::SeqCst);
            return Err(e);
        }
        c.finish()
    }
}

impl Drop for ContConn {
    fn drop(&mut self) {
        if self.stamp.is_foreign() {
            // Fork child, or a process check that failed: leak, never call
            // libdaos. `native_open` stays true.
            return;
        }
        // Object handles hold leases, so none is open here.
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
    pub(crate) mounts: HashMap<(String, String, Access), Weak<crate::dfs::Mount>>,
    /// `(pool, container, native_open)` of every container connection made
    /// through this registry whose native handle was not closed yet. Entries
    /// with a cleared flag are dropped when a connection is stored.
    open_conts: Vec<(String, String, Arc<AtomicBool>)>,
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
    /// opened, in any access mode, or when a native handle of it is still
    /// open: being closed by a connection that the registry can no longer
    /// reach, or left open for good (see `open_conts`). An error when a live
    /// entry cannot be shown to belong to this process.
    ///
    /// Every connection and mount upgraded here is moved into `upgraded`,
    /// for the caller to drop after it released the registry lock.
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
        for ((p, c, _), w) in &self.mounts {
            if p == pool && c == cont {
                if let Some(m) = w.upgrade() {
                    let live = m.live();
                    upgraded.mounts.push(m);
                    if live? {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(self
            .open_conts
            .iter()
            .any(|(p, c, open)| p == pool && c == cont && open.load(Ordering::SeqCst)))
    }
}

/// Connections and mounts upgraded under the registry lock, dropped by the
/// owner after the lock was released.
#[derive(Default)]
pub(crate) struct Upgraded {
    conts: Vec<Arc<ContConn>>,
    mounts: Vec<Arc<crate::dfs::Mount>>,
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
                r.open_conts.retain(|e| e.2.load(Ordering::SeqCst));
                r.open_conts.push((
                    c.pool_name.clone(),
                    c.name.clone(),
                    Arc::clone(&c.native_open),
                ));
            },
        )
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
            assert!(inner.open_conts[0].2.load(Ordering::SeqCst));
        }
        again.close(SECS).unwrap();
        assert!(registry
            .lock()
            .open_conts
            .iter()
            .all(|(_, _, open)| !open.load(Ordering::SeqCst)));
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
