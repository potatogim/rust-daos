// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The object side of the in-memory backend: objects, key-value objects,
//! arrays, transactions and snapshots.
//!
//! Model: per container a map from object id to dkeys, akeys and values
//! (single values, or records by index). Transactions write to a copy of
//! the objects they touch and publish them at commit (last writer wins; a
//! conflict is simulated with fault injection on `daos_tx_commit`). A
//! snapshot is a copy of the container's objects. Arrays keep cells by index;
//! byte arrays zero holes and fill the read counters, other arrays leave
//! holes untouched and write garbage into the counters, so a crate that
//! interprets them fails a test.

#![allow(clippy::too_many_arguments)]

use std::collections::{BTreeMap, HashMap};
use std::ffi::{c_char, c_int, CStr};

use super::daos::{DaosInner, DER_EXIST, DER_INVAL, DER_NONEXIST, DER_NO_HDL, DER_NO_PERM};
use crate::sys::{
    self, d_iov_t, d_sg_list_t, daos_anchor_t, daos_array_iod_t, daos_handle_t, daos_iod_t,
    daos_key_desc_t, daos_obj_id_t, daos_recx_t,
};

pub(super) const DER_KEY2BIG: c_int = 2012;
pub(super) const DER_REC2BIG: c_int = 2013;
const OBJ_BASE: u64 = 0x3000_0000_0000;
const TX_BASE: u64 = 0x4000_0000_0000;
/// Counter value written into the counters of ordinary arrays.
pub(super) const GARBAGE: u64 = 0xDEAD_BEEF_DEAD_BEEF;

#[derive(Clone, Debug)]
pub(super) enum Value {
    Single(Vec<u8>),
    Records(u64, BTreeMap<u64, Vec<u8>>),
}

#[derive(Clone, Debug, Default)]
pub(super) struct ObjData {
    pub(super) dkeys: BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, Value>>,
    /// (cell size, chunk size) of an array made by daos_array_create.
    pub(super) array_meta: Option<(u64, u64)>,
    /// Cells of an array (cell index -> bytes), and its size in cells.
    pub(super) cells: BTreeMap<u64, Vec<u8>>,
    pub(super) size: u64,
    pub(super) epoch: u64,
}

pub(super) type Store = HashMap<(u64, u64), ObjData>;

#[derive(Default)]
pub(super) struct ContStore {
    pub(super) objects: Store,
    pub(super) snapshots: BTreeMap<u64, Store>,
    pub(super) next_oid: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ObjKind {
    Obj,
    Kv,
    Array,
}

pub(super) struct ObjHandle {
    pub(super) coh: u64,
    pub(super) cont: [u8; 16],
    pub(super) oid: (u64, u64),
    pub(super) read_only: bool,
    pub(super) kind: ObjKind,
    pub(super) cell: u64,
    pub(super) chunk: u64,
}

/// libdaos transaction states (`obj_tx.c`), as far as the mock needs them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum TxStatus {
    Open,
    Committed,
    Aborted,
    /// After `DER_TX_RESTART`: only restart or close.
    Failed,
}

const DER_ALREADY: c_int = 1008;
const DER_TX_RESTART: c_int = 2025;

pub(super) struct TxHandle {
    pub(super) cont: [u8; 16],
    pub(super) status: TxStatus,
    pub(super) read_only: bool,
    pub(super) snapshot: Option<u64>,
    /// Copies of the objects this transaction wrote.
    pub(super) writes: Store,
    pub(super) epoch: u64,
}

/// Object-side state, kept inside `DaosInner`.
#[derive(Default)]
pub(super) struct Objects {
    pub(super) conts: HashMap<[u8; 16], ContStore>,
    pub(super) handles: HashMap<u64, ObjHandle>,
    pub(super) txs: HashMap<u64, TxHandle>,
    pub(super) epoch: u64,
    /// Remaining forced KEY2BIG results of list calls, and the size to
    /// report (`None`: the buffer's own capacity, an exact fit).
    pub(super) key2big: (u32, Option<u64>),
}

fn otype(oid: (u64, u64)) -> u32 {
    (oid.1 >> 56) as u32
}

fn dkey_int(oid: (u64, u64)) -> bool {
    matches!(otype(oid), 2 | 4 | 9 | 11 | 12 | 13)
}

/// Bytes of a key iov. Integer keys must be 8 bytes at an aligned address.
unsafe fn key_bytes(iov: *const d_iov_t, int: bool) -> Result<Vec<u8>, c_int> {
    if iov.is_null() {
        return Err(-DER_INVAL);
    }
    // SAFETY: an iov from the caller.
    let iov = unsafe { &*iov };
    if iov.iov_buf.is_null() || iov.iov_len == 0 || iov.iov_len > iov.iov_buf_len {
        return Err(-DER_INVAL);
    }
    if int {
        if iov.iov_len != 8 {
            return Err(-DER_INVAL);
        }
        // libdaos reads *(uint64_t *)iov_buf: insist on alignment.
        assert_eq!(
            iov.iov_buf as usize % 8,
            0,
            "integer key passed at an unaligned address"
        );
    }
    // SAFETY: iov_len readable bytes.
    Ok(unsafe { std::slice::from_raw_parts(iov.iov_buf as *const u8, iov.iov_len) }.to_vec())
}

