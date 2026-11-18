// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The libdaos side of the in-memory backend: pool and container handles,
//! containers with labels, properties and snapshots.
//!
//! Return values follow libdaos: 0 or a negative DER code. Names: pool
//! `"missing"` and containers whose name starts with `"missing"` do not
//! exist; a container name that is neither a created label nor a UUID
//! string of a created container exists implicitly on first open (POSIX,
//! unless the name starts with `"plain"`), until it is destroyed.

use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, c_int, CStr, CString};
use std::sync::Mutex;

use crate::sys::{self, daos_handle_t, daos_prop_entry, daos_prop_t};
use crate::uuid::Uuid;

pub(super) const DER_NO_PERM: c_int = 1001;
pub(super) const DER_NO_HDL: c_int = 1002;
pub(super) const DER_INVAL: c_int = 1003;
pub(super) const DER_EXIST: c_int = 1004;
pub(super) const DER_NONEXIST: c_int = 1005;
pub(super) const DER_BUSY: c_int = 1012;
pub(super) const DER_TRUNC: c_int = 1016;

/// Pool UUID the stand-in reports.
pub(super) const POOL_UUID: [u8; 16] = [
    0x6d, 0x6f, 0x63, 0x6b, 0x2d, 0x70, 0x6f, 0x6f, 0x6c, 0x2d, 0x75, 0x75, 0x69, 0x64, 0x00, 0x01,
];

pub(super) struct MemPool {
    pub(super) read_only: bool,
    pub(super) conts: usize,
}

pub(super) struct MemContHandle {
    pub(super) pool: u64,
    pub(super) container: [u8; 16],
    pub(super) read_only: bool,
    /// Mounts and objects using this handle.
    pub(super) users: usize,
}

pub(super) struct MemContainer {
    pub(super) uuid: [u8; 16],
    pub(super) label: Option<String>,
    pub(super) posix: bool,
    pub(super) props: Vec<(u32, u64)>,
    pub(super) snapshots: Vec<u64>,
    pub(super) handles: usize,
}

#[derive(Default)]
pub(super) struct DaosInner {
    next: u64,
    pub(super) pools: HashMap<u64, MemPool>,
    pub(super) conts: HashMap<u64, MemContHandle>,
    pub(super) containers: Vec<MemContainer>,
    destroyed: HashSet<String>,
    /// Property lists handed out by daos_prop_alloc and not yet freed.
    props: HashSet<usize>,
    next_epoch: u64,
    next_uuid: u64,
    pub(super) objs: super::objects::Objects,
}

impl DaosInner {
    pub(super) fn cookie(&mut self, base: u64) -> u64 {
        self.next += 1;
        base + self.next
    }

    fn new_uuid(&mut self) -> [u8; 16] {
        self.next_uuid += 1;
        let mut u = [0u8; 16];
        u[..8].copy_from_slice(b"memcont-");
        u[8..].copy_from_slice(&self.next_uuid.to_be_bytes());
        u
    }

    fn find(&self, name: &str) -> Option<usize> {
        if let Some(u) = Uuid::parse(name) {
            return self.containers.iter().position(|c| &c.uuid == u.as_bytes());
        }
        self.containers
            .iter()
            .position(|c| c.label.as_deref() == Some(name))
    }

    /// Resolve a container name, creating an implicit one when allowed.
    fn resolve(&mut self, name: &str) -> Result<usize, c_int> {
        if let Some(i) = self.find(name) {
            return Ok(i);
        }
        if name.starts_with("missing")
            || self.destroyed.contains(name)
            || Uuid::parse(name).is_some()
        {
            return Err(-DER_NONEXIST);
        }
        let uuid = self.new_uuid();
        self.containers.push(MemContainer {
            uuid,
            label: Some(name.to_string()),
            posix: !name.starts_with("plain"),
            props: Vec::new(),
            snapshots: Vec::new(),
            handles: 0,
        });
        Ok(self.containers.len() - 1)
    }

    pub(super) fn container_of(&mut self, coh: daos_handle_t) -> Result<&mut MemContainer, c_int> {
        let uuid = self.conts.get(&coh.cookie).ok_or(-DER_NO_HDL)?.container;
        self.containers
            .iter_mut()
            .find(|c| c.uuid == uuid)
            .ok_or(-DER_NONEXIST)
    }
}

