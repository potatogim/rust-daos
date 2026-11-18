// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A 16-byte UUID as DAOS uses it for pools and containers.

use std::fmt;
use std::str::FromStr;

use crate::error::Error;

/// UUID of a pool or container (`uuid_t`, 16 bytes in network order).
///
/// Formats as the 36-character lowercase form DAOS tools print:
///
/// ```
/// let u: daos::Uuid = "3f8a2c1e-0b5d-4e6f-9a7b-1c2d3e4f5a6b".parse().unwrap();
/// assert_eq!(u.to_string(), "3f8a2c1e-0b5d-4e6f-9a7b-1c2d3e4f5a6b");
/// assert!(!u.is_nil());
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Uuid([u8; 16]);

impl Uuid {
    /// The all-zero UUID.
    pub const NIL: Uuid = Uuid([0; 16]);

    /// From raw bytes.
    pub const fn from_bytes(bytes: [u8; 16]) -> Uuid {
        Uuid(bytes)
    }

    /// Raw bytes.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// True for the all-zero UUID.
    pub fn is_nil(&self) -> bool {
        self.0 == [0; 16]
    }

    /// Parse the 36-character hyphenated form (either case).
    pub fn parse(s: &str) -> Option<Uuid> {
        let b = s.as_bytes();
        if b.len() != 36 {
            return None;
        }
        let mut out = [0u8; 16];
        let mut n = 0;
        let mut i = 0;
        while i < 36 {
            if matches!(i, 8 | 13 | 18 | 23) {
                if b[i] != b'-' {
                    return None;
                }
                i += 1;
                continue;
            }
            let hi = hex(b[i])?;
            let lo = hex(*b.get(i + 1)?)?;
            out[n] = hi << 4 | lo;
            n += 1;
            i += 2;
        }
        Some(Uuid(out))
    }
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, b) in self.0.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Uuid({self})")
    }
}

impl FromStr for Uuid {
    type Err = Error;

    fn from_str(s: &str) -> Result<Uuid, Error> {
        Uuid::parse(s).ok_or(Error::InvalidArgument("not a UUID string"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_rejects() {
        let s = "00112233-4455-6677-8899-aabbccddeeff";
        let u = Uuid::parse(s).unwrap();
        assert_eq!(u.as_bytes()[0], 0x00);
        assert_eq!(u.as_bytes()[15], 0xff);
        assert_eq!(u.to_string(), s);
        assert_eq!(Uuid::parse(&s.to_uppercase()), Some(u));
        assert!(Uuid::parse("00112233-4455-6677-8899-aabbccddeef").is_none());
        assert!(Uuid::parse("0011223344556677-8899-aabbccddeeff00").is_none());
        assert!(Uuid::parse("00112233-4455-6677-8899-aabbccddeefg").is_none());
        assert!(Uuid::NIL.is_nil());
        assert!("x".parse::<Uuid>().is_err());
    }
}
