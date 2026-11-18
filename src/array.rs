// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions transcribed from DAOS headers, BSD-2-Clause-Patent, see LICENSE-THIRD-PARTY

//! DAOS arrays (`daos_array_*`): [`Array`].
//!
//! Units are cells: an array has a fixed cell size (bytes per cell) and a
//! chunk size (cells per dkey). A buffer always holds exactly
//! `sum(len) * cell_size` bytes. A request of zero cells (an empty range
//! list, ranges whose lengths are all zero, or an empty buffer) makes no
//! DAOS call and succeeds, like `std::io::Read::read(&mut [])`. It is still
//! checked like any other call first: in a fork child it returns
//! [`Error::Forked`], after [`Container::close`] [`Error::Closed`], and with a
//! transaction that is not open an error.
//!
//! What a read returns depends on the array type:
//!
//! | | [`ObjectType::ArrayByte`] | [`ObjectType::Array`], [`ObjectType::ArrayAttr`] |
//! |---|---|---|
//! | Counters ([`ArrayRead::counts`]) | `Some`: exact, libdaos compares with the array size itself | `None`: libdaos does not fill them |
//! | Holes below the array size | Zero-filled by libdaos | Buffer bytes keep their previous content |
//! | Buffer past the end of the array | Left as it was (DAOS 2.6.4 and 2.8.1) | Left as it was |
//! | End of data | `short_read_cells > 0` | Compare with [`Array::size`] |
//!
//! For byte arrays libdaos runs its internal size check without the
//! transaction, so inside a transaction or a snapshot the counters describe
//! the latest size of the array, not the size the transaction sees.

use std::fmt;
use std::sync::Arc;

use crate::conn::require;
use crate::cont::Container;
use crate::error::{Error, Result};
use crate::gate::Pass;
use crate::handles::Kind;
use crate::obj::{ObjInner, ObjectId, ObjectType, OpenMode};
use crate::sys::api::Group;
use crate::sys::{self, d_iov_t, d_sg_list_t, daos_array_iod_t, daos_handle_t, daos_range_t};
use crate::tx::{self, Tx};

/// A range of cells (`daos_range_t`: index first, then length).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct ArrayRange {
    /// First cell.
    pub index: u64,
    /// Number of cells.
    pub len: u64,
}

impl ArrayRange {
    /// `len` cells from `index`.
    pub const fn new(index: u64, len: u64) -> ArrayRange {
        ArrayRange { index, len }
    }
}

const _: () = {
    use std::mem::{align_of, offset_of, size_of};
    assert!(size_of::<ArrayRange>() == size_of::<daos_range_t>());
    assert!(align_of::<ArrayRange>() == align_of::<daos_range_t>());
    assert!(offset_of!(ArrayRange, index) == offset_of!(daos_range_t, rg_idx));
    assert!(offset_of!(ArrayRange, len) == offset_of!(daos_range_t, rg_len));
};

/// Counters of a byte-array read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ArrayCounts {
    /// Cells read (`arr_nr_read`).
    pub cells_read: u64,
    /// Cells of the request beyond the end of the array
    /// (`arr_nr_short_read`).
    pub short_read_cells: u64,
}

/// Result of a read. See the [module docs](self).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ArrayRead {
    counts: Option<ArrayCounts>,
}

impl ArrayRead {
    /// Counters; `Some` only for [`ObjectType::ArrayByte`] arrays.
    pub fn counts(&self) -> Option<ArrayCounts> {
        self.counts
    }
}

/// Size and last modification of an array (`daos_array_stat`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ArrayStat {
    /// Size in cells.
    pub size: u64,
    /// Highest epoch of a modification.
    pub max_epoch: u64,
}

/// An open array. Cheap to clone; `Send + Sync`; every call blocks.
#[derive(Clone)]
pub struct Array {
    inner: Arc<ObjInner>,
    cell_size: u64,
    chunk_size: u64,
}

impl fmt::Debug for Array {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Array")
            .field("oid", &self.inner.oid)
            .field("cell_size", &self.cell_size)
            .field("chunk_size", &self.chunk_size)
            .finish()
    }
}

/// The ranges of one call after the zero-cell rule.
enum Ranges {
    /// Nothing requested.
    Empty,
    /// One non-empty range (no allocation).
    One(daos_range_t),
    /// The list without zero-length entries.
    Many(Vec<daos_range_t>),
}

