// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions transcribed from DAOS headers, BSD-2-Clause-Patent, see LICENSE-THIRD-PARTY

//! Object identifiers, types and classes.
//!
//! The upper 32 bits of `oid.hi` belong to DAOS (`daos_obj.h`): 8 bits of
//! object type, 8 bits of redundancy (the class "ord"), 16 bits of group
//! count. The decoding here transcribes the inline functions
//! `daos_obj_id2type`, `daos_obj_id2ord`, `daos_obj_id2class` and
//! `daos_obj_id2grp_nr`, whose results the ABI probe prints for test vectors.

use std::fmt;

use crate::cont::Container;
use crate::error::{Error, Result};
use crate::sys;
use crate::sys::api::Raw;

/// Bit shift of the object type in `oid.hi` (`OID_FMT_TYPE_SHIFT`).
const TYPE_SHIFT: u32 = 56;
/// Bit shift of the redundancy value (`OID_FMT_CLASS_SHIFT`).
const CLASS_SHIFT: u32 = 48;
/// Bit shift of the group count (`OID_FMT_META_SHIFT`).
const META_SHIFT: u32 = 32;
/// `OC_REDUN_SHIFT` (`daos_obj_class.h`).
const OC_REDUN_SHIFT: u32 = 24;
/// First redundancy value after the erasure-code range (`OC_BACK_COMPAT`).
const OC_BACK_COMPAT: u32 = 50;

/// A DAOS object identifier (`daos_obj_id_t`).
///
/// `lo` and the low 32 bits of `hi` belong to the application; the upper 32
/// bits of `hi` are written by
/// [`Container::generate_oid`](crate::Container::generate_oid). Displays as
/// `hi.lo`, the form DAOS tools print.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct ObjectId {
    /// Low 64 bits.
    pub lo: u64,
    /// High 64 bits (upper 32 bits: type, class, groups).
    pub hi: u64,
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({}.{})", self.hi, self.lo)
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.hi, self.lo)
    }
}

impl ObjectId {
    /// From the two halves.
    pub const fn new(lo: u64, hi: u64) -> ObjectId {
        ObjectId { lo, hi }
    }

    /// The object type encoded in `hi`, or `None` for a value this crate
    /// does not name (for example the object ID tables of snapshots).
    pub fn object_type(self) -> Option<ObjectType> {
        ObjectType::from_raw((self.hi >> TYPE_SHIFT) as u32)
    }

    /// The redundancy value (`daos_obj_id2ord`).
    pub(crate) fn ord(self) -> u32 {
        ((self.hi >> CLASS_SHIFT) & 0xFF) as u32
    }

    /// The number of redundancy groups (`daos_obj_id2grp_nr`).
    pub fn groups(self) -> u32 {
        ((self.hi >> META_SHIFT) & 0xFFFF) as u32
    }

    /// The object class (`daos_obj_id2class`).
    pub fn class(self) -> ObjectClass {
        ObjectClass(self.ord() << OC_REDUN_SHIFT | self.groups())
    }

    /// True for the all-zero id.
    pub fn is_nil(self) -> bool {
        self.lo == 0 && self.hi == 0
    }

    /// True when the class is an erasure-code class (redundancy values from
    /// `OR_RS_2P1` up to the end of the erasure-code range).
    pub fn is_erasure_coded(self) -> bool {
        (sys::OR_RS_2P1..OC_BACK_COMPAT).contains(&self.ord())
    }

    /// Build `hi` the way `daos_obj_generate_oid` does: keep the low 32
    /// bits, write type, redundancy and groups into the upper 32.
    #[cfg(test)]
    pub(crate) fn encode(lo: u64, hi32: u32, ty: u32, ord: u32, groups: u32) -> ObjectId {
        let hi = (u64::from(ty) << TYPE_SHIFT)
            | (u64::from(ord & 0xFF) << CLASS_SHIFT)
            | (u64::from(groups.min(sys::MAX_NUM_GROUPS)) << META_SHIFT)
            | u64::from(hi32);
        ObjectId { lo, hi }
    }

