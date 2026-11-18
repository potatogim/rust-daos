// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Streaming writer.

use std::io;

use super::File;
use crate::error::Result;

/// Sequential writer: each write lands at the running offset.
///
/// No buffering inside: each [`Writer::write_all`] is one `dfs_write`. Wrap it
/// in [`std::io::BufWriter`] when producing small pieces. `flush` does
/// nothing. Dropping a `Writer` without [`Writer::finish`] leaves the bytes
/// written so far in the file.
///
/// ```no_run
/// # fn f(dfs: &daos::Dfs) -> Result<(), daos::Error> {
/// let mut w = dfs.writer("/out/data.bin")?;
/// w.write_all(b"header")?;
/// w.write_all(&[0u8; 4096])?;
/// let written = w.finish()?;
/// assert_eq!(written, 6 + 4096);
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct Writer {
    file: Option<File>,
    offset: u64,
}

impl Writer {
    pub(crate) fn new(file: File) -> Writer {
        Writer {
            file: Some(file),
            offset: 0,
        }
    }

    /// Bytes written so far (the offset of the next write).
    pub fn position(&self) -> u64 {
        self.offset
    }

    /// Path of the file.
    pub fn path(&self) -> &str {
        self.file.as_ref().map_or("", |f| f.path())
    }

    /// Write all of `data` at the current position.
    pub fn write_all(&mut self, data: &[u8]) -> Result<()> {
        let f = self.file.as_ref().expect("file is present until finish");
        f.write_all_at(data, self.offset)?;
        self.offset += data.len() as u64;
        Ok(())
    }

    /// Release the handle and return the number of bytes written.
    pub fn finish(mut self) -> Result<u64> {
        if let Some(f) = self.file.take() {
            f.close()?;
        }
        Ok(self.offset)
    }
}

impl io::Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Writer::write_all(self, buf)?;
        Ok(buf.len())
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        Writer::write_all(self, buf)?;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