fn le_u64(k: &[u8]) -> u64 {
    u64::from_ne_bytes(k.try_into().unwrap_or([0; 8]))
}

impl DaosInner {
    fn obj_handle(&self, oh: daos_handle_t) -> Result<&ObjHandle, c_int> {
        self.objs.handles.get(&oh.cookie).ok_or(-DER_NO_HDL)
    }

    /// Check a transaction handle for an object handle; returns
    /// (is_tx, read_only, snapshot).
    fn tx_check(
        &self,
        th: daos_handle_t,
        h_cont: [u8; 16],
    ) -> Result<Option<(bool, Option<u64>)>, c_int> {
        if th.cookie == 0 {
            return Ok(None);
        }
        let t = self.objs.txs.get(&th.cookie).ok_or(-DER_NO_HDL)?;
        if t.cont != h_cont {
            return Err(-DER_INVAL);
        }
        // libdaos refuses I/O in a transaction that is not open.
        if t.status != TxStatus::Open {
            return Err(-DER_NO_PERM);
        }
        Ok(Some((t.read_only, t.snapshot)))
    }

    /// The object as a read sees it.
    fn read_view(&self, th: daos_handle_t, cont: [u8; 16], oid: (u64, u64)) -> Option<&ObjData> {
        if th.cookie != 0 {
            if let Some(t) = self.objs.txs.get(&th.cookie) {
                if let Some(e) = t.snapshot {
                    return self.objs.conts.get(&cont)?.snapshots.get(&e)?.get(&oid);
                }
                if let Some(o) = t.writes.get(&oid) {
                    return Some(o);
                }
            }
        }
        self.objs.conts.get(&cont)?.objects.get(&oid)
    }

    /// The object for a write: the transaction's copy, or the container's.
    fn write_view(&mut self, th: daos_handle_t, cont: [u8; 16], oid: (u64, u64)) -> &mut ObjData {
        self.objs.epoch += 1;
        let epoch = self.objs.epoch;
        let current = self
            .objs
            .conts
            .entry(cont)
            .or_default()
            .objects
            .get(&oid)
            .cloned();
        let o = if th.cookie != 0 {
            let t = self
                .objs
                .txs
                .get_mut(&th.cookie)
                .expect("checked by tx_check");
            t.writes
                .entry(oid)
                .or_insert_with(|| current.unwrap_or_default())
        } else {
            self.objs
                .conts
                .entry(cont)
                .or_default()
                .objects
                .entry(oid)
                .or_default()
        };
        o.epoch = epoch;
        o
    }

    pub(super) fn cont_uuid(&self, coh: daos_handle_t) -> Result<[u8; 16], c_int> {
        Ok(self.conts.get(&coh.cookie).ok_or(-DER_NO_HDL)?.container)
    }

    pub(super) fn obj_open(
        &mut self,
        coh: daos_handle_t,
        oid: daos_obj_id_t,
        mode: u32,
        oh: *mut daos_handle_t,
        kind: ObjKind,
        attr: Option<(u64, u64)>,
    ) -> c_int {
        let (cont, cont_ro) = match self.conts.get(&coh.cookie) {
            None => return -DER_NO_HDL,
            Some(c) => (c.container, c.read_only),
        };
        let read_only = mode & sys::DAOS_OO_RW == 0;
        if cont_ro && !read_only {
            return -DER_NO_PERM;
        }
        let key = (oid.lo, oid.hi);
        let (mut cell, mut chunk) = attr.unwrap_or((0, 0));
        if kind == ObjKind::Array && attr.is_none() {
            match self
                .objs
                .conts
                .get(&cont)
                .and_then(|c| c.objects.get(&key))
                .and_then(|o| o.array_meta)
            {
                Some((c, k)) => {
                    cell = c;
                    chunk = k;
                }
                None => return -DER_NONEXIST,
            }
        }
        let cookie = self.cookie(OBJ_BASE);
        self.objs.handles.insert(
            cookie,
            ObjHandle {
                coh: coh.cookie,
                cont,
                oid: key,
                read_only,
                kind,
                cell,
                chunk,
            },
        );
        if let Some(c) = self.conts.get_mut(&coh.cookie) {
            c.users += 1;
        }
        // SAFETY: out-pointer from the caller.
        unsafe { *oh = daos_handle_t { cookie } };
        0
    }

    pub(super) fn obj_close(&mut self, oh: daos_handle_t, kind: ObjKind) -> c_int {
        match self.objs.handles.get(&oh.cookie) {
            Some(h) if h.kind == kind => {}
            Some(_) => return -DER_INVAL,
            None => return -DER_NO_HDL,
        }
        let h = self.objs.handles.remove(&oh.cookie).expect("checked above");
        if let Some(c) = self.conts.get_mut(&h.coh) {
            c.users -= 1;
        }
        0
    }