/// libdaos state of the stand-in.
#[derive(Default)]
pub(super) struct Daos {
    pub(super) inner: Mutex<DaosInner>,
}

const POOL_BASE: u64 = 0x1000_0000_0000;
const CONT_BASE: u64 = 0x2000_0000_0000;

unsafe fn str_arg<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    // SAFETY: NUL-terminated string from the caller.
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

fn label_ok(l: &str) -> bool {
    !l.is_empty()
        && l.len() <= sys::DAOS_PROP_LABEL_MAX_LEN
        && l.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
        && Uuid::parse(l).is_none()
}

impl Daos {
    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, DaosInner> {
        self.inner.lock().unwrap()
    }

    pub(super) fn pool_handles(&self) -> usize {
        self.lock().pools.len()
    }

    pub(super) fn container_handles(&self) -> usize {
        self.lock().conts.len()
    }

    pub(super) fn prop_lists(&self) -> usize {
        self.lock().props.len()
    }

    pub(super) fn containers(&self) -> Vec<(Uuid, Option<String>, bool)> {
        self.lock()
            .containers
            .iter()
            .map(|c| (Uuid::from_bytes(c.uuid), c.label.clone(), c.posix))
            .collect()
    }

    pub(super) unsafe fn pool_connect(
        &self,
        pool: *const c_char,
        flags: u32,
        poh: *mut daos_handle_t,
        info: *mut sys::daos_pool_info_t,
    ) -> c_int {
        // SAFETY: from the caller.
        let name = match unsafe { str_arg(pool) } {
            Some(n) if !poh.is_null() => n,
            _ => return -DER_INVAL,
        };
        if name == "missing" {
            return -DER_NONEXIST;
        }
        let mut d = self.lock();
        let cookie = d.cookie(POOL_BASE);
        d.pools.insert(
            cookie,
            MemPool {
                read_only: flags & sys::DAOS_PC_RW == 0,
                conts: 0,
            },
        );
        // SAFETY: out-pointers from the caller.
        unsafe {
            *poh = daos_handle_t { cookie };
            if !info.is_null() {
                (*info).pi_uuid = POOL_UUID;
            }
        }
        0
    }

    pub(super) fn pool_disconnect(&self, poh: daos_handle_t) -> c_int {
        let mut d = self.lock();
        match d.pools.get(&poh.cookie) {
            None => -DER_NO_HDL,
            Some(p) if p.conts > 0 => -DER_BUSY,
            Some(_) => {
                d.pools.remove(&poh.cookie);
                0
            }
        }
    }

    pub(super) unsafe fn cont_open(
        &self,
        poh: daos_handle_t,
        cont: *const c_char,
        flags: u32,
        coh: *mut daos_handle_t,
        info: *mut sys::daos_cont_info_t,
    ) -> c_int {
        // SAFETY: from the caller.
        let name = match unsafe { str_arg(cont) } {
            Some(n) if !coh.is_null() => n,
            _ => return -DER_INVAL,
        };
        let mut d = self.lock();
        let pool_ro = match d.pools.get(&poh.cookie) {
            None => return -DER_NO_HDL,
            Some(p) => p.read_only,
        };
        if pool_ro && flags & sys::DAOS_COO_RW != 0 {
            return -DER_NO_PERM;
        }
        let i = match d.resolve(name) {
            Ok(i) => i,
            Err(rc) => return rc,
        };
        d.containers[i].handles += 1;
        let uuid = d.containers[i].uuid;
        let cookie = d.cookie(CONT_BASE);
        d.conts.insert(
            cookie,
            MemContHandle {
                pool: poh.cookie,
                container: uuid,
                read_only: flags & sys::DAOS_COO_RW == 0,
                users: 0,
            },
        );
        d.pools.get_mut(&poh.cookie).expect("checked above").conts += 1;
        // SAFETY: out-pointers from the caller.
        unsafe {
            *coh = daos_handle_t { cookie };
            if !info.is_null() {
                (*info).ci_uuid = uuid;
            }
        }
        0
    }

