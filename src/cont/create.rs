// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions transcribed from DAOS headers, BSD-2-Clause-Patent, see LICENSE-THIRD-PARTY

//! Container creation.

use crate::error::{Error, Result};
use crate::obj::ObjectClass;
use crate::sys::api::Raw;
use crate::sys::{self, daos_handle_t, daos_prop_entry, daos_prop_t};
use crate::uuid::Uuid;

/// Container layout type (`DAOS_PROP_CO_LAYOUT_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Layout {
    /// No layout (a plain DAOS container for the object API).
    #[default]
    Unknown,
    /// POSIX (DFS). Created through libdfs, which writes the superblock a
    /// mount needs.
    Posix,
    /// HDF5 DAOS VOL connector.
    Hdf5,
    /// PyDAOS.
    Python,
}

impl Layout {
    pub(crate) fn value(self) -> u64 {
        match self {
            Layout::Unknown => sys::DAOS_PROP_CO_LAYOUT_UNKNOWN,
            Layout::Posix => sys::DAOS_PROP_CO_LAYOUT_POSIX,
            Layout::Hdf5 => sys::DAOS_PROP_CO_LAYOUT_HDF5,
            Layout::Python => sys::DAOS_PROP_CO_LAYOUT_PYTHON,
        }
    }
}

/// Fault domain level for object placement (`DAOS_PROP_CO_REDUN_LVL`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RedundancyLevel {
    /// Engine (rank) level.
    Rank,
    /// Node level.
    Node,
}

impl RedundancyLevel {
    pub(crate) fn value(self) -> u64 {
        match self {
            RedundancyLevel::Rank => sys::DAOS_PROP_CO_REDUN_RANK,
            RedundancyLevel::Node => sys::DAOS_PROP_CO_REDUN_NODE,
        }
    }
}

/// What [`Pool::create_container`](crate::Pool::create_container) creates.
///
/// Unset values are left to the loaded library and the pool (chunk size,
/// object classes, redundancy factor, checksums): their defaults differ
/// between DAOS 2.6 and 2.8.
///
/// ```
/// use daos::cont::{ContainerCreate, Layout};
/// let posix = ContainerCreate::new().label("datasets").layout(Layout::Posix).chunk_size(1 << 20);
/// let plain = ContainerCreate::new().label("objects").redundancy_factor(0);
/// # let _ = (posix, plain);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ContainerCreate {
    label: Option<String>,
    layout: Layout,
    redundancy_factor: Option<u8>,
    redundancy_level: Option<RedundancyLevel>,
    ec_cell_size: Option<u64>,
    chunk_size: Option<u64>,
    object_class: Option<ObjectClass>,
    dir_object_class: Option<ObjectClass>,
    file_object_class: Option<ObjectClass>,
}

/// Label rules of `daos_label_is_valid`: 1 to 127 characters of
/// `[A-Za-z0-9._:-]`, and not a UUID string.
pub(crate) fn validate_label(label: &str) -> Result<()> {
    if label.is_empty() || label.len() > sys::DAOS_PROP_LABEL_MAX_LEN {
        return Err(Error::InvalidArgument(
            "a container label has 1 to 127 characters",
        ));
    }
    let ok = label
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'));
    if !ok {
        return Err(Error::InvalidArgument(
            "a container label consists of letters, digits and . _ : -",
        ));
    }
    if Uuid::parse(label).is_some() {
        return Err(Error::InvalidArgument(
            "a container label must not be a UUID",
        ));
    }
    Ok(())
}

impl ContainerCreate {
    /// Nothing set: an unlabelled container with layout `Unknown`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Container label.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Layout type.
    pub fn layout(mut self, layout: Layout) -> Self {
        self.layout = layout;
        self
    }

    /// Redundancy factor 0 to 4 (`DAOS_PROP_CO_REDUN_FAC`).
    pub fn redundancy_factor(mut self, rf: u8) -> Self {
        self.redundancy_factor = Some(rf);
        self
    }