    pub(crate) fn raw(self) -> sys::daos_obj_id_t {
        sys::daos_obj_id_t {
            lo: self.lo,
            hi: self.hi,
        }
    }
}

/// DAOS object type (`enum daos_otype_t`). Decides how keys are compared
/// and which API may open the object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ObjectType {
    /// Multi-level KV with hashed dkeys and akeys (the default).
    MultiHashed,
    /// Integer (8-byte) dkeys.
    DkeyUint64,
    /// Integer (8-byte) akeys.
    AkeyUint64,
    /// Integer dkeys and akeys.
    MultiUint64,
    /// Lexical dkeys.
    DkeyLexical,
    /// Lexical akeys.
    AkeyLexical,
    /// Lexical dkeys and akeys.
    MultiLexical,
    /// Flat KV with hashed keys (the `daos::kv` API).
    KvHashed,
    /// Flat KV with integer keys (object API only, see `daos::kv`).
    KvUint64,
    /// Flat KV with lexical keys.
    KvLexical,
    /// Array with its cell and chunk size stored in the object.
    Array,
    /// Array whose cell and chunk size the caller supplies.
    ArrayAttr,
    /// Byte array without metadata (DFS files use this type).
    ArrayByte,
}

impl ObjectType {
    /// All named types, for tests and the probe lines.
    pub(crate) const ALL: [(ObjectType, &'static str, u32); 13] = [
        (ObjectType::MultiHashed, "DAOS_OT_MULTI_HASHED", 0),
        (ObjectType::DkeyUint64, "DAOS_OT_DKEY_UINT64", 2),
        (ObjectType::AkeyUint64, "DAOS_OT_AKEY_UINT64", 3),
        (ObjectType::MultiUint64, "DAOS_OT_MULTI_UINT64", 4),
        (ObjectType::DkeyLexical, "DAOS_OT_DKEY_LEXICAL", 5),
        (ObjectType::AkeyLexical, "DAOS_OT_AKEY_LEXICAL", 6),
        (ObjectType::MultiLexical, "DAOS_OT_MULTI_LEXICAL", 7),
        (ObjectType::KvHashed, "DAOS_OT_KV_HASHED", 8),
        (ObjectType::KvUint64, "DAOS_OT_KV_UINT64", 9),
        (ObjectType::KvLexical, "DAOS_OT_KV_LEXICAL", 10),
        (ObjectType::Array, "DAOS_OT_ARRAY", 11),
        (ObjectType::ArrayAttr, "DAOS_OT_ARRAY_ATTR", 12),
        (ObjectType::ArrayByte, "DAOS_OT_ARRAY_BYTE", 13),
    ];

    /// The `DAOS_OT_*` value.
    pub fn value(self) -> u32 {
        Self::ALL
            .iter()
            .find(|e| e.0 == self)
            .map(|e| e.2)
            .expect("every type is in the table")
    }

    fn from_raw(v: u32) -> Option<ObjectType> {
        Self::ALL.iter().find(|e| e.2 == v).map(|e| e.0)
    }

    /// dkeys are 8-byte integers (`daos_is_dkey_uint64_type`).
    pub fn dkey_is_u64(self) -> bool {
        matches!(
            self,
            ObjectType::ArrayByte
                | ObjectType::MultiUint64
                | ObjectType::DkeyUint64
                | ObjectType::KvUint64
                | ObjectType::Array
                | ObjectType::ArrayAttr
        )
    }

    /// akeys are 8-byte integers (`daos_is_akey_uint64_type`).
    pub fn akey_is_u64(self) -> bool {
        matches!(self, ObjectType::MultiUint64 | ObjectType::AkeyUint64)
    }

    /// A flat KV type (`daos_is_kv_type`).
    pub fn is_kv(self) -> bool {
        matches!(
            self,
            ObjectType::KvHashed | ObjectType::KvUint64 | ObjectType::KvLexical
        )
    }

    /// An array type (`daos_is_array_type`).
    pub fn is_array(self) -> bool {
        matches!(
            self,
            ObjectType::Array | ObjectType::ArrayAttr | ObjectType::ArrayByte
        )
    }
}

/// An object class id (`daos_oclass_id_t`): redundancy in the upper 8 bits,
/// group count in the low 16.
///
/// Only a few classes are compiled in (their ids are equal in DAOS 2.6.0,
/// 2.6.4 and 2.8.1). Every other class is looked up by name in the loaded
/// library with [`ObjectClass::from_name`], so the library decides what
/// exists (for example the `EC_*P3*` classes of 2.6.4 and later).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct ObjectClass(u32);

macro_rules! classes {
    ($( $(#[$m:meta])* $name:ident = $c:literal ($ord:expr, $grp:expr) ),* $(,)?) => {
        impl ObjectClass {
            $( $(#[$m])* pub const $name: ObjectClass = ObjectClass(($ord) << OC_REDUN_SHIFT | ($grp)); )*

            /// Compiled-in classes with their C names, for the probe lines.
            pub(crate) const NAMED: &'static [(&'static str, ObjectClass)] = &[
                $( ($c, ObjectClass::$name), )*
            ];
        }
    };
}

const X: u32 = sys::MAX_NUM_GROUPS;

classes! {
    /// No redundancy, one group.
    S1 = "OC_S1" (1, 1),
    /// No redundancy, as many groups as targets.
    SX = "OC_SX" (1, X),
    /// Two replicas, one group.
    RP_2G1 = "OC_RP_2G1" (8, 1),
    /// Two replicas, all groups.
    RP_2GX = "OC_RP_2GX" (8, X),
    /// Three replicas, one group.
    RP_3G1 = "OC_RP_3G1" (9, 1),
    /// Three replicas, all groups.
    RP_3GX = "OC_RP_3GX" (9, X),
    /// Erasure code 2+1, one group.
    EC_2P1G1 = "OC_EC_2P1G1" (32, 1),
    /// Erasure code 2+1, all groups.
    EC_2P1GX = "OC_EC_2P1GX" (32, X),
    /// Erasure code 4+1, one group.
    EC_4P1G1 = "OC_EC_4P1G1" (34, 1),
    /// Erasure code 4+1, all groups.
    EC_4P1GX = "OC_EC_4P1GX" (34, X),
    /// Erasure code 4+2, one group.
    EC_4P2G1 = "OC_EC_4P2G1" (35, 1),
    /// Erasure code 4+2, all groups.
    EC_4P2GX = "OC_EC_4P2GX" (35, X),
    /// Erasure code 8+2, one group.
    EC_8P2G1 = "OC_EC_8P2G1" (37, 1),
    /// Erasure code 8+2, all groups.
    EC_8P2GX = "OC_EC_8P2GX" (37, X),
    /// Erasure code 16+2, one group.
    EC_16P2G1 = "OC_EC_16P2G1" (39, 1),
    /// Erasure code 16+2, all groups.
    EC_16P2GX = "OC_EC_16P2GX" (39, X),
}

impl fmt::Debug for ObjectClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match Self::NAMED.iter().find(|e| e.1 == *self) {
            Some((name, _)) => write!(f, "ObjectClass({name})"),
            None => write!(f, "ObjectClass({:#x})", self.0),
        }
    }
}

impl ObjectClass {
    /// `OC_UNKNOWN`: let DAOS choose from the container's redundancy factor
    /// and the hints.
    pub const UNKNOWN: ObjectClass = ObjectClass(0);