    pub(super) fn cont_close(&self, coh: daos_handle_t) -> c_int {
        let mut d = self.lock();
        let (pool, uuid) = match d.conts.get(&coh.cookie) {
            None => return -DER_NO_HDL,
            Some(c) if c.users > 0 => return -DER_BUSY,
            Some(c) => (c.pool, c.container),
        };
        d.conts.remove(&coh.cookie);
        if let Some(p) = d.pools.get_mut(&pool) {
            p.conts -= 1;
        }
        if let Some(c) = d.containers.iter_mut().find(|c| c.uuid == uuid) {
            c.handles = c.handles.saturating_sub(1);
        }
        0
    }

    /// For dfs_mount/dfs_umount and object opens: check the pair and adjust
    /// the user count. Returns (posix, read_only) or a negative DER code.
    pub(super) fn cont_use(
        &self,
        poh: Option<daos_handle_t>,
        coh: daos_handle_t,
        delta: isize,
    ) -> Result<(bool, bool), c_int> {
        let mut d = self.lock();
        let (pool, uuid, ro) = match d.conts.get(&coh.cookie) {
            None => return Err(-DER_NO_HDL),
            Some(c) => (c.pool, c.container, c.read_only),
        };
        if let Some(poh) = poh {
            if pool != poh.cookie || !d.pools.contains_key(&poh.cookie) {
                return Err(-DER_NO_HDL);
            }
        }
        let posix = d
            .containers
            .iter()
            .find(|c| c.uuid == uuid)
            .map(|c| c.posix)
            .ok_or(-DER_NONEXIST)?;
        let h = d.conts.get_mut(&coh.cookie).expect("checked above");
        h.users = h.users.saturating_add_signed(delta);
        Ok((posix, ro))
    }

    /// Create a container. `posix_props` is the `da_props` of a DFS create
    /// (must not carry layout, label or roots entries).
    pub(super) unsafe fn cont_create(
        &self,
        poh: daos_handle_t,
        label: *const c_char,
        prop: *mut daos_prop_t,
        uuid: *mut sys::uuid_t,
        posix: bool,
    ) -> c_int {
        let mut d = self.lock();
        match d.pools.get(&poh.cookie) {
            None => return -DER_NO_HDL,
            Some(p) if p.read_only => return -DER_NO_PERM,
            Some(_) => {}
        }
        let label = if label.is_null() {
            None
        } else {
            // SAFETY: from the caller.
            match unsafe { str_arg(label) } {
                Some(l) if label_ok(l) => Some(l.to_string()),
                _ => return -DER_INVAL,
            }
        };
        if let Some(l) = &label {
            if d.find(l).is_some() {
                return -DER_EXIST;
            }
        }
        let mut props = Vec::new();
        if !prop.is_null() {
            // SAFETY: a property list from the caller.
            let p = unsafe { &*prop };
            if p.dpp_nr > 0 && p.dpp_entries.is_null() {
                return -DER_INVAL;
            }
            for i in 0..p.dpp_nr as usize {
                // SAFETY: i < dpp_nr.
                let e = unsafe { &*p.dpp_entries.add(i) };
                let forbidden = [sys::DAOS_PROP_CO_LAYOUT_TYPE, sys::DAOS_PROP_CO_LABEL, 4113];
                if posix && forbidden.contains(&e.dpe_type) {
                    // A DFS create adds these itself (2.6.0 would duplicate).
                    return -DER_INVAL;
                }
                props.push((e.dpe_type, e.dpe_val));
            }
        }
        let layout = if posix {
            sys::DAOS_PROP_CO_LAYOUT_POSIX
        } else {
            props
                .iter()
                .find(|p| p.0 == sys::DAOS_PROP_CO_LAYOUT_TYPE)
                .map_or(0, |p| p.1)
        };
        props.retain(|p| p.0 != sys::DAOS_PROP_CO_LAYOUT_TYPE);
        props.push((sys::DAOS_PROP_CO_LAYOUT_TYPE, layout));
        let new_uuid = d.new_uuid();
        if let Some(l) = &label {
            d.destroyed.remove(l);
        }
        d.containers.push(MemContainer {
            uuid: new_uuid,
            label,
            posix,
            props,
            snapshots: Vec::new(),
            handles: 0,
        });
        if !uuid.is_null() {
            // SAFETY: 16-byte out-buffer from the caller.
            unsafe { *uuid = new_uuid };
        }
        0
    }

