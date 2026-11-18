// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions transcribed from DAOS headers, BSD-2-Clause-Patent, see LICENSE-THIRD-PARTY

//! Flat key-value objects (`daos_kv_*`): [`Kv`].
//!
//! Keys of this API are C strings: not empty, no NUL byte; `&str` in,
//! `String` out. Only [`ObjectType::KvHashed`] and [`ObjectType::KvLexical`]
//! objects are opened as `Kv`. A [`ObjectType::KvUint64`] object has 8-byte
//! integer dkeys, which a C string cannot express; use it through
//! [`Object`](crate::obj::Object) with dkey [`u64_key`](crate::obj::u64_key)
//! and akey [`KV_AKEY`], a single value per key: that is the layout
//! `daos_kv_*` writes.
//!
//! An absent key and a key with an empty value cannot be told apart: both
//! read as `None`. Empty values are therefore refused by [`Kv::put`].
//! Removing an absent key succeeds (DAOS 2.6.4 and 2.8.1).
//!
//! Keys stored by other clients need not be UTF-8. The iterators return such
//! a key as [`Error::NotUtf8`] carrying its bytes, and continue with the
//! next key; the `&str` methods cannot name it. Read or remove it through
//! [`Object`](crate::obj::Object) with the key bytes as dkey (`daos_kv_*`
//! stores the C string without its NUL) and akey [`KV_AKEY`]; list all keys
//! as bytes with [`KvKeys::next_batch`].
//!
//! The keys of a transaction stay on its thread and cannot outlive it:
//!
//! ```compile_fail,E0277
//! fn send<T: Send>(_: T) {}
//! fn f(c: &daos::Container, kv: &daos::Kv) {
//!     let tx = c.tx(daos::tx::TxMode::ReadOnly).unwrap();
//!     send(kv.keys_in(&tx).unwrap());
//! }
//! ```
//!
//! ```compile_fail,E0505
//! fn f(c: &daos::Container, kv: &daos::Kv) {
//!     let tx = c.tx(daos::tx::TxMode::ReadOnly).unwrap();
//!     let keys = kv.keys_in(&tx).unwrap();
//!     tx.close().unwrap();
//!     drop(keys);
//! }
//! ```

use std::ffi::CString;
use std::fmt;
use std::sync::Arc;

use crate::conn::require;
use crate::cont::Container;
use crate::error::{Error, Result};
use crate::handles::Kind;
use crate::obj::{Cursor, KeyBatch, ListKind, ObjInner, ObjectId, ObjectType, OpenMode};
use crate::sys::api::Group;
use crate::sys::{self, daos_handle_t};
use crate::tx::{self, Tx};

/// The akey under which `daos_kv_*` stores every value (`"0"`).
pub const KV_AKEY: &[u8] = b"0";

const DER_EXIST: i32 = -1004;
const DER_REC2BIG: i32 = -2013;

/// An open key-value object. Cheap to clone; `Send + Sync`; every call
/// blocks.
#[derive(Clone)]
pub struct Kv {
    inner: Arc<ObjInner>,
}

impl fmt::Debug for Kv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Kv").field("oid", &self.inner.oid).finish()
    }
}

fn ckey(key: &str) -> Result<CString> {
    if key.is_empty() {
        return Err(Error::InvalidArgument("keys must not be empty"));
    }
    CString::new(key).map_err(|_| Error::InvalidArgument("keys must not contain NUL"))
}

impl Kv {
    /// The object id.
    pub fn id(&self) -> ObjectId {
        self.inner.oid
    }

    /// The object handle cookie, for diagnostics and logs.
    pub fn as_raw(&self) -> u64 {
        self.inner.oh.cookie
    }

    fn call(
        &self,
        tx: Option<&Tx<'_>>,
        write: bool,
        f: impl FnOnce(daos_handle_t) -> i32,
    ) -> Result<i32> {
        self.inner.cont.stamp.check()?;
        let th = tx::th(tx, &self.inner.cont, write)?;
        let _pass = self.inner.enter()?;
        let rc = f(th);
        tx::note(tx, rc);
        Ok(rc)
    }

    fn put_flags(&self, tx: Option<&Tx<'_>>, key: &str, value: &[u8], flags: u64) -> Result<i32> {
        let k = ckey(key)?;
        if value.is_empty() {
            return Err(Error::InvalidArgument(
                "empty values read like absent keys; use remove",
            ));
        }
        let inner = &self.inner;
        // SAFETY: live handle (gate held in `call`); NUL-terminated key and
        // the caller's value, alive for the blocking call; libdaos only reads
        // them; ev NULL.
        self.call(tx, true, |th| unsafe {
            inner.cont.backend.daos_kv_put(
                inner.oh,
                th,
                flags,
                k.as_ptr(),
                value.len() as u64,
                value.as_ptr().cast(),
                std::ptr::null_mut(),
            )
        })
    }

