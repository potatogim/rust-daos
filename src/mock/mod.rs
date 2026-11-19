// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! In-memory stand-in for libdaos and libdfs (feature `mock`).
//!
//! [`MemDfs`] implements the pool and container connect calls of libdaos and
//! the calls of libdfs over an in-memory tree and
//! follows the DFS rules this crate relies on: errno values, `ENOTEMPTY`
//! without force, `EPERM` for writes on a read-only mount or handle, `EINVAL`
//! for reading through a handle opened without `S_IFREG`, batch-limited
//! `dfs_iterate` with an anchor, and handles of a removed directory that, like in libdfs, still accept
//! creates (the new entries are unreachable by path). It also
//! offers call counters, fault injection and pause points for tests.
//!
//! Pool `"missing"` and containers whose name starts with `"missing"` do not
//! exist (`-DER_NONEXIST`); containers whose name starts with `"plain"` exist
//! but are not POSIX containers (`dfs_mount` fails with `EINVAL`). Any other
//! name opens: it names a container created with
//! [`Pool::create_container`](crate::Pool::create_container), or one that
//! exists implicitly from its first open until it is destroyed. All POSIX
//! containers share one file tree.
//!
//! [`MemDaos`] is the same type under the name used for the object API.
//!
//! It is selected only through [`Dfs::connect_mock`](crate::Dfs::connect_mock),
//! never by configuration, so production code cannot pick it by accident.
//!
//! ```
//! use std::sync::Arc;
//! let mem = Arc::new(daos::mock::MemDfs::new());
//! let dfs = daos::Dfs::connect_mock(mem.clone()).unwrap();
//! dfs.write("/a/b.txt", b"hi").unwrap();
//! assert_eq!(dfs.read("/a/b.txt").unwrap(), b"hi");
//! mem.fail_next("dfs_open", libc::EIO);
//! assert_eq!(dfs.open("/a/b.txt").unwrap_err().raw_os_error(), Some(libc::EIO));
//! ```

use std::collections::{BTreeMap, HashMap};
use std::ffi::{c_char, c_int, c_void, CStr};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::conn::Registry;
use crate::error::{Error, Result};
use crate::fork::Stamp;
use crate::sys::api::Raw;
use crate::sys::{
    self, d_iov_t, d_sg_list_t, daos_anchor_t, daos_array_iod_t, daos_array_stbuf_t, daos_handle_t,
    daos_iod_t, daos_key_desc_t, daos_obj_id_t, daos_prop_t, daos_recx_t, dfs_attr_t,
    dfs_filler_cb_t, dfs_obj_t, dfs_t, mode_t, stat_t,
};

mod daos;
mod objects;

use objects::ObjKind;

/// The in-memory backend under the name used for the object API.
pub type MemDaos = MemDfs;

/// Largest file the in-memory backend holds (bounded to keep tests from
/// allocating absurd amounts by accident).
const MAX_FILE_SIZE: usize = 1 << 34;

struct Entry {
    oid: u64,
    mode: u32,
    data: Vec<u8>,
    mtime: i64,
}

struct MemMount {
    read_only: bool,
    coh: u64,
}

struct MemObj {
    path: Vec<u8>,
    oid: u64,
    mode: u32,
    flags: c_int,
}

struct State {
    entries: BTreeMap<Vec<u8>, Entry>,
    next_oid: u64,
}

struct Fault {
    op: String,
    skip: u32,
    times: u32,
    rc: c_int,
}

#[derive(Default)]
struct PauseState {
    entered: bool,
    released: bool,
    /// The result of the paused call once released (`None`: as usual).
    #[cfg(all(test, feature = "mock"))]
    fail: Option<c_int>,
    /// The thread that entered the paused call.
    #[cfg(all(test, feature = "mock"))]
    thread: Option<std::thread::ThreadId>,
}

type PauseCell = Arc<(Mutex<PauseState>, Condvar)>;

/// A pause point created by [`MemDfs::pause_next`]. Dropping it releases the
/// paused call.
pub struct PauseHandle {
    state: PauseCell,
}

impl PauseHandle {
    /// Wait until the paused call has started, up to `timeout`.
    pub fn wait_entered(&self, timeout: Duration) -> bool {
        let (m, cv) = &*self.state;
        let deadline = Instant::now() + timeout;
        let mut g = m.lock().unwrap();
        while !g.entered {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            g = cv.wait_timeout(g, deadline - now).unwrap().0;
        }
        true
    }

    /// The thread that entered the paused call.
    #[cfg(all(test, feature = "mock"))]
    pub(crate) fn thread(&self) -> Option<std::thread::ThreadId> {
        self.state.0.lock().unwrap().thread
    }

    /// Let the paused call continue.
    pub fn release(&self) {
        let (m, cv) = &*self.state;
        m.lock().unwrap().released = true;
        cv.notify_all();
    }
}

impl Drop for PauseHandle {
    fn drop(&mut self) {
        self.release();
    }
}

/// The connection registry lock of a [`MemDfs`], held until dropped (see
/// [`MemDfs::hold_registry_lock`]).
#[doc(hidden)]
pub struct RegistryHold<'a> {
    _guard: std::sync::MutexGuard<'a, crate::conn::RegistryInner>,
}

/// In-memory DFS backend. Wrap it in an `Arc` and pass it to
/// [`Dfs::connect_mock`](crate::Dfs::connect_mock).
pub struct MemDfs {
    stamp: Stamp,
    registry: Registry,
    state: Mutex<State>,
    daos_ready: AtomicBool,
    dfs_ready: AtomicBool,
    init_lock: Mutex<()>,
    calls: Mutex<HashMap<String, u64>>,
    faults: Mutex<Vec<Fault>>,
    pauses: Mutex<HashMap<String, PauseCell>>,
    objects: AtomicUsize,
    created: AtomicUsize,
    mounts: AtomicUsize,
    daos: daos::Daos,
    iterate_stall: AtomicBool,
    iterate_null_name: AtomicBool,
    /// Every call, counted before any lock of the backend is taken.
    #[cfg(all(test, feature = "mock"))]
    total: std::sync::atomic::AtomicU64,
    /// Pauses queued per call after the one of `pause_next`.
    #[cfg(all(test, feature = "mock"))]
    queued: Mutex<HashMap<String, std::collections::VecDeque<PauseCell>>>,
}

impl fmt::Debug for MemDfs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemDfs")
            .field("open_objects", &self.open_objects())
            .field("open_mounts", &self.open_mounts())
            .finish()
    }
}

