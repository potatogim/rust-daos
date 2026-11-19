// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Open files and positional I/O.

use std::fmt;
use std::sync::Arc;

use super::{MountRef, ObjPtr};
use crate::dfs::metadata::Metadata;
use crate::error::{Error, Result};
use crate::path;
use crate::sys::{self, d_iov_t, d_sg_list_t, dfs_obj_t};

/// Options for [`Dfs::open_with`](crate::Dfs::open_with).
///
/// ```
/// let mut o = daos::OpenOptions::new();
/// o.read(true).write(true).create(true).mode(0o600).chunk_size(1 << 20);
/// ```
#[derive(Debug, Clone)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    create: bool,
    create_new: bool,
    truncate: bool,
    create_parents: bool,
    mode: u32,
    object_class: u32,
    chunk_size: u64,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenOptions {
    /// All flags off, mode 0644, inherited object class and chunk size.
    pub fn new() -> Self {
        OpenOptions {
            read: false,
            write: false,
            create: false,
            create_new: false,
            truncate: false,
            create_parents: false,
            mode: 0o644,
            object_class: sys::OC_UNKNOWN,
            chunk_size: 0,
        }
    }
    /// Open for reading.
    pub fn read(&mut self, yes: bool) -> &mut Self {
        self.read = yes;
        self
    }
    /// Open for writing.
    pub fn write(&mut self, yes: bool) -> &mut Self {
        self.write = yes;
        self
    }
    /// Create the file if it does not exist (`O_CREAT`). Needs `write`.
    pub fn create(&mut self, yes: bool) -> &mut Self {
        self.create = yes;
        self
    }
    /// Create the file, failing if it exists (`O_CREAT | O_EXCL`). Needs `write`.
    pub fn create_new(&mut self, yes: bool) -> &mut Self {
        self.create_new = yes;
        self
    }
    /// Truncate to zero length (`O_TRUNC`). Needs `write`.
    pub fn truncate(&mut self, yes: bool) -> &mut Self {
        self.truncate = yes;
        self
    }
    /// Create missing parent directories first (with `create` or `create_new`).
    pub fn create_parents(&mut self, yes: bool) -> &mut Self {
        self.create_parents = yes;
        self
    }
    /// Permission bits of a new file (default 0o644). The crate adds `S_IFREG`.
    pub fn mode(&mut self, permission_bits: u32) -> &mut Self {
        self.mode = permission_bits & 0o7777;
        self
    }
    /// Object class id of a new file; 0 inherits from the container (default).
    pub fn object_class(&mut self, id: u32) -> &mut Self {
        self.object_class = id;
        self
    }
    /// Chunk size of a new file in bytes; 0 inherits (default).
    pub fn chunk_size(&mut self, bytes: u64) -> &mut Self {
        self.chunk_size = bytes;
        self
    }

    fn flags(&self) -> Result<i32> {
        let mut f = match (self.read, self.write) {
            (true, false) => libc::O_RDONLY,
            (false, true) => libc::O_WRONLY,
            (true, true) => libc::O_RDWR,
            (false, false) => return Err(Error::InvalidArgument("open needs read, write or both")),
        };
        if (self.create || self.create_new || self.truncate) && !self.write {
            return Err(Error::InvalidArgument(
                "create, create_new and truncate need write",
            ));
        }
        if self.create_new {
            f |= libc::O_CREAT | libc::O_EXCL;
        } else if self.create {
            f |= libc::O_CREAT;
        }
        if self.truncate {
            f |= libc::O_TRUNC;
        }
        Ok(f)
    }
}

/// An open DFS file.
///
/// `Send + Sync`: positional I/O takes `&self`, so threads can share one
/// handle. Overlapping writes to the same range from several threads have no
/// defined order. Dropping the handle releases it (`dfs_release`) and ignores
/// errors; [`File::close`] reports them. A failed `dfs_release` leaves the
/// object allocated in libdfs; a later close of the container may then fail
/// with the `daos_cont_close` error and leave the container handle open.
pub struct File {
    mount: Arc<MountRef>,
    obj: ObjPtr,
    slot: u64,
    path: String,
    released: bool,
}

impl fmt::Debug for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("File").field("path", &self.path).finish()
    }
}