    /// Store `value` under `key` (`daos_kv_put`).
    pub fn put(&self, tx: Option<&Tx<'_>>, key: &str, value: &[u8]) -> Result<()> {
        match self.put_flags(tx, key, value, 0)? {
            0 => Ok(()),
            rc => Err(Error::daos("daos_kv_put", rc)),
        }
    }

    /// Store `value` only when `key` does not exist
    /// (`DAOS_COND_KEY_INSERT`); `false` when it exists.
    pub fn put_if_absent(&self, tx: Option<&Tx<'_>>, key: &str, value: &[u8]) -> Result<bool> {
        match self.put_flags(tx, key, value, sys::DAOS_COND_KEY_INSERT)? {
            0 => Ok(true),
            DER_EXIST => Ok(false),
            rc => Err(Error::daos("daos_kv_put", rc)),
        }
    }

    /// One `daos_kv_get`: `buf` NULL for a size probe. Returns (rc, size).
    fn raw_get(
        &self,
        tx: Option<&Tx<'_>>,
        key: &str,
        buf: Option<&mut [u8]>,
    ) -> Result<(i32, u64)> {
        let k = ckey(key)?;
        let (ptr, mut size) = match buf {
            Some(b) => (b.as_mut_ptr().cast::<std::ffi::c_void>(), b.len() as u64),
            None => (std::ptr::null_mut(), 0),
        };
        let inner = &self.inner;
        // SAFETY: live handle; NUL-terminated key; `size` in/out; `ptr` is
        // NULL (probe) or the caller's buffer of `size` bytes, mutably
        // borrowed for the blocking call; ev NULL.
        let rc = self.call(tx, false, |th| unsafe {
            inner.cont.backend.daos_kv_get(
                inner.oh,
                th,
                0,
                k.as_ptr(),
                &mut size,
                ptr,
                std::ptr::null_mut(),
            )
        })?;
        Ok((rc, size))
    }

    /// Size of the value of `key`; `None` when absent (or empty).
    pub fn size(&self, tx: Option<&Tx<'_>>, key: &str) -> Result<Option<u64>> {
        match self.raw_get(tx, key, None)? {
            (0, 0) => Ok(None),
            (0, n) => Ok(Some(n)),
            (rc, _) => Err(Error::daos("daos_kv_get", rc)),
        }
    }

    /// The value of `key`: a size probe, then the read (repeated, up to 3
    /// times, when the value grew in between). `None` when absent.
    pub fn get(&self, tx: Option<&Tx<'_>>, key: &str) -> Result<Option<Vec<u8>>> {
        let mut size = match self.size(tx, key)? {
            None => return Ok(None),
            Some(n) => n,
        };
        for _ in 0..3 {
            let len = usize::try_from(size)
                .map_err(|_| Error::InvalidArgument("value larger than the address space"))?;
            let mut v = vec![0u8; len];
            match self.get_into(tx, key, &mut v) {
                Ok(None) => return Ok(None),
                Ok(Some(n)) => {
                    v.truncate(n);
                    return Ok(Some(v));
                }
                Err(Error::BufferTooSmall { needed, .. }) => size = needed.max(size + 1),
                Err(e) => return Err(e),
            }
        }
        Err(Error::BufferTooSmall {
            op: "daos_kv_get",
            needed: size,
        })
    }

    /// Read the value of `key` into `buf` (one RPC). `None` when absent;
    /// [`Error::BufferTooSmall`] when it does not fit (with the needed size
    /// when libdaos reported it). An empty `buf` makes the call a size probe:
    /// `None` when absent, `BufferTooSmall` otherwise.
    pub fn get_into(
        &self,
        tx: Option<&Tx<'_>>,
        key: &str,
        buf: &mut [u8],
    ) -> Result<Option<usize>> {
        if buf.is_empty() {
            return match self.size(tx, key)? {
                None => Ok(None),
                Some(n) => Err(Error::BufferTooSmall {
                    op: "daos_kv_get",
                    needed: n,
                }),
            };
        }
        let cap = buf.len() as u64;
        let (rc, size) = self.raw_get(tx, key, Some(buf))?;
        match rc {
            0 if size > cap => Err(Error::BufferTooSmall {
                op: "daos_kv_get",
                needed: size,
            }),
            0 if size == 0 => Ok(None),
            0 => Ok(Some(size as usize)),
            DER_REC2BIG => Err(Error::BufferTooSmall {
                op: "daos_kv_get",
                needed: if size > cap { size } else { 0 },
            }),
            rc => Err(Error::daos("daos_kv_get", rc)),
        }
    }

    /// Remove `key` (`daos_kv_remove`).
    pub fn remove(&self, tx: Option<&Tx<'_>>, key: &str) -> Result<()> {
        let k = ckey(key)?;
        let inner = &self.inner;
        // SAFETY: live handle; NUL-terminated key; ev NULL.
        let rc = self.call(tx, true, |th| unsafe {
            inner
                .cont
                .backend
                .daos_kv_remove(inner.oh, th, 0, k.as_ptr(), std::ptr::null_mut())
        })?;
        match rc {
            0 => Ok(()),
            rc => Err(Error::daos("daos_kv_remove", rc)),
        }
    }