impl Default for MemDfs {
    fn default() -> Self {
        Self::new()
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Place of the entries created through a handle of a removed directory with
/// object ID `oid`. Starts with a NUL byte, so no path can reach it.
fn orphan_dir(oid: u64) -> Vec<u8> {
    format!("\0orphan/{oid}").into_bytes()
}

fn parent_of(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|b| *b == b'/') {
        Some(0) | None => b"/",
        Some(i) => &path[..i],
    }
}

fn child_path(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut p = dir.to_vec();
    if p != b"/" {
        p.push(b'/');
    }
    p.extend_from_slice(name);
    p
}

fn is_below(path: &[u8], dir: &[u8]) -> bool {
    if dir == b"/" {
        return path != b"/";
    }
    path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == b'/'
}

impl MemDfs {
    /// An empty file system with only the root directory.
    pub fn new() -> MemDfs {
        let mut entries = BTreeMap::new();
        entries.insert(
            b"/".to_vec(),
            Entry {
                oid: 1,
                mode: sys::S_IFDIR | 0o755,
                data: Vec::new(),
                mtime: now_secs(),
            },
        );
        MemDfs {
            stamp: Stamp::now(),
            registry: Registry::default(),
            state: Mutex::new(State {
                entries,
                next_oid: 2,
            }),
            daos_ready: AtomicBool::new(false),
            dfs_ready: AtomicBool::new(false),
            init_lock: Mutex::new(()),
            calls: Mutex::new(HashMap::new()),
            faults: Mutex::new(Vec::new()),
            pauses: Mutex::new(HashMap::new()),
            objects: AtomicUsize::new(0),
            created: AtomicUsize::new(0),
            mounts: AtomicUsize::new(0),
            daos: daos::Daos::default(),
            iterate_stall: AtomicBool::new(false),
            iterate_null_name: AtomicBool::new(false),
            #[cfg(all(test, feature = "mock"))]
            total: std::sync::atomic::AtomicU64::new(0),
            #[cfg(all(test, feature = "mock"))]
            queued: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn stamp(&self) -> Stamp {
        self.stamp
    }

    pub(crate) fn ensure_init(&self) -> Result<()> {
        let _g = self.init_lock.lock().unwrap_or_else(|e| e.into_inner());
        if !self.daos_ready.load(Ordering::SeqCst) {
            // SAFETY: no pointers involved.
            let rc = unsafe { self.daos_init() };
            if rc != 0 {
                return Err(Error::daos("daos_init", rc));
            }
        }
        if !self.dfs_ready.load(Ordering::SeqCst) {
            // SAFETY: no pointers involved.
            let rc = unsafe { self.dfs_init() };
            if rc != 0 {
                return Err(Error::Dfs {
                    op: "dfs_init",
                    errno: rc,
                    path: None,
                });
            }
        }
        Ok(())
    }

    /// Number of calls of `op` (a C function name such as `"dfs_open"`).
    pub fn calls(&self, op: &str) -> u64 {
        self.calls.lock().unwrap().get(op).copied().unwrap_or(0)
    }

    /// Make the next call of `op` return `rc` (an errno for `dfs_*`, a
    /// negative DER code for `daos_*`). `op` is the exported C name, for
    /// example `"daos_pool_connect2"` or `"dfs_mount"`.
    pub fn fail_next(&self, op: &str, rc: i32) {
        self.fail_nth(op, 0, rc, 1);
    }

    /// Let `skip` calls of `op` pass, then make `times` calls return `rc`.
    pub fn fail_nth(&self, op: &str, skip: u32, rc: i32, times: u32) {
        self.faults.lock().unwrap().push(Fault {
            op: op.to_string(),
            skip,
            times,
            rc,
        });
    }

    /// Remove every pending fault.
    pub fn clear_faults(&self) {
        self.faults.lock().unwrap().clear();
    }

    /// Block the next call of `op` until the returned handle is released or
    /// dropped.
    pub fn pause_next(&self, op: &str) -> PauseHandle {
        let state: PauseCell = Arc::new((Mutex::new(PauseState::default()), Condvar::new()));
        self.pauses
            .lock()
            .unwrap()
            .insert(op.to_string(), state.clone());
        PauseHandle { state }
    }

    /// Block a later call of `op`: pauses queue up, one per call, after
    /// the one of [`MemDfs::pause_next`].
    #[cfg(all(test, feature = "mock"))]
    pub(crate) fn pause_also(&self, op: &str) -> PauseHandle {
        self.queue_pause(op, None)
    }

    /// Block the next call of `op` and make that same call return `rc`
    /// once released.
    #[cfg(all(test, feature = "mock"))]
    pub(crate) fn pause_failing(&self, op: &str, rc: i32) -> PauseHandle {
        self.queue_pause(op, Some(rc))
    }

    #[cfg(all(test, feature = "mock"))]
    fn queue_pause(&self, op: &str, fail: Option<c_int>) -> PauseHandle {
        let st = PauseState {
            fail,
            ..PauseState::default()
        };
        let state: PauseCell = Arc::new((Mutex::new(st), Condvar::new()));
        self.queued
            .lock()
            .unwrap()
            .entry(op.to_string())
            .or_default()
            .push_back(state.clone());
        PauseHandle { state }
    }

    /// Every call so far, counted before the backend takes a lock.
    #[cfg(all(test, feature = "mock"))]
    pub(crate) fn calls_total(&self) -> u64 {
        self.total.load(Ordering::SeqCst)
    }

    /// Hold the lock of this backend's connection registry until the
    /// returned value drops; for fork tests (a lock held by a parent thread
    /// at the fork).
    #[doc(hidden)]
    pub fn hold_registry_lock(&self) -> RegistryHold<'_> {
        RegistryHold {
            _guard: self.registry.lock(),
        }
    }

    /// Objects opened (`dfs_open`, `dfs_lookup`) and not yet released.
    pub fn open_objects(&self) -> usize {
        self.objects.load(Ordering::SeqCst)
    }

    /// Objects handed out by `dfs_open` and `dfs_lookup` so far.
    pub fn objects_created(&self) -> usize {
        self.created.load(Ordering::SeqCst)
    }

    /// Mounts (`dfs_mount`) not yet unmounted.
    pub fn open_mounts(&self) -> usize {
        self.mounts.load(Ordering::SeqCst)
    }

    /// Pool handles (`daos_pool_connect`) not yet disconnected.
    pub fn open_pool_handles(&self) -> usize {
        self.daos.pool_handles()
    }

    /// Container handles (`daos_cont_open`) not yet closed.
    pub fn open_container_handles(&self) -> usize {
        self.daos.container_handles()
    }

    /// Property lists from `daos_prop_alloc` not yet freed.
    pub fn open_property_lists(&self) -> usize {
        self.daos.prop_lists()
    }

    /// Object, key-value and array handles open (`daos_obj_open`,
    /// `daos_kv_open`, `daos_array_create`/`open`) and not yet closed.
    pub fn open_object_handles(&self) -> usize {
        self.daos.lock().objs.handles.len()
    }

    /// Transactions open and not yet closed.
    pub fn open_transactions(&self) -> usize {
        self.daos.lock().objs.txs.len()
    }

    /// Make the next `times` key list calls return `DER_KEY2BIG`, reporting
    /// `needed` as the key length (`None`: the buffer's own capacity, an
    /// exact fit).
    pub fn inject_key2big(&self, times: u32, needed: Option<u64>) {
        self.daos.lock().objs.key2big = (times, needed);
    }

    /// Existing containers: UUID, label, POSIX layout.
    pub fn containers(&self) -> Vec<(crate::Uuid, Option<String>, bool)> {
        self.daos.containers()
    }

    /// When set, `dfs_iterate` returns no names and never reaches the end.
    pub fn set_iterate_stall(&self, yes: bool) {
        self.iterate_stall.store(yes, Ordering::SeqCst);
    }

    /// When set, `dfs_iterate` passes a null name to the callback (a broken
    /// library), which makes the crate's callback panic internally.
    pub fn set_iterate_null_name(&self, yes: bool) {
        self.iterate_null_name.store(yes, Ordering::SeqCst);
    }

    /// As `enter`, for a call in transaction `th`: an injected
    /// `DER_TX_RESTART` moves the transaction to the state libdaos gives it
    /// (only restart or close).
    fn enter_tx(&self, op: &str, th: daos_handle_t) -> Option<c_int> {
        let rc = self.enter(op)?;
        if th.cookie != 0 && rc == -2025 {
            self.daos.lock().tx_restart_needed(th);
        }
        Some(rc)
    }

    /// Count the call, honour a pause, and return an injected result.
    fn enter(&self, op: &str) -> Option<c_int> {
        #[cfg(all(test, feature = "mock"))]
        self.total.fetch_add(1, Ordering::SeqCst);
        *self
            .calls
            .lock()
            .unwrap()
            .entry(op.to_string())
            .or_insert(0) += 1;
        let pause = self.pauses.lock().unwrap().remove(op);
        #[cfg(all(test, feature = "mock"))]
        let pause = pause.or_else(|| {
            self.queued
                .lock()
                .unwrap()
                .get_mut(op)
                .and_then(|q| q.pop_front())
        });
        if let Some(p) = pause {
            let (m, cv) = &*p;
            let mut g = m.lock().unwrap();
            g.entered = true;
            #[cfg(all(test, feature = "mock"))]
            {
                g.thread = Some(std::thread::current().id());
            }
            cv.notify_all();
            while !g.released {
                g = cv.wait(g).unwrap();
            }
            #[cfg(all(test, feature = "mock"))]
            if let Some(rc) = g.fail {
                return Some(rc);
            }
        }
        let mut faults = self.faults.lock().unwrap();
        let i = faults.iter().position(|f| f.op == op)?;
        let f = &mut faults[i];
        if f.skip > 0 {
            f.skip -= 1;
            return None;
        }
        let rc = f.rc;
        f.times -= 1;
        if f.times == 0 {
            faults.remove(i);
        }
        Some(rc)
    }

    fn new_obj(&self, path: Vec<u8>, oid: u64, mode: u32, flags: c_int) -> *mut dfs_obj_t {
        self.objects.fetch_add(1, Ordering::SeqCst);
        self.created.fetch_add(1, Ordering::SeqCst);
        Box::into_raw(Box::new(MemObj {
            path,
            oid,
            mode,
            flags,
        })) as *mut dfs_obj_t
    }

    /// Resolve a parent directory handle (NULL = root) to its path.
    unsafe fn parent_path(
        &self,
        st: &State,
        parent: *mut dfs_obj_t,
    ) -> std::result::Result<Vec<u8>, c_int> {
        if parent.is_null() {
            return Ok(b"/".to_vec());
        }
        // SAFETY: the caller passes a pointer this backend created.
        let p = unsafe { &*(parent as *const MemObj) };
        if p.mode & sys::S_IFMT != sys::S_IFDIR {
            return Err(libc::ENOTDIR);
        }
        match st.entries.get(&p.path) {
            Some(e) if e.oid == p.oid => Ok(p.path.clone()),
            // Like libdfs: a handle of a removed directory still names the
            // removed object. Entries created through it land there, out of
            // reach of any path; lookups through it see only those.
            _ => Ok(orphan_dir(p.oid)),
        }
    }

    unsafe fn obj_entry<'a>(
        &self,
        st: &'a mut State,
        obj: *mut dfs_obj_t,
    ) -> std::result::Result<(&'a MemObj, &'a mut Entry), c_int> {
        if obj.is_null() {
            return Err(libc::EINVAL);
        }
        // SAFETY: the caller passes a pointer this backend created.
        let o = unsafe { &*(obj as *const MemObj) };
        match st.entries.get_mut(&o.path) {
            Some(e) if e.oid == o.oid => Ok((o, e)),
            _ => Err(libc::ENOENT),
        }
    }

