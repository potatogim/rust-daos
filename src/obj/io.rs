// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions transcribed from DAOS headers, BSD-2-Clause-Patent, see LICENSE-THIRD-PARTY

//! Update and fetch: one dkey, one or more akeys per call.
//!
//! Buffers are borrowed: the iovs point straight into the caller's slices
//! and nothing is copied by the crate. This is sound because every call is
//! blocking (NULL event) and the crate never opens a transaction with
//! `DAOS_TF_ZERO_COPY`, so libdaos has finished with (or copied) the buffers
//! when the call returns.
//!
//! Holes: a record fetch leaves the buffer untouched where no record exists,
//! and the reported size cannot tell. Zero the buffer first when that
//! matters; the `_vec` helpers start from zeroed memory.
//!
//! A builder borrows the caller's slices and an optional transaction and is
//! executed where it was built; it is never `Send`.
//!
//! ```compile_fail,E0277
//! fn send<T: Send>(_: T) {}
//! fn f(o: &daos::obj::Object) {
//!     send(o.update(b"d").single(b"a", b"v"));
//! }
//! ```

use std::ops::BitOr;

use super::key::Key;
use super::Object;
use crate::error::{Error, Result};
use crate::sys::{self, d_iov_t, d_sg_list_t, daos_iod_t, daos_recx_t};
use crate::tx::{self, Tx};

/// Most akeys in one update or fetch.
pub const MAX_AKEYS: usize = 64;

/// `-DER_REC2BIG`.
const REC2BIG: i32 = -2013;

/// A record extent (`daos_recx_t`): `count` records starting at `index`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, PartialOrd, Ord)]
pub struct Recx {
    /// Index of the first record.
    pub index: u64,
    /// Number of records.
    pub count: u64,
}

impl Recx {
    /// `count` records from `index`.
    pub const fn new(index: u64, count: u64) -> Recx {
        Recx { index, count }
    }
}

const _: () = {
    use std::mem::{align_of, offset_of, size_of};
    assert!(size_of::<Recx>() == size_of::<daos_recx_t>());
    assert!(align_of::<Recx>() == align_of::<daos_recx_t>());
    assert!(offset_of!(Recx, index) == offset_of!(daos_recx_t, rx_idx));
    assert!(offset_of!(Recx, count) == offset_of!(daos_recx_t, rx_nr));
};

/// Conditional flags of an update or fetch (`DAOS_COND_*`). Combine with
/// `|`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Cond(u64);

impl Cond {
    /// No condition.
    pub const NONE: Cond = Cond(0);
    /// Insert the dkey; fail with `DER_EXIST` if it exists.
    pub const DKEY_INSERT: Cond = Cond(sys::DAOS_COND_DKEY_INSERT);
    /// Update the dkey only if it exists (`DER_NONEXIST` otherwise).
    pub const DKEY_UPDATE: Cond = Cond(sys::DAOS_COND_DKEY_UPDATE);
    /// Fetch the dkey only if it exists (`DER_NONEXIST` otherwise).
    pub const DKEY_FETCH: Cond = Cond(sys::DAOS_COND_DKEY_FETCH);
    /// Insert the akeys; fail with `DER_EXIST` if one exists.
    pub const AKEY_INSERT: Cond = Cond(sys::DAOS_COND_AKEY_INSERT);
    /// Update the akeys only if they exist.
    pub const AKEY_UPDATE: Cond = Cond(sys::DAOS_COND_AKEY_UPDATE);
    /// Fetch the akeys only if they exist.
    pub const AKEY_FETCH: Cond = Cond(sys::DAOS_COND_AKEY_FETCH);

    /// The flag bits.
    pub fn bits(self) -> u64 {
        self.0
    }
}

impl BitOr for Cond {
    type Output = Cond;
    fn bitor(self, rhs: Cond) -> Cond {
        Cond(self.0 | rhs.0)
    }
}

/// Check a record request: non-zero record size and counts, and a buffer of
/// exactly `sum(count) * record_size` bytes.
pub(crate) fn check_records(record_size: u64, recxs: &[Recx], len: usize) -> Result<()> {
    if record_size == 0 {
        return Err(Error::InvalidArgument("the record size must not be 0"));
    }
    if recxs.is_empty() {
        return Err(Error::InvalidArgument("the extent list is empty"));
    }
    let mut total: u64 = 0;
    for r in recxs {
        if r.count == 0 {
            return Err(Error::InvalidArgument("an extent has no records"));
        }
        r.index.checked_add(r.count).ok_or(Error::InvalidArgument(
            "an extent overflows the index range",
        ))?;
        total = total
            .checked_add(r.count)
            .ok_or(Error::InvalidArgument("too many records"))?;
    }
    let bytes = total
        .checked_mul(record_size)
        .ok_or(Error::InvalidArgument("too many bytes"))?;
    if bytes != len as u64 {
        return Err(Error::InvalidArgument(
            "the buffer length must be the record count times the record size",
        ));
    }
    Ok(())
}