    /// The class id.
    pub fn id(self) -> u32 {
        self.0
    }

    /// From a class id.
    pub fn from_id(id: u32) -> ObjectClass {
        ObjectClass(id)
    }

    /// Look a class up by name (for example `"RP_2G1"` or `"EC_4P3GX"`) in
    /// the loaded library (`daos_oclass_name2id`). Needs a completed
    /// [`init`](crate::init); with another backend (the in-memory one of
    /// the `mock` feature) use [`Container::class_from_name`].
    pub fn from_name(name: &str) -> Result<ObjectClass> {
        let rt = crate::runtime::runtime()?;
        Self::from_name_on(rt.backend.as_ref(), name)
    }

    /// The class name from the loaded library (`daos_oclass_id2name`).
    /// Needs a completed [`init`](crate::init); with another backend use
    /// [`Container::class_name`].
    pub fn name(self) -> Result<String> {
        let rt = crate::runtime::runtime()?;
        self.name_on(rt.backend.as_ref())
    }

    fn from_name_on(backend: &dyn Raw, name: &str) -> Result<ObjectClass> {
        crate::conn::require(backend, crate::sys::api::Group::Object)?;
        let c = crate::conn::cstring(name, "class names must not contain NUL")?;
        // SAFETY: NUL-terminated name; the call only reads it.
        let id = unsafe { backend.daos_oclass_name2id(c.as_ptr()) };
        if id <= 0 {
            return Err(Error::InvalidArgument("unknown object class name"));
        }
        Ok(ObjectClass(id as u32))
    }