    unsafe fn mount_ro(dfs: *mut dfs_t) -> std::result::Result<bool, c_int> {
        if dfs.is_null() {
            return Err(libc::EINVAL);
        }
        // SAFETY: the caller passes a pointer this backend created.
        Ok(unsafe { (*(dfs as *const MemMount)).read_only })
    }

    unsafe fn name_of<'a>(name: *const c_char) -> std::result::Result<&'a [u8], c_int> {
        if name.is_null() {
            return Err(libc::EINVAL);
        }
        // SAFETY: NUL-terminated string from the caller.
        let b = unsafe { CStr::from_ptr(name) }.to_bytes();
        if b.is_empty() || b.len() > sys::DFS_MAX_NAME || b.contains(&b'/') {
            return Err(libc::EINVAL);
        }
        Ok(b)
    }

    fn fill_stat(e: &Entry, out: &mut stat_t) {
        *out = sys::zeroed_stat();
        out.st_mode = e.mode as _;
        out.st_size = e.data.len() as _;
        // SAFETY: no preconditions.
        out.st_uid = unsafe { libc::geteuid() } as _;
        // SAFETY: no preconditions.
        out.st_gid = unsafe { libc::getegid() } as _;
        out.st_mtime = e.mtime as _;
        out.st_ctime = e.mtime as _;
        out.st_atime = e.mtime as _;
    }
}

/// errno of a DER code, for the DFS calls of the stand-in.
fn der_to_errno(rc: c_int) -> c_int {
    if rc == 0 {
        0
    } else {
        crate::DerCode::from_rc(rc).to_errno()
    }
}

macro_rules! tri {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(rc) => return rc,
        }
    };
}

// SAFETY: every method only dereferences pointers created by this backend
// (mounts, objects) or passed in by the crate with the libdfs meaning, and
// all shared state is behind mutexes or atomics.
unsafe impl Raw for MemDfs {
    fn registry(&self) -> &Registry {
        &self.registry
    }

    unsafe fn daos_init(&self) -> c_int {
        if let Some(rc) = self.enter("daos_init") {
            return rc;
        }
        self.daos_ready.store(true, Ordering::SeqCst);
        0
    }

    unsafe fn dfs_init(&self) -> c_int {
        if let Some(rc) = self.enter("dfs_init") {
            return rc;
        }
        if !self.daos_ready.load(Ordering::SeqCst) {
            return libc::EIO;
        }
        self.dfs_ready.store(true, Ordering::SeqCst);
        0
    }

    unsafe fn daos_pool_connect2(
        &self,
        pool: *const c_char,
        _sys: *const c_char,
        flags: u32,
        poh: *mut daos_handle_t,
        info: *mut c_void,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("daos_pool_connect2") {
            return rc;
        }
        if !self.daos_ready.load(Ordering::SeqCst) {
            return -daos::DER_INVAL;
        }
        // SAFETY: pointers from the caller with the libdaos meaning.
        unsafe { self.daos.pool_connect(pool, flags, poh, info.cast()) }
    }

    unsafe fn daos_pool_disconnect(&self, poh: daos_handle_t, _ev: *mut c_void) -> c_int {
        if let Some(rc) = self.enter("daos_pool_disconnect") {
            return rc;
        }
        self.daos.pool_disconnect(poh)
    }

    unsafe fn daos_cont_open2(
        &self,
        poh: daos_handle_t,
        cont: *const c_char,
        flags: u32,
        coh: *mut daos_handle_t,
        info: *mut c_void,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("daos_cont_open2") {
            return rc;
        }
        // SAFETY: pointers from the caller with the libdaos meaning.
        unsafe { self.daos.cont_open(poh, cont, flags, coh, info.cast()) }
    }

    unsafe fn daos_cont_close(&self, coh: daos_handle_t, _ev: *mut c_void) -> c_int {
        if let Some(rc) = self.enter("daos_cont_close") {
            return rc;
        }
        self.daos.cont_close(coh)
    }

    unsafe fn daos_pool_query(
        &self,
        poh: daos_handle_t,
        ranks: *mut c_void,
        info: *mut sys::daos_pool_info_t,
        pool_prop: *mut daos_prop_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("daos_pool_query") {
            return rc;
        }
        if !ranks.is_null() || !pool_prop.is_null() || info.is_null() {
            return -daos::DER_INVAL;
        }
        if !self.daos.lock().pools.contains_key(&poh.cookie) {
            return -daos::DER_NO_HDL;
        }
        // SAFETY: out-pointer from the caller.
        let info = unsafe { &mut *info };
        info.pi_uuid = daos::POOL_UUID;
        info.pi_ntargets = 8;
        info.pi_nnodes = 1;
        info.pi_map_ver = 1;
        info.pi_space.ps_ntargets = 8;
        info.pi_space.ps_space.s_total = [1 << 30, 1 << 34];
        info.pi_space.ps_space.s_free = [1 << 29, 1 << 33];
        info.pi_space.ps_free_min = [1 << 26, 1 << 30];
        info.pi_space.ps_free_max = [1 << 26, 1 << 30];
        info.pi_space.ps_free_mean = [1 << 26, 1 << 30];
        info.pi_rebuild_st.rs_state = 2;
        0
    }

    unsafe fn daos_pool_query_target(
        &self,
        poh: daos_handle_t,
        tgt: u32,
        rank: u32,
        info: *mut sys::daos_target_info_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("daos_pool_query_target") {
            return rc;
        }
        if !self.daos.lock().pools.contains_key(&poh.cookie) {
            return -daos::DER_NO_HDL;
        }
        if tgt >= 8 || rank != 0 || info.is_null() {
            return -daos::DER_NONEXIST;
        }
        // SAFETY: out-pointer from the caller.
        let info = unsafe { &mut *info };
        info.ta_state = 4; // DAOS_TS_UP_IN
        info.ta_space.s_total = [1 << 27, 1 << 31];
        info.ta_space.s_free = [1 << 26, 1 << 30];
        0
    }