impl File {
    pub(crate) fn open(mount: &Arc<MountRef>, path: &str, options: &OpenOptions) -> Result<File> {
        let p = path::normalize(path)?;
        let flags = options.flags()?;
        if p == "/" {
            return Err(Error::dfs("dfs_open", libc::EISDIR, p));
        }
        path::reject_dir_syntax(path, &p)?;
        let op = mount.enter()?;
        let creating = options.create || options.create_new;
        let mode = sys::S_IFREG | options.mode;
        let open_once = || {
            mount.with_parent(&op, &p, creating, |parent, name| {
                let mut obj: *mut dfs_obj_t = std::ptr::null_mut();
                // SAFETY: parent NULL (root) or a live handle under the gate;
                // NUL-terminated name; value NULL (not a symlink); out-pointer
                // to a local.
                let rc = unsafe {
                    op.backend().dfs_open(
                        op.dfs(),
                        parent,
                        name.as_ptr(),
                        mode,
                        flags,
                        options.object_class,
                        options.chunk_size,
                        std::ptr::null(),
                        &mut obj,
                    )
                };
                if rc != 0 {
                    return Err(Error::dfs("dfs_open", rc, p.as_str()));
                }
                Ok(ObjPtr(obj))
            })
        };
        let obj = match open_once() {
            // Parents are created only when the first attempt shows that one
            // is missing, so the common case costs no extra calls.
            Err(e)
                if options.create_parents && creating && e.raw_os_error() == Some(libc::ENOENT) =>
            {
                let (parent, _) = path::split_parent(&p).expect("non-root path");
                if parent == "/" {
                    return Err(e);
                }
                crate::Dfs {
                    mount: mount.clone(),
                }
                .mkdir_all(&op, parent)?;
                open_once()?
            }
            r => r?,
        };
        let slot = mount.track(obj);
        drop(op);
        Ok(File {
            mount: mount.clone(),
            obj,
            slot,
            path: p,
            released: false,
        })
    }

    /// Normalised path this file was opened with.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Read up to `buf.len()` bytes at `offset` straight into `buf` (no
    /// intermediate copy). Returns the byte count; fewer than `buf.len()` only
    /// at end of file. A zero-length buffer returns `Ok(0)` without a call.
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        self.mount.stamp.check()?;
        if buf.is_empty() {
            return Ok(0);
        }
        let op = self.mount.enter()?;
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
        let mut got: u64 = 0;
        // SAFETY: live dfs_t and dfs_obj_t under the gate; one iov that
        // describes exactly `buf`, which stays borrowed mutably for the call;
        // ev NULL (blocking).
        let rc = unsafe {
            op.backend().dfs_read(
                op.dfs(),
                self.obj.0,
                &mut sgl,
                offset,
                &mut got,
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            return Err(Error::dfs("dfs_read", rc, self.path.as_str()));
        }
        Ok((got as usize).min(buf.len()))
    }

    /// As [`File::read_at`], but a short read is [`Error::ShortRead`].
    pub fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        let n = self.read_at(buf, offset)?;
        if n != buf.len() {
            return Err(Error::ShortRead {
                path: self.path.clone(),
                offset,
                wanted: buf.len(),
                got: n,
            });
        }
        Ok(())
    }

    /// Write all of `buf` at `offset`. DFS writes all bytes or fails.
    pub fn write_all_at(&self, buf: &[u8], offset: u64) -> Result<()> {
        self.mount.stamp.check()?;
        if buf.is_empty() {
            return Ok(());
        }
        let op = self.mount.enter()?;
        let mut iov = d_iov_t {
            iov_buf: buf.as_ptr() as *mut _,
            iov_buf_len: buf.len(),
            iov_len: buf.len(),
        };
        let mut sgl = d_sg_list_t {
            sg_nr: 1,
            sg_nr_out: 0,
            sg_iovs: &mut iov,
        };
        // SAFETY: as in read_at; dfs_write only reads through iov_buf.
        let rc = unsafe {
            op.backend()
                .dfs_write(op.dfs(), self.obj.0, &mut sgl, offset, std::ptr::null_mut())
        };
        if rc != 0 {
            return Err(Error::dfs("dfs_write", rc, self.path.as_str()));
        }
        Ok(())
    }

    /// File size (`dfs_get_size`).
    pub fn size(&self) -> Result<u64> {
        self.mount.stamp.check()?;
        let op = self.mount.enter()?;
        let mut size: u64 = 0;
        // SAFETY: live handles under the gate; out-pointer to a local.
        let rc = unsafe { op.backend().dfs_get_size(op.dfs(), self.obj.0, &mut size) };
        if rc != 0 {
            return Err(Error::dfs("dfs_get_size", rc, self.path.as_str()));
        }
        Ok(size)
    }

    /// Metadata of the open file (`dfs_ostat`).
    pub fn metadata(&self) -> Result<Metadata> {
        self.mount.stamp.check()?;
        let op = self.mount.enter()?;
        let mut st = sys::zeroed_stat();
        // SAFETY: live handles under the gate; out-pointer to a local.
        let rc = unsafe { op.backend().dfs_ostat(op.dfs(), self.obj.0, &mut st) };
        if rc != 0 {
            return Err(Error::dfs("dfs_ostat", rc, self.path.as_str()));
        }
        Ok(Metadata::from_stat(&st))
    }

    /// Release the handle (`dfs_release`) and report an error. After
    /// [`Dfs::close`](crate::Dfs::close) this does nothing and returns `Ok`.
    /// A failed release is reported once; the handle is gone either way, and
    /// the object stays allocated in libdfs, so a later close of the
    /// container may fail with the `daos_cont_close` error and leave the
    /// container handle open.
    pub fn close(mut self) -> Result<()> {
        self.mount.stamp.check_cold()?;
        self.released = true;
        self.mount.release_slot(self.slot)
    }
}

impl Drop for File {
    fn drop(&mut self) {
        if self.released || self.mount.stamp.is_foreign() {
            return; // fork child: leak, no lock, no libdfs call
        }
        let _ = self.mount.release_slot(self.slot);
    }
}