    /// Redundancy level (`DAOS_PROP_CO_REDUN_LVL`).
    pub fn redundancy_level(mut self, level: RedundancyLevel) -> Self {
        self.redundancy_level = Some(level);
        self
    }

    /// Erasure-code cell size in bytes (`DAOS_PROP_CO_EC_CELL_SZ`).
    pub fn ec_cell_size(mut self, bytes: u64) -> Self {
        self.ec_cell_size = Some(bytes);
        self
    }

    /// POSIX only: default chunk size of files.
    pub fn chunk_size(mut self, bytes: u64) -> Self {
        self.chunk_size = Some(bytes);
        self
    }

    /// POSIX only: default object class of all objects.
    pub fn object_class(mut self, class: ObjectClass) -> Self {
        self.object_class = Some(class);
        self
    }

    /// POSIX only: default object class of directories.
    pub fn dir_object_class(mut self, class: ObjectClass) -> Self {
        self.dir_object_class = Some(class);
        self
    }

    /// POSIX only: default object class of files.
    pub fn file_object_class(mut self, class: ObjectClass) -> Self {
        self.file_object_class = Some(class);
        self
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if let Some(l) = &self.label {
            validate_label(l)?;
        }
        if self.redundancy_factor.is_some_and(|rf| rf > 4) {
            return Err(Error::InvalidArgument("the redundancy factor is 0 to 4"));
        }
        if self.ec_cell_size == Some(0) {
            return Err(Error::InvalidArgument("the EC cell size must not be 0"));
        }
        let posix_only = self.chunk_size.is_some()
            || self.object_class.is_some()
            || self.dir_object_class.is_some()
            || self.file_object_class.is_some();
        if posix_only && self.layout != Layout::Posix {
            return Err(Error::InvalidArgument(
                "chunk size and default object classes exist only for POSIX containers",
            ));
        }
        Ok(())
    }

    /// Generic property entries (never layout for POSIX, never label or
    /// roots). Every value is an integer: no pointers to free.
    pub(crate) fn entries(&self) -> Vec<daos_prop_entry> {
        let entry = |t: u32, v: u64| daos_prop_entry {
            dpe_type: t,
            dpe_flags: 0,
            dpe_reserv: 0,
            dpe_val: v,
        };
        let mut v = Vec::new();
        if self.layout != Layout::Posix {
            v.push(entry(sys::DAOS_PROP_CO_LAYOUT_TYPE, self.layout.value()));
        }
        if let Some(rf) = self.redundancy_factor {
            v.push(entry(sys::DAOS_PROP_CO_REDUN_FAC, u64::from(rf)));
        }
        if let Some(l) = self.redundancy_level {
            v.push(entry(sys::DAOS_PROP_CO_REDUN_LVL, l.value()));
        }
        if let Some(c) = self.ec_cell_size {
            v.push(entry(sys::DAOS_PROP_CO_EC_CELL_SZ, c));
        }
        v
    }

