// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! DFS mounts: [`Dfs`], the mount on a crate-owned container connection, the
//! open handle table and the directory handle cache.
//!
//! A mount is shared between its users and its container connection. The
//! users ([`Dfs`], [`File`], [`ReadDir`], a directory handle while it
//! releases its object) hold a [`MountRef`]; the registry holds it weakly.
//! The container connection lists the [`Mount`] itself, and the mount holds
//! a lease of the connection until its `dfs_umount` succeeds. The drop of the last
//! `MountRef` unmounts, unless a close got there first.
//!
//! The states of a mount, the rule that every call of a mount comes from a
//! thread that holds a user (with the two teardowns as the exceptions), the
//! rule on the last container lease and the lock order are described in
//! `crate::conn`.

mod dir;
mod file;
mod metadata;
mod writer;

pub use dir::{DirEntry, ReadDir};
pub use file::{File, OpenOptions};
pub use metadata::{FileType, Metadata};
pub use writer::Writer;

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use crate::conn::{state, Access, ContConn, Key, Registry};
use crate::error::{Error, Result};
use crate::fork::Stamp;
use crate::gate::{Check, Deadline, Final, Gate, Pass};
use crate::path;
use crate::sys::api::Raw;
use crate::sys::{self, dfs_obj_t, dfs_t};

/// Default number of cached directory handles per mount.
pub const DEFAULT_DIR_CACHE_CAPACITY: usize = 1024;

/// Options of [`Dfs::connect_with`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConnectOptions {
    /// Pool label or UUID string.
    pub pool: String,
    /// Container label or UUID string.
    pub container: String,
    /// DAOS system name; `None` = the default system.
    pub sys_name: Option<String>,
    /// Mount read-only (`O_RDONLY`); default false (`O_RDWR`).
    pub read_only: bool,
    /// Number of cached directory handles; default 1024, 0 disables the cache.
    ///
    /// Cached handles are checked with one `dfs_ostat` before creates and
    /// listings, so a directory removed and recreated elsewhere is noticed. A
    /// rename of an ancestor by another writer is not noticed; use 0 when
    /// other writers rename directories this process uses.
    pub dir_cache_capacity: usize,
}

impl ConnectOptions {
    /// Options for `pool` and `container` with defaults (read-write, default
    /// system, 1024 cached directory handles).
    pub fn new(pool: impl Into<String>, container: impl Into<String>) -> Self {
        ConnectOptions {
            pool: pool.into(),
            container: container.into(),
            sys_name: None,
            read_only: false,
            dir_cache_capacity: DEFAULT_DIR_CACHE_CAPACITY,
        }
    }

    /// Set the DAOS system name.
    pub fn sys_name(mut self, name: impl Into<String>) -> Self {
        self.sys_name = Some(name.into());
        self
    }

    /// Mount read-only.
    pub fn read_only(mut self, yes: bool) -> Self {
        self.read_only = yes;
        self
    }

    /// Set the directory handle cache capacity (0 disables it).
    pub fn dir_cache_capacity(mut self, n: usize) -> Self {
        self.dir_cache_capacity = n;
        self
    }