    fn name_on(self, backend: &dyn Raw) -> Result<String> {
        crate::conn::require(backend, crate::sys::api::Group::Object)?;
        let mut buf = [0 as std::ffi::c_char; sys::MAX_OBJ_CLASS_NAME_LEN + 1];
        // SAFETY: the buffer holds MAX_OBJ_CLASS_NAME_LEN characters plus the
        // terminator, the size libdaos writes at most.
        let rc = unsafe { backend.daos_oclass_id2name(self.0, buf.as_mut_ptr()) };
        if rc < 0 {
            return Err(Error::InvalidArgument("unknown object class id"));
        }
        Ok(crate::pool::c_label(&buf))
    }
}

impl Container {
    /// As [`ObjectClass::from_name`], through the library or backend of
    /// this container.
    pub fn class_from_name(&self, name: &str) -> Result<ObjectClass> {
        self.conn.stamp.check()?;
        ObjectClass::from_name_on(self.backend(), name)
    }

    /// As [`ObjectClass::name`], through the library or backend of this
    /// container.
    pub fn class_name(&self, class: ObjectClass) -> Result<String> {
        self.conn.stamp.check()?;
        class.name_on(self.backend())
    }
}

/// Redundancy hint (`DAOS_OCH_RDD_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Redundancy {
    /// No hint: the container's redundancy factor decides.
    #[default]
    Default,
    /// No redundancy.
    None,
    /// Replication.
    Replicated,
    /// Erasure code.
    ErasureCode,
}

/// Sharding hint (`DAOS_OCH_SHD_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Sharding {
    /// No hint: all groups for arrays and flat KV, one group otherwise.
    #[default]
    Default,
    /// One group.
    Tiny,
    /// max(128, 25%) of the targets.
    Regular,
    /// max(256, 50%).
    High,
    /// max(1024, 80%).
    Extended,
    /// All targets.
    Max,
}

/// Hints for [`Container::generate_oid`](crate::Container::generate_oid)
/// with [`ObjectClass::UNKNOWN`]; ignored for any other class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClassHints {
    /// Redundancy hint.
    pub redundancy: Redundancy,
    /// Sharding hint.
    pub sharding: Sharding,
}

impl ClassHints {
    /// The `daos_oclass_hints_t` bits.
    pub fn bits(self) -> u16 {
        let r = match self.redundancy {
            Redundancy::Default => 0,
            Redundancy::None => 1 << 1,
            Redundancy::Replicated => 1 << 2,
            Redundancy::ErasureCode => 1 << 3,
        };
        let s = match self.sharding {
            Sharding::Default => 0,
            Sharding::Tiny => 1 << 5,
            Sharding::Regular => 1 << 6,
            Sharding::High => 1 << 7,
            Sharding::Extended => 1 << 8,
            Sharding::Max => 1 << 9,
        };
        r | s
    }
}