    /// Common checks of a data call: handle, transaction, write permission.
    fn data_call(
        &self,
        oh: daos_handle_t,
        th: daos_handle_t,
        write: bool,
    ) -> Result<([u8; 16], (u64, u64)), c_int> {
        let h = self.obj_handle(oh)?;
        let tx = self.tx_check(th, h.cont)?;
        if write {
            if h.read_only {
                return Err(-DER_NO_PERM);
            }
            if let Some((ro, _)) = tx {
                if ro {
                    return Err(-DER_NO_PERM);
                }
            }
        }
        Ok((h.cont, h.oid))
    }

    pub(super) unsafe fn obj_update(
        &mut self,
        oh: daos_handle_t,
        th: daos_handle_t,
        flags: u64,
        dkey: *mut d_iov_t,
        nr: u32,
        iods: *mut daos_iod_t,
        sgls: *mut d_sg_list_t,
    ) -> c_int {
        let (cont, oid) = match self.data_call(oh, th, true) {
            Ok(v) => v,
            Err(rc) => return rc,
        };
        if nr == 0 || iods.is_null() || sgls.is_null() {
            return -DER_INVAL;
        }
        // SAFETY: the caller's descriptors.
        let d = match unsafe { key_bytes(dkey, dkey_int(oid)) } {
            Ok(d) => d,
            Err(rc) => return rc,
        };
        let aint = matches!(otype(oid), 3 | 4);
        let exists_d = self
            .read_view(th, cont, oid)
            .is_some_and(|o| o.dkeys.contains_key(&d));
        if flags & sys::DAOS_COND_DKEY_INSERT != 0 && exists_d {
            return -DER_EXIST;
        }
        if flags & sys::DAOS_COND_DKEY_UPDATE != 0 && !exists_d {
            return -DER_NONEXIST;
        }
        // Collect first, then apply (all or nothing).
        let mut puts = Vec::new();
        for i in 0..nr as usize {
            // SAFETY: nr iods and sgls from the caller.
            let (iod, sgl) = unsafe { (&*iods.add(i), &*sgls.add(i)) };
            let a = match unsafe { key_bytes(&iod.iod_name, aint) } {
                Ok(a) => a,
                Err(rc) => return rc,
            };
            let exists_a = self
                .read_view(th, cont, oid)
                .and_then(|o| o.dkeys.get(&d))
                .is_some_and(|m| m.contains_key(&a));
            if flags & sys::DAOS_COND_AKEY_INSERT != 0 && exists_a {
                return -DER_EXIST;
            }
            if flags & sys::DAOS_COND_AKEY_UPDATE != 0 && !exists_a {
                return -DER_NONEXIST;
            }
            let data = match unsafe { sgl_bytes(sgl) } {
                Ok(b) => b,
                Err(rc) => return rc,
            };
            match iod.iod_type {
                sys::DAOS_IOD_SINGLE => {
                    if iod.iod_size as usize != data.len() {
                        return -DER_INVAL;
                    }
                    puts.push((a, None, data));
                }
                sys::DAOS_IOD_ARRAY => {
                    // SAFETY: iod_nr extents from the caller.
                    let recxs = unsafe { recx_list(iod) };
                    let total: u64 = recxs.iter().map(|r| r.rx_nr).sum();
                    if iod.iod_size == 0 || total * iod.iod_size != data.len() as u64 {
                        return -DER_INVAL;
                    }
                    puts.push((a, Some((iod.iod_size, recxs)), data));
                }
                _ => return -DER_INVAL,
            }
        }
        let o = self.write_view(th, cont, oid);
        let akeys = o.dkeys.entry(d).or_default();
        for (a, recs, data) in puts {
            match recs {
                None => {
                    akeys.insert(a, Value::Single(data));
                }
                Some((size, recxs)) => {
                    let v = akeys
                        .entry(a)
                        .or_insert_with(|| Value::Records(size, BTreeMap::new()));
                    if let Value::Single(_) = v {
                        *v = Value::Records(size, BTreeMap::new());
                    }
                    if let Value::Records(_, m) = v {
                        let mut off = 0usize;
                        for r in recxs {
                            for k in 0..r.rx_nr {
                                let s = size as usize;
                                m.insert(r.rx_idx + k, data[off..off + s].to_vec());
                                off += s;
                            }
                        }
                    }
                }
            }
        }
        0
    }

