// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! DFS mounts: [`Dfs`], the mount on a crate-owned container connection, the
//! open handle table and the directory handle cache.

mod dir;
mod file;
mod metadata;
mod writer;

pub use dir::{DirEntry, ReadDir};
pub use file::{File, OpenOptions};
pub use metadata::{FileType, Metadata};
pub use writer::Writer;

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use crate::conn::{Access, ContConn, Key, Registry};
use crate::error::{Error, Result};
use crate::fork::Stamp;
use crate::gate::{Gate, Pass};
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
    mount: Weak<Mount>,
    pub(crate) obj: ObjPtr,
    slot: u64,
    stamp: Stamp,
}

impl Drop for DirHandle {
    fn drop(&mut self) {
        if self.stamp.is_foreign() {
            return; // fork child: leak, never touch locks or libdfs
        }
        if let Some(m) = self.mount.upgrade() {
            let _ = m.release_slot(self.slot);
        }
        // Mount already gone: its own drop released every slot.
    }
}

// ---------------------------------------------------------------------------
// Mount

/// One `dfs_t` (from `dfs_mount`) and everything attached to it.
///
/// Close and handle synchronisation: every operation that passes the `dfs_t`
/// or a `dfs_obj_t` to libdfs holds the gate shared and checks the closed
/// flag. [`Mount::close`] takes the gate exclusively, marks the mount
/// closed, releases every pointer in the handle table, unmounts, and gives
/// up the mount's container lease. A handle never passes its pointer without
/// holding the gate, so no call can run on a freed object.
pub(crate) struct Mount {
    pub(crate) backend: Arc<dyn Raw>,
    raw: DfsPtr,
    pool: String,
    container: String,
    sys_name: Option<String>,
    read_only: bool,
    pub(crate) stamp: Stamp,
    self_ref: Weak<Mount>,
    gate: Gate,
    handles: Mutex<HashMap<u64, ObjPtr>>,
    next_slot: AtomicU64,
    dir_cache: Mutex<HashMap<String, Arc<DirHandle>>>,
    dir_cache_capacity: usize,
    unmounted: AtomicBool,
    /// errno of a failed `dfs_umount` (0: none).
    umount_errno: AtomicI32,
    /// The container lease; taken by the teardown.
    cont: Mutex<Option<Arc<ContConn>>>,
}

impl Mount {
    /// Enter an operation: hold the gate shared and check the closed flag.
    /// While a `close` waits for the gate, new operations get `Closed`.
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