    fn validate(&self) -> Result<()> {
        if self.pool.is_empty() {
            return Err(Error::InvalidArgument("pool is empty"));
        }
        if self.container.is_empty() {
            return Err(Error::InvalidArgument("container is empty"));
        }
        let has_nul = |s: &str| s.as_bytes().contains(&0);
        if has_nul(&self.pool)
            || has_nul(&self.container)
            || self.sys_name.as_deref().is_some_and(has_nul)
        {
            return Err(Error::InvalidArgument(
                "pool, container and system names must not contain NUL",
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Raw pointer wrappers

/// `dfs_t *` owned by one [`Mount`].
struct DfsPtr(*mut dfs_t);
/// `dfs_obj_t *` of an open object.
#[derive(Clone, Copy)]
pub(crate) struct ObjPtr(pub(crate) *mut dfs_obj_t);

// SAFETY: libdfs protects shared mount state with the dfs_t lock ("lock for
// threadsafety", dfs_internal.h) and the module mutexes; blocking calls use a
// per-thread private event (client/api/event.c). The calls this crate makes
// on a shared dfs_obj_t (read, write, get_size, ostat, iterate, and use as a
// parent) only read the object's fields. dfs_release, which frees the object,
// runs only under the exclusive rules of the close gate (see Mount).
unsafe impl Send for DfsPtr {}
// SAFETY: see above.
unsafe impl Sync for DfsPtr {}
// SAFETY: see above.
unsafe impl Send for ObjPtr {}
// SAFETY: see above.
unsafe impl Sync for ObjPtr {}

// ---------------------------------------------------------------------------
// Operations

/// One running operation: holds the mount's gate shared.
pub(crate) struct Op<'a> {
    mount: &'a Mount,
    _pass: Pass<'a>,
}

impl Op<'_> {
    pub(crate) fn dfs(&self) -> *mut dfs_t {
        self.mount.raw.0
    }
    pub(crate) fn backend(&self) -> &dyn Raw {
        self.mount.backend.as_ref()
    }
}

// ---------------------------------------------------------------------------
// Directory handles

/// A cached directory handle (from `dfs_lookup`). Released when the last
/// reference drops, so a call never sees a freed pointer.
pub(crate) struct DirHandle {
    /// The users' handle of the mount, not the mount itself.
    mount: Weak<MountRef>,
    pub(crate) obj: ObjPtr,
    slot: u64,
    stamp: Stamp,
}

impl Drop for DirHandle {
    fn drop(&mut self) {
        if self.stamp.is_foreign() {
            return; // fork child: leak, never touch locks or libdfs
        }
        // The upgraded handle makes this release a user of the mount until
        // `release_slot` has returned, so no teardown runs under it. When
        // the upgrade fails, the teardown releases this slot with the
        // others.
        if let Some(m) = self.mount.upgrade() {
            m.release_dir_slot(self.slot);
            drop(m);
        }
    }
}

// ---------------------------------------------------------------------------
// Mount

/// What the users of a mount hold: [`Dfs`], [`File`], [`ReadDir`], a
/// directory handle while it releases its object, and the registry (weak).
/// Its drop means that the last user is gone, and it unmounts.
pub(crate) struct MountRef {
    core: Arc<Mount>,
}

impl MountRef {
    /// The mount itself.
    #[cfg(all(test, feature = "mock"))]
    pub(crate) fn core(&self) -> Arc<Mount> {
        Arc::clone(&self.core)
    }
}

impl std::ops::Deref for MountRef {
    type Target = Mount;

    fn deref(&self) -> &Mount {
        &self.core
    }
}

#[cfg(all(test, feature = "mock"))]
pub(crate) static VIEW_DROPS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

impl Drop for MountRef {
    fn drop(&mut self) {
        #[cfg(all(test, feature = "mock"))]
        VIEW_DROPS.fetch_add(1, Ordering::SeqCst);
        // Before any lock: this may be a fork child.
        match self.core.stamp.check_cold() {
            Ok(()) => {}
            Err(Error::Forked {
                created_pid,
                current_pid: 0,
            }) => {
                // The owner cannot be confirmed: leave the mount as it is,
                // listed with its lease, and record why.
                #[cfg(all(test, feature = "mock"))]
                crate::gate::testing::run("ViewDropBeforeOrphan", 0, &());
                self.core.orphan(created_pid);
                return;
            }
            // Fork child: leak everything. Tearing down could send messages
            // on connections the parent still uses.
            Err(_) => return,
        }
        #[cfg(all(test, feature = "mock"))]
        crate::gate::testing::run("ViewDropBeforeUnmount", 0, &());
        if self.core.begin_unmount() {
            let _ = self.core.teardown();
            drop(self.core.take_dir_cache());
        }
    }
}

/// What a close of a mount did.
pub(crate) enum Unmount {
    /// The process check failed. This call touched nothing.
    Refused(Error),
    /// The mount gate could not be taken by the deadline (calls of the
    /// mount, or another close of it, held it). This call touched nothing.
    Busy(Error),
    /// Closed before, or another thread is closing it. Nothing was done
    /// here.
    Elsewhere,
    /// This call ran the teardown; its result.
    Done(Result<()>),
}

/// While it lives, new calls of the mount are refused with `Closed`, as
/// they are while a close of the mount waits. Owns the mount; raises the
/// gate's count of waiting closes by one when made and lowers it by one
/// when dropped. Takes no lock.
pub(crate) struct CloseHold(Arc<Mount>);

impl CloseHold {
    fn new(m: &Arc<Mount>) -> CloseHold {
        let m = Arc::clone(m);
        m.gate.raise_closers();
        CloseHold(m)
    }
}

impl Drop for CloseHold {
    fn drop(&mut self) {
        self.0.gate.lower_closers();
    }
}

/// One `dfs_t` (from `dfs_mount`) and everything attached to it, shared
/// between its [`MountRef`] and the mount list of its container.
///
/// Close and handle synchronisation: every operation that passes the `dfs_t`
/// or a `dfs_obj_t` to libdfs holds the gate shared and checks the closed
/// flag, and its thread holds a [`MountRef`]. An explicit close takes the
/// gate exclusively and wins the exchange of `state` from mounted to
/// unmounting before it marks the gate closed; the drop of the last
/// `MountRef` wins the same exchange with no gate. The one winner releases
/// every pointer in the handle table, unmounts, and publishes the end in
/// `state`. A handle never passes its pointer without holding the gate, so
/// no call can run on a freed object.
pub(crate) struct Mount {
    pub(crate) backend: Arc<dyn Raw>,
    raw: DfsPtr,
    pool: String,
    container: String,
    sys_name: Option<String>,
    read_only: bool,
    pub(crate) stamp: Stamp,
    gate: Gate,
    handles: Mutex<HashMap<u64, ObjPtr>>,
    next_slot: AtomicU64,
    dir_cache: Mutex<HashMap<String, Arc<DirHandle>>>,
    dir_cache_capacity: usize,
    /// Where the mount stands: mounted, being unmounted, unmounted, or left
    /// mounted for good (see `crate::conn::state`). One atomic value,
    /// readable without a lock.
    state: AtomicU64,
    /// The users' handle, for directory handles made by this mount.
    view: Weak<MountRef>,
    /// The container lease. Taken out after `dfs_umount` returned 0, kept
    /// otherwise.
    cont: Mutex<Option<Arc<ContConn>>>,
    /// The connection this mount was made on, compared by address only and
    /// never upgraded: a check that takes no lock.
    made_on: Weak<ContConn>,
}

impl Mount {
    /// Enter an operation: hold the gate shared and check the closed flag.
    /// While a close waits for the gate, new operations get `Closed`.
    pub(crate) fn enter(&self) -> Result<Op<'_>> {
        Ok(Op {
            mount: self,
            _pass: self.gate.enter(false)?,
        })
    }

    /// True after a successful close. In a fork child always true: nothing
    /// inherited may be used there.
    pub(crate) fn is_closed(&self) -> bool {
        if self.stamp.is_foreign() {
            return true;
        }
        self.gate.is_closed()
    }

    /// Whether the mount is open. An error when this process is not, or
    /// cannot be shown to be, the one that mounted it. One pid read: not
    /// `is_closed()`, which asks the stamp again and would turn a failed
    /// second read into "closed".
    pub(crate) fn live(&self) -> Result<bool> {
        self.stamp.check_cold()?;
        Ok(!self.gate.is_closed())
    }

    /// The packed state (see `crate::conn::state`).
    pub(crate) fn state(&self) -> u64 {
        self.state.load(Ordering::SeqCst)
    }

    /// The mount gate.
    #[cfg(all(test, feature = "mock"))]
    pub(crate) fn gate(&self) -> &Gate {
        &self.gate
    }

    /// Register an open object pointer; returns its slot.
    pub(crate) fn track(&self, obj: ObjPtr) -> u64 {
        let slot = self.next_slot.fetch_add(1, Ordering::Relaxed);
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(slot, obj);
        slot
    }

    /// Release the object in `slot` (dfs_release) if the mount is still open
    /// and the slot still exists. While a close holds the mount gate this
    /// waits for it, so the call returns only after that close released the
    /// slot. After close the slot is gone: nothing to do.
    pub(crate) fn release_slot(&self, slot: u64) -> Result<()> {
        // Releasing is allowed while a close waits.
        let _pass = match self.gate.enter(true) {
            Ok(p) => p,
            Err(_) => return Ok(()),
        };
        let obj = self
            .handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&slot);
        if let Some(obj) = obj {
            // SAFETY: the pointer came from dfs_open/dfs_lookup on this mount,
            // was removed from the table just now (so it is released once),
            // and the gate is held, so close cannot release it concurrently.
            let rc = unsafe { self.backend.dfs_release(obj.0) };
            if rc != 0 {
                return Err(Error::Dfs {
                    op: "dfs_release",
                    errno: rc,
                    path: None,
                });
            }
        }
        Ok(())
    }

    /// [`Mount::release_slot`] for a dropped directory handle, which reports
    /// nothing and so need not wait for a close: once the mount is closed for
    /// good, the teardown releases every slot. Read before the gate, which
    /// another close may hold exclusively, also while the caller holds a
    /// container gate.
    fn release_dir_slot(&self, slot: u64) {
        if self.gate.is_closed() {
            return;
        }
        let _ = self.release_slot(slot);
    }

    /// Exchange mounted for unmounting. The one caller that gets true runs
    /// the teardown.
    fn begin_unmount(&self) -> bool {
        self.state
            .compare_exchange(
                state::pack(state::MOUNTED, 0),
                state::pack(state::UNMOUNTING, 0),
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Exchange mounted for orphaned with the recorded process id; any other
    /// state is kept.
    fn orphan(&self, pid: u32) {
        let _ = self.state.compare_exchange(
            state::pack(state::MOUNTED, 0),
            state::pack(state::ORPHANED, pid),
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    fn take_dir_cache(&self) -> HashMap<String, Arc<DirHandle>> {
        std::mem::take(&mut *self.dir_cache.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Release every tracked object, unmount, and, when `dfs_umount`
    /// succeeded, take the mount out of the list and give up the container
    /// lease (which closes the container, and then the pool, when it was the
    /// last lease). Run only by the thread whose [`Mount::begin_unmount`]
    /// returned true: no call of the mount runs or can start then.
    ///
    /// A `dfs_release` that fails leaves its object allocated inside libdfs
    /// (`obj.c`: the object is freed only on success); the crate forgets the
    /// pointer anyway and reports the first error, so such an object leaks.
    /// A failed `dfs_umount` is never tried again: the mount stays listed
    /// with its lease.
    fn teardown(&self) -> Result<()> {
        let objs: Vec<ObjPtr> = self
            .handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, o)| o)
            .collect();
        let mut first_err = None;
        for obj in objs {
            // SAFETY: the one teardown (see above); each pointer was taken
            // out of the table, so it is released once.
            let rc = unsafe { self.backend.dfs_release(obj.0) };
            if rc != 0 && first_err.is_none() {
                first_err = Some(Error::Dfs {
                    op: "dfs_release",
                    errno: rc,
                    path: None,
                });
            }
        }
        // SAFETY: every object of this mount was released above; the dfs_t is
        // unmounted at most once (the exchange to unmounting has one winner,
        // and a failed unmount is never tried again).
        let rc = unsafe { self.backend.dfs_umount(self.raw.0) };
        if rc != 0 {
            // The dfs_t may still use the container handle: the lease and
            // the list entry stay.
            self.state
                .store(state::pack(state::FAILED, rc as u32), Ordering::SeqCst);
            #[cfg(all(test, feature = "mock"))]
            crate::gate::testing::run("AfterFailedStore", 0, &());
            return Err(crate::conn::umount_error(
                &format!("{}/{}", self.pool, self.container),
                rc,
            ));
        }
        self.state
            .store(state::pack(state::UNMOUNTED, 0), Ordering::SeqCst);
        #[cfg(all(test, feature = "mock"))]
        crate::gate::testing::run("AfterUnmountedStore", 0, &());
        let lease = self.cont.lock().unwrap_or_else(|e| e.into_inner()).take();
        let released = match lease {
            Some(lease) => {
                let removed = remove_listed(&lease, self);
                // Outside the list lock.
                drop(removed);
                // The only reference this call held.
                ContConn::release(lease)
            }
            None => Ok(()),
        };
        match first_err {
            Some(e) => Err(e),
            None => released,
        }
    }

    /// Close this mount, waiting for its calls until `deadline`.
    pub(crate) fn close_by(&self, deadline: Deadline) -> Unmount {
        // Before any lock: this may be a fork child.
        if let Err(e) = self.stamp.check_cold() {
            return Unmount::Refused(e);
        }
        self.close_checked(deadline)
    }

    /// [`Mount::close_by`] after its process check.
    fn close_checked(&self, deadline: Deadline) -> Unmount {
        let won = Cell::new(false);
        let r = self.gate.close_by(
            deadline,
            || None,
            |_| {
                // Before the gate reads closed: a container close that sees
                // the closed gate also sees the mount leave the mounted
                // state.
                won.set(self.begin_unmount());
                Check::Ready
            },
            |last, _| Final::Error(last.reason()),
            || {
                #[cfg(all(test, feature = "mock"))]
                crate::gate::testing::run("MountGateClosed", 0, &());
                won.get().then(|| (self.teardown(), self.take_dir_cache()))
            },
        );
        match r {
            Err(e) => Unmount::Busy(e),
            Ok(None) | Ok(Some(None)) => Unmount::Elsewhere,
            Ok(Some(Some((result, cached)))) => {
                #[cfg(all(test, feature = "mock"))]
                crate::gate::testing::run("BeforeCachedDrop", 0, &());
                // Dropped after the gate is released: their slots are gone
                // already, and their release enters no gate.
                drop(cached);
                Unmount::Done(result)
            }
        }
    }

    /// `Dfs::close`: waits for calls of the mount up to `timeout`.
    pub(crate) fn close(&self, timeout: Duration) -> Result<()> {
        let deadline = Deadline::after(timeout);
        #[cfg(all(test, feature = "mock"))]
        crate::gate::testing::note(crate::gate::testing::Note::Made("mount", deadline.id));
        match self.close_by(deadline) {
            Unmount::Refused(e) | Unmount::Busy(e) => Err(e),
            Unmount::Elsewhere => Ok(()),
            Unmount::Done(r) => r,
        }
    }

    /// For the container close: one try of the mount gate, no sleep. When
    /// the process check has passed and `hold` is given and empty, a
    /// [`CloseHold`] of this mount is put into it before the gate is tried.
    pub(crate) fn close_now(this: &Arc<Mount>, hold: Option<&mut Option<CloseHold>>) -> Unmount {
        #[cfg(all(test, feature = "mock"))]
        crate::fork::testing::fault_point("mount_close");
        if let Err(e) = this.stamp.check_cold() {
            return Unmount::Refused(e);
        }
        if let Some(h) = hold {
            if h.is_none() {
                *h = Some(CloseHold::new(this));
            }
        }
        let deadline = Deadline::passed();
        #[cfg(all(test, feature = "mock"))]
        crate::gate::testing::note(crate::gate::testing::Note::Handed(deadline.id));
        this.close_checked(deadline)
    }

    fn access(&self) -> Access {
        if self.read_only {
            Access::ReadOnly
        } else {
            Access::ReadWrite
        }
    }

    /// The container connection, or `Closed` after unmount.
    fn container_conn(&self) -> Result<Arc<ContConn>> {
        if !self.live()? {
            return Err(Error::Closed);
        }
        self.cont
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or(Error::Closed)
    }

    fn lookup_flags(&self) -> i32 {
        if self.read_only {
            libc::O_RDONLY
        } else {
            libc::O_RDWR
        }
    }

    /// Look up a directory with dfs_lookup (no cache).
    fn lookup_dir(&self, op: &Op<'_>, dir: &str) -> Result<Arc<DirHandle>> {
        let c = cstring(dir)?;
        let mut obj: *mut dfs_obj_t = std::ptr::null_mut();
        let mut mode: sys::mode_t = 0;
        // SAFETY: valid dfs_t under the gate, NUL-terminated absolute path,
        // out-pointers to locals; stbuf may be NULL.
        let rc = unsafe {
            op.backend().dfs_lookup(
                op.dfs(),
                c.as_ptr(),
                self.lookup_flags(),
                &mut obj,
                &mut mode,
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            return Err(Error::dfs("dfs_lookup", rc, dir));
        }
        let slot = self.track(ObjPtr(obj));
        let handle = Arc::new(DirHandle {
            mount: self.view.clone(),
            obj: ObjPtr(obj),
            slot,
            stamp: self.stamp,
        });
        if mode & sys::S_IFMT != sys::S_IFDIR {
            drop(handle);
            return Err(Error::dfs("dfs_lookup", libc::ENOTDIR, dir));
        }
        Ok(handle)
    }

    /// Directory handle for `dir` (normalised, not root): from the cache, or
    /// looked up and cached. Returns whether it came from the cache.
    fn dir_handle(&self, op: &Op<'_>, dir: &str) -> Result<(Arc<DirHandle>, bool)> {
        if self.dir_cache_capacity > 0 {
            if let Some(h) = self
                .dir_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(dir)
            {
                return Ok((h.clone(), true));
            }
        }
        let h = self.lookup_dir(op, dir)?;
        if self.dir_cache_capacity > 0 {
            // Every handle that leaves the cache is dropped after the cache
            // lock is released: its drop may release its object.
            let (evicted, replaced) = {
                let mut cache = self.dir_cache.lock().unwrap_or_else(|e| e.into_inner());
                let evicted = if cache.len() >= self.dir_cache_capacity {
                    std::mem::take(&mut *cache)
                } else {
                    HashMap::new()
                };
                let replaced = cache.insert(dir.to_string(), h.clone());
                (evicted, replaced)
            };
            drop(replaced);
            drop(evicted);
        }
        Ok((h, false))
    }

    /// One `dfs_ostat` on a cached directory handle: true when the entry it
    /// was looked up from still exists with the same object ID (libdfs
    /// compares the OID, `common.c` `entry_stat`, in 2.6 and 2.8) and is a
    /// directory. A directory removed (and maybe recreated) by another mount
    /// or process fails this check. Creating through such a handle would
    /// succeed in libdfs and leave an unreachable entry.
    fn handle_valid(&self, op: &Op<'_>, h: &DirHandle) -> bool {
        let mut st = sys::zeroed_stat();
        // SAFETY: live handle under the gate; out-pointer to a local.
        let rc = unsafe { op.backend().dfs_ostat(op.dfs(), h.obj.0, &mut st) };
        rc == 0 && st.st_mode & sys::S_IFMT == sys::S_IFDIR
    }

    /// Directory handle for `dir`. With `validate`, a handle from the cache is
    /// checked with [`Mount::handle_valid`] and replaced by a fresh lookup when
    /// stale. Returns whether the handle came from the cache unchecked.
    pub(crate) fn dir_handle_checked(
        &self,
        op: &Op<'_>,
        dir: &str,
        validate: bool,
    ) -> Result<(Arc<DirHandle>, bool)> {
        let (h, cached) = self.dir_handle(op, dir)?;
        if !(cached && validate) {
            return Ok((h, cached));
        }
        if self.handle_valid(op, &h) {
            return Ok((h, false));
        }
        drop(h);
        self.evict(dir);
        let (h, _) = self.dir_handle(op, dir)?;
        Ok((h, false))
    }

    /// Drop cache entries at or below `dir`.
    pub(crate) fn evict(&self, dir: &str) {
        let removed: Vec<Arc<DirHandle>> = {
            let mut cache = self.dir_cache.lock().unwrap_or_else(|e| e.into_inner());
            let keys: Vec<String> = cache
                .keys()
                .filter(|k| path::is_at_or_below(k, dir))
                .cloned()
                .collect();
            keys.into_iter().filter_map(|k| cache.remove(&k)).collect()
        };
        drop(removed);
    }

    /// Run `f(parent_obj, name)` for the normalised non-root `path`, with the
    /// parent directory handle from the cache.
    ///
    /// `create`: the call creates an entry (`dfs_open` with `O_CREAT`,
    /// `dfs_mkdir`). A cached parent is then validated first, because libdfs
    /// would create the entry in a removed directory without an error.
    ///
    /// Otherwise an `ENOENT` or `ESTALE` from a call that used an unchecked
    /// cached parent triggers the same validation: a valid parent means the
    /// error stands (an ordinary "not found"); a stale one is evicted and the
    /// call is retried once with a fresh lookup.
    pub(crate) fn with_parent<T>(
        &self,
        op: &Op<'_>,
        path: &str,
        create: bool,
        f: impl Fn(*mut dfs_obj_t, &CStr) -> Result<T>,
    ) -> Result<T> {
        let (parent, name) = path::split_parent(path)
            .ok_or(Error::InvalidArgument("the container root has no parent"))?;
        let cname = cstring(name)?;
        if parent == "/" {
            return f(std::ptr::null_mut(), &cname);
        }
        let (h, unchecked) = self.dir_handle_checked(op, parent, create)?;
        match f(h.obj.0, &cname) {
            Err(e)
                if unchecked
                    && matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ESTALE)) =>
            {
                if self.handle_valid(op, &h) {
                    return Err(e);
                }
                drop(h);
                self.evict(parent);
                let (h, _) = self.dir_handle(op, parent)?;
                f(h.obj.0, &cname)
            }
            r => r,
        }
    }
}

/// Take `mount` out of the list of `cont`, keeping the order of the others,
/// and raise the list generation in the same hold of the list lock. Returns
/// the entry, for the caller to drop after the lock is released.
pub(crate) fn remove_listed(cont: &ContConn, mount: &Mount) -> Option<Arc<Mount>> {
    let mut list = cont.mounts.lock().unwrap_or_else(|e| e.into_inner());
    let i = list.iter().position(|m| std::ptr::eq(&**m, mount));
    cont.mounts_gen.fetch_add(1, Ordering::SeqCst);
    i.map(|i| list.remove(i))
}

pub(crate) fn cstring(s: &str) -> Result<CString> {
    CString::new(s).map_err(|_| Error::InvalidPath {
        path: s.to_string(),
        reason: "contains a NUL byte",
    })
}

// ---------------------------------------------------------------------------
// Mounting

/// Attempts of `dfs_mount` while it reports `ENOENT`: container creation and
/// superblock creation are not atomic, so libdfs's own `dfs_connect` retries
/// (`src/client/dfs/mnt.c`, 7 attempts with a sleep of 10^b microseconds).
const MOUNT_ATTEMPTS: u32 = 7;

/// `dfs_mount` on a container connection, retried while it reports `ENOENT`.
fn mount_raw(cont: &Arc<ContConn>, access: Access) -> std::result::Result<*mut dfs_t, i32> {
    let mut rc = 0;
    for attempt in 0..MOUNT_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(Duration::from_micros(10u64.pow(attempt - 1)));
        }
        let mut raw: *mut dfs_t = std::ptr::null_mut();
        // SAFETY: live pool and container handles (the Arc holds both
        // leases); out-pointer to a local.
        rc = unsafe {
            cont.backend
                .dfs_mount(cont.poh, cont.coh, access.open_flags(), &mut raw)
        };
        if rc == 0 {
            return Ok(raw);
        }
        if rc != libc::ENOENT {
            break;
        }
    }
    Err(rc)
}

/// Options of [`Container::mount_dfs_with`](crate::Container::mount_dfs_with).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MountOptions {
    /// Number of cached directory handles; default 1024, 0 disables the
    /// cache (see [`ConnectOptions::dir_cache_capacity`]).
    pub dir_cache_capacity: usize,
}

impl Default for MountOptions {
    fn default() -> Self {
        MountOptions {
            dir_cache_capacity: DEFAULT_DIR_CACHE_CAPACITY,
        }
    }
}

impl MountOptions {
    /// Default options (1024 cached directory handles).
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the directory handle cache capacity (0 disables it).
    pub fn dir_cache_capacity(mut self, n: usize) -> Self {
        self.dir_cache_capacity = n;
        self
    }
}

/// The shared mount for the options' key: pool connection, container
/// connection, then `dfs_mount` (each shared through the registry).
///
/// Opens existing containers only. A container that cannot be mounted (not
/// a POSIX container) gives the `dfs_mount` error; the connections opened
/// for this call are closed again when their leases drop.
pub(crate) fn mount(
    registry: &Registry,
    backend: Arc<dyn Raw>,
    opts: &ConnectOptions,
) -> Result<Arc<MountRef>> {
    opts.validate()?;
    let sys = opts.sys_name.as_deref();
    let access = if opts.read_only {
        Access::ReadOnly
    } else {
        Access::ReadWrite
    };
    let sys_key = crate::conn::system_key(sys);
    // Check the system rule before any connection is opened.
    registry.acquire::<MountRef>(
        Key::Mount(opts.pool.clone(), opts.container.clone(), access),
        &sys_key,
        |r| {
            let key = (opts.pool.clone(), opts.container.clone(), access);
            r.mounts.get(&key).and_then(Weak::upgrade)
        },
        |m| m.live(),
        || {
            let attempt = || {
                let cont =
                    registry.container(&backend, &opts.pool, &opts.container, sys, access)?;
                let stamp = Stamp::fresh()?;
                new_mount(cont, opts.sys_name.clone(), opts.dir_cache_capacity, stamp)
            };
            match attempt() {
                // The shared container connection was closed by another
                // view while this call used it: connect again once.
                Err(Error::Closed) => attempt(),
                r => r,
            }
        },
        store_mount,
    )
}

/// The shared mount on an existing container connection.
pub(crate) fn mount_on(
    cont: &Arc<ContConn>,
    sys_name: Option<String>,
    options: &MountOptions,
) -> Result<Arc<MountRef>> {
    let registry = cont.backend.registry();
    let sys_key = crate::conn::system_key(sys_name.as_deref());
    let key = (cont.pool_name.clone(), cont.name.clone(), cont.access);
    registry.acquire::<MountRef>(
        Key::Mount(key.0.clone(), key.1.clone(), key.2),
        &sys_key,
        |r| r.mounts.get(&key).and_then(Weak::upgrade),
        |m| {
            if !m.live()? {
                return Ok(false);
            }
            // A live mount of another connection under the same key: this
            // connection is closing or closed, since a usable one would be
            // the registry's entry.
            if !std::ptr::eq(m.made_on.as_ptr(), Arc::as_ptr(cont)) {
                return Err(Error::Closed);
            }
            Ok(true)
        },
        || {
            if !cont.usable()? {
                return Err(Error::Closed);
            }
            let stamp = Stamp::fresh()?;
            new_mount(
                cont.clone(),
                sys_name.clone(),
                options.dir_cache_capacity,
                stamp,
            )
        },
        store_mount,
    )
}

fn store_mount(r: &mut crate::conn::RegistryInner, m: &Arc<MountRef>) {
    r.mounts.retain(|_, w| w.strong_count() > 0);
    r.mounts.insert(
        (m.pool.clone(), m.container.clone(), m.access()),
        Arc::downgrade(m),
    );
}

/// A new mount on `cont`, listed on the connection before the container
/// pass is released, so a container close that waits for that pass finds
/// it.
fn new_mount(
    cont: Arc<ContConn>,
    sys_name: Option<String>,
    dir_cache_capacity: usize,
    stamp: Stamp,
) -> Result<Arc<MountRef>> {
    let access = cont.access;
    let pass = match cont.enter() {
        // A container close is waiting: wait for its outcome. After a
        // `Busy` the container is open again; after a close this is
        // `Closed` again.
        Err(Error::Closed) if !cont.gate.is_closed() => {
            cont.gate.wait_closers();
            cont.enter()?
        }
        r => r?,
    };
    // A connection that can never be closed gets no new mount. Checked once:
    // the retries of `dfs_mount` belong to this one attempt.
    if let Some(e) = cont.standing_error() {
        return Err(e);
    }
    let raw = mount_raw(&cont, access).map_err(|rc| Error::dfs("dfs_mount", rc, cont.target()))?;
    let m = Arc::new_cyclic(|view| MountRef {
        core: Arc::new(Mount {
            backend: cont.backend.clone(),
            raw: DfsPtr(raw),
            pool: cont.pool_name.clone(),
            container: cont.name.clone(),
            sys_name,
            read_only: access == Access::ReadOnly,
            stamp,
            gate: Gate::new(),
            handles: Mutex::new(HashMap::new()),
            next_slot: AtomicU64::new(1),
            dir_cache: Mutex::new(HashMap::new()),
            dir_cache_capacity,
            state: AtomicU64::new(state::pack(state::MOUNTED, 0)),
            view: view.clone(),
            cont: Mutex::new(Some(cont.clone())),
            made_on: Arc::downgrade(&cont),
        }),
    });
    {
        let mut list = cont.mounts.lock().unwrap_or_else(|e| e.into_inner());
        list.push(Arc::clone(&m.core));
        cont.mounts_gen.fetch_add(1, Ordering::SeqCst);
    }
    // Released only now: a container close waits for this pass, so it
    // finds the mount in its list.
    drop(pass);
    Ok(m)
}

// ---------------------------------------------------------------------------
// Dfs

/// A connected DFS mount. Cheap to clone; all clones share one `dfs_t`.
///
/// `Dfs` is `Send + Sync`; all calls are blocking. From async code, call it
/// inside `spawn_blocking`.
///
/// ```no_run
/// # fn main() -> Result<(), daos::Error> {
/// unsafe { daos::init(&daos::InitOptions::default())? };
/// let dfs = daos::Dfs::connect("tank", "data")?;
/// dfs.write("/dir/hello.txt", b"hello")?;
/// let mut buf = [0u8; 5];
/// let file = dfs.open("/dir/hello.txt")?;
/// file.read_exact_at(&mut buf, 0)?;
/// for entry in dfs.read_dir("/dir")? {
///     println!("{}", entry?.path());
/// }
/// dfs.remove_all("/dir")?;
/// dfs.close(std::time::Duration::from_secs(10))?;
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct Dfs {
    pub(crate) mount: Arc<MountRef>,
}

impl fmt::Debug for Dfs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dfs")
            .field("pool", &self.mount.pool)
            .field("container", &self.mount.container)
            .field("sys_name", &self.mount.sys_name)
            .field("read_only", &self.mount.read_only)
            .finish()
    }
}