    pub(super) unsafe fn obj_fetch(
        &mut self,
        oh: daos_handle_t,
        th: daos_handle_t,
        flags: u64,
        dkey: *mut d_iov_t,
        nr: u32,
        iods: *mut daos_iod_t,
        sgls: *mut d_sg_list_t,
    ) -> c_int {
        let (cont, oid) = match self.data_call(oh, th, false) {
            Ok(v) => v,
            Err(rc) => return rc,
        };
        if nr == 0 || iods.is_null() {
            return -DER_INVAL;
        }
        // SAFETY: the caller's descriptors.
        let d = match unsafe { key_bytes(dkey, dkey_int(oid)) } {
            Ok(d) => d,
            Err(rc) => return rc,
        };
        let aint = matches!(otype(oid), 3 | 4);
        let akeys = self
            .read_view(th, cont, oid)
            .and_then(|o| o.dkeys.get(&d))
            .cloned();
        if flags & sys::DAOS_COND_DKEY_FETCH != 0 && akeys.is_none() {
            return -DER_NONEXIST;
        }
        let mut rc = 0;
        for i in 0..nr as usize {
            // SAFETY: nr iods (and sgls when not NULL) from the caller.
            let iod = unsafe { &mut *iods.add(i) };
            let a = match unsafe { key_bytes(&iod.iod_name, aint) } {
                Ok(a) => a,
                Err(e) => return e,
            };
            let v = akeys.as_ref().and_then(|m| m.get(&a));
            if flags & sys::DAOS_COND_AKEY_FETCH != 0 && v.is_none() {
                return -DER_NONEXIST;
            }
            match iod.iod_type {
                sys::DAOS_IOD_SINGLE => {
                    let value: &[u8] = match v {
                        Some(Value::Single(b)) => b,
                        _ => &[],
                    };
                    iod.iod_size = value.len() as u64;
                    if !sgls.is_null() && !value.is_empty() {
                        // SAFETY: sgl i from the caller.
                        let sgl = unsafe { &mut *sgls.add(i) };
                        match unsafe { sgl_capacity(sgl) } {
                            Ok(cap) if cap >= value.len() => unsafe { sgl_write(sgl, value) },
                            Ok(_) => rc = -DER_REC2BIG,
                            Err(e) => return e,
                        }
                    }
                }
                sys::DAOS_IOD_ARRAY => {
                    // SAFETY: iod_nr extents from the caller.
                    let recxs = unsafe { recx_list(iod) };
                    let (size, m) = match v {
                        Some(Value::Records(s, m)) => (*s, Some(m)),
                        _ => (iod.iod_size, None),
                    };
                    if m.is_none() {
                        iod.iod_size = 0;
                    } else if iod.iod_size != 0 && iod.iod_size != size {
                        return -DER_INVAL;
                    } else {
                        iod.iod_size = size;
                    }
                    if sgls.is_null() || m.is_none() {
                        continue;
                    }
                    let m = m.expect("checked");
                    // SAFETY: sgl i from the caller.
                    let sgl = unsafe { &mut *sgls.add(i) };
                    let mut off = 0usize;
                    for r in recxs {
                        for k in 0..r.rx_nr {
                            if let Some(rec) = m.get(&(r.rx_idx + k)) {
                                // Absent records leave the buffer untouched.
                                unsafe { sgl_write_at(sgl, off, rec) };
                            }
                            off += size as usize;
                        }
                    }
                }
                _ => return -DER_INVAL,
            }
        }
        rc
    }

    pub(super) unsafe fn obj_punch(
        &mut self,
        oh: daos_handle_t,
        th: daos_handle_t,
        dkeys: Option<&[d_iov_t]>,
        dkey_for_akeys: Option<*mut d_iov_t>,
        akeys: Option<&[d_iov_t]>,
    ) -> c_int {
        let (cont, oid) = match self.data_call(oh, th, true) {
            Ok(v) => v,
            Err(rc) => return rc,
        };
        let dint = dkey_int(oid);
        let aint = matches!(otype(oid), 3 | 4);
        let mut dk = Vec::new();
        for k in dkeys.unwrap_or(&[]) {
            match unsafe { key_bytes(k, dint) } {
                Ok(b) => dk.push(b),
                Err(rc) => return rc,
            }
        }
        let parent = match dkey_for_akeys {
            Some(p) => match unsafe { key_bytes(p, dint) } {
                Ok(b) => Some(b),
                Err(rc) => return rc,
            },
            None => None,
        };
        let mut ak = Vec::new();
        for k in akeys.unwrap_or(&[]) {
            match unsafe { key_bytes(k, aint) } {
                Ok(b) => ak.push(b),
                Err(rc) => return rc,
            }
        }
        let o = self.write_view(th, cont, oid);
        match (dkeys, parent) {
            (None, None) => {
                o.dkeys.clear();
                o.cells.clear();
                o.size = 0;
            }
            (Some(_), _) => {
                for d in dk {
                    o.dkeys.remove(&d);
                }
            }
            (None, Some(p)) => {
                if let Some(m) = o.dkeys.get_mut(&p) {
                    for a in ak {
                        m.remove(&a);
                    }
                    if m.is_empty() {
                        o.dkeys.remove(&p);
                    }
                }
            }
        }
        0
    }

