// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions transcribed from DAOS headers, BSD-2-Clause-Patent, see LICENSE-THIRD-PARTY

//! Raw declarations transcribed from the DAOS public headers.
//!
//! Every type here was compared between DAOS v2.6.0, v2.6.4 and v2.8.1 by the
//! ABI probe (`tools/abi-probe`), whose output is checked in under `abi/` and
//! compared with these declarations by a unit test.
//!
//! Private to the crate: the raw surface carries no semver promise.

#![allow(non_camel_case_types)]
#![allow(dead_code)]

use std::ffi::{c_char, c_int, c_void};
use std::marker::{PhantomData, PhantomPinned};

pub(crate) mod api;
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub(crate) mod lib_dfs;

/// `daos_size_t` (`daos_types.h`).
pub(crate) type daos_size_t = u64;
/// `daos_off_t` (`daos_types.h`).
pub(crate) type daos_off_t = u64;
/// `daos_oclass_id_t` (`daos_obj_class.h`).
pub(crate) type daos_oclass_id_t = u32;
/// `mode_t` of 64-bit Linux.
pub(crate) type mode_t = u32;

/// Opaque `dfs_t` (`typedef struct dfs dfs_t;`).
#[repr(C)]
pub(crate) struct dfs_t {
    _data: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

/// Opaque `dfs_obj_t` (`typedef struct dfs_obj dfs_obj_t;`).
#[repr(C)]
pub(crate) struct dfs_obj_t {
    _data: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

/// `d_iov_t` (`gurt/types.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct d_iov_t {
    pub iov_buf: *mut c_void,
    pub iov_buf_len: usize,
    pub iov_len: usize,
}

/// `d_sg_list_t` (`gurt/types.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct d_sg_list_t {
    pub sg_nr: u32,
    pub sg_nr_out: u32,
    pub sg_iovs: *mut d_iov_t,
}

/// `DAOS_ANCHOR_BUF_MAX` (`daos_types.h`).
pub(crate) const DAOS_ANCHOR_BUF_MAX: usize = 104;
/// `DAOS_ANCHOR_TYPE_EOF` (`daos_types.h`).
pub(crate) const DAOS_ANCHOR_TYPE_EOF: u16 = 3;

/// `daos_anchor_t` (`daos_types.h`). All zero is the start position
/// (`DAOS_ANCHOR_INIT`).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct daos_anchor_t {
    pub da_type: u16,
    pub da_shard: u16,
    pub da_flags: u32,
    pub da_sub_anchors: u64,
    pub da_buf: [u8; DAOS_ANCHOR_BUF_MAX],
}

impl daos_anchor_t {
    pub(crate) const fn new() -> Self {
        daos_anchor_t {
            da_type: 0,
            da_shard: 0,
            da_flags: 0,
            da_sub_anchors: 0,
            da_buf: [0; DAOS_ANCHOR_BUF_MAX],
        }
    }

    /// `daos_anchor_is_eof` (`daos_api.h`), an inline function in C.
    pub(crate) fn is_eof(&self) -> bool {
        self.da_type == DAOS_ANCHOR_TYPE_EOF
    }
}

/// `daos_obj_id_t` (`daos_types.h`). Only passed as NULL in v0.1.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct daos_obj_id_t {
    pub lo: u64,
    pub hi: u64,
}

/// `daos_handle_t` (`daos_types.h`): a cookie that names a pool, container,
/// object, transaction or event queue inside libdaos. Passed by value.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) struct daos_handle_t {
    pub cookie: u64,
}

/// `DAOS_PC_RO` (`daos_pool.h`): connect to a pool read-only.
pub(crate) const DAOS_PC_RO: u32 = 1 << 0;
/// `DAOS_PC_RW` (`daos_pool.h`): connect to a pool read-write.
pub(crate) const DAOS_PC_RW: u32 = 1 << 1;
/// `DAOS_COO_RO` (`daos_cont.h`): open a container read-only.
pub(crate) const DAOS_COO_RO: u32 = 1 << 0;
/// `DAOS_COO_RW` (`daos_cont.h`): open a container read-write.
pub(crate) const DAOS_COO_RW: u32 = 1 << 1;

/// `uuid_t` (`uuid/uuid.h`).
pub(crate) type uuid_t = [u8; 16];
/// `daos_epoch_t` (`daos_types.h`).
pub(crate) type daos_epoch_t = u64;
/// `d_rank_t` (`gurt/types.h`).
pub(crate) type d_rank_t = u32;

/// `daos_epoch_range_t` (`daos_types.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct daos_epoch_range_t {
    pub epr_lo: daos_epoch_t,
    pub epr_hi: daos_epoch_t,
}

/// `struct daos_space` (`daos_pool.h`): index 0 is SCM, 1 is NVMe
/// (`enum daos_media_type_t`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct daos_space {
    pub s_total: [u64; 2],
    pub s_free: [u64; 2],
}

/// `struct daos_pool_space` (`daos_pool.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct daos_pool_space {
    pub ps_space: daos_space,
    pub ps_free_min: [u64; 2],
    pub ps_free_max: [u64; 2],
    pub ps_free_mean: [u64; 2],
    pub ps_ntargets: u32,
    pub ps_padding: u32,
}