    unsafe fn daos_pool_list_cont(
        &self,
        poh: daos_handle_t,
        ncont: *mut u64,
        cbuf: *mut sys::daos_pool_cont_info,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("daos_pool_list_cont") {
            return rc;
        }
        // SAFETY: pointers from the caller with the libdaos meaning.
        unsafe { self.daos.list_cont(poh, ncont, cbuf) }
    }

    unsafe fn daos_cont_create2(
        &self,
        poh: daos_handle_t,
        uuid: *mut sys::uuid_t,
        cont_prop: *mut daos_prop_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("daos_cont_create2") {
            return rc;
        }
        // SAFETY: pointers from the caller with the libdaos meaning.
        unsafe {
            self.daos
                .cont_create(poh, std::ptr::null(), cont_prop, uuid, false)
        }
    }

    unsafe fn daos_cont_create_with_label(
        &self,
        poh: daos_handle_t,
        label: *const c_char,
        cont_prop: *mut daos_prop_t,
        uuid: *mut sys::uuid_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("daos_cont_create_with_label") {
            return rc;
        }
        if label.is_null() {
            return -daos::DER_INVAL;
        }
        // SAFETY: pointers from the caller with the libdaos meaning.
        unsafe { self.daos.cont_create(poh, label, cont_prop, uuid, false) }
    }

    unsafe fn daos_cont_destroy2(
        &self,
        poh: daos_handle_t,
        cont: *const c_char,
        force: c_int,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("daos_cont_destroy2") {
            return rc;
        }
        // SAFETY: pointers from the caller with the libdaos meaning.
        unsafe { self.daos.cont_destroy(poh, cont, force) }
    }

    unsafe fn daos_cont_query(
        &self,
        coh: daos_handle_t,
        info: *mut sys::daos_cont_info_t,
        cont_prop: *mut daos_prop_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("daos_cont_query") {
            return rc;
        }
        // SAFETY: pointers from the caller with the libdaos meaning.
        unsafe { self.daos.cont_query(coh, info, cont_prop) }
    }

    unsafe fn daos_cont_create_snap(
        &self,
        coh: daos_handle_t,
        epoch: *mut u64,
        name: *mut c_char,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("daos_cont_create_snap") {
            return rc;
        }
        if !name.is_null() {
            return -1010; // DER_NOSYS, like the DAOS client
        }
        self.daos.create_snap(coh, epoch)
    }

    unsafe fn daos_cont_list_snap(
        &self,
        coh: daos_handle_t,
        nr: *mut c_int,
        epochs: *mut u64,
        names: *mut *mut c_char,
        _anchor: *mut daos_anchor_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("daos_cont_list_snap") {
            return rc;
        }
        if !names.is_null() {
            return -daos::DER_INVAL;
        }
        // SAFETY: pointers from the caller with the libdaos meaning.
        unsafe { self.daos.list_snap(coh, nr, epochs) }
    }

    unsafe fn daos_cont_destroy_snap(
        &self,
        coh: daos_handle_t,
        epr: sys::daos_epoch_range_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("daos_cont_destroy_snap") {
            return rc;
        }
        self.daos.destroy_snap(coh, epr)
    }

    unsafe fn daos_prop_alloc(&self, entries_nr: u32) -> *mut daos_prop_t {
        let _ = self.enter("daos_prop_alloc");
        self.daos.prop_alloc(entries_nr)
    }

    unsafe fn daos_prop_free(&self, prop: *mut daos_prop_t) {
        let _ = self.enter("daos_prop_free");
        // SAFETY: from the caller.
        unsafe { self.daos.prop_free(prop) }
    }

    unsafe fn dfs_cont_create(
        &self,
        poh: daos_handle_t,
        uuid: *mut sys::uuid_t,
        attr: *mut dfs_attr_t,
        coh: *mut daos_handle_t,
        dfs: *mut *mut dfs_t,
    ) -> c_int {
        if let Some(rc) = self.enter("dfs_cont_create") {
            return rc;
        }
        if uuid.is_null() || !coh.is_null() || !dfs.is_null() {
            return libc::EINVAL;
        }
        // SAFETY: attr from the caller (may be NULL).
        let props = if attr.is_null() {
            std::ptr::null_mut()
        } else {
            unsafe { (*attr).da_props }
        };
        // SAFETY: pointers from the caller with the libdfs meaning.
        let rc = unsafe {
            self.daos
                .cont_create(poh, std::ptr::null(), props, uuid, true)
        };
        der_to_errno(rc)
    }

    unsafe fn dfs_cont_create_with_label(
        &self,
        poh: daos_handle_t,
        label: *const c_char,
        attr: *mut dfs_attr_t,
        uuid: *mut sys::uuid_t,
        coh: *mut daos_handle_t,
        dfs: *mut *mut dfs_t,
    ) -> c_int {
        if let Some(rc) = self.enter("dfs_cont_create_with_label") {
            return rc;
        }
        if label.is_null() || !coh.is_null() || !dfs.is_null() {
            return libc::EINVAL;
        }
        // SAFETY: attr from the caller (may be NULL).
        let props = if attr.is_null() {
            std::ptr::null_mut()
        } else {
            unsafe { (*attr).da_props }
        };
        // SAFETY: pointers from the caller with the libdfs meaning.
        let rc = unsafe { self.daos.cont_create(poh, label, props, uuid, true) };
        der_to_errno(rc)
    }

    unsafe fn daos_cont_alloc_oids(
        &self,
        coh: daos_handle_t,
        num_oids: u64,
        oid: *mut u64,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("daos_cont_alloc_oids") {
            return rc;
        }
        if oid.is_null() || num_oids == 0 {
            return -daos::DER_INVAL;
        }
        self.daos.lock().alloc_oids(coh, num_oids, oid)
    }

    unsafe fn daos_obj_generate_oid2(
        &self,
        coh: daos_handle_t,
        oid: *mut daos_obj_id_t,
        otype: u32,
        cid: u32,
        hints: u16,
        _args: u32,
    ) -> c_int {
        if let Some(rc) = self.enter("daos_obj_generate_oid2") {
            return rc;
        }
        if oid.is_null() || otype > 14 || otype == 1 {
            return -daos::DER_INVAL;
        }
        if self.daos.lock().cont_uuid(coh).is_err() {
            return -daos::DER_NO_HDL;
        }
        // OC_UNKNOWN: S1 by default, RP_2G1 with the replication hint.
        let cid = match cid {
            0 if hints & (1 << 2) != 0 => (8 << 24) | 1,
            0 => (1 << 24) | 1,
            c => c,
        };
        let (ord, grp) = (u64::from(cid >> 24), u64::from(cid & 0xFFFF));
        // SAFETY: in/out pointer from the caller.
        unsafe {
            let o = &mut *oid;
            o.hi = (o.hi & 0xFFFF_FFFF) | (u64::from(otype) << 56) | (ord << 48) | (grp << 32);
        }
        0
    }

    unsafe fn daos_oclass_name2id(&self, name: *const c_char) -> c_int {
        let _ = self.enter("daos_oclass_name2id");
        // SAFETY: NUL-terminated name from the caller.
        let n = unsafe { CStr::from_ptr(name) }.to_bytes();
        let n = n.strip_prefix(b"OC_").unwrap_or(n);
        crate::ObjectClass::NAMED
            .iter()
            .find(|e| e.0.as_bytes().strip_prefix(b"OC_") == Some(n))
            .map_or(0, |e| e.1.id() as c_int)
    }