    /// List dkeys (`akeys_of: None`), akeys of a dkey, or KV keys.
    pub(super) unsafe fn obj_list(
        &mut self,
        oh: daos_handle_t,
        th: daos_handle_t,
        akeys_of: Option<*mut d_iov_t>,
        nr: *mut u32,
        kds: *mut daos_key_desc_t,
        sgl: *mut d_sg_list_t,
        anchor: *mut daos_anchor_t,
    ) -> c_int {
        let (cont, oid) = match self.data_call(oh, th, false) {
            Ok(v) => v,
            Err(rc) => return rc,
        };
        if nr.is_null() || kds.is_null() || sgl.is_null() || anchor.is_null() {
            return -DER_INVAL;
        }
        let keys: Vec<Vec<u8>> = {
            let view = self.read_view(th, cont, oid);
            match akeys_of {
                None => view
                    .map(|o| o.dkeys.keys().cloned().collect())
                    .unwrap_or_default(),
                Some(d) => {
                    let d = match unsafe { key_bytes(d, dkey_int(oid)) } {
                        Ok(d) => d,
                        Err(rc) => return rc,
                    };
                    view.and_then(|o| o.dkeys.get(&d))
                        .map(|m| m.keys().cloned().collect())
                        .unwrap_or_default()
                }
            }
        };
        // SAFETY: in/out pointers and buffers from the caller.
        let (nr, anchor, sgl) = unsafe { (&mut *nr, &mut *anchor, &mut *sgl) };
        let cap = match unsafe { sgl_capacity(sgl) } {
            Ok(c) => c,
            Err(rc) => return rc,
        };
        let start = anchor.da_sub_anchors as usize;
        if self.objs.key2big.0 > 0 {
            self.objs.key2big.0 -= 1;
            let report = self.objs.key2big.1.unwrap_or(cap as u64);
            // SAFETY: at least one descriptor.
            unsafe { (*kds).kd_key_len = report };
            *nr = 0;
            return -DER_KEY2BIG;
        }
        let mut n = 0usize;
        let mut off = 0usize;
        let mut end = start;
        while end < keys.len() && n < *nr as usize {
            let k = &keys[end];
            if off + k.len() > cap {
                if n == 0 {
                    // SAFETY: at least one descriptor.
                    unsafe { (*kds).kd_key_len = k.len() as u64 };
                    *nr = 0;
                    return -DER_KEY2BIG;
                }
                break;
            }
            // SAFETY: descriptor n < *nr; the key fits the buffer.
            unsafe {
                (*kds.add(n)).kd_key_len = k.len() as u64;
                sgl_write_at(sgl, off, k);
            }
            off += k.len();
            n += 1;
            end += 1;
        }
        anchor.da_sub_anchors = end as u64;
        if end >= keys.len() {
            anchor.da_type = sys::DAOS_ANCHOR_TYPE_EOF;
        }
        *nr = n as u32;
        0
    }

    pub(super) unsafe fn obj_query_key(
        &mut self,
        oh: daos_handle_t,
        th: daos_handle_t,
        flags: u64,
        dkey: *mut d_iov_t,
        akey: *mut d_iov_t,
        recx: *mut daos_recx_t,
    ) -> c_int {
        let (cont, oid) = match self.data_call(oh, th, false) {
            Ok(v) => v,
            Err(rc) => return rc,
        };
        if dkey.is_null() {
            return -DER_INVAL;
        }
        let max = flags & sys::DAOS_GET_MAX != 0;
        let o = match self.read_view(th, cont, oid) {
            Some(o) => o.clone(),
            None => return -DER_NONEXIST,
        };
        let pick = |it: &mut dyn Iterator<Item = u64>| if max { it.max() } else { it.min() };
        // SAFETY: the caller's key iovs point to aligned u64 storage.
        let d = if flags & sys::DAOS_GET_DKEY != 0 {
            match pick(&mut o.dkeys.keys().map(|k| le_u64(k))) {
                Some(v) => v,
                None => return -DER_NONEXIST,
            }
        } else {
            unsafe { *((*dkey).iov_buf as *const u64) }
        };
        unsafe { *((*dkey).iov_buf as *mut u64) = d };
        let akeys = match o.dkeys.get(d.to_ne_bytes().as_slice()) {
            Some(m) => m,
            None => return -DER_NONEXIST,
        };
        if akey.is_null() {
            return 0;
        }
        let a = if flags & sys::DAOS_GET_AKEY != 0 {
            match pick(&mut akeys.keys().map(|k| le_u64(k))) {
                Some(v) => v,
                None => return -DER_NONEXIST,
            }
        } else {
            unsafe { *((*akey).iov_buf as *const u64) }
        };
        unsafe { *((*akey).iov_buf as *mut u64) = a };
        if flags & sys::DAOS_GET_RECX != 0 && !recx.is_null() {
            let idx = match akeys.get(a.to_ne_bytes().as_slice()) {
                Some(Value::Records(_, m)) => pick(&mut m.keys().copied()),
                _ => None,
            };
            match idx {
                Some(i) => unsafe {
                    *recx = daos_recx_t {
                        rx_idx: i,
                        rx_nr: 1,
                    }
                },
                None => return -DER_NONEXIST,
            }
        }
        0
    }

    // -- KV ------------------------------------------------------------------

    pub(super) unsafe fn kv_put(
        &mut self,
        oh: daos_handle_t,
        th: daos_handle_t,
        flags: u64,
        key: *const c_char,
        size: u64,
        buf: *const u8,
    ) -> c_int {
        let (cont, oid) = match self.data_call(oh, th, true) {
            Ok(v) => v,
            Err(rc) => return rc,
        };
        if key.is_null() || (size > 0 && buf.is_null()) {
            return -DER_INVAL;
        }
        // SAFETY: NUL-terminated key and `size` bytes from the caller.
        let k = unsafe { CStr::from_ptr(key) }.to_bytes().to_vec();
        let v = unsafe { std::slice::from_raw_parts(buf, size as usize) }.to_vec();
        let exists = self
            .read_view(th, cont, oid)
            .is_some_and(|o| o.dkeys.contains_key(&k));
        if flags & sys::DAOS_COND_KEY_INSERT != 0 && exists {
            return -DER_EXIST;
        }
        let o = self.write_view(th, cont, oid);
        o.dkeys
            .entry(k)
            .or_default()
            .insert(b"0".to_vec(), Value::Single(v));
        0
    }