    pub(super) unsafe fn cont_destroy(
        &self,
        poh: daos_handle_t,
        cont: *const c_char,
        force: c_int,
    ) -> c_int {
        // SAFETY: from the caller.
        let name = match unsafe { str_arg(cont) } {
            Some(n) => n,
            None => return -DER_INVAL,
        };
        let mut d = self.lock();
        if !d.pools.contains_key(&poh.cookie) {
            return -DER_NO_HDL;
        }
        let i = match d.find(name) {
            Some(i) => i,
            None => return -DER_NONEXIST,
        };
        if d.containers[i].handles > 0 && force == 0 {
            return -DER_BUSY;
        }
        let c = d.containers.remove(i);
        if let Some(l) = c.label {
            d.destroyed.insert(l);
        }
        0
    }

    pub(super) unsafe fn cont_query(
        &self,
        coh: daos_handle_t,
        info: *mut sys::daos_cont_info_t,
        prop: *mut daos_prop_t,
    ) -> c_int {
        let mut d = self.lock();
        let handles = d.conts.values().filter(|h| {
            d.conts
                .get(&coh.cookie)
                .is_some_and(|me| me.container == h.container)
        });
        let nhandles = handles.count() as u32;
        if !prop.is_null() && !d.props.contains(&(prop as usize)) {
            panic!("daos_cont_query: property list not from daos_prop_alloc");
        }
        let c = match d.container_of(coh) {
            Ok(c) => c,
            Err(rc) => return rc,
        };
        if !info.is_null() {
            // SAFETY: out-pointer from the caller.
            let info = unsafe { &mut *info };
            info.ci_uuid = c.uuid;
            info.ci_nhandles = nhandles;
            info.ci_nsnapshots = c.snapshots.len() as u32;
            info.ci_lsnapshot = c.snapshots.last().copied().unwrap_or(0);
        }
        if !prop.is_null() {
            let mut entries: Vec<daos_prop_entry> = Vec::new();
            let entry = |t: u32, v: u64| daos_prop_entry {
                dpe_type: t,
                dpe_flags: 0,
                dpe_reserv: 0,
                dpe_val: v,
            };
            let label = c.label.clone().unwrap_or_default();
            entries.push(entry(
                sys::DAOS_PROP_CO_LABEL,
                CString::new(label).unwrap().into_raw() as usize as u64,
            ));
            entries.push(entry(
                sys::DAOS_PROP_CO_OWNER,
                CString::new("mock@").unwrap().into_raw() as usize as u64,
            ));
            let mut seen = HashSet::new();
            for (t, v) in c.props.iter().rev() {
                if seen.insert(*t) {
                    entries.push(entry(*t, *v));
                }
            }
            entries.push(entry(sys::DAOS_PROP_CO_STATUS, 0));
            entries.push(daos_prop_entry {
                dpe_flags: sys::DAOS_PROP_ENTRY_NOT_SET,
                ..entry(sys::DAOS_PROP_CO_SNAPSHOT_MAX, 0)
            });
            let entries = entries.into_boxed_slice();
            // SAFETY: the list from daos_prop_alloc (checked above).
            let p = unsafe { &mut *prop };
            p.dpp_nr = entries.len() as u32;
            p.dpp_entries = Box::into_raw(entries) as *mut daos_prop_entry;
        }
        0
    }

    pub(super) fn prop_alloc(&self, nr: u32) -> *mut daos_prop_t {
        assert_eq!(nr, 0, "the crate asks for all properties");
        let p = Box::into_raw(Box::new(daos_prop_t {
            dpp_nr: 0,
            dpp_reserv: 0,
            dpp_entries: std::ptr::null_mut(),
        }));
        self.lock().props.insert(p as usize);
        p
    }

