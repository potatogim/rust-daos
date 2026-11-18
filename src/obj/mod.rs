// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! DAOS objects: identifiers, classes and the object API.
//!
//! An object holds dkeys; each dkey holds akeys; each akey holds either a
//! single value or an array of fixed-size records. Opening any object id
//! succeeds, and a missing dkey or akey reads as size 0, so the getters
//! return `Option` and a miss is not an error. Conditional flags ([`Cond`])
//! turn a miss into `DER_NONEXIST` when that is wanted.
//!
//! Keys are `&[u8]`. For object types with integer keys
//! ([`ObjectType::dkey_is_u64`], [`ObjectType::akey_is_u64`]) a key is
//! exactly 8 bytes, built with [`u64_key`]; the crate copies it into
//! aligned storage for the call.
//!
//! # Buffers
//!
//! [`Update`] and [`Fetch`] take one dkey and up to [`MAX_AKEYS`] akeys.
//! Buffers are borrowed: the I/O descriptors point straight into the
//! caller's slices and nothing is copied. This is sound because every call
//! blocks and the crate never opens a transaction with `DAOS_TF_ZERO_COPY`.
//! A record fetch leaves the buffer untouched where no record exists, and
//! the reported size cannot tell: zero the buffer first when that matters.
//! A builder borrows its slices and transaction and is never `Send`.
//!
//! # Enumeration
//!
//! [`KeyIter`] (and [`TxKeyIter`] inside a transaction) lists keys with an
//! anchor owned by the iterator. The order is unspecified: collect and sort
//! when it matters; [`Object::query_key`] finds the extreme integer key.
//! Objects of an erasure-code class are not enumerable in this version
//! ([`Error::Unsupported`]): libdaos attaches memory to the anchor and frees
//! it only at the end of the enumeration, with no public call to free it when
//! an iterator is dropped early.
//!
//! ```no_run
//! # fn main() -> daos::Result<()> {
//! use daos::obj::{ClassHints, ObjectClass, ObjectType, OpenMode};
//! unsafe { daos::init(&daos::InitOptions::default())? };
//! let cont = daos::Container::open("tank", "objects")?;
//! let oids = cont.oid_allocator();
//! let oid = oids.next(ObjectType::MultiHashed, ObjectClass::UNKNOWN, ClassHints::default())?;
//! let obj = cont.object(oid, OpenMode::ReadWrite)?;
//! obj.put_single(None, b"user-1", b"name", b"Ada")?;
//! assert_eq!(obj.get_single_vec(None, b"user-1", b"name")?.as_deref(), Some(&b"Ada"[..]));
//! for dkey in obj.dkeys()? {
//!     println!("{:?}", dkey?);
//! }
//! # Ok(()) }
//! ```

mod alloc;
mod handle;
mod io;
mod key;
mod list;
mod oid;

pub use alloc::OidAllocator;
pub use io::{Cond, Fetch, FetchOutput, Recx, Update, MAX_AKEYS};
pub use key::{key_u64, u64_key};
pub use list::{
    KeyBatch, KeyIter, TxKeyIter, DEFAULT_BATCH_BYTES, DEFAULT_BATCH_KEYS, DEFAULT_MAX_KEY_BUFFER,
};
pub use oid::{ClassHints, ObjectClass, ObjectId, ObjectType, Redundancy, Sharding};

pub(crate) use handle::ObjInner;
use key::Key;
pub(crate) use list::{Cursor, ListKind};
#[cfg(test)]
pub(crate) use oid::probe_lines;

use std::fmt;
use std::sync::Arc;

use crate::conn::require;
use crate::cont::Container;
use crate::error::{Error, Result};
use crate::gate::Pass;
use crate::handles::Kind;
use crate::sys::api::Group;
use crate::sys::{self, daos_handle_t, daos_key_t, daos_recx_t};
use crate::tx::{self, Tx};

/// Open mode of an object (`DAOS_OO_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum OpenMode {
    /// Shared read (`DAOS_OO_RO`).
    ReadOnly,
    /// Shared read and write (`DAOS_OO_RW`).
    #[default]
    ReadWrite,
    /// Exclusive write (`DAOS_OO_RW | DAOS_OO_EXCL`).
    Exclusive,
}