enum UpdateItem<'a> {
    Single(&'a [u8], &'a [u8]),
    Records(&'a [u8], u64, &'a [Recx], &'a [u8]),
}

/// Builder of one `daos_obj_update`. See the [module docs](crate::obj).
///
/// ```no_run
/// # fn f(obj: &daos::obj::Object) -> daos::Result<()> {
/// use daos::obj::{Cond, Recx};
/// let data = vec![7u8; 4096];
/// obj.update(b"dkey")
///     .single(b"size", &4096u64.to_le_bytes())
///     .records(b"data", 1, &[Recx::new(0, 4096)], &data)
///     .cond(Cond::DKEY_INSERT)
///     .execute()?;
/// # Ok(()) }
/// ```
#[must_use = "a builder does nothing until execute"]
pub struct Update<'a> {
    obj: &'a Object,
    dkey: &'a [u8],
    items: Vec<UpdateItem<'a>>,
    cond: Cond,
    tx: Option<&'a Tx<'a>>,
}

impl<'a> Update<'a> {
    pub(crate) fn new(obj: &'a Object, dkey: &'a [u8]) -> Update<'a> {
        Update {
            obj,
            dkey,
            items: Vec::new(),
            cond: Cond::NONE,
            tx: None,
        }
    }

    /// Write `value` as the single value of `akey` (`DAOS_IOD_SINGLE`). An
    /// empty value is refused (it would punch the akey; use
    /// [`Object::punch_akeys`]).
    pub fn single(mut self, akey: &'a [u8], value: &'a [u8]) -> Self {
        self.items.push(UpdateItem::Single(akey, value));
        self
    }

    /// Write records of `record_size` bytes at the extents `recxs` of `akey`
    /// (`DAOS_IOD_ARRAY`); `data` holds them in extent order.
    pub fn records(
        mut self,
        akey: &'a [u8],
        record_size: u64,
        recxs: &'a [Recx],
        data: &'a [u8],
    ) -> Self {
        self.items
            .push(UpdateItem::Records(akey, record_size, recxs, data));
        self
    }

    /// Conditional flags.
    pub fn cond(mut self, cond: Cond) -> Self {
        self.cond = cond;
        self
    }

    /// Run inside a transaction.
    pub fn tx(mut self, tx: &'a Tx<'a>) -> Self {
        self.tx = Some(tx);
        self
    }

    /// Run the update.
    pub fn execute(self) -> Result<()> {
        let obj = &self.obj.inner;
        obj.cont.stamp.check()?;
        if self.items.is_empty() || self.items.len() > MAX_AKEYS {
            return Err(Error::InvalidArgument("an update has 1 to 64 akeys"));
        }
        let (dkey_int, akey_int) = self.obj.key_types();
        let dkey = Key::new(self.dkey, dkey_int)?;
        let mut akeys = Vec::with_capacity(self.items.len());
        for it in &self.items {
            match it {
                UpdateItem::Single(a, v) => {
                    if v.is_empty() {
                        return Err(Error::InvalidArgument(
                            "a single value must not be empty (use punch_akeys)",
                        ));
                    }
                    akeys.push(Key::new(a, akey_int)?);
                }
                UpdateItem::Records(a, size, recxs, data) => {
                    check_records(*size, recxs, data.len())?;
                    akeys.push(Key::new(a, akey_int)?);
                }
            }
        }
        let mut iods = Vec::with_capacity(akeys.len());
        let mut iovs = Vec::with_capacity(akeys.len());
        for (it, k) in self.items.iter().zip(&akeys) {
            match it {
                UpdateItem::Single(_, v) => {
                    iods.push(daos_iod_t {
                        iod_name: k.iov(),
                        iod_type: sys::DAOS_IOD_SINGLE,
                        iod_size: v.len() as u64,
                        iod_flags: 0,
                        iod_nr: 1,
                        iod_recxs: std::ptr::null_mut(),
                    });
                    iovs.push(const_iov(v));
                }
                UpdateItem::Records(_, size, recxs, data) => {
                    iods.push(daos_iod_t {
                        iod_name: k.iov(),
                        iod_type: sys::DAOS_IOD_ARRAY,
                        iod_size: *size,
                        iod_flags: 0,
                        iod_nr: recxs.len() as u32,
                        iod_recxs: recxs.as_ptr() as *mut daos_recx_t,
                    });
                    iovs.push(const_iov(data));
                }
            }
        }
        let mut sgls: Vec<d_sg_list_t> = iovs
            .iter_mut()
            .map(|iov| d_sg_list_t {
                sg_nr: 1,
                sg_nr_out: 0,
                sg_iovs: iov,
            })
            .collect();
        let th = tx::th(self.tx, &obj.cont, true)?;
        let mut dkey_iov = dkey.iov();
        let _pass = obj.enter()?;
        // SAFETY: live object handle (gate held); the dkey, akeys, iods,
        // recx lists, iovs and sgls all live in this frame (or are the
        // caller's borrowed slices) until the blocking call returns; libdaos
        // only reads them on update; ev NULL.
        let rc = unsafe {
            obj.cont.backend.daos_obj_update(
                obj.oh,
                th,
                self.cond.bits(),
                &mut dkey_iov,
                iods.len() as u32,
                iods.as_mut_ptr(),
                sgls.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        };
        tx::note(self.tx, rc);
        if rc != 0 {
            return Err(Error::daos("daos_obj_update", rc));
        }
        Ok(())
    }
}