    pub(super) unsafe fn prop_free(&self, prop: *mut daos_prop_t) {
        if prop.is_null() {
            return;
        }
        if !self.lock().props.remove(&(prop as usize)) {
            // A list built by the crate must never come here.
            panic!("daos_prop_free: foreign property list");
        }
        // SAFETY: allocated by prop_alloc and filled by cont_query.
        let p = unsafe { Box::from_raw(prop) };
        if !p.dpp_entries.is_null() {
            // SAFETY: a boxed slice of dpp_nr entries from cont_query.
            let entries = unsafe {
                Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                    p.dpp_entries,
                    p.dpp_nr as usize,
                ))
            };
            for e in entries.iter() {
                if matches!(
                    e.dpe_type,
                    sys::DAOS_PROP_CO_LABEL | sys::DAOS_PROP_CO_OWNER
                ) && e.dpe_val != 0
                {
                    // SAFETY: from CString::into_raw in cont_query.
                    drop(unsafe { CString::from_raw(e.dpe_val as usize as *mut c_char) });
                }
            }
        }
    }

    pub(super) fn create_snap(&self, coh: daos_handle_t, epoch: *mut u64) -> c_int {
        let mut d = self.lock();
        d.next_epoch += 1000;
        let e = d.next_epoch;
        let uuid = match d.container_of(coh) {
            Ok(c) => {
                c.snapshots.push(e);
                c.uuid
            }
            Err(rc) => return rc,
        };
        d.snapshot_objects(uuid, e);
        if !epoch.is_null() {
            // SAFETY: out-pointer from the caller.
            unsafe { *epoch = e };
        }
        0
    }

    pub(super) unsafe fn list_snap(
        &self,
        coh: daos_handle_t,
        nr: *mut c_int,
        epochs: *mut u64,
    ) -> c_int {
        let mut d = self.lock();
        let c = match d.container_of(coh) {
            Ok(c) => c,
            Err(rc) => return rc,
        };
        if nr.is_null() {
            return -DER_INVAL;
        }
        // SAFETY: in/out pointer and buffer of *nr epochs from the caller.
        unsafe {
            let cap = (*nr).max(0) as usize;
            if !epochs.is_null() {
                for (i, e) in c.snapshots.iter().take(cap).enumerate() {
                    *epochs.add(i) = *e;
                }
            }
            *nr = c.snapshots.len() as c_int;
        }
        0
    }

    pub(super) fn destroy_snap(&self, coh: daos_handle_t, epr: sys::daos_epoch_range_t) -> c_int {
        let mut d = self.lock();
        let c = match d.container_of(coh) {
            Ok(c) => c,
            Err(rc) => return rc,
        };
        let before = c.snapshots.len();
        c.snapshots
            .retain(|e| !(epr.epr_lo..=epr.epr_hi).contains(e));
        if c.snapshots.len() == before {
            return -DER_NONEXIST;
        }
        let uuid = c.uuid;
        d.drop_snapshots(uuid, epr.epr_lo, epr.epr_hi);
        0
    }

    pub(super) unsafe fn list_cont(
        &self,
        poh: daos_handle_t,
        ncont: *mut u64,
        cbuf: *mut sys::daos_pool_cont_info,
    ) -> c_int {
        let d = self.lock();
        if !d.pools.contains_key(&poh.cookie) {
            return -DER_NO_HDL;
        }
        if ncont.is_null() {
            return -DER_INVAL;
        }
        // SAFETY: in/out count and a buffer of *ncont entries from the caller.
        unsafe {
            let cap = *ncont as usize;
            let n = d.containers.len();
            *ncont = n as u64;
            if cbuf.is_null() {
                return 0;
            }
            if n > cap {
                return -DER_TRUNC;
            }
            for (i, c) in d.containers.iter().enumerate() {
                let out = &mut *cbuf.add(i);
                out.pci_uuid = c.uuid;
                out.pci_label = [0; sys::DAOS_PROP_LABEL_MAX_LEN + 1];
                if let Some(l) = &c.label {
                    for (j, b) in l.bytes().enumerate() {
                        out.pci_label[j] = b as c_char;
                    }
                }
            }
        }
        0
    }
}