impl OpenMode {
    pub(crate) fn bits(self) -> u32 {
        match self {
            OpenMode::ReadOnly => sys::DAOS_OO_RO,
            OpenMode::ReadWrite => sys::DAOS_OO_RW,
            OpenMode::Exclusive => sys::DAOS_OO_RW | sys::DAOS_OO_EXCL,
        }
    }
}

/// An open object (`daos_obj_open`). Cheap to clone; `Send + Sync`; every
/// call blocks. The last clone closes the handle (`daos_obj_close`) when it
/// drops; [`Object::close`] reports errors. After
/// [`Container::close`] every call returns [`Error::Closed`].
#[derive(Clone)]
pub struct Object {
    pub(crate) inner: Arc<ObjInner>,
}

impl fmt::Debug for Object {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Object")
            .field("oid", &self.inner.oid)
            .finish()
    }
}

/// Which extreme [`Object::query_key`] looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Extreme {
    /// The largest key or index (`DAOS_GET_MAX`).
    Max,
    /// The smallest (`DAOS_GET_MIN`).
    Min,
}

/// A key level of [`KeyQuery`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyLevel {
    /// Find the extreme key of this level.
    Find,
    /// Use this key.
    Given(u64),
}

/// Query of [`Object::query_key`]. Integer keys only (as `daos_obj_query_key`
/// documents). libdaos also accepts a given byte akey for the record extent
/// (the array size query does that); this version cannot express it, so the
/// extent of a byte akey of a `DkeyUint64`, `KvUint64` or array object
/// cannot be queried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct KeyQuery {
    /// Largest or smallest.
    pub which: Extreme,
    /// The dkey level.
    pub dkey: KeyLevel,
    /// The akey level; `None` when only the dkey is wanted.
    pub akey: Option<KeyLevel>,
    /// Also find the extreme record extent of the akey.
    pub recx: bool,
}

impl KeyQuery {
    /// Find the extreme dkey only.
    pub fn dkey(which: Extreme) -> KeyQuery {
        KeyQuery {
            which,
            dkey: KeyLevel::Find,
            akey: None,
            recx: false,
        }
    }

    /// Set the akey level.
    pub fn akey(mut self, level: KeyLevel) -> KeyQuery {
        self.akey = Some(level);
        self
    }

    /// Set the dkey level.
    pub fn with_dkey(mut self, level: KeyLevel) -> KeyQuery {
        self.dkey = level;
        self
    }

    /// Also find the extreme record extent (needs an akey level).
    pub fn recx(mut self, yes: bool) -> KeyQuery {
        self.recx = yes;
        self
    }
}

/// Result of [`Object::query_key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct KeyQueryResult {
    /// The dkey (found or given).
    pub dkey: u64,
    /// The akey, when an akey level was asked for.
    pub akey: Option<u64>,
    /// The extent, when asked for.
    pub recx: Option<Recx>,
}

const DER_NONEXIST: i32 = -1005;
const DER_REC2BIG: i32 = -2013;

impl Object {
    /// The object id.
    pub fn id(&self) -> ObjectId {
        self.inner.oid
    }

    /// The object handle cookie, for diagnostics and logs. Valid only while
    /// the object is open; must not be closed by the caller.
    pub fn as_raw(&self) -> u64 {
        self.inner.oh.cookie
    }

    /// (dkey is an integer, akey is an integer) for this object's type.
    pub(crate) fn key_types(&self) -> (bool, bool) {
        let ty = self.inner.oid.object_type();
        (
            ty.is_some_and(ObjectType::dkey_is_u64),
            ty.is_some_and(ObjectType::akey_is_u64),
        )
    }

    /// Start an update of `dkey` (see [`Update`]).
    pub fn update<'a>(&'a self, dkey: &'a [u8]) -> Update<'a> {
        Update::new(self, dkey)
    }

    /// Start a fetch of `dkey` (see [`Fetch`]).
    pub fn fetch<'a>(&'a self, dkey: &'a [u8]) -> Fetch<'a> {
        Fetch::new(self, dkey)
    }

    /// Write the single value of `dkey`/`akey`.
    pub fn put_single(
        &self,
        tx: Option<&Tx<'_>>,
        dkey: &[u8],
        akey: &[u8],
        value: &[u8],
    ) -> Result<()> {
        let u = self.update(dkey).single(akey, value);
        match tx {
            Some(t) => with_tx_update(u, t),
            None => u.execute(),
        }
    }

