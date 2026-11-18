// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Keys as libdaos receives them.
//!
//! For object types whose dkeys (or akeys) are integers, libdaos reads the
//! key as `*(uint64_t *)iov_buf` (`obj_dkey2hash`), which needs an 8-byte
//! aligned address. The caller's slice is therefore never passed for an
//! integer key: its 8 bytes are decoded with `u64::from_ne_bytes` into a
//! [`Key`] that owns an aligned `u64`, and the iov points there. Hashed and
//! lexical keys are passed as the caller's bytes (libdaos only reads them).

use crate::error::{Error, Result};
use crate::sys::{d_iov_t, daos_key_t};

/// A key prepared for one call. Must not move while an iov built from it is
/// in use (the iov points into it).
#[derive(Debug, Clone)]
pub(crate) enum Key<'a> {
    /// Caller's bytes (hashed or lexical key).
    Bytes(&'a [u8]),
    /// An integer key in aligned storage.
    U64(u64),
}

impl<'a> Key<'a> {
    /// Prepare `key` for a level whose keys are integers when `int`.
    pub(crate) fn new(key: &'a [u8], int: bool) -> Result<Key<'a>> {
        if key.is_empty() {
            return Err(Error::InvalidArgument("keys must not be empty"));
        }
        if int {
            let b: [u8; 8] = key.try_into().map_err(|_| {
                Error::InvalidArgument("keys of integer-keyed object types are 8 bytes (u64_key)")
            })?;
            return Ok(Key::U64(u64::from_ne_bytes(b)));
        }
        Ok(Key::Bytes(key))
    }

    /// The iov for the call. libdaos does not write through key pointers on
    /// update, fetch, punch and list; the `*const` to `*mut` cast only
    /// matches the C prototype.
    pub(crate) fn iov(&self) -> daos_key_t {
        match self {
            Key::Bytes(b) => d_iov_t {
                iov_buf: b.as_ptr() as *mut _,
                iov_buf_len: b.len(),
                iov_len: b.len(),
            },
            Key::U64(v) => d_iov_t {
                iov_buf: v as *const u64 as *mut _,
                iov_buf_len: 8,
                iov_len: 8,
            },
        }
    }

    /// An owned copy (for iterators that keep a dkey).
    pub(crate) fn to_owned_key(&self) -> OwnedKey {
        match self {
            Key::Bytes(b) => OwnedKey::Bytes(b.to_vec()),
            Key::U64(v) => OwnedKey::U64(*v),
        }
    }
}

/// An owned key (kept by iterators across calls).
#[derive(Debug, Clone)]
pub(crate) enum OwnedKey {
    Bytes(Vec<u8>),
    U64(u64),
}

impl OwnedKey {
    pub(crate) fn iov(&self) -> daos_key_t {
        match self {
            OwnedKey::Bytes(b) => d_iov_t {
                iov_buf: b.as_ptr() as *mut _,
                iov_buf_len: b.len(),
                iov_len: b.len(),
            },
            OwnedKey::U64(v) => d_iov_t {
                iov_buf: v as *const u64 as *mut _,
                iov_buf_len: 8,
                iov_len: 8,
            },
        }
    }
}

/// The 8 bytes of an integer key, in the native byte order C callers use
/// when they pass a `uint64_t`.
pub fn u64_key(v: u64) -> [u8; 8] {
    v.to_ne_bytes()
}

/// The integer of an 8-byte key (for example from an enumeration of an
/// integer-keyed object); `None` for any other length.
pub fn key_u64(key: &[u8]) -> Option<u64> {
    key.try_into().ok().map(u64::from_ne_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_keys_are_copied_to_aligned_storage() {
        let mut buf = [0u8; 16];
        buf[1..9].copy_from_slice(&u64_key(0x0102_0304_0506_0708));
        let odd = &buf[1..9];
        let k = Key::new(odd, true).unwrap();
        let iov = k.iov();
        assert_eq!(iov.iov_buf as usize % 8, 0, "aligned");
        assert_ne!(
            iov.iov_buf as *const u8,
            odd.as_ptr(),
            "not the caller's pointer"
        );
        assert_eq!(iov.iov_len, 8);
        // SAFETY: the iov points at the u64 inside `k`.
        assert_eq!(
            unsafe { *(iov.iov_buf as *const u64) },
            0x0102_0304_0506_0708
        );
        assert!(Key::new(&buf[..7], true).is_err());
        assert!(Key::new(&[], false).is_err());
        let bytes = *b"abc";
        let h = Key::new(&bytes, false).unwrap();
        assert_eq!(h.iov().iov_buf as *const u8, bytes.as_ptr());
        assert_eq!(key_u64(&u64_key(7)), Some(7));
        assert_eq!(key_u64(b"abc"), None);
    }
}
