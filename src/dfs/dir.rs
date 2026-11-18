// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Directory iteration with `dfs_iterate`.

use std::collections::VecDeque;
use std::ffi::{c_char, c_int, c_void, CStr, OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use super::{DirHandle, Mount};
use crate::error::{Error, Result};
use crate::path;
use crate::sys::{daos_anchor_t, dfs_obj_t, dfs_t, DFS_MAX_NAME};

/// Names requested per `dfs_iterate` call.
const BATCH_NAMES: u32 = 1024;
/// Key buffer size per call: room for 1024 maximal names.
const BATCH_BYTES: usize = 1024 * (DFS_MAX_NAME + 1);
/// Consecutive empty batches without EOF before giving up with EIO.
const MAX_EMPTY_BATCHES: u32 = 64;

/// One entry of a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    parent: Arc<str>,
    name: OsString,
}

impl DirEntry {
    /// Entry name (bytes as stored in DFS).
    pub fn file_name(&self) -> &OsStr {
        &self.name
    }

    /// Parent path + `/` + name. Bytes of the name that are not UTF-8 are
    /// replaced with U+FFFD, so the result may not address the entry; use
    /// [`DirEntry::file_name`] for the exact bytes.
    pub fn path(&self) -> String {
        path::join(&self.parent, &self.name.to_string_lossy())
    }
}

struct Batch {
    names: Vec<OsString>,
    panicked: bool,
}

/// Filler callback. Copies the name into owned storage: the pointer is valid
/// only during the call (DAOS 2.8 passes a stack copy; DAOS 2.6 terminates
/// the name in place inside its key buffer and restores the byte after the
/// callback). It never fails on purpose: a non-zero return makes DFS stop
/// with the anchor already past the batch, so the rest of the batch is lost
/// (in 2.6 the in-place terminator is then also left in the buffer, which is
/// freed right after). A panic is caught, flagged and reported as EIO.
unsafe extern "C" fn filler(
    _dfs: *mut dfs_t,
    _obj: *mut dfs_obj_t,
    name: *const c_char,
    arg: *mut c_void,
) -> c_int {
    let r = catch_unwind(AssertUnwindSafe(|| {
        assert!(!arg.is_null(), "dfs_iterate passed a null argument");
        assert!(!name.is_null(), "dfs_iterate passed a null name");
        // SAFETY: arg is the &mut Batch passed to dfs_iterate below, alive
        // for the whole call; name is a NUL-terminated string from libdfs.
        let batch = unsafe { &mut *(arg as *mut Batch) };
        let bytes = unsafe { CStr::from_ptr(name) }.to_bytes();
        batch.names.push(OsString::from_vec(bytes.to_vec()));
    }));
    match r {
        Ok(()) => 0,
        Err(_) => {
            if !arg.is_null() {
                // SAFETY: as above.
                unsafe { (*(arg as *mut Batch)).panicked = true };
            }
            libc::EIO
        }
    }
}

/// Iterator over the names in one directory, fetched in batches of 1024.
///
/// `"."` and `".."` are not produced. Entry types are not known without a
/// [`Dfs::metadata`](crate::Dfs::metadata) call per entry. After an error the
/// iterator ends.
///
/// Known limitation: on a directory whose object class is an erasure-code
/// class, libdaos attaches memory to the listing anchor and frees it only
/// when the listing reaches its end. Dropping the iterator earlier leaves
/// that memory allocated. Directories get replicated classes by default;
/// iterate directories of an erasure-code class to the end.
pub struct ReadDir {
    mount: Arc<Mount>,
    dir: Arc<DirHandle>,
    path: Arc<str>,
    anchor: Box<daos_anchor_t>,
    pending: VecDeque<OsString>,
    done: bool,
}

impl fmt::Debug for ReadDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadDir")
            .field("path", &self.path)
            .field("done", &self.done)
            .finish()
    }
}

impl ReadDir {
    pub(crate) fn new(mount: Arc<Mount>, dir: Arc<DirHandle>, path: String) -> ReadDir {
        ReadDir {
            mount,
            dir,
            path: path.into(),
            anchor: Box::new(daos_anchor_t::new()),
            pending: VecDeque::new(),
            done: false,
        }
    }

    /// Fetch batches until at least one name arrives or the end is reached.
    fn fetch(&mut self) -> Result<()> {
        self.mount.stamp.check()?;
        let op = self.mount.enter()?;
        let mut empty = 0;
        loop {
            let mut batch = Batch {
                names: Vec::new(),
                panicked: false,
            };
            let mut nr = BATCH_NAMES;
            // SAFETY: live dfs_t and directory handle under the gate; the
            // anchor is owned by this iterator; `batch` outlives the call and
            // is only touched by `filler` during it.
            let rc = unsafe {
                op.backend().dfs_iterate(
                    op.dfs(),
                    self.dir.obj.0,
                    &mut *self.anchor,
                    &mut nr,
                    BATCH_BYTES,
                    Some(filler),
                    &mut batch as *mut Batch as *mut c_void,
                )
            };
            if batch.panicked {
                return Err(Error::dfs("dfs_iterate", libc::EIO, &*self.path));
            }
            if rc != 0 {
                return Err(Error::dfs("dfs_iterate", rc, &*self.path));
            }
            let got = batch.names.len();
            self.pending.extend(batch.names);
            if self.anchor.is_eof() {
                self.done = true;
                return Ok(());
            }
            if got > 0 {
                return Ok(());
            }
            empty += 1;
            if empty >= MAX_EMPTY_BATCHES {
                return Err(Error::dfs("dfs_iterate", libc::EIO, &*self.path));
            }
        }
    }
}

impl Iterator for ReadDir {
    type Item = Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(name) = self.pending.pop_front() {
                return Some(Ok(DirEntry {
                    parent: self.path.clone(),
                    name,
                }));
            }
            if self.done {
                return None;
            }
            if let Err(e) = self.fetch() {
                self.done = true;
                self.pending.clear();
                return Some(Err(e));
            }
        }
    }
}

impl DirEntry {
    /// Entry name as bytes.
    pub fn name_bytes(&self) -> &[u8] {
        self.name.as_bytes()
    }
}