    /// Read the single value of `dkey`/`akey` into `buf` (one RPC). `None`
    /// when there is no value (absent and empty cannot be told apart),
    /// `Some(len)` otherwise. A value larger than `buf` is
    /// [`Error::BufferTooSmall`] with the needed size. An empty `buf` makes
    /// the call a size probe.
    pub fn get_single(
        &self,
        tx: Option<&Tx<'_>>,
        dkey: &[u8],
        akey: &[u8],
        buf: &mut [u8],
    ) -> Result<Option<usize>> {
        if buf.is_empty() {
            return match self.single_size(tx, dkey, akey)? {
                None => Ok(None),
                Some(n) => Err(Error::BufferTooSmall {
                    op: "daos_obj_fetch",
                    needed: n,
                }),
            };
        }
        let f = self.fetch(dkey).single(akey, buf);
        let out = match tx {
            Some(t) => with_tx_fetch(f, t)?,
            None => f.execute()?,
        };
        Ok(match out.sizes()[0] {
            0 => None,
            n => Some(n as usize),
        })
    }

    /// Read the single value into a new `Vec`: a size probe, then the read
    /// (repeated, up to 3 times, when the value grew in between).
    pub fn get_single_vec(
        &self,
        tx: Option<&Tx<'_>>,
        dkey: &[u8],
        akey: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        let mut size = match self.single_size(tx, dkey, akey)? {
            None => return Ok(None),
            Some(n) => n,
        };
        for _ in 0..3 {
            let len = usize::try_from(size)
                .map_err(|_| Error::InvalidArgument("value larger than the address space"))?;
            let mut v = vec![0u8; len];
            match self.get_single(tx, dkey, akey, &mut v) {
                Ok(None) => return Ok(None),
                Ok(Some(n)) => {
                    v.truncate(n);
                    return Ok(Some(v));
                }
                Err(Error::BufferTooSmall { needed, .. }) if needed > size => size = needed,
                Err(e) => return Err(e),
            }
        }
        Err(Error::BufferTooSmall {
            op: "daos_obj_fetch",
            needed: size,
        })
    }

    /// Size of the single value (`iod_size` with `DAOS_REC_ANY` and no
    /// buffer); `None` when there is no value.
    pub fn single_size(
        &self,
        tx: Option<&Tx<'_>>,
        dkey: &[u8],
        akey: &[u8],
    ) -> Result<Option<u64>> {
        let mut empty: [u8; 0] = [];
        let f = self.fetch(dkey).single(akey, &mut empty);
        let out = match tx {
            Some(t) => with_tx_fetch(f, t)?,
            None => f.execute()?,
        };
        Ok(match out.sizes()[0] {
            0 => None,
            n => Some(n),
        })
    }

    /// Write `data` as records of `record_size` bytes from index `start`.
    pub fn write_records(
        &self,
        tx: Option<&Tx<'_>>,
        dkey: &[u8],
        akey: &[u8],
        record_size: u64,
        start: u64,
        data: &[u8],
    ) -> Result<()> {
        let recx = [self.records_recx(record_size, start, data.len())?];
        let u = self.update(dkey).records(akey, record_size, &recx, data);
        match tx {
            Some(t) => with_tx_update(u, t),
            None => u.execute(),
        }
    }

    /// Read records of `record_size` bytes from index `start` into `buf`.
    /// Absent records leave their bytes of `buf` unchanged.
    pub fn read_records(
        &self,
        tx: Option<&Tx<'_>>,
        dkey: &[u8],
        akey: &[u8],
        record_size: u64,
        start: u64,
        buf: &mut [u8],
    ) -> Result<()> {
        let recx = [self.records_recx(record_size, start, buf.len())?];
        let f = self.fetch(dkey).records(akey, record_size, &recx, buf);
        match tx {
            Some(t) => with_tx_fetch(f, t).map(|_| ()),
            None => f.execute().map(|_| ()),
        }
    }

    fn records_recx(&self, record_size: u64, start: u64, len: usize) -> Result<Recx> {
        if record_size == 0 || len as u64 % record_size != 0 || len == 0 {
            return Err(Error::InvalidArgument(
                "the buffer length must be a non-zero multiple of the record size",
            ));
        }
        Ok(Recx::new(start, len as u64 / record_size))
    }