/// `DAOS_OCH_*` names and values, for the probe lines.
#[cfg(test)]
const HINT_NAMES: [(&str, u32); 10] = [
    ("DAOS_OCH_RDD_DEF", 1 << 0),
    ("DAOS_OCH_RDD_NO", 1 << 1),
    ("DAOS_OCH_RDD_RP", 1 << 2),
    ("DAOS_OCH_RDD_EC", 1 << 3),
    ("DAOS_OCH_SHD_DEF", 1 << 4),
    ("DAOS_OCH_SHD_TINY", 1 << 5),
    ("DAOS_OCH_SHD_REG", 1 << 6),
    ("DAOS_OCH_SHD_HI", 1 << 7),
    ("DAOS_OCH_SHD_EXT", 1 << 8),
    ("DAOS_OCH_SHD_MAX", 1 << 9),
];

/// Test vectors printed by the probe: (type, redundancy, groups, low 32).
#[cfg(test)]
const OID_VECTORS: [(u32, u32, u32, u32); 3] = [
    (8, 1, 1, 0x1234_5678),
    (13, 34, 65535, 0x9abc_def0),
    (4, 9, 16, 0),
];

/// Lines of the ABI probe that describe this module, in the probe's order.
#[cfg(test)]
pub(crate) fn probe_lines() -> Vec<String> {
    let mut v = Vec::new();
    for (_, name, value) in ObjectType::ALL {
        v.push(format!("const {name} {value}"));
    }
    for (name, class) in ObjectClass::NAMED {
        v.push(format!("const {name} {}", class.id()));
    }
    for (name, value) in HINT_NAMES {
        v.push(format!("const {name} {value}"));
    }
    for (ty, ord, grp, low) in OID_VECTORS {
        let oid = ObjectId::encode(1, low, ty, ord, grp);
        let raw_type = (oid.hi >> TYPE_SHIFT) as u32;
        v.push(format!(
            "oid {} {} {} {}",
            oid.hi,
            raw_type,
            oid.class().id(),
            oid.groups()
        ));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_and_classify() {
        let oid = ObjectId::encode(5, 0x1234_5678, ObjectType::ArrayByte.value(), 34, 65535);
        assert_eq!(oid.object_type(), Some(ObjectType::ArrayByte));
        assert_eq!(oid.class(), ObjectClass::EC_4P1GX);
        assert!(oid.is_erasure_coded());
        assert_eq!(oid.hi & 0xFFFF_FFFF, 0x1234_5678);
        assert_eq!(oid.to_string(), format!("{}.5", oid.hi));
        let oid = ObjectId::encode(5, 0, ObjectType::KvHashed.value(), 8, 1);
        assert_eq!(oid.class(), ObjectClass::RP_2G1);
        assert!(!oid.is_erasure_coded());
        assert!(ObjectType::KvUint64.dkey_is_u64());
        assert!(ObjectType::Array.dkey_is_u64());
        assert!(!ObjectType::KvHashed.dkey_is_u64());
        assert!(ObjectType::MultiUint64.akey_is_u64());
        assert!(!ObjectType::DkeyUint64.akey_is_u64());
        assert_eq!(ObjectClass::S1.id(), 16_777_217);
        assert_eq!(ObjectClass::SX.id(), 16_842_751);
        assert_eq!(ObjectClass::EC_16P2GX.id(), 654_376_959);
        assert_eq!(
            ClassHints {
                redundancy: Redundancy::Replicated,
                sharding: Sharding::Max
            }
            .bits(),
            (1 << 2) | (1 << 9)
        );
        assert_eq!(ClassHints::default().bits(), 0);
        assert!(ObjectId::default().is_nil());
    }
}