    pub(super) unsafe fn kv_get(
        &mut self,
        oh: daos_handle_t,
        th: daos_handle_t,
        key: *const c_char,
        size: *mut u64,
        buf: *mut u8,
    ) -> c_int {
        let (cont, oid) = match self.data_call(oh, th, false) {
            Ok(v) => v,
            Err(rc) => return rc,
        };
        if key.is_null() || size.is_null() {
            return -DER_INVAL;
        }
        // SAFETY: from the caller.
        let k = unsafe { CStr::from_ptr(key) }.to_bytes().to_vec();
        let v: Vec<u8> = match self
            .read_view(th, cont, oid)
            .and_then(|o| o.dkeys.get(&k))
            .and_then(|m| m.get(&b"0"[..]))
        {
            Some(Value::Single(v)) => v.clone(),
            _ => Vec::new(),
        };
        // SAFETY: in/out size and a buffer of *size bytes.
        unsafe {
            let cap = *size;
            *size = v.len() as u64;
            if buf.is_null() || cap == 0 {
                return 0;
            }
            if (v.len() as u64) > cap {
                return -DER_REC2BIG;
            }
            std::ptr::copy_nonoverlapping(v.as_ptr(), buf, v.len());
        }
        0
    }

    pub(super) unsafe fn kv_remove(
        &mut self,
        oh: daos_handle_t,
        th: daos_handle_t,
        key: *const c_char,
    ) -> c_int {
        let (cont, oid) = match self.data_call(oh, th, true) {
            Ok(v) => v,
            Err(rc) => return rc,
        };
        if key.is_null() {
            return -DER_INVAL;
        }
        // SAFETY: from the caller.
        let k = unsafe { CStr::from_ptr(key) }.to_bytes().to_vec();
        self.write_view(th, cont, oid).dkeys.remove(&k);
        0
    }

    pub(super) fn obj_destroy(&mut self, oh: daos_handle_t, th: daos_handle_t) -> c_int {
        let (cont, oid) = match self.data_call(oh, th, true) {
            Ok(v) => v,
            Err(rc) => return rc,
        };
        let o = self.write_view(th, cont, oid);
        *o = ObjData::default();
        0
    }

    // -- arrays --------------------------------------------------------------

    pub(super) fn array_create(
        &mut self,
        coh: daos_handle_t,
        oid: daos_obj_id_t,
        cell: u64,
        chunk: u64,
        oh: *mut daos_handle_t,
    ) -> c_int {
        let cont = match self.cont_uuid(coh) {
            Ok(c) => c,
            Err(rc) => return rc,
        };
        if otype((oid.lo, oid.hi)) != 11 || cell == 0 || chunk == 0 {
            return -DER_INVAL;
        }
        let objs = &mut self.objs.conts.entry(cont).or_default().objects;
        let o = objs.entry((oid.lo, oid.hi)).or_default();
        if o.array_meta.is_some() {
            return -DER_EXIST;
        }
        o.array_meta = Some((cell, chunk));
        self.obj_open(
            coh,
            oid,
            sys::DAOS_OO_RW,
            oh,
            ObjKind::Array,
            Some((cell, chunk)),
        )
    }

    fn array_handle(&self, oh: daos_handle_t) -> Result<(u64, bool), c_int> {
        let h = self.obj_handle(oh)?;
        if h.kind != ObjKind::Array {
            return Err(-DER_INVAL);
        }
        Ok((h.cell, otype(h.oid) == 13))
    }

    pub(super) unsafe fn array_io(
        &mut self,
        oh: daos_handle_t,
        th: daos_handle_t,
        iod: *mut daos_array_iod_t,
        sgl: *mut d_sg_list_t,
        write: bool,
    ) -> c_int {
        let (cell, byte) = match self.array_handle(oh) {
            Ok(v) => v,
            Err(rc) => return rc,
        };
        let (cont, oid) = match self.data_call(oh, th, write) {
            Ok(v) => v,
            Err(rc) => return rc,
        };
        // SAFETY: the caller's descriptors.
        let iod = unsafe { &mut *iod };
        assert!(
            iod.arr_nr > 0,
            "array call with zero ranges reached the backend"
        );
        let rgs = unsafe { std::slice::from_raw_parts(iod.arr_rgs, iod.arr_nr as usize) }.to_vec();
        let sgl = unsafe { &mut *sgl };
        let cs = cell as usize;
        if write {
            let data = match unsafe { sgl_bytes(sgl) } {
                Ok(d) => d,
                Err(rc) => return rc,
            };
            let o = self.write_view(th, cont, oid);
            let mut off = 0usize;
            for r in rgs {
                for k in 0..r.rg_len {
                    o.cells.insert(r.rg_idx + k, data[off..off + cs].to_vec());
                    off += cs;
                    o.size = o.size.max(r.rg_idx + k + 1);
                }
            }
            return 0;
        }
        let o = self.read_view(th, cont, oid).cloned().unwrap_or_default();
        // Byte arrays: the size check runs without the transaction.
        let size = if byte {
            self.objs
                .conts
                .get(&cont)
                .and_then(|c| c.objects.get(&oid))
                .map_or(0, |o| o.size)
        } else {
            o.size
        };
        let (mut read, mut short) = (0u64, 0u64);
        let mut off = 0usize;
        for r in rgs {
            for k in 0..r.rg_len {
                let idx = r.rg_idx + k;
                match o.cells.get(&idx) {
                    Some(c) => unsafe { sgl_write_at(sgl, off, c) },
                    None if byte && idx < size => unsafe { sgl_write_at(sgl, off, &vec![0u8; cs]) },
                    None => {}
                }
                if idx < size {
                    read += 1;
                } else {
                    short += 1;
                }
                off += cs;
            }
        }
        if byte {
            iod.arr_nr_read = read;
            iod.arr_nr_short_read = short;
        } else {
            iod.arr_nr_read = GARBAGE;
            iod.arr_nr_short_read = GARBAGE;
        }
        0
    }