    unsafe fn daos_oclass_id2name(&self, oc_id: u32, name: *mut c_char) -> c_int {
        let _ = self.enter("daos_oclass_id2name");
        match crate::ObjectClass::NAMED.iter().find(|e| e.1.id() == oc_id) {
            None => -daos::DER_NONEXIST,
            Some((n, _)) => {
                let n = &n.as_bytes()[3..];
                // SAFETY: the caller's buffer holds MAX_OBJ_CLASS_NAME_LEN + 1.
                unsafe {
                    std::ptr::copy_nonoverlapping(n.as_ptr() as *const c_char, name, n.len());
                    *name.add(n.len()) = 0;
                }
                0
            }
        }
    }

    unsafe fn daos_obj_open(
        &self,
        coh: daos_handle_t,
        oid: daos_obj_id_t,
        mode: u32,
        oh: *mut daos_handle_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("daos_obj_open") {
            return rc;
        }
        self.daos
            .lock()
            .obj_open(coh, oid, mode, oh, ObjKind::Obj, None)
    }

    unsafe fn daos_obj_close(&self, oh: daos_handle_t, _ev: *mut c_void) -> c_int {
        if let Some(rc) = self.enter("daos_obj_close") {
            return rc;
        }
        self.daos.lock().obj_close(oh, ObjKind::Obj)
    }

    unsafe fn daos_obj_punch(
        &self,
        oh: daos_handle_t,
        th: daos_handle_t,
        _flags: u64,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter_tx("daos_obj_punch", th) {
            return rc;
        }
        // SAFETY: no pointers.
        unsafe { self.daos.lock().obj_punch(oh, th, None, None, None) }
    }

    unsafe fn daos_obj_punch_dkeys(
        &self,
        oh: daos_handle_t,
        th: daos_handle_t,
        _flags: u64,
        nr: u32,
        dkeys: *mut d_iov_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter_tx("daos_obj_punch_dkeys", th) {
            return rc;
        }
        if dkeys.is_null() || nr == 0 {
            return -daos::DER_INVAL;
        }
        // SAFETY: nr keys from the caller.
        unsafe {
            let k = std::slice::from_raw_parts(dkeys, nr as usize);
            self.daos.lock().obj_punch(oh, th, Some(k), None, None)
        }
    }

    unsafe fn daos_obj_punch_akeys(
        &self,
        oh: daos_handle_t,
        th: daos_handle_t,
        _flags: u64,
        dkey: *mut d_iov_t,
        nr: u32,
        akeys: *mut d_iov_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter_tx("daos_obj_punch_akeys", th) {
            return rc;
        }
        if akeys.is_null() || nr == 0 || dkey.is_null() {
            return -daos::DER_INVAL;
        }
        // SAFETY: nr keys from the caller.
        unsafe {
            let k = std::slice::from_raw_parts(akeys, nr as usize);
            self.daos
                .lock()
                .obj_punch(oh, th, None, Some(dkey), Some(k))
        }
    }

    unsafe fn daos_obj_fetch(
        &self,
        oh: daos_handle_t,
        th: daos_handle_t,
        flags: u64,
        dkey: *mut d_iov_t,
        nr: u32,
        iods: *mut daos_iod_t,
        sgls: *mut d_sg_list_t,
        ioms: *mut c_void,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter_tx("daos_obj_fetch", th) {
            return rc;
        }
        if !ioms.is_null() {
            return -daos::DER_INVAL;
        }
        // SAFETY: descriptors from the caller.
        unsafe {
            self.daos
                .lock()
                .obj_fetch(oh, th, flags, dkey, nr, iods, sgls)
        }
    }

    unsafe fn daos_obj_update(
        &self,
        oh: daos_handle_t,
        th: daos_handle_t,
        flags: u64,
        dkey: *mut d_iov_t,
        nr: u32,
        iods: *mut daos_iod_t,
        sgls: *mut d_sg_list_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter_tx("daos_obj_update", th) {
            return rc;
        }
        // SAFETY: descriptors from the caller.
        unsafe {
            self.daos
                .lock()
                .obj_update(oh, th, flags, dkey, nr, iods, sgls)
        }
    }

    unsafe fn daos_obj_list_dkey(
        &self,
        oh: daos_handle_t,
        th: daos_handle_t,
        nr: *mut u32,
        kds: *mut daos_key_desc_t,
        sgl: *mut d_sg_list_t,
        anchor: *mut daos_anchor_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter_tx("daos_obj_list_dkey", th) {
            return rc;
        }
        // SAFETY: descriptors from the caller.
        unsafe {
            self.daos
                .lock()
                .obj_list(oh, th, None, nr, kds, sgl, anchor)
        }
    }

    unsafe fn daos_obj_list_akey(
        &self,
        oh: daos_handle_t,
        th: daos_handle_t,
        dkey: *mut d_iov_t,
        nr: *mut u32,
        kds: *mut daos_key_desc_t,
        sgl: *mut d_sg_list_t,
        anchor: *mut daos_anchor_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter_tx("daos_obj_list_akey", th) {
            return rc;
        }
        // SAFETY: descriptors from the caller.
        unsafe {
            self.daos
                .lock()
                .obj_list(oh, th, Some(dkey), nr, kds, sgl, anchor)
        }
    }

    unsafe fn daos_obj_query_key(
        &self,
        oh: daos_handle_t,
        th: daos_handle_t,
        flags: u64,
        dkey: *mut d_iov_t,
        akey: *mut d_iov_t,
        recx: *mut daos_recx_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter_tx("daos_obj_query_key", th) {
            return rc;
        }
        // SAFETY: descriptors from the caller.
        unsafe {
            self.daos
                .lock()
                .obj_query_key(oh, th, flags, dkey, akey, recx)
        }
    }

    unsafe fn daos_kv_open(
        &self,
        coh: daos_handle_t,
        oid: daos_obj_id_t,
        mode: u32,
        oh: *mut daos_handle_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("daos_kv_open") {
            return rc;
        }
        if !matches!(oid.hi >> 56, 8..=10) {
            return -daos::DER_INVAL;
        }
        self.daos
            .lock()
            .obj_open(coh, oid, mode, oh, ObjKind::Kv, None)
    }

    unsafe fn daos_kv_close(&self, oh: daos_handle_t, _ev: *mut c_void) -> c_int {
        if let Some(rc) = self.enter("daos_kv_close") {
            return rc;
        }
        self.daos.lock().obj_close(oh, ObjKind::Kv)
    }

    unsafe fn daos_kv_destroy(
        &self,
        oh: daos_handle_t,
        th: daos_handle_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter_tx("daos_kv_destroy", th) {
            return rc;
        }
        self.daos.lock().obj_destroy(oh, th)
    }

    unsafe fn daos_kv_put(
        &self,
        oh: daos_handle_t,
        th: daos_handle_t,
        flags: u64,
        key: *const c_char,
        size: u64,
        buf: *const c_void,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter_tx("daos_kv_put", th) {
            return rc;
        }
        // SAFETY: pointers from the caller.
        unsafe {
            self.daos
                .lock()
                .kv_put(oh, th, flags, key, size, buf.cast())
        }
    }

    unsafe fn daos_kv_get(
        &self,
        oh: daos_handle_t,
        th: daos_handle_t,
        _flags: u64,
        key: *const c_char,
        size: *mut u64,
        buf: *mut c_void,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter_tx("daos_kv_get", th) {
            return rc;
        }
        // SAFETY: pointers from the caller.
        unsafe { self.daos.lock().kv_get(oh, th, key, size, buf.cast()) }
    }

    unsafe fn daos_kv_remove(
        &self,
        oh: daos_handle_t,
        th: daos_handle_t,
        _flags: u64,
        key: *const c_char,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter_tx("daos_kv_remove", th) {
            return rc;
        }
        // SAFETY: pointers from the caller.
        unsafe { self.daos.lock().kv_remove(oh, th, key) }
    }

    unsafe fn daos_kv_list(
        &self,
        oh: daos_handle_t,
        th: daos_handle_t,
        nr: *mut u32,
        kds: *mut daos_key_desc_t,
        sgl: *mut d_sg_list_t,
        anchor: *mut daos_anchor_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter_tx("daos_kv_list", th) {
            return rc;
        }
        // SAFETY: descriptors from the caller.
        unsafe {
            self.daos
                .lock()
                .obj_list(oh, th, None, nr, kds, sgl, anchor)
        }
    }

