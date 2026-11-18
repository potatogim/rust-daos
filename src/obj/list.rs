// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Key enumeration: [`KeyIter`] and [`TxKeyIter`].
//!
//! Each refill is one `daos_obj_list_dkey` (or `_akey`, or `daos_kv_list`)
//! call with an anchor owned by the iterator. The order of keys is
//! unspecified (for classes with several groups the client lists group after
//! group, so even integer keys are not sorted): do not paginate by "last key
//! seen" and do not stop a prefix scan at the first key that does not match;
//! collect and sort when order matters. `query_key` finds the largest or
//! smallest integer key.
//!
//! Objects of erasure-code classes are not enumerable in this version
//! ([`Error::Unsupported`]): libdaos attaches memory to the anchor at the
//! first call and frees it only at the end of the enumeration, and there is
//! no public call to free it when an iterator is dropped early.
//!
//! An iterator inside a transaction borrows it and cannot leave its thread:
//!
//! ```compile_fail,E0277
//! fn send<T: Send>(_: T) {}
//! fn f(c: &daos::Container, o: &daos::obj::Object) {
//!     let tx = c.tx(daos::tx::TxMode::ReadOnly).unwrap();
//!     send(o.dkeys_in(&tx).unwrap());
//! }
//! ```
//!
//! ```compile_fail,E0505
//! fn f(c: &daos::Container, o: &daos::obj::Object) {
//!     let tx = c.tx(daos::tx::TxMode::ReadOnly).unwrap();
//!     let it = o.dkeys_in(&tx).unwrap();
//!     tx.close().unwrap();
//!     drop(it);
//! }
//! ```

use std::sync::Arc;

use super::handle::ObjInner;
use super::key::OwnedKey;
use crate::error::{Error, Result};
use crate::sys::{d_iov_t, d_sg_list_t, daos_anchor_t, daos_handle_t, daos_key_desc_t};
use crate::tx::{self, Tx};

/// Default keys per refill.
pub const DEFAULT_BATCH_KEYS: u32 = 128;
/// Default key buffer per refill.
pub const DEFAULT_BATCH_BYTES: usize = 64 << 10;
/// Default limit of the key buffer when a key does not fit.
pub const DEFAULT_MAX_KEY_BUFFER: usize = 16 << 20;
/// Consecutive `DER_KEY2BIG` results without progress before giving up.
const MAX_KEY2BIG_ROUNDS: u32 = 8;
/// Consecutive empty refills without the end before giving up.
const MAX_EMPTY_ROUNDS: u32 = 64;
const KEY2BIG: i32 = -2012;
const DER_IO: i32 = -2001;

/// What a cursor lists.
pub(crate) enum ListKind {
    Dkeys,
    Akeys(OwnedKey),
    KvKeys,
}

impl ListKind {
    fn op(&self) -> &'static str {
        match self {
            ListKind::Dkeys => "daos_obj_list_dkey",
            ListKind::Akeys(_) => "daos_obj_list_akey",
            ListKind::KvKeys => "daos_kv_list",
        }
    }
}

/// Refill logic shared by the iterator types.
pub(crate) struct Cursor {
    obj: Arc<ObjInner>,
    kind: ListKind,
    anchor: Box<daos_anchor_t>,
    kds: Vec<daos_key_desc_t>,
    buf: Vec<u8>,
    pub(crate) max_key_buffer: usize,
    /// Byte ranges of the keys of the current batch in `buf`.
    batch: Vec<(usize, usize)>,
    /// Batch sizes set after the iteration started, applied at the next
    /// refill (the current batch still points into `buf`).
    pending_batch: Option<(u32, usize)>,
    next: usize,
    done: bool,
}

impl Cursor {
    pub(crate) fn new(obj: Arc<ObjInner>, kind: ListKind) -> Result<Cursor> {
        obj.cont.stamp.check()?;
        if obj.oid.is_erasure_coded() {
            return Err(Error::Unsupported {
                what: "key enumeration of objects of an erasure-code class",
            });
        }
        Ok(Cursor {
            obj,
            kind,
            anchor: Box::new(daos_anchor_t::new()),
            kds: vec![daos_key_desc_t::default(); DEFAULT_BATCH_KEYS as usize],
            buf: vec![0; DEFAULT_BATCH_BYTES],
            max_key_buffer: DEFAULT_MAX_KEY_BUFFER,
            batch: Vec::new(),
            pending_batch: None,
            next: 0,
            done: false,
        })
    }