    /// True while the `dfs_t` is mounted: before the teardown, and after a
    /// teardown whose `dfs_umount` failed (the mount then keeps its
    /// container lease).
    pub(crate) fn is_mounted(&self) -> bool {
        self.cont
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// The error of the failed `dfs_umount` (see [`Mount::is_mounted`]).
    pub(crate) fn umount_error(&self) -> Error {
        Error::Dfs {
            op: "dfs_umount",
            errno: match self.umount_errno.load(Ordering::SeqCst) {
                0 => libc::EBUSY,
                e => e,
            },
            path: Some(format!("{}/{}", self.pool, self.container)),
        }
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
    /// and the slot still exists. After close the slot is gone: nothing to do.
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

    /// Release every tracked object, unmount, and give up the container
    /// lease (which closes the container, and then the pool, when it was the
    /// last lease). Caller holds the gate exclusively or is the last owner.
    ///
    /// A `dfs_release` that fails leaves its object allocated inside libdfs
    /// (`obj.c`: the object is freed only on success); the crate forgets the
    /// pointer anyway and reports the first error, so such an object leaks.
    fn teardown(&self) -> Result<()> {
        if self.unmounted.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let objs: Vec<ObjPtr> = self
            .handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, o)| o)
            .collect();
        let mut first_err = None;
        for obj in objs {
            // SAFETY: exclusive access (see above); each pointer is released once.
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
        // unmounted exactly once (guarded by `unmounted`).
        let rc = unsafe { self.backend.dfs_umount(self.raw.0) };
        if rc != 0 {
            self.umount_errno.store(rc, Ordering::SeqCst);
            // The dfs_t may still use the container handle: keep the lease.
            // It goes with the Mount, and libdaos refuses that container
            // close while objects of the container are open.
            return Err(Error::Dfs {
                op: "dfs_umount",
                errno: rc,
                path: Some(format!("{}/{}", self.pool, self.container)),
            });
        }
        let cont = self.cont.lock().unwrap_or_else(|e| e.into_inner()).take();
        let released = match cont {
            Some(c) => ContConn::release(c),
            None => Ok(()),
        };
        match first_err {
            Some(e) => Err(e),
            None => released,
        }
    }

    pub(crate) fn close(&self, timeout: Duration) -> Result<()> {
        self.stamp.check_cold()?;
        let done = self.gate.close(timeout, || {
            let cached: HashMap<String, Arc<DirHandle>> =
                std::mem::take(&mut *self.dir_cache.lock().unwrap_or_else(|e| e.into_inner()));
            (self.teardown(), cached)
        })?;
        match done {
            None => Ok(()),
            Some((result, cached)) => {
                // Dropped after the gate is released: their slots are gone
                // already.
                drop(cached);
                result
            }
        }
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
        self.stamp.check()?;
        if self.is_closed() {
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
            mount: self.self_ref.clone(),
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
            let evicted = {
                let mut cache = self.dir_cache.lock().unwrap_or_else(|e| e.into_inner());
                let evicted = if cache.len() >= self.dir_cache_capacity {
                    std::mem::take(&mut *cache)
                } else {
                    HashMap::new()
                };
                cache.insert(dir.to_string(), h.clone());
                evicted
            };
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

impl Drop for Mount {
    fn drop(&mut self) {
        if self.stamp.is_foreign() {
            // Fork child: leak everything. Tearing down could send messages on
            // connections the parent still uses.
            return;
        }
        if !self.gate.is_closed_mut() {
            let _ = self.teardown();
        }
        // DirHandles in the cache drop after this; their Weak<Mount> no
        // longer upgrades and their slots were released above.
    }
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
    stamp: Stamp,
) -> Result<Arc<Mount>> {
    opts.validate()?;
    let sys = opts.sys_name.as_deref();
    let access = if opts.read_only {
        Access::ReadOnly
    } else {
        Access::ReadWrite
    };
    let sys_key = crate::conn::system_key(sys);
    // Check the system rule before any connection is opened.
    registry.acquire::<Mount>(
        Key::Mount(opts.pool.clone(), opts.container.clone(), access),
        &sys_key,
        |r| {
            r.mounts
                .get(&(opts.pool.clone(), opts.container.clone(), access))
                .and_then(Weak::upgrade)
                .filter(|m| !m.is_closed())
        },
        || {
            let attempt = || {
                let cont =
                    registry.container(&backend, &opts.pool, &opts.container, sys, access)?;
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
) -> Result<Arc<Mount>> {
    let registry = cont.backend.registry();
    let sys_key = crate::conn::system_key(sys_name.as_deref());
    let key = (cont.pool_name.clone(), cont.name.clone(), cont.access);
    registry.acquire::<Mount>(
        Key::Mount(key.0.clone(), key.1.clone(), key.2),
        &sys_key,
        |r| {
            r.mounts
                .get(&key)
                .and_then(Weak::upgrade)
                .filter(|m| !m.is_closed())
        },
        || {
            if !cont.is_usable() {
                return Err(Error::Closed);
            }
            new_mount(
                cont.clone(),
                sys_name.clone(),
                options.dir_cache_capacity,
                Stamp::now(),
            )
        },
        store_mount,
    )
}

fn store_mount(r: &mut crate::conn::RegistryInner, m: &Arc<Mount>) {
    r.mounts.retain(|_, w| w.strong_count() > 0);
    r.mounts.insert(
        (m.pool.clone(), m.container.clone(), m.access()),
        Arc::downgrade(m),
    );
}

fn new_mount(
    cont: Arc<ContConn>,
    sys_name: Option<String>,
    dir_cache_capacity: usize,
    stamp: Stamp,
) -> Result<Arc<Mount>> {
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
    let raw = mount_raw(&cont, access).map_err(|rc| Error::dfs("dfs_mount", rc, cont.target()))?;
    let m = Arc::new_cyclic(|me| Mount {
        backend: cont.backend.clone(),
        raw: DfsPtr(raw),
        pool: cont.pool_name.clone(),
        container: cont.name.clone(),
        sys_name,
        read_only: access == Access::ReadOnly,
        stamp,
        self_ref: me.clone(),
        gate: Gate::new(),
        handles: Mutex::new(HashMap::new()),
        next_slot: AtomicU64::new(1),
        dir_cache: Mutex::new(HashMap::new()),
        dir_cache_capacity,
        unmounted: AtomicBool::new(false),
        umount_errno: AtomicI32::new(0),
        cont: Mutex::new(Some(cont.clone())),
    });
    {
        let mut mounts = cont.mounts.lock().unwrap_or_else(|e| e.into_inner());
        mounts.retain(|w| w.strong_count() > 0);
        mounts.push(Arc::downgrade(&m));
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
    pub(crate) mount: Arc<Mount>,
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
    pub fn connect_with(options: &ConnectOptions) -> Result<Dfs> {
        let rt = crate::runtime::runtime()?;
        let mount = mount(
            rt.backend.registry(),
            rt.backend.clone(),
            options,
            Stamp::now(),
        )?;
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
        let mount = mount(backend.registry(), raw, options, Stamp::now())?;
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
    /// container is closed and then the pool disconnected. Idempotent.
    ///
    /// Affects every clone of this `Dfs`. Later operations on old handles
    /// return [`Error::Closed`]; a later [`Dfs::connect`] creates a new mount.
    /// DAOS itself stays initialised.
    ///
    /// [`Error::Busy`] when operations still run after `timeout`; the mount is
    /// then left connected and accepts operations again.
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
    const fn send<T: Send>() {}
    send_sync::<Dfs>();
    send_sync::<File>();
    send::<Writer>();
    send::<ReadDir>();
};