    unsafe fn daos_array_create(
        &self,
        coh: daos_handle_t,
        oid: daos_obj_id_t,
        _th: daos_handle_t,
        cell_size: u64,
        chunk_size: u64,
        oh: *mut daos_handle_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("daos_array_create") {
            return rc;
        }
        self.daos
            .lock()
            .array_create(coh, oid, cell_size, chunk_size, oh)
    }

    unsafe fn daos_array_open(
        &self,
        coh: daos_handle_t,
        oid: daos_obj_id_t,
        _th: daos_handle_t,
        mode: u32,
        cell_size: *mut u64,
        chunk_size: *mut u64,
        oh: *mut daos_handle_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("daos_array_open") {
            return rc;
        }
        if oid.hi >> 56 != 11 {
            return -daos::DER_INVAL;
        }
        let mut d = self.daos.lock();
        let rc = d.obj_open(coh, oid, mode, oh, ObjKind::Array, None);
        if rc == 0 {
            // SAFETY: out-pointers from the caller; the handle was just made.
            unsafe {
                let h = &d.objs.handles[&(*oh).cookie];
                *cell_size = h.cell;
                *chunk_size = h.chunk;
            }
        }
        rc
    }

    unsafe fn daos_array_open_with_attr(
        &self,
        coh: daos_handle_t,
        oid: daos_obj_id_t,
        _th: daos_handle_t,
        mode: u32,
        cell_size: u64,
        chunk_size: u64,
        oh: *mut daos_handle_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("daos_array_open_with_attr") {
            return rc;
        }
        if !matches!(oid.hi >> 56, 12 | 13) || cell_size == 0 {
            return -daos::DER_INVAL;
        }
        self.daos.lock().obj_open(
            coh,
            oid,
            mode,
            oh,
            ObjKind::Array,
            Some((cell_size, chunk_size)),
        )
    }

    unsafe fn daos_array_close(&self, oh: daos_handle_t, _ev: *mut c_void) -> c_int {
        if let Some(rc) = self.enter("daos_array_close") {
            return rc;
        }
        self.daos.lock().obj_close(oh, ObjKind::Array)
    }

    unsafe fn daos_array_read(
        &self,
        oh: daos_handle_t,
        th: daos_handle_t,
        iod: *mut daos_array_iod_t,
        sgl: *mut d_sg_list_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter_tx("daos_array_read", th) {
            return rc;
        }
        // SAFETY: descriptors from the caller.
        unsafe { self.daos.lock().array_io(oh, th, iod, sgl, false) }
    }

    unsafe fn daos_array_write(
        &self,
        oh: daos_handle_t,
        th: daos_handle_t,
        iod: *mut daos_array_iod_t,
        sgl: *mut d_sg_list_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter_tx("daos_array_write", th) {
            return rc;
        }
        // SAFETY: descriptors from the caller.
        unsafe { self.daos.lock().array_io(oh, th, iod, sgl, true) }
    }

    unsafe fn daos_array_get_size(
        &self,
        oh: daos_handle_t,
        th: daos_handle_t,
        size: *mut u64,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter_tx("daos_array_get_size", th) {
            return rc;
        }
        match self.daos.lock().array_size(oh, th, None) {
            Ok((n, _)) => {
                // SAFETY: out-pointer from the caller.
                unsafe { *size = n };
                0
            }
            Err(rc) => rc,
        }
    }

    unsafe fn daos_array_set_size(
        &self,
        oh: daos_handle_t,
        th: daos_handle_t,
        size: u64,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter_tx("daos_array_set_size", th) {
            return rc;
        }
        match self.daos.lock().array_size(oh, th, Some(size)) {
            Ok(_) => 0,
            Err(rc) => rc,
        }
    }

    unsafe fn daos_array_stat(
        &self,
        oh: daos_handle_t,
        th: daos_handle_t,
        stbuf: *mut daos_array_stbuf_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter_tx("daos_array_stat", th) {
            return rc;
        }
        match self.daos.lock().array_size(oh, th, None) {
            Ok((n, e)) => {
                // SAFETY: out-pointer from the caller.
                unsafe {
                    (*stbuf).st_size = n;
                    (*stbuf).st_max_epoch = e;
                }
                0
            }
            Err(rc) => rc,
        }
    }

    unsafe fn daos_array_punch(
        &self,
        oh: daos_handle_t,
        th: daos_handle_t,
        iod: *mut daos_array_iod_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter_tx("daos_array_punch", th) {
            return rc;
        }
        // SAFETY: descriptors from the caller.
        unsafe { self.daos.lock().array_punch(oh, th, iod) }
    }

    unsafe fn daos_array_destroy(
        &self,
        oh: daos_handle_t,
        th: daos_handle_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter_tx("daos_array_destroy", th) {
            return rc;
        }
        self.daos.lock().obj_destroy(oh, th)
    }

    unsafe fn daos_tx_open(
        &self,
        coh: daos_handle_t,
        th: *mut daos_handle_t,
        flags: u64,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("daos_tx_open") {
            return rc;
        }
        self.daos.lock().tx_open(coh, flags, None, th)
    }

    unsafe fn daos_tx_open_snap(
        &self,
        coh: daos_handle_t,
        epoch: u64,
        th: *mut daos_handle_t,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("daos_tx_open_snap") {
            return rc;
        }
        self.daos.lock().tx_open(coh, 0, Some(epoch), th)
    }

    unsafe fn daos_tx_commit(&self, th: daos_handle_t, _ev: *mut c_void) -> c_int {
        if let Some(rc) = self.enter("daos_tx_commit") {
            self.daos.lock().tx_commit_failed(th, rc);
            return rc;
        }
        self.daos.lock().tx_end(th, "commit")
    }

    unsafe fn daos_tx_abort(&self, th: daos_handle_t, _ev: *mut c_void) -> c_int {
        if let Some(rc) = self.enter("daos_tx_abort") {
            return rc;
        }
        self.daos.lock().tx_end(th, "abort")
    }

    unsafe fn daos_tx_close(&self, th: daos_handle_t, _ev: *mut c_void) -> c_int {
        if let Some(rc) = self.enter("daos_tx_close") {
            return rc;
        }
        self.daos.lock().tx_end(th, "close")
    }

    unsafe fn daos_tx_restart(&self, th: daos_handle_t, _ev: *mut c_void) -> c_int {
        if let Some(rc) = self.enter("daos_tx_restart") {
            return rc;
        }
        self.daos.lock().tx_end(th, "restart")
    }

    unsafe fn daos_tx_hdl2epoch(&self, th: daos_handle_t, epoch: *mut u64) -> c_int {
        if let Some(rc) = self.enter("daos_tx_hdl2epoch") {
            return rc;
        }
        self.daos.lock().tx_epoch(th, epoch)
    }

    unsafe fn dfs_mount(
        &self,
        poh: daos_handle_t,
        coh: daos_handle_t,
        flags: c_int,
        dfs: *mut *mut dfs_t,
    ) -> c_int {
        if let Some(rc) = self.enter("dfs_mount") {
            return rc;
        }
        if !self.dfs_ready.load(Ordering::SeqCst) {
            return libc::EACCES;
        }
        if dfs.is_null() {
            return libc::EINVAL;
        }
        match self.daos.cont_use(Some(poh), coh, 0) {
            Ok((true, ro)) => {
                if ro && flags & libc::O_ACCMODE == libc::O_RDWR {
                    return libc::EPERM;
                }
            }
            Ok((false, _)) | Err(_) => return libc::EINVAL,
        }
        let _ = self.daos.cont_use(Some(poh), coh, 1);
        let read_only = flags & libc::O_ACCMODE == libc::O_RDONLY;
        self.mounts.fetch_add(1, Ordering::SeqCst);
        // SAFETY: out-pointer from the caller.
        unsafe {
            *dfs = Box::into_raw(Box::new(MemMount {
                read_only,
                coh: coh.cookie,
            })) as *mut dfs_t
        };
        0
    }