/// `struct daos_rebuild_status` (`daos_pool.h`). Bytes 16 to 19 changed
/// meaning between 2.6.0, 2.6.4 and 2.8.1 and are not read.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct daos_rebuild_status {
    pub rs_version: u32,
    pub rs_seconds: u32,
    pub rs_errno: i32,
    pub rs_state: i32,
    pub rs_reserved: [u8; 4],
    pub rs_fail_rank: i32,
    pub rs_toberb_obj_nr: u64,
    pub rs_obj_nr: u64,
    pub rs_rec_nr: u64,
    pub rs_size: u64,
}

/// `daos_pool_info_t` (`daos_pool.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct daos_pool_info_t {
    pub pi_uuid: uuid_t,
    pub pi_ntargets: u32,
    pub pi_nnodes: u32,
    pub pi_ndisabled: u32,
    pub pi_map_ver: u32,
    pub pi_leader: u32,
    pub pi_bits: u64,
    pub pi_space: daos_pool_space,
    pub pi_rebuild_st: daos_rebuild_status,
}

/// `DPI_SPACE` (`daos_pool.h`).
pub(crate) const DPI_SPACE: u64 = 1 << 0;
/// `DPI_REBUILD_STATUS` (`daos_pool.h`).
pub(crate) const DPI_REBUILD_STATUS: u64 = 1 << 1;

/// `daos_target_info_t` (`daos_pool.h`). The first field is `ta_type` in
/// 2.6 and padding in 2.8; it is not read.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct daos_target_info_t {
    pub ta_reserved: u32,
    pub ta_state: i32,
    pub ta_perf: i32,
    pub ta_perf_padding: u32,
    pub ta_space: daos_space,
}

/// `struct daos_pool_cont_info` (`daos_pool.h`).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct daos_pool_cont_info {
    pub pci_uuid: uuid_t,
    pub pci_label: [c_char; DAOS_PROP_LABEL_MAX_LEN + 1],
}

/// `daos_cont_info_t` (`daos_cont.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct daos_cont_info_t {
    pub ci_uuid: uuid_t,
    pub ci_lsnapshot: daos_epoch_t,
    pub ci_nhandles: u32,
    pub ci_nsnapshots: u32,
    pub ci_md_otime: u64,
    pub ci_md_mtime: u64,
}

/// `struct daos_prop_entry` (`daos_prop.h`). The value is a union of
/// `uint64_t dpe_val`, `d_string_t dpe_str` and `void *dpe_val_ptr`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct daos_prop_entry {
    pub dpe_type: u32,
    pub dpe_flags: u16,
    pub dpe_reserv: u16,
    pub dpe_val: u64,
}

/// `daos_prop_t` (`daos_prop.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct daos_prop_t {
    pub dpp_nr: u32,
    pub dpp_reserv: u32,
    pub dpp_entries: *mut daos_prop_entry,
}

/// `DAOS_CONT_HINT_MAX_LEN` (`daos_cont.h`).
pub(crate) const DAOS_CONT_HINT_MAX_LEN: usize = 128;

/// `dfs_attr_t` (`daos_fs.h`).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct dfs_attr_t {
    pub da_id: u64,
    pub da_chunk_size: daos_size_t,
    pub da_oclass_id: daos_oclass_id_t,
    pub da_props: *mut daos_prop_t,
    pub da_mode: u32,
    pub da_dir_oclass_id: daos_oclass_id_t,
    pub da_file_oclass_id: daos_oclass_id_t,
    pub da_hints: [c_char; DAOS_CONT_HINT_MAX_LEN],
}

impl dfs_attr_t {
    pub(crate) fn zeroed() -> Self {
        // SAFETY: integers, a nullable pointer and a char array: all-zero is
        // the "nothing set" value libdfs expects.
        unsafe { std::mem::zeroed() }
    }
}

/// `DAOS_PROP_LABEL_MAX_LEN` (`daos_prop.h`).
pub(crate) const DAOS_PROP_LABEL_MAX_LEN: usize = 127;
/// `DAOS_PROP_ENTRIES_MAX_NR` (`daos_prop.h`).
pub(crate) const DAOS_PROP_ENTRIES_MAX_NR: usize = 128;

/// Container property types (`enum daos_cont_props`, `daos_prop.h`).
pub(crate) const DAOS_PROP_CO_LABEL: u32 = 4097;
pub(crate) const DAOS_PROP_CO_LAYOUT_TYPE: u32 = 4098;
pub(crate) const DAOS_PROP_CO_LAYOUT_VER: u32 = 4099;
pub(crate) const DAOS_PROP_CO_CSUM: u32 = 4100;
pub(crate) const DAOS_PROP_CO_REDUN_FAC: u32 = 4103;
pub(crate) const DAOS_PROP_CO_REDUN_LVL: u32 = 4104;
pub(crate) const DAOS_PROP_CO_SNAPSHOT_MAX: u32 = 4105;
pub(crate) const DAOS_PROP_CO_OWNER: u32 = 4109;
pub(crate) const DAOS_PROP_CO_OWNER_GROUP: u32 = 4110;
pub(crate) const DAOS_PROP_CO_STATUS: u32 = 4114;
pub(crate) const DAOS_PROP_CO_ALLOCED_OID: u32 = 4115;
pub(crate) const DAOS_PROP_CO_EC_CELL_SZ: u32 = 4116;
/// `DAOS_PROP_ENTRY_NOT_SET` (`daos_prop.h`): flag of an entry without value.
pub(crate) const DAOS_PROP_ENTRY_NOT_SET: u16 = 1 << 0;