    pub(super) fn array_size(
        &mut self,
        oh: daos_handle_t,
        th: daos_handle_t,
        set: Option<u64>,
    ) -> Result<(u64, u64), c_int> {
        self.array_handle(oh)?;
        let (cont, oid) = self.data_call(oh, th, set.is_some())?;
        match set {
            Some(n) => {
                let o = self.write_view(th, cont, oid);
                o.cells.retain(|k, _| *k < n);
                o.size = n;
                Ok((n, o.epoch))
            }
            None => Ok(self
                .read_view(th, cont, oid)
                .map_or((0, 0), |o| (o.size, o.epoch))),
        }
    }

    pub(super) unsafe fn array_punch(
        &mut self,
        oh: daos_handle_t,
        th: daos_handle_t,
        iod: *mut daos_array_iod_t,
    ) -> c_int {
        if let Err(rc) = self.array_handle(oh) {
            return rc;
        }
        let (cont, oid) = match self.data_call(oh, th, true) {
            Ok(v) => v,
            Err(rc) => return rc,
        };
        // SAFETY: from the caller.
        let iod = unsafe { &*iod };
        assert!(
            iod.arr_nr > 0,
            "array punch with zero ranges reached the backend"
        );
        let rgs = unsafe { std::slice::from_raw_parts(iod.arr_rgs, iod.arr_nr as usize) }.to_vec();
        let o = self.write_view(th, cont, oid);
        for r in rgs {
            for k in 0..r.rg_len {
                o.cells.remove(&(r.rg_idx + k));
            }
        }
        0
    }

    // -- ids -------------------------------------------------------------------

    pub(super) fn alloc_oids(&mut self, coh: daos_handle_t, num: u64, out: *mut u64) -> c_int {
        let cont = match self.cont_uuid(coh) {
            Ok(c) => c,
            Err(rc) => return rc,
        };
        let s = self.objs.conts.entry(cont).or_default();
        let first = s.next_oid.max(1);
        s.next_oid = first + num;
        // SAFETY: out-pointer from the caller.
        unsafe { *out = first };
        0
    }

    // -- transactions ------------------------------------------------------------

    pub(super) fn tx_open(
        &mut self,
        coh: daos_handle_t,
        flags: u64,
        snapshot: Option<u64>,
        th: *mut daos_handle_t,
    ) -> c_int {
        assert_eq!(
            flags & sys::DAOS_TF_ZERO_COPY,
            0,
            "DAOS_TF_ZERO_COPY must never be passed"
        );
        let cont = match self.cont_uuid(coh) {
            Ok(c) => c,
            Err(rc) => return rc,
        };
        if let Some(e) = snapshot {
            let has = self
                .objs
                .conts
                .get(&cont)
                .is_some_and(|c| c.snapshots.contains_key(&e));
            if !has {
                return -DER_NONEXIST;
            }
        }
        self.objs.epoch += 1;
        let epoch = self.objs.epoch;
        let cookie = self.cookie(TX_BASE);
        self.objs.txs.insert(
            cookie,
            TxHandle {
                cont,
                status: TxStatus::Open,
                read_only: flags & sys::DAOS_TF_RDONLY != 0 || snapshot.is_some(),
                snapshot,
                writes: Store::new(),
                epoch,
            },
        );
        // SAFETY: out-pointer from the caller.
        unsafe { *th = daos_handle_t { cookie } };
        0
    }

    pub(super) fn tx_end(&mut self, th: daos_handle_t, op: &str) -> c_int {
        let t = match self.objs.txs.get_mut(&th.cookie) {
            Some(t) => t,
            None => return -DER_NO_HDL,
        };
        // State rules of libdaos (dc_tx_commit, dc_tx_abort,
        // dc_tx_restart_begin; the same in 2.6.4 and 2.8.1).
        match (op, t.status) {
            ("commit", TxStatus::Committed) | ("abort", TxStatus::Aborted) => return -DER_ALREADY,
            ("commit" | "abort", s) if s != TxStatus::Open => return -DER_NO_PERM,
            ("restart", s) if s != TxStatus::Failed => return -DER_NO_PERM,
            _ => {}
        }
        match op {
            "commit" => {
                t.status = TxStatus::Committed;
                let writes = std::mem::take(&mut t.writes);
                let cont = t.cont;
                let objs = &mut self.objs.conts.entry(cont).or_default().objects;
                for (k, v) in writes {
                    objs.insert(k, v);
                }
            }
            "abort" => {
                t.status = TxStatus::Aborted;
                t.writes.clear();
            }
            "restart" => {
                t.status = TxStatus::Open;
                t.writes.clear();
            }
            "close" => {
                self.objs.txs.remove(&th.cookie);
            }
            _ => return -DER_INVAL,
        }
        0
    }