    unsafe fn dfs_umount(&self, dfs: *mut dfs_t) -> c_int {
        if let Some(rc) = self.enter("dfs_umount") {
            return rc;
        }
        if dfs.is_null() {
            return libc::EINVAL;
        }
        // SAFETY: created by dfs_mount above, unmounted once.
        let m = unsafe { Box::from_raw(dfs as *mut MemMount) };
        let _ = self
            .daos
            .cont_use(None, daos_handle_t { cookie: m.coh }, -1);
        self.mounts.fetch_sub(1, Ordering::SeqCst);
        0
    }

    unsafe fn dfs_lookup(
        &self,
        dfs: *mut dfs_t,
        path: *const c_char,
        flags: c_int,
        obj: *mut *mut dfs_obj_t,
        mode: *mut mode_t,
        stbuf: *mut stat_t,
    ) -> c_int {
        if let Some(rc) = self.enter("dfs_lookup") {
            return rc;
        }
        let ro = tri!(unsafe { Self::mount_ro(dfs) });
        if ro && flags & libc::O_ACCMODE != libc::O_RDONLY {
            return libc::EPERM;
        }
        if path.is_null() || obj.is_null() {
            return libc::EINVAL;
        }
        // SAFETY: NUL-terminated path from the caller.
        let p = unsafe { CStr::from_ptr(path) }.to_bytes().to_vec();
        if p.first() != Some(&b'/') {
            return libc::EINVAL;
        }
        let st = self.state.lock().unwrap();
        // Walk the components so that a file in the middle gives ENOTDIR.
        let mut cur: Vec<u8> = b"/".to_vec();
        for comp in p.split(|b| *b == b'/').filter(|c| !c.is_empty()) {
            match st.entries.get(&cur) {
                Some(e) if e.mode & sys::S_IFMT == sys::S_IFDIR => {}
                Some(_) => return libc::ENOTDIR,
                None => return libc::ENOENT,
            }
            cur = child_path(&cur, comp);
        }
        let e = match st.entries.get(&cur) {
            Some(e) => e,
            None => return libc::ENOENT,
        };
        if !mode.is_null() {
            // SAFETY: out-pointer from the caller.
            unsafe { *mode = e.mode };
        }
        if !stbuf.is_null() {
            // SAFETY: out-pointer from the caller.
            Self::fill_stat(e, unsafe { &mut *stbuf });
        }
        let o = self.new_obj(cur, e.oid, e.mode, flags);
        // SAFETY: out-pointer from the caller.
        unsafe { *obj = o };
        0
    }

    unsafe fn dfs_open(
        &self,
        dfs: *mut dfs_t,
        parent: *mut dfs_obj_t,
        name: *const c_char,
        mode: mode_t,
        flags: c_int,
        _cid: u32,
        _chunk_size: u64,
        _value: *const c_char,
        obj: *mut *mut dfs_obj_t,
    ) -> c_int {
        if let Some(rc) = self.enter("dfs_open") {
            return rc;
        }
        let ro = tri!(unsafe { Self::mount_ro(dfs) });
        if flags & libc::O_APPEND != 0 {
            return libc::ENOTSUP;
        }
        if ro && flags & libc::O_CREAT != 0 {
            return libc::EPERM;
        }
        if obj.is_null() {
            return libc::EINVAL;
        }
        if mode & sys::S_IFMT != sys::S_IFREG {
            return libc::EINVAL; // only files are opened through this crate
        }
        let mut st = self.state.lock().unwrap();
        let dir = tri!(unsafe { self.parent_path(&st, parent) });
        let name = tri!(unsafe { Self::name_of(name) });
        let path = child_path(&dir, name);
        let oid = match st.entries.get_mut(&path) {
            Some(e) => {
                if flags & libc::O_CREAT != 0 && flags & libc::O_EXCL != 0 {
                    return libc::EEXIST;
                }
                if e.mode & sys::S_IFMT != sys::S_IFREG {
                    return libc::EINVAL;
                }
                if flags & libc::O_TRUNC != 0 {
                    if ro {
                        return libc::EPERM;
                    }
                    e.data.clear();
                }
                e.oid
            }
            None => {
                if flags & libc::O_CREAT == 0 {
                    return libc::ENOENT;
                }
                let oid = st.next_oid;
                st.next_oid += 1;
                st.entries.insert(
                    path.clone(),
                    Entry {
                        oid,
                        mode,
                        data: Vec::new(),
                        mtime: now_secs(),
                    },
                );
                oid
            }
        };
        let o = self.new_obj(path, oid, mode, flags);
        // SAFETY: out-pointer from the caller.
        unsafe { *obj = o };
        0
    }

    unsafe fn dfs_release(&self, obj: *mut dfs_obj_t) -> c_int {
        if let Some(rc) = self.enter("dfs_release") {
            return rc;
        }
        if obj.is_null() {
            return libc::EINVAL;
        }
        self.objects.fetch_sub(1, Ordering::SeqCst);
        // SAFETY: created by new_obj, released once by the crate.
        drop(unsafe { Box::from_raw(obj as *mut MemObj) });
        0
    }