/// Container layout types (`daos_prop.h`).
pub(crate) const DAOS_PROP_CO_LAYOUT_UNKNOWN: u64 = 0;
pub(crate) const DAOS_PROP_CO_LAYOUT_POSIX: u64 = 1;
pub(crate) const DAOS_PROP_CO_LAYOUT_HDF5: u64 = 2;
pub(crate) const DAOS_PROP_CO_LAYOUT_PYTHON: u64 = 3;
/// Redundancy levels (`daos_prop.h`).
pub(crate) const DAOS_PROP_CO_REDUN_RANK: u64 = 1;
pub(crate) const DAOS_PROP_CO_REDUN_NODE: u64 = 2;

/// `DAOS_COO_FORCE` (`daos_cont.h`).
pub(crate) const DAOS_COO_FORCE: u32 = 1 << 3;

/// `daos_key_t` (`daos_types.h`): a `d_iov_t`.
pub(crate) type daos_key_t = d_iov_t;

/// `daos_recx_t` (`daos_obj.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct daos_recx_t {
    pub rx_idx: u64,
    pub rx_nr: u64,
}

/// `daos_iod_type_t` (`daos_obj.h`), a C enum (4 bytes).
pub(crate) type daos_iod_type_t = u32;
/// `DAOS_IOD_SINGLE` (`daos_obj.h`).
pub(crate) const DAOS_IOD_SINGLE: daos_iod_type_t = 1;
/// `DAOS_IOD_ARRAY` (`daos_obj.h`).
pub(crate) const DAOS_IOD_ARRAY: daos_iod_type_t = 2;
/// `DAOS_REC_ANY` (`daos_obj.h`).
pub(crate) const DAOS_REC_ANY: u64 = 0;

/// `daos_iod_t` (`daos_obj.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct daos_iod_t {
    pub iod_name: daos_key_t,
    pub iod_type: daos_iod_type_t,
    pub iod_size: daos_size_t,
    pub iod_flags: u64,
    pub iod_nr: u32,
    pub iod_recxs: *mut daos_recx_t,
}

/// `daos_key_desc_t` (`daos_obj.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct daos_key_desc_t {
    pub kd_key_len: daos_size_t,
    pub kd_val_type: u32,
}

/// Object open modes (`daos_obj.h`).
pub(crate) const DAOS_OO_RO: u32 = 1 << 1;
pub(crate) const DAOS_OO_RW: u32 = 1 << 2;
pub(crate) const DAOS_OO_EXCL: u32 = 1 << 3;

/// Conditional flags of update, fetch and punch (`daos_obj.h`).
pub(crate) const DAOS_COND_PUNCH: u64 = 1 << 0;
pub(crate) const DAOS_COND_DKEY_INSERT: u64 = 1 << 1;
pub(crate) const DAOS_COND_DKEY_UPDATE: u64 = 1 << 2;
pub(crate) const DAOS_COND_DKEY_FETCH: u64 = 1 << 3;
pub(crate) const DAOS_COND_AKEY_INSERT: u64 = 1 << 4;
pub(crate) const DAOS_COND_AKEY_UPDATE: u64 = 1 << 5;
pub(crate) const DAOS_COND_AKEY_FETCH: u64 = 1 << 6;
/// Conditional flags of the KV API (`daos_kv.h`).
pub(crate) const DAOS_COND_KEY_INSERT: u64 = 1 << 1;

/// `daos_obj_query_key` flags (`daos_obj.h`).
pub(crate) const DAOS_GET_MAX: u64 = 1 << 0;
pub(crate) const DAOS_GET_MIN: u64 = 1 << 1;
pub(crate) const DAOS_GET_DKEY: u64 = 1 << 2;
pub(crate) const DAOS_GET_AKEY: u64 = 1 << 3;
pub(crate) const DAOS_GET_RECX: u64 = 1 << 4;

/// `daos_oclass_hints_t` (`daos_obj_class.h`).
pub(crate) type daos_oclass_hints_t = u16;
/// `enum daos_otype_t` (`daos_obj.h`), 4 bytes.
pub(crate) type daos_otype_t = u32;
/// `MAX_OBJ_CLASS_NAME_LEN` (`daos_obj_class.h`).
pub(crate) const MAX_OBJ_CLASS_NAME_LEN: usize = 24;
/// `OR_RS_2P1` (`daos_obj_class.h`): first erasure-code redundancy value.
pub(crate) const OR_RS_2P1: u32 = 32;
/// `MAX_NUM_GROUPS` (`daos_obj_class.h`): the "X" group count.
pub(crate) const MAX_NUM_GROUPS: u32 = 65535;