    /// The keys (`daos_kv_list`); order unspecified. [`Error::Unsupported`]
    /// for objects of an erasure-code class.
    pub fn keys(&self) -> Result<KvKeys> {
        Ok(KvKeys {
            cur: Cursor::new(self.inner.clone(), ListKind::KvKeys)?,
        })
    }

    /// The keys as seen by a transaction.
    pub fn keys_in<'tx>(&self, tx: &'tx Tx<'tx>) -> Result<TxKvKeys<'tx>> {
        tx.handle_for(&self.inner.cont, false)?;
        Ok(TxKvKeys {
            cur: Cursor::new(self.inner.clone(), ListKind::KvKeys)?,
            tx,
        })
    }

    /// Destroy the object (`daos_kv_destroy`), then close this handle.
    pub fn destroy(self, tx: Option<&Tx<'_>>) -> Result<()> {
        let inner = &self.inner;
        // SAFETY: live handle; ev NULL.
        let rc = self.call(tx, true, |th| unsafe {
            inner
                .cont
                .backend
                .daos_kv_destroy(inner.oh, th, std::ptr::null_mut())
        })?;
        if rc != 0 {
            return Err(Error::daos("daos_kv_destroy", rc));
        }
        self.close()
    }

    /// Close the handle when this is the last clone, reporting errors.
    pub fn close(self) -> Result<()> {
        ObjInner::close(self.inner)
    }
}

fn utf8(r: Result<Vec<u8>>) -> Result<String> {
    String::from_utf8(r?).map_err(|e| Error::NotUtf8 {
        bytes: e.into_bytes(),
    })
}

/// Keys of a [`Kv`] without a transaction (`'static`, `Send`).
pub struct KvKeys {
    cur: Cursor,
}

/// Keys of a [`Kv`] inside a transaction.
pub struct TxKvKeys<'tx> {
    cur: Cursor,
    tx: &'tx Tx<'tx>,
}

impl Iterator for KvKeys {
    type Item = Result<String>;
    fn next(&mut self) -> Option<Result<String>> {
        self.cur.next_key(None).map(utf8)
    }
}

impl Iterator for TxKvKeys<'_> {
    type Item = Result<String>;
    fn next(&mut self) -> Option<Result<String>> {
        let tx = self.tx;
        self.cur.next_key(Some(tx)).map(utf8)
    }
}

macro_rules! kv_iter_common {
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

kv_iter_common!(KvKeys);
kv_iter_common!(TxKvKeys<'_>);

impl KvKeys {
    /// Visit the keys of the next refill as bytes (also keys that are not
    /// UTF-8). `Ok(None)` at the end.
    pub fn next_batch(&mut self) -> Result<Option<KeyBatch<'_>>> {
        self.cur.next_batch(None)
    }
}

impl TxKvKeys<'_> {
    /// Visit the keys of the next refill as bytes.
    pub fn next_batch(&mut self) -> Result<Option<KeyBatch<'_>>> {
        let tx = self.tx;
        self.cur.next_batch(Some(tx))
    }
}

impl fmt::Debug for KvKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KvKeys").finish_non_exhaustive()
    }
}

impl fmt::Debug for TxKvKeys<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TxKvKeys").finish_non_exhaustive()
    }
}

impl Container {
    /// Open a key-value object (`daos_kv_open`): `KvHashed` or `KvLexical`
    /// ids only (see the [module docs](crate::kv) for `KvUint64`).
    pub fn kv(&self, oid: ObjectId, mode: OpenMode) -> Result<Kv> {
        require(self.backend(), Group::Kv)?;
        match oid.object_type() {
            Some(ObjectType::KvHashed) | Some(ObjectType::KvLexical) => {}
            Some(ObjectType::KvUint64) => {
                return Err(Error::InvalidArgument(
                    "KvUint64 objects are used through the object API with u64 dkeys",
                ))
            }
            _ => {
                return Err(Error::InvalidArgument(
                    "Container::kv needs a KvHashed or KvLexical object id",
                ))
            }
        }
        let _pass = self.conn.enter()?;
        let mut oh = daos_handle_t::default();
        // SAFETY: live container handle; oid by value; out-pointer to a
        // local; ev NULL.
        let rc = unsafe {
            self.backend().daos_kv_open(
                self.coh(),
                oid.raw(),
                mode.bits(),
                &mut oh,
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            return Err(Error::daos("daos_kv_open", rc));
        }
        Ok(Kv {
            inner: Arc::new(ObjInner::new(self.conn.clone(), oh, oid, Kind::Kv)),
        })
    }
}

const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    const fn send<T: Send>() {}
    send_sync::<Kv>();
    send::<KvKeys>();
};