    /// The dkeys (see [`KeyIter`]). [`Error::Unsupported`] for objects of an
    /// erasure-code class.
    pub fn dkeys(&self) -> Result<KeyIter> {
        Ok(KeyIter {
            cur: Cursor::new(self.inner.clone(), ListKind::Dkeys)?,
        })
    }

    /// The akeys of `dkey`.
    pub fn akeys(&self, dkey: &[u8]) -> Result<KeyIter> {
        let k = Key::new(dkey, self.key_types().0)?.to_owned_key();
        Ok(KeyIter {
            cur: Cursor::new(self.inner.clone(), ListKind::Akeys(k))?,
        })
    }

    /// The dkeys as seen by a transaction.
    pub fn dkeys_in<'tx>(&self, tx: &'tx Tx<'tx>) -> Result<TxKeyIter<'tx>> {
        tx.handle_for(&self.inner.cont, false)?;
        Ok(TxKeyIter {
            cur: Cursor::new(self.inner.clone(), ListKind::Dkeys)?,
            tx,
        })
    }

    /// The akeys of `dkey` as seen by a transaction.
    pub fn akeys_in<'tx>(&self, tx: &'tx Tx<'tx>, dkey: &[u8]) -> Result<TxKeyIter<'tx>> {
        tx.handle_for(&self.inner.cont, false)?;
        let k = Key::new(dkey, self.key_types().0)?.to_owned_key();
        Ok(TxKeyIter {
            cur: Cursor::new(self.inner.clone(), ListKind::Akeys(k))?,
            tx,
        })
    }

    /// The checks every call makes first, in this order: fork stamp,
    /// handle state (the container gate, held until the pass drops), then
    /// the transaction.
    fn begin(&self, tx: Option<&Tx<'_>>, write: bool) -> Result<(daos_handle_t, Pass<'_>)> {
        self.inner.cont.stamp.check()?;
        let pass = self.inner.enter()?;
        let th = tx::th(tx, &self.inner.cont, write)?;
        Ok((th, pass))
    }

    fn call(
        &self,
        tx: Option<&Tx<'_>>,
        write: bool,
        f: impl FnOnce(daos_handle_t) -> i32,
    ) -> Result<i32> {
        let (th, _pass) = self.begin(tx, write)?;
        let rc = f(th);
        tx::note(tx, rc);
        Ok(rc)
    }

    /// Punch the whole object (`daos_obj_punch`).
    pub fn punch(&self, tx: Option<&Tx<'_>>) -> Result<()> {
        let inner = &self.inner;
        // SAFETY: live handle (gate held in `call`); ev NULL.
        let rc = self.call(tx, true, |th| unsafe {
            inner
                .cont
                .backend
                .daos_obj_punch(inner.oh, th, 0, std::ptr::null_mut())
        })?;
        ok("daos_obj_punch", rc)
    }