fn const_iov(b: &[u8]) -> d_iov_t {
    d_iov_t {
        iov_buf: b.as_ptr() as *mut _,
        iov_buf_len: b.len(),
        iov_len: b.len(),
    }
}

fn mut_iov(b: &mut [u8]) -> d_iov_t {
    d_iov_t {
        iov_buf: b.as_mut_ptr().cast(),
        iov_buf_len: b.len(),
        iov_len: b.len(),
    }
}

enum FetchItem<'a> {
    Single(&'a [u8], &'a mut [u8]),
    Records(&'a [u8], u64, &'a [Recx], &'a mut [u8]),
}

/// Sizes reported by a fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchOutput {
    sizes: Vec<u64>,
}

impl FetchOutput {
    /// `iod_size` per akey, in the order added: the size of the single
    /// value (0 when absent), or the record size (0 when no record of the
    /// requested extents exists).
    pub fn sizes(&self) -> &[u64] {
        &self.sizes
    }
}

/// Builder of one `daos_obj_fetch`. See the [module docs](crate::obj).
///
/// Single values: a buffer at least as large as the value receives it and
/// the reported size says how many bytes; a smaller one gives
/// [`Error::BufferTooSmall`]. If every akey of the fetch has an empty buffer,
/// the call is a size probe (no data is transferred). Empty and non-empty
/// buffers cannot be mixed in one fetch.
#[must_use = "a builder does nothing until execute"]
pub struct Fetch<'a> {
    obj: &'a Object,
    dkey: &'a [u8],
    items: Vec<FetchItem<'a>>,
    cond: Cond,
    tx: Option<&'a Tx<'a>>,
}

