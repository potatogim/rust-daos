// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! File metadata from `struct stat`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::sys::{self, stat_t};

/// Type of a DFS entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    /// Regular file.
    File,
    /// Directory.
    Dir,
    /// Symbolic link (followed by lookups; never created by this crate).
    Symlink,
    /// Anything else.
    Other,
}

/// Metadata of a DFS entry (from `dfs_stat` or `dfs_ostat`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    mode: u32,
    size: u64,
    uid: u32,
    gid: u32,
    atime: (i64, i64),
    mtime: (i64, i64),
    ctime: (i64, i64),
}

fn to_time((sec, nsec): (i64, i64)) -> SystemTime {
    let nsec = nsec.clamp(0, 999_999_999) as u32;
    if sec >= 0 {
        UNIX_EPOCH + Duration::new(sec as u64, nsec)
    } else {
        UNIX_EPOCH - Duration::new(sec.unsigned_abs(), 0) + Duration::new(0, nsec)
    }
}

impl Metadata {
    #[allow(clippy::unnecessary_cast)]
    pub(crate) fn from_stat(st: &stat_t) -> Metadata {
        Metadata {
            mode: st.st_mode as u32,
            size: st.st_size.max(0) as u64,
            uid: st.st_uid as u32,
            gid: st.st_gid as u32,
            atime: (st.st_atime as i64, st.st_atime_nsec as i64),
            mtime: (st.st_mtime as i64, st.st_mtime_nsec as i64),
            ctime: (st.st_ctime as i64, st.st_ctime_nsec as i64),
        }
    }

    /// Entry type from `st_mode`.
    pub fn file_type(&self) -> FileType {
        match self.mode & sys::S_IFMT {
            m if m == sys::S_IFREG => FileType::File,
            m if m == sys::S_IFDIR => FileType::Dir,
            m if m == sys::S_IFLNK => FileType::Symlink,
            _ => FileType::Other,
        }
    }

    /// Regular file.
    pub fn is_file(&self) -> bool {
        self.file_type() == FileType::File
    }

    /// Directory.
    pub fn is_dir(&self) -> bool {
        self.file_type() == FileType::Dir
    }

    /// `st_size` for files; 0 for directories.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u64 {
        if self.is_dir() {
            0
        } else {
            self.size
        }
    }

    /// `st_mode` (type and permission bits).
    pub fn mode(&self) -> u32 {
        self.mode
    }

    /// Owner user id.
    pub fn uid(&self) -> u32 {
        self.uid
    }

    /// Owner group id.
    pub fn gid(&self) -> u32 {
        self.gid
    }

    /// Modification time.
    pub fn modified(&self) -> SystemTime {
        to_time(self.mtime)
    }

    /// Status change time.
    pub fn changed(&self) -> SystemTime {
        to_time(self.ctime)
    }

    /// Access time.
    pub fn accessed(&self) -> SystemTime {
        to_time(self.atime)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_stat_and_types() {
        let mut st = sys::zeroed_stat();
        st.st_mode = (sys::S_IFREG | 0o644) as _;
        st.st_size = 42;
        st.st_mtime = 10;
        st.st_mtime_nsec = 5;
        let m = Metadata::from_stat(&st);
        assert!(m.is_file());
        assert_eq!(m.len(), 42);
        assert_eq!(m.mode() & 0o777, 0o644);
        assert_eq!(m.modified(), UNIX_EPOCH + Duration::new(10, 5));
        st.st_mode = (sys::S_IFDIR | 0o755) as _;
        st.st_size = 99;
        let m = Metadata::from_stat(&st);
        assert!(m.is_dir());
        assert_eq!(m.len(), 0);
        st.st_mode = (sys::S_IFLNK | 0o777) as _;
        assert_eq!(Metadata::from_stat(&st).file_type(), FileType::Symlink);
        st.st_mtime = -2;
        st.st_mtime_nsec = 0;
        assert_eq!(
            Metadata::from_stat(&st).modified(),
            UNIX_EPOCH - Duration::new(2, 0)
        );
    }
}