impl Ranges {
    /// `None` when nothing is requested; otherwise `arr_nr` and `arr_rgs`.
    fn iod_parts(&mut self) -> Option<(u64, *mut daos_range_t)> {
        match self {
            Ranges::Empty => None,
            Ranges::One(r) => Some((1, r as *mut daos_range_t)),
            Ranges::Many(v) => Some((v.len() as u64, v.as_mut_ptr())),
        }
    }
}

/// Validate the ranges of one call and drop zero-length entries.
fn effective_ranges(ranges: &[ArrayRange]) -> Result<Ranges> {
    if ranges.len() > sys::DAOS_ARRAY_LIST_IO_LIMIT {
        return Err(Error::InvalidArgument("at most 16384 ranges per call"));
    }
    let mut nonempty = 0usize;
    for r in ranges {
        r.index
            .checked_add(r.len)
            .ok_or(Error::InvalidArgument("a range overflows the index range"))?;
        if r.len > 0 {
            nonempty += 1;
        }
    }
    let raw = |r: &ArrayRange| daos_range_t {
        rg_idx: r.index,
        rg_len: r.len,
    };
    let mut it = ranges.iter().filter(|r| r.len > 0);
    Ok(match nonempty {
        0 => Ranges::Empty,
        1 => Ranges::One(raw(it.next().expect("counted above"))),
        _ => Ranges::Many(it.map(raw).collect()),
    })
}

fn total_bytes(ranges: &[ArrayRange], cell_size: u64) -> Result<u64> {
    let mut cells: u64 = 0;
    for r in ranges {
        cells = cells
            .checked_add(r.len)
            .ok_or(Error::InvalidArgument("too many cells"))?;
    }
    cells
        .checked_mul(cell_size)
        .ok_or(Error::InvalidArgument("too many bytes"))
}

impl Array {
    /// The object id.
    pub fn id(&self) -> ObjectId {
        self.inner.oid
    }

    /// The object handle cookie, for diagnostics and logs.
    pub fn as_raw(&self) -> u64 {
        self.inner.oh.cookie
    }

    /// Bytes per cell.
    pub fn cell_size(&self) -> u64 {
        self.cell_size
    }

    /// Cells per dkey.
    pub fn chunk_size(&self) -> u64 {
        self.chunk_size
    }

    fn is_byte_array(&self) -> bool {
        self.inner.oid.object_type() == Some(ObjectType::ArrayByte)
    }

    fn check_len(&self, ranges: &[ArrayRange], len: usize) -> Result<()> {
        if total_bytes(ranges, self.cell_size)? != len as u64 {
            return Err(Error::InvalidArgument(
                "the buffer length must be the cell count times the cell size",
            ));
        }
        Ok(())
    }

    /// The checks every call makes first, in this order: fork stamp,
    /// handle state (the container gate, held until the pass drops), then
    /// the transaction. Returns the transaction handle and the pass.
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

    fn cells_of(&self, len: usize) -> Result<u64> {
        if len as u64 % self.cell_size != 0 {
            return Err(Error::InvalidArgument(
                "the buffer length must be a multiple of the cell size",
            ));
        }
        Ok(len as u64 / self.cell_size)
    }

    /// Write `data` at cell `index`.
    pub fn write_at(&self, tx: Option<&Tx<'_>>, index: u64, data: &[u8]) -> Result<()> {
        let (th, pass) = self.begin(tx, true)?;
        let r = [ArrayRange::new(index, self.cells_of(data.len())?)];
        self.write_checked(tx, th, pass, &r, data)
    }

    /// Read into `buf` from cell `index`.
    pub fn read_at(&self, tx: Option<&Tx<'_>>, index: u64, buf: &mut [u8]) -> Result<ArrayRead> {
        let (th, pass) = self.begin(tx, false)?;
        let r = [ArrayRange::new(index, self.cells_of(buf.len())?)];
        self.read_checked(tx, th, pass, &r, buf)
    }

    /// Write `data` to the cells of `ranges`, in range order.
    pub fn write_ranges(
        &self,
        tx: Option<&Tx<'_>>,
        ranges: &[ArrayRange],
        data: &[u8],
    ) -> Result<()> {
        let (th, pass) = self.begin(tx, true)?;
        self.write_checked(tx, th, pass, ranges, data)
    }