impl<'a> Fetch<'a> {
    pub(crate) fn new(obj: &'a Object, dkey: &'a [u8]) -> Fetch<'a> {
        Fetch {
            obj,
            dkey,
            items: Vec::new(),
            cond: Cond::NONE,
            tx: None,
        }
    }

    /// Fetch the single value of `akey` into `buf`.
    pub fn single(mut self, akey: &'a [u8], buf: &'a mut [u8]) -> Self {
        self.items.push(FetchItem::Single(akey, buf));
        self
    }

    /// Fetch records of `record_size` bytes at `recxs` of `akey` into `buf`
    /// (exactly `sum(count) * record_size` bytes). Absent records leave
    /// their bytes of `buf` unchanged.
    pub fn records(
        mut self,
        akey: &'a [u8],
        record_size: u64,
        recxs: &'a [Recx],
        buf: &'a mut [u8],
    ) -> Self {
        self.items
            .push(FetchItem::Records(akey, record_size, recxs, buf));
        self
    }

    /// Conditional flags.
    pub fn cond(mut self, cond: Cond) -> Self {
        self.cond = cond;
        self
    }

    /// Run inside a transaction.
    pub fn tx(mut self, tx: &'a Tx<'a>) -> Self {
        self.tx = Some(tx);
        self
    }

    /// Run the fetch.
    pub fn execute(mut self) -> Result<FetchOutput> {
        let obj = self.obj.inner.clone();
        obj.cont.stamp.check()?;
        if self.items.is_empty() || self.items.len() > MAX_AKEYS {
            return Err(Error::InvalidArgument("a fetch has 1 to 64 akeys"));
        }
        let (dkey_int, akey_int) = self.obj.key_types();
        let dkey = Key::new(self.dkey, dkey_int)?;
        let empties = self
            .items
            .iter()
            .filter(|it| matches!(it, FetchItem::Single(_, b) if b.is_empty()))
            .count();
        let probe = empties == self.items.len();
        if empties > 0 && !probe {
            return Err(Error::InvalidArgument(
                "empty and non-empty destination buffers cannot be mixed in one fetch",
            ));
        }
        let mut akeys = Vec::with_capacity(self.items.len());
        let mut caps = Vec::with_capacity(self.items.len());
        for it in &self.items {
            match it {
                FetchItem::Single(a, b) => {
                    akeys.push(Key::new(a, akey_int)?);
                    caps.push(Some(b.len() as u64));
                }
                FetchItem::Records(a, size, recxs, b) => {
                    check_records(*size, recxs, b.len())?;
                    akeys.push(Key::new(a, akey_int)?);
                    caps.push(None);
                }
            }
        }
        let mut iods = Vec::with_capacity(akeys.len());
        let mut iovs = Vec::with_capacity(akeys.len());
        for (it, k) in self.items.iter_mut().zip(&akeys) {
            match it {
                FetchItem::Single(_, b) => {
                    iods.push(daos_iod_t {
                        iod_name: k.iov(),
                        iod_type: sys::DAOS_IOD_SINGLE,
                        iod_size: sys::DAOS_REC_ANY,
                        iod_flags: 0,
                        iod_nr: 1,
                        iod_recxs: std::ptr::null_mut(),
                    });
                    iovs.push(mut_iov(b));
                }
                FetchItem::Records(_, size, recxs, b) => {
                    iods.push(daos_iod_t {
                        iod_name: k.iov(),
                        iod_type: sys::DAOS_IOD_ARRAY,
                        iod_size: *size,
                        iod_flags: 0,
                        iod_nr: recxs.len() as u32,
                        iod_recxs: recxs.as_ptr() as *mut daos_recx_t,
                    });
                    iovs.push(mut_iov(b));
                }
            }
        }
        let mut sgls: Vec<d_sg_list_t> = iovs
            .iter_mut()
            .map(|iov| d_sg_list_t {
                sg_nr: 1,
                sg_nr_out: 0,
                sg_iovs: iov,
            })
            .collect();
        let th = tx::th(self.tx, &obj.cont, false)?;
        let mut dkey_iov = dkey.iov();
        let rc = {
            let _pass = obj.enter()?;
            // SAFETY: live object handle (gate held); dkey, akeys, iods,
            // recx lists, iovs and sgls live in this frame, and the iovs
            // describe the caller's buffers, which stay mutably borrowed
            // until the blocking call returns; sgls NULL for a size probe;
            // ioms NULL; ev NULL.
            unsafe {
                obj.cont.backend.daos_obj_fetch(
                    obj.oh,
                    th,
                    self.cond.bits(),
                    &mut dkey_iov,
                    iods.len() as u32,
                    iods.as_mut_ptr(),
                    if probe {
                        std::ptr::null_mut()
                    } else {
                        sgls.as_mut_ptr()
                    },
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            }
        };
        tx::note(self.tx, rc);
        let sizes: Vec<u64> = iods.iter().map(|i| i.iod_size).collect();
        // A reported single-value size beyond the buffer is "too small",
        // also when libdaos returned 0; a probe reports sizes only.
        let needed = sizes
            .iter()
            .zip(&caps)
            .filter_map(|(s, c)| match c {
                Some(cap) if !probe && s > cap => Some(*s),
                _ => None,
            })
            .max();
        if rc == REC2BIG {
            return Err(Error::BufferTooSmall {
                op: "daos_obj_fetch",
                needed: needed.unwrap_or(0),
            });
        }
        if rc != 0 {
            return Err(Error::daos("daos_obj_fetch", rc));
        }
        if let Some(n) = needed {
            return Err(Error::BufferTooSmall {
                op: "daos_obj_fetch",
                needed: n,
            });
        }
        Ok(FetchOutput { sizes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_checks() {
        check_records(4, &[Recx::new(0, 2), Recx::new(10, 1)], 12).unwrap();
        assert!(check_records(0, &[Recx::new(0, 1)], 0).is_err());
        assert!(check_records(1, &[], 0).is_err());
        assert!(check_records(1, &[Recx::new(0, 0)], 0).is_err());
        assert!(check_records(1, &[Recx::new(u64::MAX, 2)], 2).is_err());
        assert!(check_records(u64::MAX, &[Recx::new(0, 2)], 2).is_err());
        assert!(check_records(2, &[Recx::new(0, 2)], 3).is_err());
        assert_eq!((Cond::DKEY_INSERT | Cond::AKEY_FETCH).bits(), 2 | 64);
    }
}