    /// An injected commit result `rc`: the state change libdaos makes for
    /// it (`DER_TX_RESTART`: may restart; other errors: aborted).
    pub(super) fn tx_commit_failed(&mut self, th: daos_handle_t, rc: c_int) {
        if let Some(t) = self.objs.txs.get_mut(&th.cookie) {
            if rc != 0 && t.status == TxStatus::Open {
                t.writes.clear();
                t.status = if rc == -DER_TX_RESTART {
                    TxStatus::Failed
                } else {
                    TxStatus::Aborted
                };
            }
        }
    }

    /// A call of transaction `th` returned `DER_TX_RESTART`.
    pub(super) fn tx_restart_needed(&mut self, th: daos_handle_t) {
        if let Some(t) = self.objs.txs.get_mut(&th.cookie) {
            if t.status == TxStatus::Open {
                t.status = TxStatus::Failed;
            }
        }
    }

    pub(super) fn tx_epoch(&self, th: daos_handle_t, epoch: *mut u64) -> c_int {
        match self.objs.txs.get(&th.cookie) {
            None => -DER_NO_HDL,
            Some(t) => {
                // SAFETY: out-pointer from the caller.
                unsafe { *epoch = t.snapshot.unwrap_or(t.epoch) };
                0
            }
        }
    }

    /// A snapshot copies the container's objects.
    pub(super) fn snapshot_objects(&mut self, cont: [u8; 16], epoch: u64) {
        let s = self.objs.conts.entry(cont).or_default();
        let copy = s.objects.clone();
        s.snapshots.insert(epoch, copy);
    }

    pub(super) fn drop_snapshots(&mut self, cont: [u8; 16], lo: u64, hi: u64) {
        if let Some(s) = self.objs.conts.get_mut(&cont) {
            s.snapshots.retain(|e, _| !(lo..=hi).contains(e));
        }
    }
}

unsafe fn recx_list(iod: &daos_iod_t) -> Vec<daos_recx_t> {
    if iod.iod_recxs.is_null() || iod.iod_nr == 0 {
        return Vec::new();
    }
    // SAFETY: iod_nr extents from the caller.
    unsafe { std::slice::from_raw_parts(iod.iod_recxs, iod.iod_nr as usize) }.to_vec()
}

unsafe fn sgl_capacity(sgl: &d_sg_list_t) -> Result<usize, c_int> {
    if sgl.sg_iovs.is_null() {
        return Err(-DER_INVAL);
    }
    let mut cap = 0;
    for i in 0..sgl.sg_nr as usize {
        // SAFETY: sg_nr iovs.
        let iov = unsafe { &*sgl.sg_iovs.add(i) };
        if iov.iov_len > iov.iov_buf_len {
            return Err(-DER_INVAL);
        }
        cap += iov.iov_buf_len;
    }
    Ok(cap)
}

unsafe fn sgl_bytes(sgl: &d_sg_list_t) -> Result<Vec<u8>, c_int> {
    if sgl.sg_iovs.is_null() {
        return Err(-DER_INVAL);
    }
    let mut v = Vec::new();
    for i in 0..sgl.sg_nr as usize {
        // SAFETY: sg_nr iovs with iov_len readable bytes.
        let iov = unsafe { &*sgl.sg_iovs.add(i) };
        if iov.iov_len > iov.iov_buf_len {
            return Err(-DER_INVAL);
        }
        v.extend_from_slice(unsafe {
            std::slice::from_raw_parts(iov.iov_buf as *const u8, iov.iov_len)
        });
    }
    Ok(v)
}

unsafe fn sgl_write(sgl: &mut d_sg_list_t, data: &[u8]) {
    // SAFETY: capacity checked by the caller.
    unsafe { sgl_write_at(sgl, 0, data) };
    sgl.sg_nr_out = sgl.sg_nr;
}

/// Write `data` at byte `off` of the concatenated iovs.
unsafe fn sgl_write_at(sgl: &mut d_sg_list_t, mut off: usize, data: &[u8]) {
    let mut src = data;
    for i in 0..sgl.sg_nr as usize {
        if src.is_empty() {
            break;
        }
        // SAFETY: sg_nr iovs with iov_buf_len writable bytes.
        let iov = unsafe { &mut *sgl.sg_iovs.add(i) };
        if off >= iov.iov_buf_len {
            off -= iov.iov_buf_len;
            continue;
        }
        let n = (iov.iov_buf_len - off).min(src.len());
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), (iov.iov_buf as *mut u8).add(off), n)
        };
        src = &src[n..];
        off = 0;
    }
}