/// Mode bits of new directories.
const DIR_MODE: u32 = 0o755;

impl Dfs {
    /// Connect read-write to the default system.
    ///
    /// Requires a completed [`init`](crate::init); otherwise
    /// [`Error::NotInitialized`].
    pub fn connect(pool: &str, container: &str) -> Result<Dfs> {
        Self::connect_with(&ConnectOptions::new(pool, container))
    }

    /// Connect with options.
    ///
    /// Connects to the pool, opens the existing container and mounts it with
    /// `dfs_mount`. Pool and container connections are shared through a
    /// process-wide registry keyed by the strings given and the access mode;
    /// a second connect with the same pool, container and access mode
    /// returns the live mount (with that mount's settings). A read-only and
    /// a read-write mount of the same container use separate pool and
    /// container handles.
    ///
    /// The first successful connect of the process fixes the system name
    /// (`None` and `"daos_server"`, the DAOS default, are the same); another
    /// system returns [`Error::Conflict`].
    ///
    /// The container must exist and be a POSIX container. Errors:
    /// [`Error::Daos`] with `op` `"daos_pool_connect"` or
    /// `"daos_cont_open"` (for example `DER_NONEXIST`, for which
    /// [`Error::is_not_found`] is true), or [`Error::Dfs`] with `op`
    /// `"dfs_mount"`.
    ///
    /// A live mount keeps referring to the container it opened, even when
    /// another process renames it or destroys it and creates a new one under
    /// the same label.
    ///
    /// The live mount of the key is returned if there is one. Else, when the
    /// container connection is kept open for good (see
    /// [`Container::close`](crate::Container::close)), the error that keeps
    /// it open is returned and no mount is made. That check is made once,
    /// before `dfs_mount`; its retries on `ENOENT` are part of the same
    /// attempt.
    pub fn connect_with(options: &ConnectOptions) -> Result<Dfs> {
        let rt = crate::runtime::runtime()?;
        let mount = mount(rt.backend.registry(), rt.backend.clone(), options)?;
        Ok(Dfs { mount })
    }