    /// New batch sizes. They take effect at the next refill, so keys of a
    /// batch already fetched are still returned from the buffer they are in.
    pub(crate) fn set_batch(&mut self, keys: u32, bytes: usize) {
        self.pending_batch = Some((keys.max(1), bytes.max(1)));
    }

    fn apply_batch(&mut self) {
        if let Some((keys, bytes)) = self.pending_batch.take() {
            self.kds = vec![daos_key_desc_t::default(); keys as usize];
            self.buf = vec![0; bytes];
        }
    }

    /// One list call. Returns the number of keys or the raw rc.
    fn call(&mut self, th: daos_handle_t) -> Result<(i32, u32)> {
        let mut nr = self.kds.len() as u32;
        let mut iov = d_iov_t {
            iov_buf: self.buf.as_mut_ptr().cast(),
            iov_buf_len: self.buf.len(),
            iov_len: self.buf.len(),
        };
        let mut sgl = d_sg_list_t {
            sg_nr: 1,
            sg_nr_out: 0,
            sg_iovs: &mut iov,
        };
        let obj = &self.obj;
        let _pass = obj.enter()?;
        let backend = obj.cont.backend.as_ref();
        // SAFETY: live handle (gate held); nr, kds, sgl and the buffer
        // describe memory owned by this cursor, alive for the blocking call;
        // the anchor is owned by the cursor and used for one enumeration
        // only; ev NULL.
        let rc = unsafe {
            match &self.kind {
                ListKind::Dkeys => backend.daos_obj_list_dkey(
                    obj.oh,
                    th,
                    &mut nr,
                    self.kds.as_mut_ptr(),
                    &mut sgl,
                    &mut *self.anchor,
                    std::ptr::null_mut(),
                ),
                ListKind::Akeys(dkey) => {
                    let mut d = dkey.iov();
                    backend.daos_obj_list_akey(
                        obj.oh,
                        th,
                        &mut d,
                        &mut nr,
                        self.kds.as_mut_ptr(),
                        &mut sgl,
                        &mut *self.anchor,
                        std::ptr::null_mut(),
                    )
                }
                ListKind::KvKeys => backend.daos_kv_list(
                    obj.oh,
                    th,
                    &mut nr,
                    self.kds.as_mut_ptr(),
                    &mut sgl,
                    &mut *self.anchor,
                    std::ptr::null_mut(),
                ),
            }
        };
        Ok((rc, nr))
    }

    /// Refill until a batch with at least one key arrives or the end is
    /// reached. Returns false at the end.
    fn refill(&mut self, tx: Option<&Tx<'_>>) -> Result<bool> {
        self.batch.clear();
        self.next = 0;
        self.apply_batch();
        if self.done {
            return Ok(false);
        }
        let th = tx::th(tx, &self.obj.cont, false)?;
        let op = self.kind.op();
        let mut key2big = 0;
        let mut empty = 0;
        loop {
            let (rc, nr) = self.call(th)?;
            tx::note(tx, rc);
            if rc == KEY2BIG {
                key2big += 1;
                if key2big > MAX_KEY2BIG_ROUNDS {
                    return Err(Error::daos(op, rc));
                }
                // The reported size is a hint only; the new capacity is
                // strictly larger than both the old one and the hint.
                let needed = self.kds[0].kd_key_len;
                let base = (self.buf.len() as u64).max(needed);
                let cap = base.checked_mul(2).and_then(|c| usize::try_from(c).ok());
                match cap {
                    Some(c) if c <= self.max_key_buffer => self.buf = vec![0; c],
                    _ => {
                        return Err(Error::BufferTooSmall {
                            op,
                            needed: needed.max(self.buf.len() as u64 + 1),
                        })
                    }
                }
                continue;
            }
            if rc != 0 {
                return Err(Error::daos(op, rc));
            }
            let nr = (nr as usize).min(self.kds.len());
            let mut off = 0usize;
            for kd in &self.kds[..nr] {
                let len = usize::try_from(kd.kd_key_len).unwrap_or(usize::MAX);
                let end = off.checked_add(len).filter(|e| *e <= self.buf.len());
                match end {
                    Some(end) => {
                        self.batch.push((off, end));
                        off = end;
                    }
                    None => return Err(Error::daos(op, DER_IO)),
                }
            }
            if self.anchor.is_eof() {
                self.done = true;
            }
            if !self.batch.is_empty() {
                return Ok(true);
            }
            if self.done {
                return Ok(false);
            }
            empty += 1;
            if empty >= MAX_EMPTY_ROUNDS {
                return Err(Error::daos(op, DER_IO));
            }
        }
    }