    /// Run the creation route:
    ///
    /// | Layout | Label | Call |
    /// |---|---|---|
    /// | POSIX | yes | `dfs_cont_create_with_label` |
    /// | POSIX | no | `dfs_cont_create` |
    /// | other | yes | `daos_cont_create_with_label` |
    /// | other | no | `daos_cont_create2` |
    ///
    /// The property list is built in Rust and only read by libdaos and
    /// libdfs during the call; it is never passed to `daos_prop_free`.
    pub(crate) fn create(&self, backend: &dyn Raw, poh: daos_handle_t, pool: &str) -> Result<Uuid> {
        let mut entries = self.entries();
        let mut prop = daos_prop_t {
            dpp_nr: entries.len() as u32,
            dpp_reserv: 0,
            dpp_entries: if entries.is_empty() {
                std::ptr::null_mut()
            } else {
                entries.as_mut_ptr()
            },
        };
        let prop_ptr: *mut daos_prop_t = if entries.is_empty() {
            std::ptr::null_mut()
        } else {
            &mut prop
        };
        let label = self
            .label
            .as_deref()
            .map(|l| crate::conn::cstring(l, "labels must not contain NUL"))
            .transpose()?;
        let target = match &self.label {
            Some(l) => format!("{pool}/{l}"),
            None => pool.to_string(),
        };
        let mut uuid: sys::uuid_t = [0; 16];
        if self.layout == Layout::Posix {
            let mut attr = sys::dfs_attr_t::zeroed();
            attr.da_chunk_size = self.chunk_size.unwrap_or(0);
            attr.da_oclass_id = self.object_class.map_or(0, |c| c.id());
            attr.da_dir_oclass_id = self.dir_object_class.map_or(0, |c| c.id());
            attr.da_file_oclass_id = self.file_object_class.map_or(0, |c| c.id());
            attr.da_props = prop_ptr;
            let (op, rc) = match &label {
                // SAFETY: live pool handle (the caller holds the pool gate);
                // NUL-terminated label; attr and its property list are locals
                // that outlive the call (libdfs swaps da_props for its own
                // merged list during the call and restores it); uuid is an
                // out-buffer of 16 bytes; coh and dfs NULL (not opened).
                Some(l) => ("dfs_cont_create_with_label", unsafe {
                    backend.dfs_cont_create_with_label(
                        poh,
                        l.as_ptr(),
                        &mut attr,
                        &mut uuid,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                }),
                // SAFETY: as above; the uuid out-pointer must not be NULL.
                None => ("dfs_cont_create", unsafe {
                    backend.dfs_cont_create(
                        poh,
                        &mut uuid,
                        &mut attr,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                }),
            };
            debug_assert!(attr.da_props == prop_ptr, "libdfs restores da_props");
            if rc != 0 {
                return Err(Error::dfs(op, rc, target));
            }
        } else {
            let (op, rc) = match &label {
                // SAFETY: live pool handle; NUL-terminated label; the
                // property list points to `entries`, alive for the call;
                // uuid is a 16-byte out-buffer; ev NULL.
                Some(l) => ("daos_cont_create_with_label", unsafe {
                    backend.daos_cont_create_with_label(
                        poh,
                        l.as_ptr(),
                        prop_ptr,
                        &mut uuid,
                        std::ptr::null_mut(),
                    )
                }),
                // SAFETY: as above.
                None => ("daos_cont_create", unsafe {
                    backend.daos_cont_create2(poh, &mut uuid, prop_ptr, std::ptr::null_mut())
                }),
            };
            if rc != 0 {
                return Err(Error::daos_at(op, rc, target));
            }
        }
        drop(entries);
        Ok(Uuid::from_bytes(uuid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        validate_label("rust-daos-test-1_a.b:c").unwrap();
        validate_label(&"x".repeat(127)).unwrap();
        assert!(validate_label("").is_err());
        assert!(validate_label(&"x".repeat(128)).is_err());
        assert!(validate_label("a b").is_err());
        assert!(validate_label("a/b").is_err());
        assert!(validate_label("00112233-4455-6677-8899-aabbccddeeff").is_err());
    }

    #[test]
    fn posix_only_fields_and_entries() {
        let c = ContainerCreate::new().chunk_size(1);
        assert!(c.validate().is_err());
        let c = ContainerCreate::new().layout(Layout::Posix).chunk_size(1);
        c.validate().unwrap();
        assert!(
            c.entries().is_empty(),
            "POSIX never carries the layout entry"
        );
        let c = ContainerCreate::new()
            .redundancy_factor(2)
            .redundancy_level(RedundancyLevel::Node)
            .ec_cell_size(1 << 20);
        let e = c.entries();
        let types: Vec<u32> = e.iter().map(|e| e.dpe_type).collect();
        assert_eq!(
            types,
            [
                sys::DAOS_PROP_CO_LAYOUT_TYPE,
                sys::DAOS_PROP_CO_REDUN_FAC,
                sys::DAOS_PROP_CO_REDUN_LVL,
                sys::DAOS_PROP_CO_EC_CELL_SZ
            ]
        );
        assert_eq!(e[1].dpe_val, 2);
        assert!(ContainerCreate::new()
            .redundancy_factor(5)
            .validate()
            .is_err());
    }
}