    /// Connect to an in-memory backend (feature `mock`). Uses pool `"mock"`
    /// and container `"mock"`. Needs no [`init`](crate::init).
    #[cfg(feature = "mock")]
    pub fn connect_mock(backend: Arc<crate::mock::MemDfs>) -> Result<Dfs> {
        Self::connect_mock_with(backend, &ConnectOptions::new("mock", "mock"))
    }

    /// Connect to an in-memory backend with options (feature `mock`). The
    /// registry rules apply per backend.
    #[cfg(feature = "mock")]
    pub fn connect_mock_with(
        backend: Arc<crate::mock::MemDfs>,
        options: &ConnectOptions,
    ) -> Result<Dfs> {
        backend.stamp().check_cold()?;
        backend.ensure_init()?;
        let raw: Arc<dyn Raw> = backend.clone();
        let mount = mount(backend.registry(), raw, options)?;
        Ok(Dfs { mount })
    }

    /// Pool label or UUID as given.
    pub fn pool(&self) -> &str {
        &self.mount.pool
    }
    /// Container label or UUID as given.
    pub fn container(&self) -> &str {
        &self.mount.container
    }
    /// System name as given.
    pub fn sys_name(&self) -> Option<&str> {
        self.mount.sys_name.as_deref()
    }
    /// True for a read-only mount.
    pub fn is_read_only(&self) -> bool {
        self.mount.read_only
    }