    /// `write_ranges` after `begin`.
    fn write_checked(
        &self,
        tx: Option<&Tx<'_>>,
        th: daos_handle_t,
        _pass: Pass<'_>,
        ranges: &[ArrayRange],
        data: &[u8],
    ) -> Result<()> {
        self.check_len(ranges, data.len())?;
        let mut rgs = effective_ranges(ranges)?;
        let (arr_nr, arr_rgs) = match rgs.iod_parts() {
            None => return Ok(()),
            Some(p) => p,
        };
        let mut iod = daos_array_iod_t {
            arr_nr,
            arr_rgs,
            arr_nr_short_read: 0,
            arr_nr_read: 0,
        };
        let mut iov = d_iov_t {
            iov_buf: data.as_ptr() as *mut _,
            iov_buf_len: data.len(),
            iov_len: data.len(),
        };
        let mut sgl = d_sg_list_t {
            sg_nr: 1,
            sg_nr_out: 0,
            sg_iovs: &mut iov,
        };
        let inner = &self.inner;
        // SAFETY: live handle (gate held by `_pass`); iod, ranges, iov and
        // sgl are locals describing the caller's buffer, alive for the
        // blocking call; arr_nr > 0; libdaos only reads the buffer; ev NULL.
        let rc = unsafe {
            inner.cont.backend.daos_array_write(
                inner.oh,
                th,
                &mut iod,
                &mut sgl,
                std::ptr::null_mut(),
            )
        };
        tx::note(tx, rc);
        ok("daos_array_write", rc)
    }

    /// Read the cells of `ranges` into `buf`, in range order. See the
    /// [module docs](self) for holes and counters.
    pub fn read_ranges(
        &self,
        tx: Option<&Tx<'_>>,
        ranges: &[ArrayRange],
        buf: &mut [u8],
    ) -> Result<ArrayRead> {
        let (th, pass) = self.begin(tx, false)?;
        self.read_checked(tx, th, pass, ranges, buf)
    }

    /// `read_ranges` after `begin`.
    fn read_checked(
        &self,
        tx: Option<&Tx<'_>>,
        th: daos_handle_t,
        _pass: Pass<'_>,
        ranges: &[ArrayRange],
        buf: &mut [u8],
    ) -> Result<ArrayRead> {
        self.check_len(ranges, buf.len())?;
        let byte = self.is_byte_array();
        let mut rgs = effective_ranges(ranges)?;
        let (arr_nr, arr_rgs) = match rgs.iod_parts() {
            None => {
                return Ok(ArrayRead {
                    counts: byte.then_some(ArrayCounts::default()),
                })
            }
            Some(p) => p,
        };
        let mut iod = daos_array_iod_t {
            arr_nr,
            arr_rgs,
            arr_nr_short_read: 0,
            arr_nr_read: 0,
        };
        let mut iov = d_iov_t {
            iov_buf: buf.as_mut_ptr().cast(),
            iov_buf_len: buf.len(),
            iov_len: buf.len(),
        };
        let mut sgl = d_sg_list_t {
            sg_nr: 1,
            sg_nr_out: 0,
            sg_iovs: &mut iov,
        };
        let inner = &self.inner;
        // SAFETY: as in write_checked; the buffer stays mutably borrowed for
        // the blocking call.
        let rc = unsafe {
            inner.cont.backend.daos_array_read(
                inner.oh,
                th,
                &mut iod,
                &mut sgl,
                std::ptr::null_mut(),
            )
        };
        tx::note(tx, rc);
        ok("daos_array_read", rc)?;
        Ok(ArrayRead {
            // Only byte arrays have counters; others are never read.
            counts: byte.then_some(ArrayCounts {
                cells_read: iod.arr_nr_read,
                short_read_cells: iod.arr_nr_short_read,
            }),
        })
    }

    /// File-like read of a byte array ([`ObjectType::ArrayByte`] only):
    /// returns the cells read, fewer than requested at the end of the array.
    pub fn read_at_to_end(&self, tx: Option<&Tx<'_>>, index: u64, buf: &mut [u8]) -> Result<u64> {
        if !self.is_byte_array() {
            return Err(Error::InvalidArgument(
                "read_at_to_end needs a byte array (ObjectType::ArrayByte)",
            ));
        }
        let r = self.read_at(tx, index, buf)?;
        Ok(r.counts.map_or(0, |c| c.cells_read))
    }

    /// Size in cells (`daos_array_get_size`).
    pub fn size(&self, tx: Option<&Tx<'_>>) -> Result<u64> {
        let mut size = 0u64;
        let inner = &self.inner;
        // SAFETY: live handle; out-pointer to a local; ev NULL.
        let rc = self.call(tx, false, |th| unsafe {
            inner
                .cont
                .backend
                .daos_array_get_size(inner.oh, th, &mut size, std::ptr::null_mut())
        })?;
        ok("daos_array_get_size", rc)?;
        Ok(size)
    }

