// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions transcribed from DAOS headers, BSD-2-Clause-Patent, see LICENSE-THIRD-PARTY

//! Reading container properties.
//!
//! Two owners, never mixed: a property list that libdaos fills must have
//! been allocated by `daos_prop_alloc`, because `daos_prop_free` frees every
//! entry value with the DAOS allocator. Lists built by the crate (container
//! creation) are plain Rust memory and never reach `daos_prop_free`.

use std::ffi::CStr;

use super::Layout;
use crate::error::{Error, Result};
use crate::sys::api::Raw;
use crate::sys::{self, daos_handle_t, daos_prop_t};

/// Container properties (a copy taken by
/// [`Container::properties`](crate::Container::properties)).
///
/// Fields are `None` when the server did not report them. The defaults
/// behind unset properties come from the pool and the client library and can
/// differ between DAOS versions and systems; do not assume them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Properties {
    /// Current label (may differ from the string the container was opened
    /// with).
    pub label: Option<String>,
    /// Layout type (`Unknown` also for layout values this crate does not
    /// name; see `layout_value`).
    pub layout: Option<Layout>,
    /// Raw layout type value.
    pub layout_value: Option<u64>,
    /// Layout version.
    pub layout_version: Option<u64>,
    /// Redundancy factor (0 to 4).
    pub redundancy_factor: Option<u64>,
    /// Redundancy level (1 = rank, 2 = node, 3 = fault domain on 2.8).
    pub redundancy_level: Option<u64>,
    /// Erasure-code cell size in bytes.
    pub ec_cell_size: Option<u64>,
    /// Owner (`user@`).
    pub owner: Option<String>,
    /// Owner group (`group@`).
    pub owner_group: Option<String>,
    /// Checksum type number (`DAOS_PROP_CO_CSUM_*`).
    pub checksum: Option<u64>,
    /// Maximum number of snapshots kept (0 = no limit).
    pub snapshot_max: Option<u64>,
    /// Container health: `Some(true)` healthy, `Some(false)` unclean.
    pub healthy: Option<bool>,
    /// Highest object ID allocated so far (`DAOS_PROP_CO_ALLOCED_OID`).
    pub allocated_oid: Option<u64>,
}

impl Properties {
    /// Name of the checksum type, for the values DAOS defines.
    pub fn checksum_name(&self) -> Option<&'static str> {
        Some(match self.checksum? {
            0 => "off",
            1 => "crc16",
            2 => "crc32",
            3 => "crc64",
            4 => "sha1",
            5 => "sha256",
            6 => "sha512",
            7 => "adler32",
            _ => return None,
        })
    }
}

/// A property list allocated by `daos_prop_alloc`, freed in `Drop`.
struct PropQuery<'a> {
    backend: &'a dyn Raw,
    prop: *mut daos_prop_t,
}

impl Drop for PropQuery<'_> {
    fn drop(&mut self) {
        // SAFETY: allocated by daos_prop_alloc below, freed once.
        unsafe { self.backend.daos_prop_free(self.prop) };
    }
}

fn layout_of(v: u64) -> Layout {
    match v {
        sys::DAOS_PROP_CO_LAYOUT_POSIX => Layout::Posix,
        sys::DAOS_PROP_CO_LAYOUT_HDF5 => Layout::Hdf5,
        sys::DAOS_PROP_CO_LAYOUT_PYTHON => Layout::Python,
        _ => Layout::Unknown,
    }
}

/// `daos_cont_query` with an empty list from `daos_prop_alloc(0)` (which
/// asks for every property), copied into [`Properties`].
pub(crate) fn query(backend: &dyn Raw, coh: daos_handle_t, target: &str) -> Result<Properties> {
    // SAFETY: plain allocation call.
    let prop = unsafe { backend.daos_prop_alloc(0) };
    if prop.is_null() {
        return Err(Error::daos_at("daos_prop_alloc", -1009, target));
    }
    let q = PropQuery { backend, prop };
    // SAFETY: live container handle (the caller holds the gate); info NULL;
    // the property list came from daos_prop_alloc; ev NULL.
    let rc =
        unsafe { backend.daos_cont_query(coh, std::ptr::null_mut(), q.prop, std::ptr::null_mut()) };
    if rc != 0 {
        return Err(Error::daos_at("daos_cont_query", rc, target));
    }
    // SAFETY: libdaos filled the list: dpp_nr entries at dpp_entries.
    let entries = unsafe {
        let p = &*q.prop;
        if p.dpp_entries.is_null() || p.dpp_nr == 0 {
            &[][..]
        } else {
            std::slice::from_raw_parts(p.dpp_entries, p.dpp_nr as usize)
        }
    };
    let mut out = Properties::default();
    for e in entries {
        if e.dpe_flags & sys::DAOS_PROP_ENTRY_NOT_SET != 0 {
            continue;
        }
        let string = || {
            let ptr = e.dpe_val as usize as *const std::ffi::c_char;
            if ptr.is_null() {
                None
            } else {
                // SAFETY: for the string properties the union holds a
                // NUL-terminated string owned by the list.
                Some(
                    unsafe { CStr::from_ptr(ptr) }
                        .to_string_lossy()
                        .into_owned(),
                )
            }
        };
        match e.dpe_type {
            sys::DAOS_PROP_CO_LABEL => out.label = string(),
            sys::DAOS_PROP_CO_OWNER => out.owner = string(),
            sys::DAOS_PROP_CO_OWNER_GROUP => out.owner_group = string(),
            sys::DAOS_PROP_CO_LAYOUT_TYPE => {
                out.layout = Some(layout_of(e.dpe_val));
                out.layout_value = Some(e.dpe_val);
            }
            sys::DAOS_PROP_CO_LAYOUT_VER => out.layout_version = Some(e.dpe_val),
            sys::DAOS_PROP_CO_REDUN_FAC => out.redundancy_factor = Some(e.dpe_val),
            sys::DAOS_PROP_CO_REDUN_LVL => out.redundancy_level = Some(e.dpe_val),
            sys::DAOS_PROP_CO_EC_CELL_SZ => out.ec_cell_size = Some(e.dpe_val),
            sys::DAOS_PROP_CO_CSUM => out.checksum = Some(e.dpe_val),
            sys::DAOS_PROP_CO_SNAPSHOT_MAX => out.snapshot_max = Some(e.dpe_val),
            // DAOS_PROP_CO_STATUS_VAL: status in bits 32 to 47.
            sys::DAOS_PROP_CO_STATUS => out.healthy = Some((e.dpe_val >> 32) & 0xFFFF == 0),
            sys::DAOS_PROP_CO_ALLOCED_OID => out.allocated_oid = Some(e.dpe_val),
            // ACL and roots point to nested structures: skipped.
            _ => {}
        }
    }
    drop(q);
    Ok(out)
}