    unsafe fn dfs_read(
        &self,
        dfs: *mut dfs_t,
        obj: *mut dfs_obj_t,
        sgl: *mut d_sg_list_t,
        off: u64,
        read_size: *mut u64,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("dfs_read") {
            return rc;
        }
        tri!(unsafe { Self::mount_ro(dfs) });
        if sgl.is_null() || read_size.is_null() {
            return libc::EINVAL;
        }
        let mut st = self.state.lock().unwrap();
        let (o, e) = tri!(unsafe { self.obj_entry(&mut st, obj) });
        if o.mode & sys::S_IFMT != sys::S_IFREG {
            return libc::EINVAL;
        }
        if o.flags & libc::O_ACCMODE == libc::O_WRONLY {
            return libc::EPERM;
        }
        // SAFETY: sgl from the caller describes sg_nr iovs.
        let sgl = unsafe { &*sgl };
        let mut pos = off as usize;
        let mut total = 0u64;
        for i in 0..sgl.sg_nr as usize {
            // SAFETY: i < sg_nr.
            let iov = unsafe { &*sgl.sg_iovs.add(i) };
            if iov.iov_len > iov.iov_buf_len {
                return libc::EINVAL;
            }
            let avail = e.data.len().saturating_sub(pos);
            let n = avail.min(iov.iov_len);
            if n > 0 {
                // SAFETY: iov_buf has iov_len >= n writable bytes.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        e.data.as_ptr().add(pos),
                        iov.iov_buf as *mut u8,
                        n,
                    )
                };
            }
            pos += n;
            total += n as u64;
        }
        // SAFETY: out-pointer from the caller.
        unsafe { *read_size = total };
        0
    }

    unsafe fn dfs_write(
        &self,
        dfs: *mut dfs_t,
        obj: *mut dfs_obj_t,
        sgl: *mut d_sg_list_t,
        off: u64,
        _ev: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("dfs_write") {
            return rc;
        }
        if tri!(unsafe { Self::mount_ro(dfs) }) {
            return libc::EPERM;
        }
        if sgl.is_null() {
            return libc::EINVAL;
        }
        let mut st = self.state.lock().unwrap();
        let (o, e) = tri!(unsafe { self.obj_entry(&mut st, obj) });
        if o.mode & sys::S_IFMT != sys::S_IFREG {
            return libc::EINVAL;
        }
        if o.flags & libc::O_ACCMODE == libc::O_RDONLY {
            return libc::EPERM;
        }
        // SAFETY: sgl from the caller describes sg_nr iovs.
        let sgl = unsafe { &*sgl };
        let mut pos = match usize::try_from(off) {
            Ok(p) if p <= MAX_FILE_SIZE => p,
            _ => return libc::EFBIG,
        };
        for i in 0..sgl.sg_nr as usize {
            // SAFETY: i < sg_nr.
            let iov = unsafe { &*sgl.sg_iovs.add(i) };
            if iov.iov_len > iov.iov_buf_len {
                return libc::EINVAL;
            }
            // SAFETY: iov_buf has iov_len readable bytes.
            let src = unsafe { std::slice::from_raw_parts(iov.iov_buf as *const u8, iov.iov_len) };
            let end = match pos.checked_add(src.len()) {
                Some(end) if end <= MAX_FILE_SIZE => end,
                _ => return libc::EFBIG,
            };
            if e.data.len() < end {
                e.data.resize(end, 0);
            }
            e.data[pos..end].copy_from_slice(src);
            pos += src.len();
        }
        e.mtime = now_secs();
        0
    }

    unsafe fn dfs_get_size(&self, dfs: *mut dfs_t, obj: *mut dfs_obj_t, size: *mut u64) -> c_int {
        if let Some(rc) = self.enter("dfs_get_size") {
            return rc;
        }
        tri!(unsafe { Self::mount_ro(dfs) });
        if size.is_null() {
            return libc::EINVAL;
        }
        let mut st = self.state.lock().unwrap();
        let (_, e) = tri!(unsafe { self.obj_entry(&mut st, obj) });
        // SAFETY: out-pointer from the caller.
        unsafe { *size = e.data.len() as u64 };
        0
    }

    unsafe fn dfs_iterate(
        &self,
        dfs: *mut dfs_t,
        obj: *mut dfs_obj_t,
        anchor: *mut daos_anchor_t,
        nr: *mut u32,
        size: usize,
        op: dfs_filler_cb_t,
        arg: *mut c_void,
    ) -> c_int {
        if let Some(rc) = self.enter("dfs_iterate") {
            return rc;
        }
        tri!(unsafe { Self::mount_ro(dfs) });
        if nr.is_null() || anchor.is_null() {
            return libc::EINVAL;
        }
        let op = match op {
            Some(f) => f,
            None => return libc::EINVAL,
        };
        let names: Vec<Vec<u8>> = {
            let st = self.state.lock().unwrap();
            if obj.is_null() {
                return libc::EINVAL;
            }
            // SAFETY: a pointer this backend created.
            let o = unsafe { &*(obj as *const MemObj) };
            if o.mode & sys::S_IFMT != sys::S_IFDIR {
                return libc::ENOTDIR;
            }
            // A removed directory lists only what was created through stale
            // handles of it, like the removed object in libdfs.
            let dir = match st.entries.get(&o.path) {
                Some(e) if e.oid == o.oid => o.path.clone(),
                _ => orphan_dir(o.oid),
            };
            st.entries
                .keys()
                .filter(|k| is_below(k, &dir) && parent_of(k) == dir.as_slice())
                .map(|k| k[k.iter().rposition(|b| *b == b'/').unwrap() + 1..].to_vec())
                .collect()
        };
        // SAFETY: in/out pointers from the caller.
        let (anchor, nr) = unsafe { (&mut *anchor, &mut *nr) };
        if size == 0 || *nr == 0 {
            return 0;
        }
        if self.iterate_stall.load(Ordering::SeqCst) {
            *nr = 0;
            return 0;
        }
        // The anchor holds the index of the next name.
        let start = anchor.da_sub_anchors as usize;
        let mut end = start;
        let mut bytes = 0usize;
        while end < names.len() && ((end - start) as u32) < *nr && bytes + names[end].len() < size {
            bytes += names[end].len() + 1;
            end += 1;
        }
        // Like DFS: the anchor moves past the whole batch before the callbacks.
        anchor.da_sub_anchors = end as u64;
        if end >= names.len() {
            anchor.da_type = sys::DAOS_ANCHOR_TYPE_EOF;
        }
        let null_name = self.iterate_null_name.load(Ordering::SeqCst);
        for (i, n) in names[start..end].iter().enumerate() {
            let mut buf = n.clone();
            buf.push(0);
            let p = if null_name && i == 0 {
                std::ptr::null()
            } else {
                buf.as_ptr() as *const c_char
            };
            // SAFETY: callback supplied by the crate; p is NULL (fault
            // injection) or a NUL-terminated buffer alive for the call.
            let rc = unsafe { op(dfs, obj, p, arg) };
            if rc != 0 {
                return rc; // like DFS: *nr is not updated
            }
        }
        *nr = (end - start) as u32;
        0
    }

    unsafe fn dfs_mkdir(
        &self,
        dfs: *mut dfs_t,
        parent: *mut dfs_obj_t,
        name: *const c_char,
        mode: mode_t,
        _cid: u32,
    ) -> c_int {
        if let Some(rc) = self.enter("dfs_mkdir") {
            return rc;
        }
        if tri!(unsafe { Self::mount_ro(dfs) }) {
            return libc::EPERM;
        }
        let mut st = self.state.lock().unwrap();
        let dir = tri!(unsafe { self.parent_path(&st, parent) });
        let name = tri!(unsafe { Self::name_of(name) });
        let path = child_path(&dir, name);
        if st.entries.contains_key(&path) {
            return libc::EEXIST;
        }
        let oid = st.next_oid;
        st.next_oid += 1;
        st.entries.insert(
            path,
            Entry {
                oid,
                mode: sys::S_IFDIR | mode,
                data: Vec::new(),
                mtime: now_secs(),
            },
        );
        0
    }

    unsafe fn dfs_remove(
        &self,
        dfs: *mut dfs_t,
        parent: *mut dfs_obj_t,
        name: *const c_char,
        force: bool,
        _oid: *mut daos_obj_id_t,
    ) -> c_int {
        if let Some(rc) = self.enter("dfs_remove") {
            return rc;
        }
        if tri!(unsafe { Self::mount_ro(dfs) }) {
            return libc::EPERM;
        }
        let mut st = self.state.lock().unwrap();
        let dir = tri!(unsafe { self.parent_path(&st, parent) });
        let name = tri!(unsafe { Self::name_of(name) });
        let path = child_path(&dir, name);
        let is_dir = match st.entries.get(&path) {
            Some(e) => e.mode & sys::S_IFMT == sys::S_IFDIR,
            None => return libc::ENOENT,
        };
        if is_dir {
            let below: Vec<Vec<u8>> = st
                .entries
                .keys()
                .filter(|k| is_below(k, &path))
                .cloned()
                .collect();
            if !below.is_empty() && !force {
                return libc::ENOTEMPTY;
            }
            for k in below {
                st.entries.remove(&k);
            }
        }
        st.entries.remove(&path);
        0
    }

    unsafe fn dfs_stat(
        &self,
        dfs: *mut dfs_t,
        parent: *mut dfs_obj_t,
        name: *const c_char,
        stbuf: *mut stat_t,
    ) -> c_int {
        if let Some(rc) = self.enter("dfs_stat") {
            return rc;
        }
        tri!(unsafe { Self::mount_ro(dfs) });
        if stbuf.is_null() {
            return libc::EINVAL;
        }
        let st = self.state.lock().unwrap();
        let path = if parent.is_null() && name.is_null() {
            b"/".to_vec()
        } else {
            let dir = tri!(unsafe { self.parent_path(&st, parent) });
            let name = tri!(unsafe { Self::name_of(name) });
            child_path(&dir, name)
        };
        match st.entries.get(&path) {
            // SAFETY: out-pointer from the caller.
            Some(e) => Self::fill_stat(e, unsafe { &mut *stbuf }),
            None => return libc::ENOENT,
        }
        0
    }

    unsafe fn dfs_ostat(&self, dfs: *mut dfs_t, obj: *mut dfs_obj_t, stbuf: *mut stat_t) -> c_int {
        if let Some(rc) = self.enter("dfs_ostat") {
            return rc;
        }
        tri!(unsafe { Self::mount_ro(dfs) });
        if stbuf.is_null() {
            return libc::EINVAL;
        }
        let mut st = self.state.lock().unwrap();
        let (_, e) = tri!(unsafe { self.obj_entry(&mut st, obj) });
        // SAFETY: out-pointer from the caller.
        Self::fill_stat(e, unsafe { &mut *stbuf });
        0
    }
}