    /// The container connection this mount uses: the same handle, as a new
    /// [`Container`](crate::Container) view. [`Error::Closed`] after
    /// [`Dfs::close`].
    ///
    /// Objects created through it in a POSIX container are invisible to DFS
    /// (and may be reported or removed by a DFS consistency check); see
    /// [`Container`](crate::Container).
    pub fn container_handle(&self) -> Result<crate::Container> {
        Ok(crate::Container::from_conn(self.mount.container_conn()?))
    }

    /// The pool connection this mount uses, as a new
    /// [`Pool`](crate::Pool) view. [`Error::Closed`] after [`Dfs::close`].
    pub fn pool_handle(&self) -> Result<crate::Pool> {
        Ok(crate::Pool::from_conn(self.mount.container_conn()?.pool()?))
    }

    // -- files -------------------------------------------------------------

    /// Open an existing file read-only.
    pub fn open(&self, path: &str) -> Result<File> {
        self.open_with(path, OpenOptions::new().read(true))
    }

    /// Create or truncate a file, opened read-write, mode 0644. The parent
    /// directory must exist.
    pub fn create(&self, path: &str) -> Result<File> {
        self.open_with(
            path,
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true),
        )
    }

    /// Open with explicit options.
    pub fn open_with(&self, path: &str, options: &OpenOptions) -> Result<File> {
        self.mount.stamp.check()?;
        File::open(&self.mount, path, options)
    }

    /// Streaming writer: create or truncate, parents created.
    pub fn writer(&self, path: &str) -> Result<Writer> {
        let f = self.open_with(
            path,
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .create_parents(true),
        )?;
        Ok(Writer::new(f))
    }

    // -- whole-object helpers ------------------------------------------------

    /// Read a whole file. A file that shrinks during the read gives
    /// [`Error::ShortRead`].
    pub fn read(&self, path: &str) -> Result<Vec<u8>> {
        let f = self.open(path)?;
        let size = f.size()?;
        let len = usize::try_from(size)
            .map_err(|_| Error::InvalidArgument("file larger than the address space"))?;
        let mut buf = vec![0u8; len];
        f.read_exact_at(&mut buf, 0)?;
        f.close()?;
        Ok(buf)
    }

    /// Read up to `buf.len()` bytes at `offset` into `buf`; returns the count.
    pub fn read_range_into(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let f = self.open(path)?;
        let n = f.read_at(buf, offset)?;
        f.close()?;
        Ok(n)
    }

    /// Create or truncate `path` (parents created) and write `data`.
    pub fn write(&self, path: &str, data: &[u8]) -> Result<()> {
        let f = self.open_with(
            path,
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .create_parents(true),
        )?;
        f.write_all_at(data, 0)?;
        f.close()
    }

    // -- metadata ------------------------------------------------------------

    /// Metadata of `path` (`dfs_stat`).
    pub fn metadata(&self, path: &str) -> Result<Metadata> {
        self.mount.stamp.check()?;
        let p = path::normalize(path)?;
        let op = self.mount.enter()?;
        let mut st = sys::zeroed_stat();
        let stp: *mut sys::stat_t = &mut st;
        if p == "/" {
            // SAFETY: NULL parent and NULL name address the container root.
            let rc = unsafe {
                op.backend()
                    .dfs_stat(op.dfs(), std::ptr::null_mut(), std::ptr::null(), stp)
            };
            if rc != 0 {
                return Err(Error::dfs("dfs_stat", rc, p));
            }
        } else {
            self.mount.with_parent(&op, &p, false, |parent, name| {
                // SAFETY: parent is NULL (root) or a live handle under the
                // gate; name is NUL-terminated; stp points to a local.
                let rc = unsafe { op.backend().dfs_stat(op.dfs(), parent, name.as_ptr(), stp) };
                if rc != 0 {
                    return Err(Error::dfs("dfs_stat", rc, p.as_str()));
                }
                Ok(())
            })?;
        }
        Ok(Metadata::from_stat(&st))
    }

    /// Metadata, or `None` when the path does not exist (`ENOENT`, `ENOTDIR`).
    pub fn try_metadata(&self, path: &str) -> Result<Option<Metadata>> {
        match self.metadata(path) {
            Ok(m) => Ok(Some(m)),
            Err(e)
                if matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR))
                    && is_dfs(&e) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// True when `path` exists.
    pub fn exists(&self, path: &str) -> Result<bool> {
        Ok(self.try_metadata(path)?.is_some())
    }

    // -- directories -----------------------------------------------------------

    /// Create a directory (mode 0755). The parent must exist; an existing
    /// entry is `EEXIST`.
    pub fn create_dir(&self, path: &str) -> Result<()> {
        self.mount.stamp.check()?;
        let p = path::normalize(path)?;
        if p == "/" {
            return Err(Error::dfs("dfs_mkdir", libc::EEXIST, p));
        }
        let op = self.mount.enter()?;
        self.mkdir_one(&op, &p)
    }

    fn mkdir_one(&self, op: &Op<'_>, p: &str) -> Result<()> {
        self.mount.with_parent(op, p, true, |parent, name| {
            // SAFETY: parent NULL or live under the gate; NUL-terminated name.
            let rc = unsafe {
                op.backend().dfs_mkdir(
                    op.dfs(),
                    parent,
                    name.as_ptr(),
                    sys::S_IFDIR | DIR_MODE,
                    sys::OC_UNKNOWN,
                )
            };
            if rc != 0 {
                return Err(Error::dfs("dfs_mkdir", rc, p));
            }
            Ok(())
        })
    }

    /// Create a directory and all missing parents. An existing directory is
    /// success; an existing non-directory is `EEXIST`. Tolerates concurrent
    /// creators.
    pub fn create_dir_all(&self, path: &str) -> Result<()> {
        self.mount.stamp.check()?;
        let p = path::normalize(path)?;
        if p == "/" {
            return Ok(());
        }
        let op = self.mount.enter()?;
        self.mkdir_all(&op, &p)
    }

    pub(crate) fn mkdir_all(&self, op: &Op<'_>, p: &str) -> Result<()> {
        // Walk up until a mkdir does not fail with ENOENT, then come back down.
        let mut pending: Vec<&str> = Vec::new();
        let mut cur = p;
        loop {
            match self.mkdir_one(op, cur) {
                Ok(()) => break,
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {
                    self.require_dir(op, cur, e)?;
                    break;
                }
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
                    let (parent, _) = path::split_parent(cur).expect("non-root path");
                    if parent == "/" {
                        return Err(e);
                    }
                    pending.push(cur);
                    cur = parent;
                }
                Err(e) => return Err(e),
            }
        }
        while let Some(dir) = pending.pop() {
            match self.mkdir_one(op, dir) {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => self.require_dir(op, dir, e)?,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn require_dir(&self, op: &Op<'_>, p: &str, exists_err: Error) -> Result<()> {
        let mut st = sys::zeroed_stat();
        let stp: *mut sys::stat_t = &mut st;
        self.mount.with_parent(op, p, false, |parent, name| {
            // SAFETY: as in metadata().
            let rc = unsafe { op.backend().dfs_stat(op.dfs(), parent, name.as_ptr(), stp) };
            if rc != 0 {
                return Err(Error::dfs("dfs_stat", rc, p));
            }
            Ok(())
        })?;
        if st.st_mode & sys::S_IFMT == sys::S_IFDIR {
            Ok(())
        } else {
            Err(exists_err)
        }
    }

    /// Iterate over the names in a directory.
    pub fn read_dir(&self, path: &str) -> Result<ReadDir> {
        self.mount.stamp.check()?;
        let p = path::normalize(path)?;
        let op = self.mount.enter()?;
        // A cached handle of a directory removed (and maybe recreated)
        // elsewhere would list the old, removed object: validate it.
        let (h, _) = self.mount.dir_handle_checked(&op, &p, p != "/")?;
        drop(op);
        Ok(ReadDir::new(self.mount.clone(), h, p))
    }

    // -- removal -----------------------------------------------------------------

    /// Remove a file. `EISDIR` for a directory.
    pub fn remove_file(&self, path: &str) -> Result<()> {
        self.remove_impl(path, Expect::File, false)
    }

    /// Remove an empty directory. `ENOTDIR` for a file, `ENOTEMPTY` when not empty.
    pub fn remove_dir(&self, path: &str) -> Result<()> {
        self.remove_impl(path, Expect::Dir, false)
    }

    /// Remove a directory and everything below it (`dfs_remove` with force).
    /// `ENOTDIR` for a file.
    pub fn remove_dir_all(&self, path: &str) -> Result<()> {
        self.remove_impl(path, Expect::Dir, true)
    }

    /// Remove a file, or a directory with everything below it (`dfs_remove`
    /// with force). Refuses the container root with `EBUSY`.
    pub fn remove_all(&self, path: &str) -> Result<()> {
        self.remove_impl(path, Expect::Any, true)
    }

    fn remove_impl(&self, path: &str, expect: Expect, force: bool) -> Result<()> {
        self.mount.stamp.check()?;
        let p = path::normalize(path)?;
        if expect == Expect::File {
            path::reject_dir_syntax(path, &p)?;
        }
        if p == "/" {
            return Err(Error::dfs("dfs_remove", libc::EBUSY, p));
        }
        let op = self.mount.enter()?;
        if expect != Expect::Any {
            let mut st = sys::zeroed_stat();
            let stp: *mut sys::stat_t = &mut st;
            self.mount.with_parent(&op, &p, false, |parent, name| {
                // SAFETY: as in metadata().
                let rc = unsafe { op.backend().dfs_stat(op.dfs(), parent, name.as_ptr(), stp) };
                if rc != 0 {
                    return Err(Error::dfs("dfs_stat", rc, p.as_str()));
                }
                Ok(())
            })?;
            let is_dir = st.st_mode & sys::S_IFMT == sys::S_IFDIR;
            match expect {
                Expect::File if is_dir => return Err(Error::dfs("dfs_remove", libc::EISDIR, p)),
                Expect::Dir if !is_dir => return Err(Error::dfs("dfs_remove", libc::ENOTDIR, p)),
                _ => {}
            }
        }
        self.mount.evict(&p);
        let r = self.mount.with_parent(&op, &p, false, |parent, name| {
            // SAFETY: parent NULL or live under the gate; NUL-terminated
            // name; oid is NULL (not wanted).
            let rc = unsafe {
                op.backend().dfs_remove(
                    op.dfs(),
                    parent,
                    name.as_ptr(),
                    force,
                    std::ptr::null_mut(),
                )
            };
            if rc != 0 {
                return Err(Error::dfs("dfs_remove", rc, p.as_str()));
            }
            Ok(())
        });
        // Another thread of this mount may have cached a handle below `p`
        // between the first eviction and the removal.
        if r.is_ok() {
            self.mount.evict(&p);
        }
        r
    }

    // -- lifetime ------------------------------------------------------------------

    /// Stop accepting operations (new ones get [`Error::Closed`] from the
    /// moment `close` starts), wait up to `timeout` for running ones, release
    /// every open handle of this mount (also those held by [`File`],
    /// [`Writer`] and [`ReadDir`] values), `dfs_umount`, and give up the
    /// mount's container connection: when nothing else uses it, the
    /// container is closed and then the pool disconnected. Idempotent: this
    /// returns `Ok` when the mount is closed already, or when its unmount was
    /// started elsewhere by the time this call takes the mount gate.
    ///
    /// Affects every clone of this `Dfs`. Later operations on old handles
    /// return [`Error::Closed`] (a [`ReadDir`] may first return names it had
    /// already read); a later [`Dfs::connect`] creates a new mount. DAOS
    /// itself stays initialised.
    ///
    /// [`Error::Busy`] when operations still run after `timeout`, or when
    /// another close of the mount (also a container close) still holds the
    /// mount gate at the deadline, while it waits or while it unmounts; this
    /// call then released and unmounted nothing. When that other close was
    /// unmounting, the mount is closed already and this call still returns
    /// `Busy`. Otherwise this call leaves the mount connected; new calls are
    /// accepted again only if the mount is still open after the last
    /// waiting close of it has returned.
    ///
    /// A failed `dfs_umount` is never tried again: the mount is then closed
    /// for its users and its files are released, but the container
    /// connection stays open for the rest of the process and makes no new
    /// mount (see [`Container::close`](crate::Container::close)).
    ///
    /// On an inherited handle `close` returns [`Error::Forked`] before any
    /// DAOS call when the process check at entry fails. It also returns that
    /// error when, during cleanup, the mount held the last reference of its
    /// container connection (or that one the last reference of its pool
    /// connection) and this process cannot be confirmed as the one that
    /// opened it: the native release of that handle is skipped, and the
    /// unmount or container close already done is not undone. A failed
    /// `dfs_umount` is returned before any other error (the container is then
    /// left open). After a successful unmount, when several cleanup steps
    /// fail, the error returned is the first of: a `dfs_release` error, then
    /// the error of the container release (this one or a `daos_cont_close`
    /// error), then that of the pool release.
    pub fn close(&self, timeout: Duration) -> Result<()> {
        self.mount.close(timeout)
    }

    /// True after a successful [`Dfs::close`], and always in a fork child.
    pub fn is_closed(&self) -> bool {
        self.mount.is_closed()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Expect {
    File,
    Dir,
    Any,
}

fn is_dfs(e: &Error) -> bool {
    matches!(e, Error::Dfs { .. })
}

// Send/Sync decisions (crate docs, "Threads").
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<Dfs>();
    send_sync::<File>();
    send_sync::<Writer>();
    send_sync::<ReadDir>();
};

#[cfg(all(test, feature = "mock"))]
mod tests {
    use super::*;
    use crate::fork::testing::{
        count_raw_pid_reads, force_plain, rerun_alone, with_raw_pid_after, with_raw_pid_once_after,
    };
    use crate::mock::MemDfs;

    /// Plain mode first, then the backend, then its init, then a container
    /// connection of the registry.
    fn plain_container() -> (Arc<MemDfs>, Arc<dyn Raw>, Arc<ContConn>) {
        force_plain();
        let mem = Arc::new(MemDfs::new());
        mem.ensure_init().unwrap();
        let backend: Arc<dyn Raw> = mem.clone();
        let registry = backend.registry();
        let p = registry
            .pool(&backend, "p", None, Access::ReadWrite)
            .unwrap();
        let c = registry.container_on(&p, "c").unwrap();
        (mem, backend, c)
    }

    fn is_forked<T>(r: &Result<T>) -> bool {
        matches!(r, Err(Error::Forked { .. }))
    }

    /// A lookup that filters with a boolean mounts a second time.
    #[test]
    fn failed_pid_read_keeps_the_cached_mount() {
        if rerun_alone("dfs::tests::failed_pid_read_keeps_the_cached_mount") {
            return;
        }
        let (mem, backend, c) = plain_container();
        let options = MountOptions::default();
        let m1 = mount_on(&c, None, &options).unwrap();
        assert_eq!(mem.calls("dfs_mount"), 1);

        let call = || mount_on(&c, None, &options);
        let mut hit = None;
        let n = count_raw_pid_reads(|| hit = Some(call()));
        assert!(n >= 1);
        assert!(Arc::ptr_eq(&hit.unwrap().unwrap(), &m1));
        let r = with_raw_pid_once_after(n - 1, -1, call);
        assert!(is_forked(&r), "{:?}", r.err());
        assert_eq!(mem.calls("dfs_mount"), 1);
        assert!(Arc::ptr_eq(&call().unwrap(), &m1));
        assert_eq!(mem.calls("dfs_mount"), 1);

        let opts = ConnectOptions::new("p", "c");
        let call = || mount(backend.registry(), backend.clone(), &opts);
        let mut hit = None;
        let n = count_raw_pid_reads(|| hit = Some(call()));
        assert!(n >= 1);
        assert!(Arc::ptr_eq(&hit.unwrap().unwrap(), &m1));
        let r = with_raw_pid_once_after(n - 1, -1, call);
        assert!(is_forked(&r), "{:?}", r.err());
        assert_eq!(mem.calls("dfs_mount"), 1);
        assert!(Arc::ptr_eq(&call().unwrap(), &m1));
        assert_eq!(mem.calls("dfs_mount"), 1);
    }

    /// A `live()` that asks the stamp twice takes a failed second read for
    /// "closed" and goes on to mount again.
    #[test]
    fn cached_mount_is_found_with_one_pid_read() {
        if rerun_alone("dfs::tests::cached_mount_is_found_with_one_pid_read") {
            return;
        }
        let (mem, _, c) = plain_container();
        let options = MountOptions::default();
        let m1 = mount_on(&c, None, &options).unwrap();
        let mut r = None;
        let n = count_raw_pid_reads(|| {
            // One healthy read, every later read fails.
            r = Some(with_raw_pid_after(1, -1, || mount_on(&c, None, &options)));
        });
        assert!(Arc::ptr_eq(&r.unwrap().unwrap(), &m1));
        assert_eq!(mem.calls("dfs_mount"), 1);
        assert_eq!(n, 1);
    }
}

/// Unmounts, directory handles and new mounts on a connection.
#[cfg(all(test, feature = "mock"))]
mod mount_tests {
    use super::*;
    use crate::conn::support::*;
    use crate::cont::{Container, ContainerOptions};
    use crate::fork::testing::{rerun_alone, with_pid_fault_at};
    use crate::gate::testing;
    use crate::mock::MemDfs;
    use crate::tx::TxMode;
    use std::cell::RefCell;

    const T1: Duration = Duration::from_secs(1);

    #[test]
    fn directory_release_holds_off_the_unmount() {
        testing::clear();
        let mem = Arc::new(MemDfs::new());
        let options = ConnectOptions::new("mock", "mock").dir_cache_capacity(0);
        let dfs = Dfs::connect_mock_with(mem.clone(), &options).unwrap();
        dfs.create_dir("/d").unwrap();
        let rd = dfs.read_dir("/d").unwrap();
        let pause = mem.pause_next("dfs_release");
        let a = std::thread::spawn(move || drop(rd));
        assert!(pause.wait_entered(LIMIT));
        std::thread::spawn(move || drop(dfs)).join().unwrap();
        assert_eq!(mem.calls("dfs_umount"), 0);
        assert_eq!(mem.calls("daos_cont_close"), 0);
        pause.release();
        a.join().unwrap();
        assert_eq!(mem.calls("dfs_umount"), 1);
        assert_eq!(mem.calls("daos_cont_close"), 1);
        assert_eq!(mem.open_objects(), 0);
    }

    #[test]
    fn replaced_cache_entry_is_dropped_outside_the_cache_lock() {
        testing::clear();
        let mem = Arc::new(MemDfs::new());
        let dfs = Dfs::connect_mock(mem.clone()).unwrap();
        dfs.create_dir("/z").unwrap();
        dfs.create_dir("/d").unwrap();
        // Caches /z.
        assert!(!dfs.exists("/z/x").unwrap());
        let first = mem.pause_also("dfs_lookup");
        let second = mem.pause_also("dfs_lookup");
        let look = |dfs: &Dfs| {
            let dfs = dfs.clone();
            std::thread::spawn(move || dfs.exists("/d/x").unwrap())
        };
        let t1 = look(&dfs);
        let t2 = look(&dfs);
        assert!(first.wait_entered(LIMIT));
        assert!(second.wait_entered(LIMIT));
        // Both missed the cache. The first inserts and its caller drops
        // its handle.
        let (inserter, replacer, t_ins, t_rep) = if first.thread() == Some(t1.thread().id()) {
            (first, second, t1, t2)
        } else {
            (first, second, t2, t1)
        };
        inserter.release();
        assert!(!t_ins.join().unwrap());
        // The second replaces it; the release of the replaced handle stops
        // inside dfs_release.
        let release = mem.pause_next("dfs_release");
        replacer.release();
        assert!(release.wait_entered(LIMIT));
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let d = dfs.clone();
        let other = std::thread::spawn(move || done_tx.send(d.exists("/z/x").unwrap()).unwrap());
        let found = done_rx
            .recv_timeout(LIMIT)
            .expect("the cache lock was held");
        assert!(!found);
        other.join().unwrap();
        release.release();
        assert!(!t_rep.join().unwrap());
    }

    #[test]
    fn winner_alone_removes_its_list_entry() {
        if rerun_alone("dfs::mount_tests::winner_alone_removes_its_list_entry") {
            return;
        }
        // The container close of a mount whose last user is gone.
        let f = fixture_plain();
        let dfs = f.mount();
        let m = core(&dfs);
        drop(dfs);
        f.cont.close(T1).unwrap();
        assert!(listed(f.conn()).is_empty());
        assert_eq!(Arc::strong_count(&m), 1);

        // The winner of a close is stopped after it stored the end.
        let f = fixture();
        let dfs = f.mount();
        let m = core(&dfs);
        let (at_tx, at_rx) = signal();
        let (go_tx, go_rx) = signal();
        let winner = std::thread::spawn(move || {
            testing::set("AfterUnmountedStore", None, move |_| {
                at_tx.send(()).unwrap();
                wait(&go_rx);
            });
            dfs.close(T1)
        });
        wait(&at_rx);
        let cont = f.cont.clone();
        std::thread::spawn(move || cont.close(T1))
            .join()
            .unwrap()
            .unwrap();
        assert_eq!(f.calls("daos_cont_close"), 1);
        let still = listed(f.conn());
        assert_eq!(still.len(), 1);
        assert!(Arc::ptr_eq(&still[0], &m));
        drop(still);
        go_tx.send(()).unwrap();
        winner.join().unwrap().unwrap();
        assert!(listed(f.conn()).is_empty());

        // The mount holds the last container reference and the release
        // finds the process unconfirmed.
        let mem = Arc::new(MemDfs::new());
        let dfs = Dfs::connect_mock(mem.clone()).unwrap();
        let r = with_pid_fault_at("cont_release", || dfs.close(T1));
        assert!(matches!(r, Err(Error::Forked { .. })), "{r:?}");

        // The removal keeps the order of the others.
        let f = fixture();
        let mut cores = Vec::new();
        for _ in 0..4 {
            let d = f.mount();
            cores.push(core(&d));
            d.close(T1).unwrap();
        }
        *f.conn().mounts.lock().unwrap() = cores.clone();
        let gen = f.conn().mounts_gen.load(Ordering::SeqCst);
        let removed = remove_listed(f.conn(), &cores[1]).unwrap();
        assert!(Arc::ptr_eq(&removed, &cores[1]));
        let left = listed(f.conn());
        let expected = [&cores[0], &cores[2], &cores[3]];
        assert_eq!(left.len(), 3);
        for (l, e) in left.iter().zip(expected) {
            assert!(Arc::ptr_eq(l, e));
        }
        assert_eq!(f.conn().mounts_gen.load(Ordering::SeqCst), gen + 1);
        f.conn().mounts.lock().unwrap().clear();
    }

    #[test]
    fn standing_connection_reuses_a_live_mount_and_makes_no_new_one() {
        let shared = |f: &Fixture, b: &Dfs| {
            let mounts = f.calls("dfs_mount");
            let x = f.cont.mount_dfs().unwrap();
            let y = Dfs::connect_mock(f.mem.clone()).unwrap();
            assert!(Arc::ptr_eq(&x.mount, &b.mount));
            assert!(Arc::ptr_eq(&y.mount, &b.mount));
            assert_eq!(f.calls("dfs_mount"), mounts);
        };
        let refused = |f: &Fixture, check: &dyn Fn(&Result<Dfs>)| {
            let mounts = f.calls("dfs_mount");
            check(&f.cont.mount_dfs());
            check(&Dfs::connect_mock(f.mem.clone()));
            assert_eq!(f.calls("dfs_mount"), mounts);
        };
        let eio = |r: &Result<Dfs>| assert_eq!(umount_errno(r), libc::EIO);
        let tx_fault = |r: &Result<Dfs>| match r {
            Err(Error::Daos {
                op: "daos_tx_close",
                ..
            }) => {}
            other => panic!("{other:?}"),
        };
        let fail_tx = |f: &Fixture| {
            let tx = f.cont.tx(TxMode::ReadWrite).unwrap();
            f.mem.fail_next("daos_tx_close", -1012);
            assert!(tx.close().is_err());
        };

        // A failed mount and a live one.
        let f = fixture();
        let parked = f.park_failing_drop(f.mount(), libc::EIO);
        let b = f.mount();
        parked.finish();
        shared(&f, &b);
        drop(b);
        refused(&f, &eio);

        // A transaction fault and a live mount.
        let f = fixture();
        let b = f.mount();
        fail_tx(&f);
        shared(&f, &b);
        drop(b);
        refused(&f, &tx_fault);

        // A transaction fault only.
        let f = fixture();
        fail_tx(&f);
        refused(&f, &tx_fault);

        // A mount admitted before the error appeared retries on ENOENT.
        let f = fixture();
        let parked = f.park_failing_drop(f.mount(), libc::EIO);
        let mounts = f.calls("dfs_mount");
        let pause = f.mem.pause_failing("dfs_mount", libc::ENOENT);
        let cont = f.cont.clone();
        let t = std::thread::spawn(move || cont.mount_dfs());
        assert!(pause.wait_entered(LIMIT));
        parked.finish();
        pause.release();
        let made = t.join().unwrap().unwrap();
        assert_eq!(f.calls("dfs_mount"), mounts + 2);
        drop(made);

        // A mount read as being unmounted fails, and then a transaction
        // close fails: the unmount error came first at every instant.
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
            assert!(tx.close().is_err());
            closed_tx.send(()).unwrap();
        });
        wait(&open_rx);
        let mem = f.mem.clone();
        testing::set("AfterStateRead", Some(0), move |_| {
            parked.borrow_mut().take().unwrap().finish();
            mem.fail_next("daos_tx_close", -1012);
            close_tx.send(()).unwrap();
            wait(&closed_rx);
        });
        let mounts = f.calls("dfs_mount");
        let r = f.cont.mount_dfs();
        testing::clear();
        assert_eq!(umount_errno(&r), libc::EIO);
        assert_eq!(f.calls("dfs_mount"), mounts);
        worker.join().unwrap();
        refused(&f, &eio);
    }

    #[test]
    fn closing_connection_gets_no_mount_of_a_newer_one() {
        let f = fixture();
        let pause = f.mem.pause_next("daos_cont_close");
        let a = f.cont.clone();
        let closer = std::thread::spawn(move || a.close(T1));
        assert!(pause.wait_entered(LIMIT));
        // The pool lease of the closing connection is still held.
        assert!(f.conn().pool().is_ok());
        let options = ContainerOptions::new("mock", "mock");
        let b = Container::open_mock(f.mem.clone(), &options).unwrap();
        assert!(!Arc::ptr_eq(&b.conn, f.conn()));
        let newer = b.mount_dfs().unwrap();
        let mounts = f.calls("dfs_mount");
        let r = f.cont.mount_dfs();
        assert!(matches!(r, Err(Error::Closed)), "{r:?}");
        assert_eq!(f.calls("dfs_mount"), mounts);
        pause.release();
        closer.join().unwrap().unwrap();
        // The newer connection keeps its mount.
        assert!(Arc::ptr_eq(&b.mount_dfs().unwrap().mount, &newer.mount));
    }

    #[test]
    fn cached_directory_dropped_by_a_container_close_enters_no_gate() {
        let f = fixture();
        let b = f.mount();
        b.create_dir("/d").unwrap();
        // Caches /d; the cache holds the only reference of its handle.
        assert!(!b.exists("/d/x").unwrap());
        let m = core(&b);
        // Another close holds the closed mount gate exclusively while the
        // container close drops the cached handles.
        let (take_tx, take_rx) = signal();
        let (held_tx, held_rx) = signal();
        let (free_tx, free_rx) = signal();
        let holder = std::thread::spawn(move || {
            wait(&take_rx);
            let guard = m.gate().hold();
            held_tx.send(()).unwrap();
            free_rx.recv().unwrap();
            drop(guard);
        });
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let cont = f.cont.clone();
        let closer = std::thread::spawn(move || {
            testing::set("BeforeCachedDrop", None, move |_| {
                take_tx.send(()).unwrap();
                wait(&held_rx);
            });
            done_tx.send(cont.close(T1)).unwrap();
        });
        let r = done_rx.recv_timeout(LIMIT);
        let _ = free_tx.send(());
        holder.join().unwrap();
        closer.join().unwrap();
        r.expect("the cached handle waited for the mount gate")
            .unwrap();
        assert_eq!(f.calls("dfs_umount"), 1);
        assert_eq!(f.calls("daos_cont_close"), 1);
        assert_eq!(f.mem.open_objects(), 0);
        assert!(matches!(b.metadata("/d"), Err(Error::Closed)));
    }

    /// What the thread of an explicit release saw.
    #[derive(Debug)]
    enum Seen {
        /// It reached the mount gate.
        AtGate,
        /// The release returned, with the `dfs_release` calls made by then
        /// and whether the teardown had finished its unmount by then.
        Returned(Result<()>, u64, bool),
    }

    /// `Dfs::close` stops after it closed the mount gate and before its
    /// teardown released any object, and `release` (made by `open` on the
    /// mount) runs in a thread: the release waits at the gate and returns
    /// only after the teardown released the object.
    fn explicit_release_waits_for_the_teardown(
        open: impl FnOnce(&Dfs) -> Box<dyn FnOnce() -> Result<()> + Send>,
    ) {
        let f = fixture();
        let b = f.mount();
        let release = open(&b);
        let before = f.calls("dfs_release");
        let (at_tx, at_rx) = signal();
        let (go_tx, go_rx) = signal();
        // Set once the teardown stored its end, after every object of the
        // mount was released and the mount unmounted.
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let closer = {
            let finished = finished.clone();
            std::thread::spawn(move || {
                testing::set("MountGateClosed", None, move |_| {
                    at_tx.send(()).unwrap();
                    go_rx.recv().unwrap();
                });
                testing::set("AfterUnmountedStore", None, move |_| {
                    finished.store(true, Ordering::SeqCst);
                });
                b.close(T1)
            })
        };
        wait(&at_rx);
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        let mem = f.mem.clone();
        let releaser = std::thread::spawn(move || {
            let at_gate = seen_tx.clone();
            testing::set("BeforeSharedLock", None, move |_| {
                at_gate.send(Seen::AtGate).unwrap();
            });
            let r = release();
            let done = finished.load(Ordering::SeqCst);
            seen_tx
                .send(Seen::Returned(r, mem.calls("dfs_release"), done))
                .unwrap();
        });
        let first = seen_rx.recv_timeout(LIMIT);
        go_tx.send(()).unwrap();
        let last = match first {
            Ok(Seen::AtGate) => seen_rx.recv_timeout(LIMIT),
            other => other,
        };
        releaser.join().unwrap();
        closer.join().unwrap().unwrap();
        match last {
            Ok(Seen::Returned(r, releases, done)) => {
                r.unwrap();
                assert!(
                    releases > before,
                    "the release returned before the teardown released the object"
                );
                assert!(
                    done,
                    "the release returned before the teardown had finished"
                );
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(f.mem.open_objects(), 0);
    }

    #[test]
    fn file_close_returns_after_the_teardown_of_a_closing_mount() {
        explicit_release_waits_for_the_teardown(|b| {
            let file = b.create("/f").unwrap();
            Box::new(move || file.close())
        });
    }

    #[test]
    fn writer_finish_returns_after_the_teardown_of_a_closing_mount() {
        explicit_release_waits_for_the_teardown(|b| {
            let mut w = b.writer("/w").unwrap();
            w.write_all(b"x").unwrap();
            Box::new(move || w.finish().map(|_| ()))
        });
    }

    #[test]
    fn handle_types_keep_their_auto_traits() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Dfs>();
        send_sync::<File>();
        send_sync::<ReadDir>();
        send_sync::<Writer>();
    }
}