/// `daos_range_t` (`daos_types.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct daos_range_t {
    pub rg_idx: daos_off_t,
    pub rg_len: daos_size_t,
}

/// `daos_array_iod_t` (`daos_array.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct daos_array_iod_t {
    pub arr_nr: daos_size_t,
    pub arr_rgs: *mut daos_range_t,
    pub arr_nr_short_read: daos_size_t,
    pub arr_nr_read: daos_size_t,
}

/// `daos_array_stbuf_t` (`daos_array.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct daos_array_stbuf_t {
    pub st_size: daos_size_t,
    pub st_max_epoch: daos_epoch_t,
}

/// `DAOS_ARRAY_LIST_IO_LIMIT` (`daos_array.h` from 2.6.4; applied on every
/// version by the crate).
pub(crate) const DAOS_ARRAY_LIST_IO_LIMIT: usize = 16384;

/// `DAOS_TF_RDONLY` (`daos_api.h`).
pub(crate) const DAOS_TF_RDONLY: u64 = 1 << 0;
/// `DAOS_TF_ZERO_COPY` (`daos_api.h`). Never passed by this crate: borrowed
/// buffers are sound only because libdaos copies them during the call.
pub(crate) const DAOS_TF_ZERO_COPY: u64 = 1 << 1;
/// `DAOS_TX_NONE` (`daos_types.h`).
pub(crate) const DAOS_TX_NONE: daos_handle_t = daos_handle_t { cookie: 0 };

/// `OC_UNKNOWN` (`daos_obj_class.h`): inherit the object class.
pub(crate) const OC_UNKNOWN: daos_oclass_id_t = 0;
/// `DFS_MAX_NAME` (`daos_fs.h`).
pub(crate) const DFS_MAX_NAME: usize = 255;
/// `DFS_MAX_PATH` (`daos_fs.h`).
pub(crate) const DFS_MAX_PATH: usize = 4096;

/// `dfs_filler_cb_t` (`daos_fs.h`).
pub(crate) type dfs_filler_cb_t = Option<
    unsafe extern "C" fn(
        dfs: *mut dfs_t,
        obj: *mut dfs_obj_t,
        name: *const c_char,
        arg: *mut c_void,
    ) -> c_int,
>;

/// `struct stat` of the platform. On 64-bit Linux this is `libc::stat`, the
/// layout libdfs fills. Elsewhere a stand-in with the fields this crate reads,
/// so that the crate (and the in-memory backend) compiles.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub(crate) type stat_t = libc::stat;

/// Stand-in for `struct stat` on platforms without DAOS.
#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct stat_t {
    pub st_mode: u32,
    pub st_size: i64,
    pub st_uid: u32,
    pub st_gid: u32,
    pub st_atime: i64,
    pub st_atime_nsec: i64,
    pub st_mtime: i64,
    pub st_mtime_nsec: i64,
    pub st_ctime: i64,
    pub st_ctime_nsec: i64,
}

/// A zeroed `stat_t`.
pub(crate) fn zeroed_stat() -> stat_t {
    // SAFETY: stat_t is a plain C struct of integers; all-zero is a valid value.
    unsafe { std::mem::zeroed() }
}

// Host constants used as DFS flags and modes (`daos_fs.h` defines
// DFS_RDONLY/DFS_RDWR as the fcntl.h values). Converted once here.
#[allow(clippy::unnecessary_cast)]
pub(crate) const S_IFMT: u32 = libc::S_IFMT as u32;
#[allow(clippy::unnecessary_cast)]
pub(crate) const S_IFREG: u32 = libc::S_IFREG as u32;
#[allow(clippy::unnecessary_cast)]
pub(crate) const S_IFDIR: u32 = libc::S_IFDIR as u32;
#[allow(clippy::unnecessary_cast)]
pub(crate) const S_IFLNK: u32 = libc::S_IFLNK as u32;