    pub(crate) fn next_key(&mut self, tx: Option<&Tx<'_>>) -> Option<Result<Vec<u8>>> {
        if self.next >= self.batch.len() {
            match self.refill(tx) {
                Ok(true) => {}
                Ok(false) => return None,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
        let (a, b) = self.batch[self.next];
        self.next += 1;
        Some(Ok(self.buf[a..b].to_vec()))
    }

    pub(crate) fn next_batch(&mut self, tx: Option<&Tx<'_>>) -> Result<Option<KeyBatch<'_>>> {
        // Keys of a partly consumed batch are returned first.
        if self.next >= self.batch.len() {
            match self.refill(tx) {
                Ok(true) => {}
                Ok(false) => return Ok(None),
                Err(e) => {
                    self.done = true;
                    return Err(e);
                }
            }
        }
        let start = self.next;
        self.next = self.batch.len();
        Ok(Some(KeyBatch {
            buf: &self.buf,
            ranges: &self.batch[start..],
            pos: 0,
        }))
    }
}

/// The keys of one refill, borrowed from the iterator's buffer.
#[derive(Debug)]
pub struct KeyBatch<'a> {
    buf: &'a [u8],
    ranges: &'a [(usize, usize)],
    pos: usize,
}

impl<'a> Iterator for KeyBatch<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let (a, b) = *self.ranges.get(self.pos)?;
        self.pos += 1;
        Some(&self.buf[a..b])
    }
}

impl KeyBatch<'_> {
    /// Keys left in this batch.
    pub fn remaining(&self) -> usize {
        self.ranges.len() - self.pos
    }
}

/// Iterator over keys without a transaction: owns its object handle, so it
/// is `'static` and `Send`. See the [module docs](crate::obj).
pub struct KeyIter {
    pub(crate) cur: Cursor,
}

/// Iterator over keys inside a transaction: borrows it, so it stays on the
/// transaction's thread and cannot outlive it.
pub struct TxKeyIter<'tx> {
    pub(crate) cur: Cursor,
    pub(crate) tx: &'tx Tx<'tx>,
}

macro_rules! iter_common {
    ($t:ty) => {
        impl $t {
            /// Keys and buffer bytes per refill (defaults 128 and 64 KiB).
            /// Set after the iteration started, the sizes apply from the
            /// next refill on.
            pub fn batch_size(mut self, keys: u32, bytes: usize) -> Self {
                self.cur.set_batch(keys, bytes);
                self
            }

            /// Limit of the key buffer when a key does not fit (default
            /// 16 MiB); beyond it the iteration ends with
            /// [`Error::BufferTooSmall`].
            pub fn max_key_buffer(mut self, bytes: usize) -> Self {
                self.cur.max_key_buffer = bytes;
                self
            }
        }
    };
}

iter_common!(KeyIter);
iter_common!(TxKeyIter<'_>);

impl KeyIter {
    /// Visit the keys of the next refill without allocating per key.
    /// `Ok(None)` at the end.
    pub fn next_batch(&mut self) -> Result<Option<KeyBatch<'_>>> {
        self.cur.next_batch(None)
    }
}

impl TxKeyIter<'_> {
    /// Visit the keys of the next refill without allocating per key.
    /// `Ok(None)` at the end.
    pub fn next_batch(&mut self) -> Result<Option<KeyBatch<'_>>> {
        let tx = self.tx;
        self.cur.next_batch(Some(tx))
    }
}

impl Iterator for KeyIter {
    type Item = Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.cur.next_key(None)
    }
}

impl Iterator for TxKeyIter<'_> {
    type Item = Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        let tx = self.tx;
        self.cur.next_key(Some(tx))
    }
}

impl std::fmt::Debug for KeyIter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyIter")
            .field("oid", &self.cur.obj.oid)
            .finish()
    }
}

impl std::fmt::Debug for TxKeyIter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TxKeyIter")
            .field("oid", &self.cur.obj.oid)
            .finish()
    }
}