    /// Set the size in cells (`daos_array_set_size`): shrinking punches,
    /// growing extends with a hole.
    pub fn set_size(&self, tx: Option<&Tx<'_>>, cells: u64) -> Result<()> {
        let inner = &self.inner;
        // SAFETY: live handle; ev NULL.
        let rc = self.call(tx, true, |th| unsafe {
            inner
                .cont
                .backend
                .daos_array_set_size(inner.oh, th, cells, std::ptr::null_mut())
        })?;
        ok("daos_array_set_size", rc)
    }

    /// Punch the cells of `ranges` (`daos_array_punch`). An empty request
    /// makes no call.
    pub fn punch(&self, tx: Option<&Tx<'_>>, ranges: &[ArrayRange]) -> Result<()> {
        let (th, _pass) = self.begin(tx, true)?;
        let mut rgs = effective_ranges(ranges)?;
        let (arr_nr, arr_rgs) = match rgs.iod_parts() {
            None => return Ok(()),
            Some(p) => p,
        };
        let mut iod = daos_array_iod_t {
            arr_nr,
            arr_rgs,
            arr_nr_short_read: 0,
            arr_nr_read: 0,
        };
        let inner = &self.inner;
        // SAFETY: live handle (gate held by `_pass`); iod and ranges are
        // locals; arr_nr > 0; ev NULL.
        let rc = unsafe {
            inner
                .cont
                .backend
                .daos_array_punch(inner.oh, th, &mut iod, std::ptr::null_mut())
        };
        tx::note(tx, rc);
        ok("daos_array_punch", rc)
    }

    /// Size and last modification epoch (`daos_array_stat`).
    pub fn stat(&self, tx: Option<&Tx<'_>>) -> Result<ArrayStat> {
        let mut st = sys::daos_array_stbuf_t::default();
        let inner = &self.inner;
        // SAFETY: live handle; out-pointer to a local; ev NULL.
        let rc = self.call(tx, false, |th| unsafe {
            inner
                .cont
                .backend
                .daos_array_stat(inner.oh, th, &mut st, std::ptr::null_mut())
        })?;
        ok("daos_array_stat", rc)?;
        Ok(ArrayStat {
            size: st.st_size,
            max_epoch: st.st_max_epoch,
        })
    }

    /// Destroy the array (`daos_array_destroy`), then close this handle.
    pub fn destroy(self, tx: Option<&Tx<'_>>) -> Result<()> {
        let inner = &self.inner;
        // SAFETY: live handle; ev NULL.
        let rc = self.call(tx, true, |th| unsafe {
            inner
                .cont
                .backend
                .daos_array_destroy(inner.oh, th, std::ptr::null_mut())
        })?;
        ok("daos_array_destroy", rc)?;
        self.close()
    }

    /// Close the handle when this is the last clone, reporting errors.
    pub fn close(self) -> Result<()> {
        ObjInner::close(self.inner)
    }
}

fn ok(op: &'static str, rc: i32) -> Result<()> {
    if rc != 0 {
        return Err(Error::daos(op, rc));
    }
    Ok(())
}

fn check_cell_chunk(cell: u64, chunk: u64) -> Result<()> {
    if cell == 0 || chunk == 0 {
        return Err(Error::InvalidArgument(
            "cell size and chunk size must not be 0",
        ));
    }
    Ok(())
}

impl Container {
    fn array_from(&self, oh: daos_handle_t, oid: ObjectId, cell: u64, chunk: u64) -> Array {
        Array {
            inner: Arc::new(ObjInner::new(self.conn.clone(), oh, oid, Kind::Array)),
            cell_size: cell,
            chunk_size: chunk,
        }
    }

    /// Create an array (`daos_array_create`) with `ObjectType::Array` id;
    /// cell and chunk size are stored in the object. Opened read-write.
    pub fn array_create(&self, oid: ObjectId, cell_size: u64, chunk_size: u64) -> Result<Array> {
        require(self.backend(), Group::Array)?;
        if oid.object_type() != Some(ObjectType::Array) {
            return Err(Error::InvalidArgument(
                "array_create needs an ObjectType::Array object id",
            ));
        }
        check_cell_chunk(cell_size, chunk_size)?;
        let _pass = self.conn.enter()?;
        let mut oh = daos_handle_t::default();
        // SAFETY: live container handle; oid by value; out-pointer to a
        // local; ev NULL.
        let rc = unsafe {
            self.backend().daos_array_create(
                self.coh(),
                oid.raw(),
                sys::DAOS_TX_NONE,
                cell_size,
                chunk_size,
                &mut oh,
                std::ptr::null_mut(),
            )
        };
        ok("daos_array_create", rc)?;
        Ok(self.array_from(oh, oid, cell_size, chunk_size))
    }