    /// Punch dkeys (`daos_obj_punch_dkeys`). An empty list makes no call
    /// (after the fork, handle and transaction checks).
    pub fn punch_dkeys(&self, tx: Option<&Tx<'_>>, dkeys: &[&[u8]]) -> Result<()> {
        let (th, _pass) = self.begin(tx, true)?;
        if dkeys.is_empty() {
            return Ok(());
        }
        let int = self.key_types().0;
        let keys: Vec<Key<'_>> = dkeys
            .iter()
            .map(|k| Key::new(k, int))
            .collect::<Result<_>>()?;
        let mut iovs: Vec<daos_key_t> = keys.iter().map(Key::iov).collect();
        let inner = &self.inner;
        // SAFETY: live handle (gate held by `_pass`); `iovs` point into
        // `keys` (aligned integers or the caller's bytes), alive for the
        // call; ev NULL.
        let rc = unsafe {
            inner.cont.backend.daos_obj_punch_dkeys(
                inner.oh,
                th,
                0,
                iovs.len() as u32,
                iovs.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        };
        tx::note(tx, rc);
        ok("daos_obj_punch_dkeys", rc)
    }

    /// Punch akeys of `dkey` (`daos_obj_punch_akeys`). An empty list makes
    /// no call (after the fork, handle and transaction checks).
    pub fn punch_akeys(&self, tx: Option<&Tx<'_>>, dkey: &[u8], akeys: &[&[u8]]) -> Result<()> {
        let (th, _pass) = self.begin(tx, true)?;
        if akeys.is_empty() {
            return Ok(());
        }
        let (dint, aint) = self.key_types();
        let d = Key::new(dkey, dint)?;
        let keys: Vec<Key<'_>> = akeys
            .iter()
            .map(|k| Key::new(k, aint))
            .collect::<Result<_>>()?;
        let mut iovs: Vec<daos_key_t> = keys.iter().map(Key::iov).collect();
        let mut div = d.iov();
        let inner = &self.inner;
        // SAFETY: as in punch_dkeys.
        let rc = unsafe {
            inner.cont.backend.daos_obj_punch_akeys(
                inner.oh,
                th,
                0,
                &mut div,
                iovs.len() as u32,
                iovs.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        };
        tx::note(tx, rc);
        ok("daos_obj_punch_akeys", rc)
    }

    /// Find the largest or smallest integer key, or record extent
    /// (`daos_obj_query_key`). `None` when the object (or the given key) has
    /// no data.
    pub fn query_key(&self, tx: Option<&Tx<'_>>, q: KeyQuery) -> Result<Option<KeyQueryResult>> {
        let (dint, aint) = self.key_types();
        let mut flags = match q.which {
            Extreme::Max => sys::DAOS_GET_MAX,
            Extreme::Min => sys::DAOS_GET_MIN,
        };
        if !dint {
            return Err(Error::InvalidArgument(
                "query_key needs an object type with integer dkeys",
            ));
        }
        // Keys received back go to aligned locals, never to caller memory.
        let mut dkey_val: u64 = match q.dkey {
            KeyLevel::Find => {
                flags |= sys::DAOS_GET_DKEY;
                0
            }
            KeyLevel::Given(v) => v,
        };
        let mut akey_val: u64 = 0;
        match q.akey {
            None if q.recx => {
                return Err(Error::InvalidArgument("a record query needs an akey level"))
            }
            None => {}
            Some(_) if !aint => {
                return Err(Error::InvalidArgument(
                    "an akey query needs an object type with integer akeys",
                ))
            }
            Some(KeyLevel::Find) => flags |= sys::DAOS_GET_AKEY,
            Some(KeyLevel::Given(v)) => akey_val = v,
        }
        if q.recx {
            flags |= sys::DAOS_GET_RECX;
        }
        let mut dkey = sys::d_iov_t {
            iov_buf: (&mut dkey_val as *mut u64).cast(),
            iov_buf_len: 8,
            iov_len: 8,
        };
        let mut akey = sys::d_iov_t {
            iov_buf: (&mut akey_val as *mut u64).cast(),
            iov_buf_len: 8,
            iov_len: 8,
        };
        let mut recx = daos_recx_t::default();
        let want_akey = q.akey.is_some();
        let inner = &self.inner;
        // SAFETY: live handle; the key iovs point to aligned u64 locals that
        // libdaos may overwrite with the found keys; recx is a local; ev
        // NULL.
        let rc = self.call(tx, false, |th| unsafe {
            inner.cont.backend.daos_obj_query_key(
                inner.oh,
                th,
                flags,
                &mut dkey,
                if want_akey {
                    &mut akey
                } else {
                    std::ptr::null_mut()
                },
                if q.recx {
                    &mut recx
                } else {
                    std::ptr::null_mut()
                },
                std::ptr::null_mut(),
            )
        })?;
        if rc == DER_NONEXIST {
            return Ok(None);
        }
        ok("daos_obj_query_key", rc)?;
        Ok(Some(KeyQueryResult {
            dkey: dkey_val,
            akey: want_akey.then_some(akey_val),
            recx: q.recx.then(|| Recx::new(recx.rx_idx, recx.rx_nr)),
        }))
    }

    /// The largest integer dkey, or `None` for an empty object.
    pub fn max_dkey_u64(&self, tx: Option<&Tx<'_>>) -> Result<Option<u64>> {
        Ok(self
            .query_key(tx, KeyQuery::dkey(Extreme::Max))?
            .map(|r| r.dkey))
    }

    /// The extent with the highest index of `dkey`/`akey` (integer keys).
    pub fn max_recx(&self, tx: Option<&Tx<'_>>, dkey: u64, akey: u64) -> Result<Option<Recx>> {
        let q = KeyQuery::dkey(Extreme::Max)
            .with_dkey(KeyLevel::Given(dkey))
            .akey(KeyLevel::Given(akey))
            .recx(true);
        Ok(self.query_key(tx, q)?.and_then(|r| r.recx))
    }

    /// Close the handle when this is the last clone, reporting errors.
    pub fn close(self) -> Result<()> {
        ObjInner::close(self.inner)
    }
}

fn ok(op: &'static str, rc: i32) -> Result<()> {
    match rc {
        0 => Ok(()),
        DER_REC2BIG => Err(Error::BufferTooSmall { op, needed: 0 }),
        rc => Err(Error::daos(op, rc)),
    }
}

// The builders take `&'a Tx<'a>`; the convenience methods take any
// `&Tx<'_>`, so the lifetimes are tied here.
fn with_tx_update(u: Update<'_>, t: &Tx<'_>) -> Result<()> {
    u.tx(t).execute()
}

fn with_tx_fetch(f: Fetch<'_>, t: &Tx<'_>) -> Result<FetchOutput> {
    f.tx(t).execute()
}

impl Container {
    /// Open an object (`daos_obj_open`). Any object type may be opened
    /// through the object API.
    pub fn object(&self, oid: ObjectId, mode: OpenMode) -> Result<Object> {
        require(self.backend(), Group::Object)?;
        let conn = &self.conn;
        let _pass = conn.enter()?;
        let mut oh = daos_handle_t::default();
        // SAFETY: live container handle (gate held); oid by value; out-pointer
        // to a local; ev NULL.
        let rc = unsafe {
            conn.backend.daos_obj_open(
                conn.coh,
                oid.raw(),
                mode.bits(),
                &mut oh,
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            return Err(Error::daos("daos_obj_open", rc));
        }
        Ok(Object {
            inner: Arc::new(ObjInner::new(conn.clone(), oh, oid, Kind::Object)),
        })
    }

    /// Reserve `count` object ids unique in this container
    /// (`daos_cont_alloc_oids`); returns the first. The values are for the
    /// `lo` half of an [`ObjectId`].
    pub fn alloc_oids(&self, count: u64) -> Result<u64> {
        require(self.backend(), Group::Object)?;
        if count == 0 {
            return Err(Error::InvalidArgument("count must not be 0"));
        }
        let _pass = self.conn.enter()?;
        let mut first = 0u64;
        // SAFETY: live container handle; out-pointer to a local; ev NULL.
        let rc = unsafe {
            self.backend()
                .daos_cont_alloc_oids(self.coh(), count, &mut first, std::ptr::null_mut())
        };
        if rc != 0 {
            return Err(Error::daos_at(
                "daos_cont_alloc_oids",
                rc,
                self.conn.target(),
            ));
        }
        Ok(first)
    }

    /// Encode type and class into an object id (`daos_obj_generate_oid`).
    /// `lo` and the low 32 bits of `hi` are the caller's; DAOS writes the
    /// upper 32 bits. With [`ObjectClass::UNKNOWN`] the class is chosen from
    /// the container's redundancy factor and the hints (the result differs
    /// between DAOS 2.6 and 2.8 defaults).
    pub fn generate_oid(
        &self,
        lo: u64,
        hi32: u32,
        ty: ObjectType,
        class: ObjectClass,
        hints: ClassHints,
    ) -> Result<ObjectId> {
        require(self.backend(), Group::Object)?;
        let _pass = self.conn.enter()?;
        let mut oid = sys::daos_obj_id_t {
            lo,
            hi: u64::from(hi32),
        };
        // SAFETY: live container handle; in/out pointer to a local.
        let rc = unsafe {
            self.backend().daos_obj_generate_oid2(
                self.coh(),
                &mut oid,
                ty.value(),
                class.id(),
                hints.bits(),
                0,
            )
        };
        if rc != 0 {
            return Err(Error::daos_at(
                "daos_obj_generate_oid",
                rc,
                self.conn.target(),
            ));
        }
        Ok(ObjectId::new(oid.lo, oid.hi))
    }

    /// An allocator of unique object ids for this container (ranges from
    /// `daos_cont_alloc_oids`, 1024 at a time).
    pub fn oid_allocator(&self) -> OidAllocator {
        OidAllocator::new(self.clone())
    }
}

// Send/Sync decisions (crate docs, "Threads").
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    const fn send<T: Send>() {}
    send_sync::<Object>();
    send_sync::<OidAllocator>();
    send::<KeyIter>();
};