// Compile-time layout checks (LP64). The unit test below compares the same
// facts with the output of the ABI probe built against real DAOS headers.
#[cfg(target_pointer_width = "64")]
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<d_iov_t>() == 24);
    assert!(offset_of!(d_iov_t, iov_buf_len) == 8);
    assert!(offset_of!(d_iov_t, iov_len) == 16);
    assert!(size_of::<d_sg_list_t>() == 16);
    assert!(offset_of!(d_sg_list_t, sg_nr_out) == 4);
    assert!(offset_of!(d_sg_list_t, sg_iovs) == 8);
    assert!(size_of::<daos_anchor_t>() == 120);
    assert!(offset_of!(daos_anchor_t, da_sub_anchors) == 8);
    assert!(offset_of!(daos_anchor_t, da_buf) == 16);
    assert!(size_of::<daos_obj_id_t>() == 16);
    assert!(size_of::<daos_handle_t>() == 8);
    assert!(size_of::<daos_epoch_range_t>() == 16);
    assert!(size_of::<daos_space>() == 32);
    assert!(size_of::<daos_pool_space>() == 88);
    assert!(size_of::<daos_rebuild_status>() == 56);
    assert!(offset_of!(daos_rebuild_status, rs_fail_rank) == 20);
    assert!(size_of::<daos_pool_info_t>() == 192);
    assert!(offset_of!(daos_pool_info_t, pi_bits) == 40);
    assert!(offset_of!(daos_pool_info_t, pi_space) == 48);
    assert!(offset_of!(daos_pool_info_t, pi_rebuild_st) == 136);
    assert!(size_of::<daos_target_info_t>() == 48);
    assert!(offset_of!(daos_target_info_t, ta_state) == 4);
    assert!(offset_of!(daos_target_info_t, ta_space) == 16);
    assert!(size_of::<daos_pool_cont_info>() == 144);
    assert!(size_of::<daos_cont_info_t>() == 48);
    assert!(size_of::<daos_prop_entry>() == 16);
    assert!(size_of::<daos_prop_t>() == 16);
    assert!(size_of::<dfs_attr_t>() == 176);
    assert!(size_of::<daos_recx_t>() == 16);
    assert!(size_of::<daos_iod_t>() == 64);
    assert!(offset_of!(daos_iod_t, iod_type) == 24);
    assert!(offset_of!(daos_iod_t, iod_size) == 32);
    assert!(offset_of!(daos_iod_t, iod_nr) == 48);
    assert!(offset_of!(daos_iod_t, iod_recxs) == 56);
    assert!(size_of::<daos_key_desc_t>() == 16);
    assert!(size_of::<daos_range_t>() == 16);
    assert!(size_of::<daos_array_iod_t>() == 32);
    assert!(size_of::<daos_array_stbuf_t>() == 16);
    assert!(offset_of!(dfs_attr_t, da_props) == 24);
    assert!(offset_of!(dfs_attr_t, da_hints) == 44);
    assert!(size_of::<mode_t>() == 4);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::der_table::DER_TABLE;
    use std::collections::HashMap;
    use std::mem::{offset_of, size_of};

    /// The lines the ABI probe prints, produced from the crate's declarations.
    fn crate_lines() -> Vec<String> {
        let mut v = Vec::new();
        macro_rules! sz {
            ($t:ty, $n:expr) => {
                v.push(format!("sizeof {} {}", $n, size_of::<$t>()))
            };
        }
        macro_rules! off {
            ($t:ty, $n:expr, $f:ident) => {
                v.push(format!(
                    "offsetof {} {} {}",
                    $n,
                    stringify!($f),
                    offset_of!($t, $f)
                ))
            };
        }
        macro_rules! width {
            ($t:ty, $n:expr) => {
                v.push(format!(
                    "width {} {} {}",
                    $n,
                    size_of::<$t>() * 8,
                    u8::from(<$t>::MIN != 0)
                ))
            };
        }
        sz!(d_iov_t, "d_iov_t");
        off!(d_iov_t, "d_iov_t", iov_buf);
        off!(d_iov_t, "d_iov_t", iov_buf_len);
        off!(d_iov_t, "d_iov_t", iov_len);
        sz!(d_sg_list_t, "d_sg_list_t");
        off!(d_sg_list_t, "d_sg_list_t", sg_nr);
        off!(d_sg_list_t, "d_sg_list_t", sg_nr_out);
        off!(d_sg_list_t, "d_sg_list_t", sg_iovs);
        sz!(daos_anchor_t, "daos_anchor_t");
        off!(daos_anchor_t, "daos_anchor_t", da_type);
        off!(daos_anchor_t, "daos_anchor_t", da_shard);
        off!(daos_anchor_t, "daos_anchor_t", da_flags);
        off!(daos_anchor_t, "daos_anchor_t", da_sub_anchors);
        off!(daos_anchor_t, "daos_anchor_t", da_buf);
        sz!(daos_obj_id_t, "daos_obj_id_t");
        off!(daos_obj_id_t, "daos_obj_id_t", lo);
        off!(daos_obj_id_t, "daos_obj_id_t", hi);
        width!(daos_size_t, "daos_size_t");
        width!(daos_off_t, "daos_off_t");
        width!(daos_oclass_id_t, "daos_oclass_id_t");
        width!(mode_t, "mode_t");
        v.push(format!("const DAOS_ANCHOR_TYPE_EOF {DAOS_ANCHOR_TYPE_EOF}"));
        v.push(format!("const DAOS_ANCHOR_BUF_MAX {DAOS_ANCHOR_BUF_MAX}"));
        v.push(format!("const DFS_MAX_NAME {DFS_MAX_NAME}"));
        v.push(format!("const DFS_MAX_PATH {DFS_MAX_PATH}"));
        v.push(format!("const OC_UNKNOWN {OC_UNKNOWN}"));
        // Pool, container, object, array and transaction declarations.
        sz!(daos_handle_t, "daos_handle_t");
        off!(daos_handle_t, "daos_handle_t", cookie);
        sz!(daos_epoch_range_t, "daos_epoch_range_t");
        off!(daos_epoch_range_t, "daos_epoch_range_t", epr_lo);
        off!(daos_epoch_range_t, "daos_epoch_range_t", epr_hi);
        sz!(daos_space, "daos_space");
        off!(daos_space, "daos_space", s_total);
        off!(daos_space, "daos_space", s_free);
        sz!(daos_pool_space, "daos_pool_space");
        off!(daos_pool_space, "daos_pool_space", ps_space);
        off!(daos_pool_space, "daos_pool_space", ps_free_min);
        off!(daos_pool_space, "daos_pool_space", ps_free_max);
        off!(daos_pool_space, "daos_pool_space", ps_free_mean);
        off!(daos_pool_space, "daos_pool_space", ps_ntargets);
        sz!(daos_rebuild_status, "daos_rebuild_status");
        off!(daos_rebuild_status, "daos_rebuild_status", rs_version);
        off!(daos_rebuild_status, "daos_rebuild_status", rs_seconds);
        off!(daos_rebuild_status, "daos_rebuild_status", rs_errno);
        off!(daos_rebuild_status, "daos_rebuild_status", rs_state);
        off!(daos_rebuild_status, "daos_rebuild_status", rs_fail_rank);
        off!(daos_rebuild_status, "daos_rebuild_status", rs_toberb_obj_nr);
        off!(daos_rebuild_status, "daos_rebuild_status", rs_obj_nr);
        off!(daos_rebuild_status, "daos_rebuild_status", rs_rec_nr);
        off!(daos_rebuild_status, "daos_rebuild_status", rs_size);
        sz!(daos_pool_info_t, "daos_pool_info_t");
        off!(daos_pool_info_t, "daos_pool_info_t", pi_uuid);
        off!(daos_pool_info_t, "daos_pool_info_t", pi_ntargets);
        off!(daos_pool_info_t, "daos_pool_info_t", pi_nnodes);
        off!(daos_pool_info_t, "daos_pool_info_t", pi_ndisabled);
        off!(daos_pool_info_t, "daos_pool_info_t", pi_map_ver);
        off!(daos_pool_info_t, "daos_pool_info_t", pi_leader);
        off!(daos_pool_info_t, "daos_pool_info_t", pi_bits);
        off!(daos_pool_info_t, "daos_pool_info_t", pi_space);
        off!(daos_pool_info_t, "daos_pool_info_t", pi_rebuild_st);
        sz!(daos_target_info_t, "daos_target_info_t");
        off!(daos_target_info_t, "daos_target_info_t", ta_state);
        off!(daos_target_info_t, "daos_target_info_t", ta_perf);
        off!(daos_target_info_t, "daos_target_info_t", ta_space);
        sz!(daos_pool_cont_info, "daos_pool_cont_info");
        off!(daos_pool_cont_info, "daos_pool_cont_info", pci_uuid);
        off!(daos_pool_cont_info, "daos_pool_cont_info", pci_label);
        sz!(daos_cont_info_t, "daos_cont_info_t");
        off!(daos_cont_info_t, "daos_cont_info_t", ci_uuid);
        off!(daos_cont_info_t, "daos_cont_info_t", ci_lsnapshot);
        off!(daos_cont_info_t, "daos_cont_info_t", ci_nhandles);
        off!(daos_cont_info_t, "daos_cont_info_t", ci_nsnapshots);
        off!(daos_cont_info_t, "daos_cont_info_t", ci_md_otime);
        off!(daos_cont_info_t, "daos_cont_info_t", ci_md_mtime);
        sz!(daos_prop_entry, "daos_prop_entry");
        off!(daos_prop_entry, "daos_prop_entry", dpe_type);
        off!(daos_prop_entry, "daos_prop_entry", dpe_flags);
        off!(daos_prop_entry, "daos_prop_entry", dpe_reserv);
        off!(daos_prop_entry, "daos_prop_entry", dpe_val);
        sz!(daos_prop_t, "daos_prop_t");
        off!(daos_prop_t, "daos_prop_t", dpp_nr);
        off!(daos_prop_t, "daos_prop_t", dpp_reserv);
        off!(daos_prop_t, "daos_prop_t", dpp_entries);
        sz!(dfs_attr_t, "dfs_attr_t");
        off!(dfs_attr_t, "dfs_attr_t", da_id);
        off!(dfs_attr_t, "dfs_attr_t", da_chunk_size);
        off!(dfs_attr_t, "dfs_attr_t", da_oclass_id);
        off!(dfs_attr_t, "dfs_attr_t", da_props);
        off!(dfs_attr_t, "dfs_attr_t", da_mode);
        off!(dfs_attr_t, "dfs_attr_t", da_dir_oclass_id);
        off!(dfs_attr_t, "dfs_attr_t", da_file_oclass_id);
        off!(dfs_attr_t, "dfs_attr_t", da_hints);
        sz!(daos_recx_t, "daos_recx_t");
        off!(daos_recx_t, "daos_recx_t", rx_idx);
        off!(daos_recx_t, "daos_recx_t", rx_nr);
        sz!(daos_iod_t, "daos_iod_t");
        off!(daos_iod_t, "daos_iod_t", iod_name);
        off!(daos_iod_t, "daos_iod_t", iod_type);
        off!(daos_iod_t, "daos_iod_t", iod_size);
        off!(daos_iod_t, "daos_iod_t", iod_flags);
        off!(daos_iod_t, "daos_iod_t", iod_nr);
        off!(daos_iod_t, "daos_iod_t", iod_recxs);
        sz!(daos_key_desc_t, "daos_key_desc_t");
        off!(daos_key_desc_t, "daos_key_desc_t", kd_key_len);
        off!(daos_key_desc_t, "daos_key_desc_t", kd_val_type);
        sz!(daos_range_t, "daos_range_t");
        off!(daos_range_t, "daos_range_t", rg_idx);
        off!(daos_range_t, "daos_range_t", rg_len);
        sz!(daos_array_iod_t, "daos_array_iod_t");
        off!(daos_array_iod_t, "daos_array_iod_t", arr_nr);
        off!(daos_array_iod_t, "daos_array_iod_t", arr_rgs);
        off!(daos_array_iod_t, "daos_array_iod_t", arr_nr_short_read);
        off!(daos_array_iod_t, "daos_array_iod_t", arr_nr_read);
        sz!(daos_array_stbuf_t, "daos_array_stbuf_t");
        off!(daos_array_stbuf_t, "daos_array_stbuf_t", st_size);
        off!(daos_array_stbuf_t, "daos_array_stbuf_t", st_max_epoch);
        width!(daos_epoch_t, "daos_epoch_t");
        width!(d_rank_t, "d_rank_t");
        width!(daos_iod_type_t, "daos_iod_type_t");
        width!(daos_oclass_hints_t, "daos_oclass_hints_t");
        width!(daos_otype_t, "daos_otype_t");
        v.push(format!("const DAOS_PC_RO {}", DAOS_PC_RO));
        v.push(format!("const DAOS_PC_RW {}", DAOS_PC_RW));
        v.push(format!("const DAOS_COO_RO {}", DAOS_COO_RO));
        v.push(format!("const DAOS_COO_RW {}", DAOS_COO_RW));
        v.push(format!("const DAOS_COO_FORCE {}", DAOS_COO_FORCE));
        v.push(format!("const DPI_SPACE {}", DPI_SPACE));
        v.push(format!("const DPI_REBUILD_STATUS {}", DPI_REBUILD_STATUS));
        v.push(format!(
            "const DAOS_PROP_LABEL_MAX_LEN {}",
            DAOS_PROP_LABEL_MAX_LEN
        ));
        v.push(format!(
            "const DAOS_PROP_ENTRIES_MAX_NR {}",
            DAOS_PROP_ENTRIES_MAX_NR
        ));
        v.push(format!(
            "const DAOS_CONT_HINT_MAX_LEN {}",
            DAOS_CONT_HINT_MAX_LEN
        ));
        v.push(format!("const DAOS_PROP_CO_LABEL {}", DAOS_PROP_CO_LABEL));
        v.push(format!(
            "const DAOS_PROP_CO_LAYOUT_TYPE {}",
            DAOS_PROP_CO_LAYOUT_TYPE
        ));
        v.push(format!(
            "const DAOS_PROP_CO_LAYOUT_VER {}",
            DAOS_PROP_CO_LAYOUT_VER
        ));
        v.push(format!("const DAOS_PROP_CO_CSUM {}", DAOS_PROP_CO_CSUM));
        v.push(format!(
            "const DAOS_PROP_CO_REDUN_FAC {}",
            DAOS_PROP_CO_REDUN_FAC
        ));
        v.push(format!(
            "const DAOS_PROP_CO_REDUN_LVL {}",
            DAOS_PROP_CO_REDUN_LVL
        ));
        v.push(format!(
            "const DAOS_PROP_CO_SNAPSHOT_MAX {}",
            DAOS_PROP_CO_SNAPSHOT_MAX
        ));
        v.push(format!("const DAOS_PROP_CO_OWNER {}", DAOS_PROP_CO_OWNER));
        v.push(format!(
            "const DAOS_PROP_CO_OWNER_GROUP {}",
            DAOS_PROP_CO_OWNER_GROUP
        ));
        v.push(format!("const DAOS_PROP_CO_STATUS {}", DAOS_PROP_CO_STATUS));
        v.push(format!(
            "const DAOS_PROP_CO_ALLOCED_OID {}",
            DAOS_PROP_CO_ALLOCED_OID
        ));
        v.push(format!(
            "const DAOS_PROP_CO_EC_CELL_SZ {}",
            DAOS_PROP_CO_EC_CELL_SZ
        ));
        v.push(format!(
            "const DAOS_PROP_ENTRY_NOT_SET {}",
            DAOS_PROP_ENTRY_NOT_SET
        ));
        v.push(format!(
            "const DAOS_PROP_CO_LAYOUT_UNKNOWN {}",
            DAOS_PROP_CO_LAYOUT_UNKNOWN
        ));
        v.push(format!(
            "const DAOS_PROP_CO_LAYOUT_POSIX {}",
            DAOS_PROP_CO_LAYOUT_POSIX
        ));
        v.push(format!(
            "const DAOS_PROP_CO_LAYOUT_HDF5 {}",
            DAOS_PROP_CO_LAYOUT_HDF5
        ));
        v.push(format!(
            "const DAOS_PROP_CO_LAYOUT_PYTHON {}",
            DAOS_PROP_CO_LAYOUT_PYTHON
        ));
        v.push(format!(
            "const DAOS_PROP_CO_REDUN_RANK {}",
            DAOS_PROP_CO_REDUN_RANK
        ));
        v.push(format!(
            "const DAOS_PROP_CO_REDUN_NODE {}",
            DAOS_PROP_CO_REDUN_NODE
        ));
        v.push(format!("const DAOS_IOD_SINGLE {}", DAOS_IOD_SINGLE));
        v.push(format!("const DAOS_IOD_ARRAY {}", DAOS_IOD_ARRAY));
        v.push(format!("const DAOS_REC_ANY {}", DAOS_REC_ANY));
        v.push(format!("const DAOS_OO_RO {}", DAOS_OO_RO));
        v.push(format!("const DAOS_OO_RW {}", DAOS_OO_RW));
        v.push(format!("const DAOS_OO_EXCL {}", DAOS_OO_EXCL));
        v.push(format!("const DAOS_COND_PUNCH {}", DAOS_COND_PUNCH));
        v.push(format!(
            "const DAOS_COND_DKEY_INSERT {}",
            DAOS_COND_DKEY_INSERT
        ));
        v.push(format!(
            "const DAOS_COND_DKEY_UPDATE {}",
            DAOS_COND_DKEY_UPDATE
        ));
        v.push(format!(
            "const DAOS_COND_DKEY_FETCH {}",
            DAOS_COND_DKEY_FETCH
        ));
        v.push(format!(
            "const DAOS_COND_AKEY_INSERT {}",
            DAOS_COND_AKEY_INSERT
        ));
        v.push(format!(
            "const DAOS_COND_AKEY_UPDATE {}",
            DAOS_COND_AKEY_UPDATE
        ));
        v.push(format!(
            "const DAOS_COND_AKEY_FETCH {}",
            DAOS_COND_AKEY_FETCH
        ));
        v.push(format!(
            "const DAOS_COND_KEY_INSERT {}",
            DAOS_COND_KEY_INSERT
        ));
        v.push(format!("const DAOS_GET_MAX {}", DAOS_GET_MAX));
        v.push(format!("const DAOS_GET_MIN {}", DAOS_GET_MIN));
        v.push(format!("const DAOS_GET_DKEY {}", DAOS_GET_DKEY));
        v.push(format!("const DAOS_GET_AKEY {}", DAOS_GET_AKEY));
        v.push(format!("const DAOS_GET_RECX {}", DAOS_GET_RECX));
        v.push(format!(
            "const MAX_OBJ_CLASS_NAME_LEN {}",
            MAX_OBJ_CLASS_NAME_LEN
        ));
        v.push(format!("const OR_RS_2P1 {}", OR_RS_2P1));
        v.push(format!("const MAX_NUM_GROUPS {}", MAX_NUM_GROUPS));
        v.push(format!("const DAOS_TF_RDONLY {}", DAOS_TF_RDONLY));
        v.push(format!("const DAOS_TF_ZERO_COPY {}", DAOS_TF_ZERO_COPY));
        // Object types, compiled-in class ids, class hints and object id
        // vectors of crate::obj.
        v.extend(crate::obj::probe_lines());
        v
    }

    const PROBE_FILES: &[(&str, &str)] = &[
        ("v2.6.0", include_str!("../../abi/layout-v2.6.0.txt")),
        ("v2.6.4", include_str!("../../abi/layout-v2.6.4.txt")),
        ("v2.8.1", include_str!("../../abi/layout-v2.8.1.txt")),
        (
            "installed-2.8.1-4",
            include_str!("../../abi/layout-installed-2.8.1-4.txt"),
        ),
    ];

    #[test]
    fn abi_layout_matches_probe_files() {
        let ours = crate_lines();
        let crate_der: HashMap<&str, i32> = DER_TABLE.iter().map(|e| (e.1, e.0)).collect();
        for (label, text) in PROBE_FILES {
            let probe: Vec<&str> = text.lines().filter(|l| !l.starts_with("der ")).collect();
            assert_eq!(
                probe,
                ours.iter().map(String::as_str).collect::<Vec<_>>(),
                "layout differs from abi/layout-{label}.txt"
            );
            // DER codes: the crate table is a superset with equal values.
            for line in text.lines().filter(|l| l.starts_with("der ")) {
                let mut it = line.split_whitespace().skip(1);
                let name = it.next().unwrap();
                let value: i32 = it.next().unwrap().parse().unwrap();
                assert_eq!(crate_der.get(name), Some(&value), "{label}: {name}");
            }
        }
    }

    #[test]
    fn anchor_eof() {
        let mut a = daos_anchor_t::new();
        assert!(!a.is_eof());
        a.da_type = DAOS_ANCHOR_TYPE_EOF;
        assert!(a.is_eof());
    }

    #[test]
    fn path_limits_agree_with_dfs() {
        assert_eq!(crate::path::MAX_NAME, DFS_MAX_NAME);
        assert_eq!(crate::path::MAX_PATH, DFS_MAX_PATH);
    }
}