    /// Open an array created with [`Container::array_create`]
    /// (`daos_array_open`, `ObjectType::Array` only); reads cell and chunk
    /// size back from the object.
    pub fn array_open(&self, oid: ObjectId, mode: OpenMode) -> Result<Array> {
        require(self.backend(), Group::Array)?;
        if oid.object_type() != Some(ObjectType::Array) {
            return Err(Error::InvalidArgument(
                "array_open needs an ObjectType::Array object id",
            ));
        }
        let _pass = self.conn.enter()?;
        let (mut cell, mut chunk) = (0u64, 0u64);
        let mut oh = daos_handle_t::default();
        // SAFETY: live container handle; out-pointers to locals; ev NULL.
        let rc = unsafe {
            self.backend().daos_array_open(
                self.coh(),
                oid.raw(),
                sys::DAOS_TX_NONE,
                mode.bits(),
                &mut cell,
                &mut chunk,
                &mut oh,
                std::ptr::null_mut(),
            )
        };
        ok("daos_array_open", rc)?;
        Ok(self.array_from(oh, oid, cell.max(1), chunk))
    }

    /// Open an array whose cell and chunk size the caller supplies
    /// (`daos_array_open_with_attr`; `ObjectType::ArrayAttr` and
    /// `ObjectType::ArrayByte` only).
    ///
    /// The sizes are not stored in such objects. Pass the values the array
    /// was always used with: other values map cells to other dkeys and
    /// corrupt the view of the data, and the crate cannot detect it.
    pub fn array_open_with_attr(
        &self,
        oid: ObjectId,
        mode: OpenMode,
        cell_size: u64,
        chunk_size: u64,
    ) -> Result<Array> {
        require(self.backend(), Group::Array)?;
        if !matches!(
            oid.object_type(),
            Some(ObjectType::ArrayAttr) | Some(ObjectType::ArrayByte)
        ) {
            return Err(Error::InvalidArgument(
                "array_open_with_attr needs an ArrayAttr or ArrayByte object id",
            ));
        }
        check_cell_chunk(cell_size, chunk_size)?;
        let _pass = self.conn.enter()?;
        let mut oh = daos_handle_t::default();
        // SAFETY: live container handle; out-pointer to a local; ev NULL.
        let rc = unsafe {
            self.backend().daos_array_open_with_attr(
                self.coh(),
                oid.raw(),
                sys::DAOS_TX_NONE,
                mode.bits(),
                cell_size,
                chunk_size,
                &mut oh,
                std::ptr::null_mut(),
            )
        };
        ok("daos_array_open_with_attr", rc)?;
        Ok(self.array_from(oh, oid, cell_size, chunk_size))
    }
}

const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<Array>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_cell_rule_and_limits() {
        assert!(matches!(effective_ranges(&[]).unwrap(), Ranges::Empty));
        assert!(matches!(
            effective_ranges(&[ArrayRange::new(5, 0)]).unwrap(),
            Ranges::Empty
        ));
        // One non-empty range: no allocation.
        match effective_ranges(&[ArrayRange::new(5, 0), ArrayRange::new(1, 2)]).unwrap() {
            Ranges::One(r) => assert_eq!((r.rg_idx, r.rg_len), (1, 2)),
            _ => panic!("expected one range"),
        }
        let mut r = effective_ranges(&[
            ArrayRange::new(5, 0),
            ArrayRange::new(1, 2),
            ArrayRange::new(9, 0),
            ArrayRange::new(7, 3),
        ])
        .unwrap();
        match &r {
            Ranges::Many(v) => assert_eq!(
                v.iter().map(|r| (r.rg_idx, r.rg_len)).collect::<Vec<_>>(),
                [(1, 2), (7, 3)]
            ),
            _ => panic!("expected two ranges"),
        }
        assert_eq!(r.iod_parts().unwrap().0, 2);
        assert!(effective_ranges(&[ArrayRange::new(u64::MAX, 2)]).is_err());
        let many = vec![ArrayRange::new(0, 1); sys::DAOS_ARRAY_LIST_IO_LIMIT + 1];
        assert!(effective_ranges(&many).is_err());
        assert_eq!(total_bytes(&[ArrayRange::new(0, 3)], 4).unwrap(), 12);
        assert!(total_bytes(&[ArrayRange::new(0, u64::MAX)], 2).is_err());
    }
}
