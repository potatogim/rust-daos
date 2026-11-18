// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions transcribed from DAOS headers, BSD-2-Clause-Patent, see LICENSE-THIRD-PARTY

//! The backend abstraction: one method per C function the crate calls.
//!
//! The list of functions is written once, in the `raw_api!` invocation at
//! the end of this file. The macro generates:
//!
//! - the `Raw` trait (one `unsafe fn` per C function, with a default body
//!   that panics, so test fakes implement only what they use),
//! - the function pointer table of the dlopen backend (`RawFns`) and its
//!   loader, which resolves each symbol through the libdfs handle, trying the
//!   exported `*2` name first and the plain name second where the headers
//!   rename a function with a macro (`daos_pool_connect` is
//!   `#define daos_pool_connect daos_pool_connect2`).
//!
//! Every prototype is identical in DAOS v2.6.0, v2.6.4 and v2.8.1 (see
//! `abi/prototypes.txt`).
//!
//! Symbols are grouped. `Group::Core` and `Group::Dfs` must resolve when
//! the library is loaded; a missing symbol of another group is reported as
//! `Error::MissingSymbol` by the first call
//! that needs the group.
//!
//! `daos_fini`, `dfs_fini` and `daos_reinit` are deliberately absent from the
//! callable list: the crate never finalises DAOS (a finalised and
//! re-initialised libdaos leaves stale thread-local events in other threads)
//! and never reuses DAOS state in a fork child.

use std::ffi::{c_char, c_int, c_uint, c_void};

use super::{
    d_rank_t, d_sg_list_t, daos_anchor_t, daos_array_iod_t, daos_array_stbuf_t, daos_cont_info_t,
    daos_epoch_range_t, daos_epoch_t, daos_handle_t, daos_iod_t, daos_key_desc_t, daos_key_t,
    daos_obj_id_t, daos_oclass_hints_t, daos_otype_t, daos_pool_cont_info, daos_pool_info_t,
    daos_prop_t, daos_recx_t, daos_target_info_t, dfs_attr_t, dfs_filler_cb_t, dfs_obj_t, dfs_t,
    mode_t, stat_t, uuid_t,
};

/// Symbol groups of the dlopen backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Group {
    /// `daos_init` and the pool and container connect calls under DFS.
    Core = 0,
    /// The `dfs_*` calls of the file system layer.
    Dfs = 1,
    /// Pool query, container create, destroy, query, snapshots.
    PoolCont = 2,
    /// Objects: open, update, fetch, list, punch, query, OID generation.
    Object = 3,
    /// Key-value objects (`daos_kv_*`).
    Kv = 4,
    /// Arrays (`daos_array_*`).
    Array = 5,
    /// Transactions (`daos_tx_*`).
    Tx = 6,
}

/// Number of `Group` values.
pub(crate) const GROUPS: usize = 7;

impl Group {
    /// Human-readable name for errors.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Group::Core => "core",
            Group::Dfs => "dfs",
            Group::PoolCont => "pool and container",
            Group::Object => "object",
            Group::Kv => "key-value",
            Group::Array => "array",
            Group::Tx => "transaction",
        }
    }
}

/// Generates the `Raw` trait, the function table and its loader.
///
/// Input: groups of `name [/ fallback] (args) -> ret;` entries.
macro_rules! raw_api {
    ($( $group:ident { $( $name:ident $( / $alt:ident )? ( $( $arg:ident : $ty:ty ),* $(,)? ) -> $ret:ty ; )* } )*) => {
        /// One method per C function. Implemented by the dlopen symbol table
        /// and by the in-memory stand-in.
        ///
        /// # Safety
        ///
        /// Implementations must behave like libdaos and libdfs for the pointer
        /// arguments: read and write only through the pointers given, within
        /// the sizes the C API defines, and be callable from several threads
        /// at once.
        pub(crate) unsafe trait Raw: Send + Sync {
            /// The error to report when a symbol of `group` is missing, or
            /// `None` when the group can be used.
            fn missing(&self, group: Group) -> Option<crate::error::Error> {
                let _ = group;
                None
            }

            /// The connection registry of this backend.
            fn registry(&self) -> &crate::conn::Registry {
                panic!("this backend has no connection registry")
            }

            $( $(
                #[allow(clippy::too_many_arguments)]
                unsafe fn $name(&self, $( $arg: $ty ),*) -> $ret {
                    let _ = ($( $arg, )*);
                    panic!(concat!("this backend does not implement ", stringify!($name)))
                }
            )* )*
        }

        /// Function pointers resolved from the loaded library.
        #[allow(non_snake_case)]
        pub(crate) struct RawFns {
            $( $(
                pub(crate) $name: Option<unsafe extern "C" fn($( $ty ),*) -> $ret>,
            )* )*
            /// Symbols found only under their plain (header) name.
            pub(crate) plain_names: Vec<&'static str>,
            /// First missing symbol of each group.
            pub(crate) missing: [Option<&'static str>; GROUPS],
        }

        impl RawFns {
            /// Resolve every symbol with `lookup`, which returns the address
            /// of a symbol or NULL.
            pub(crate) fn resolve(lookup: impl Fn(&str) -> *mut c_void) -> RawFns {
                let mut plain_names = Vec::new();
                let mut missing: [Option<&'static str>; GROUPS] = [None; GROUPS];
                $( $(
                    let $name = {
                        #[allow(unused_mut)]
                        let mut p = lookup(stringify!($name));
                        $(
                            if p.is_null() {
                                p = lookup(stringify!($alt));
                                if !p.is_null() {
                                    plain_names.push(stringify!($alt));
                                }
                            }
                        )?
                        if p.is_null() {
                            let slot = &mut missing[Group::$group as usize];
                            if slot.is_none() {
                                *slot = Some(stringify!($name));
                            }
                            None
                        } else {
                            // SAFETY: the type is the transcription of the
                            // prototype in abi/prototypes.txt; a function
                            // pointer has the size of a data pointer here.
                            Some(unsafe {
                                std::mem::transmute::<
                                    *mut c_void,
                                    unsafe extern "C" fn($( $ty ),*) -> $ret,
                                >(p)
                            })
                        }
                    };
                )* )*
                RawFns {
                    $( $( $name, )* )*
                    plain_names,
                    missing,
                }
            }
        }

        // Forward every call of the trait to the resolved pointer. A call of a
        // missing symbol is a crate bug (the group is checked before any
        // call) and panics.
        //
        // SAFETY: the function pointers are plain code addresses of a library
        // that is never unloaded; libdfs and libdaos are thread safe for the
        // calls made by this crate (dfs_t lock, module mutexes, per-thread
        // private events; see the crate docs, "Threads").
        #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
        unsafe impl Raw for super::lib_dfs::LibDfs {
            fn missing(&self, group: Group) -> Option<crate::error::Error> {
                self.missing_error(group)
            }
            fn registry(&self) -> &crate::conn::Registry {
                &self.registry
            }
            $( $(
                #[allow(clippy::too_many_arguments)]
                unsafe fn $name(&self, $( $arg: $ty ),*) -> $ret {
                    let f = self.fns.$name.expect(concat!(
                        "symbol ",
                        stringify!($name),
                        " used without checking its group"
                    ));
                    // SAFETY: the caller upholds the requirements of the C
                    // prototype; f has that prototype.
                    unsafe { f($( $arg ),*) }
                }
            )* )*
        }
    };
}

raw_api! {
    Core {
        daos_init() -> c_int;
        daos_pool_connect2 / daos_pool_connect (
            pool: *const c_char,
            sys: *const c_char,
            flags: c_uint,
            poh: *mut daos_handle_t,
            info: *mut c_void,
            ev: *mut c_void,
        ) -> c_int;
        daos_pool_disconnect(poh: daos_handle_t, ev: *mut c_void) -> c_int;
        daos_cont_open2 / daos_cont_open (
            poh: daos_handle_t,
            cont: *const c_char,
            flags: c_uint,
            coh: *mut daos_handle_t,
            info: *mut c_void,
            ev: *mut c_void,
        ) -> c_int;
        daos_cont_close(coh: daos_handle_t, ev: *mut c_void) -> c_int;
    }
    Dfs {
        dfs_init() -> c_int;
        dfs_mount(poh: daos_handle_t, coh: daos_handle_t, flags: c_int, dfs: *mut *mut dfs_t) -> c_int;
        dfs_umount(dfs: *mut dfs_t) -> c_int;
        dfs_lookup(
            dfs: *mut dfs_t,
            path: *const c_char,
            flags: c_int,
            obj: *mut *mut dfs_obj_t,
            mode: *mut mode_t,
            stbuf: *mut stat_t,
        ) -> c_int;
        dfs_open(
            dfs: *mut dfs_t,
            parent: *mut dfs_obj_t,
            name: *const c_char,
            mode: mode_t,
            flags: c_int,
            cid: u32,
            chunk_size: u64,
            value: *const c_char,
            obj: *mut *mut dfs_obj_t,
        ) -> c_int;
        dfs_release(obj: *mut dfs_obj_t) -> c_int;
        dfs_read(
            dfs: *mut dfs_t,
            obj: *mut dfs_obj_t,
            sgl: *mut d_sg_list_t,
            off: u64,
            read_size: *mut u64,
            ev: *mut c_void,
        ) -> c_int;
        dfs_write(
            dfs: *mut dfs_t,
            obj: *mut dfs_obj_t,
            sgl: *mut d_sg_list_t,
            off: u64,
            ev: *mut c_void,
        ) -> c_int;
        dfs_get_size(dfs: *mut dfs_t, obj: *mut dfs_obj_t, size: *mut u64) -> c_int;
        dfs_iterate(
            dfs: *mut dfs_t,
            obj: *mut dfs_obj_t,
            anchor: *mut daos_anchor_t,
            nr: *mut u32,
            size: usize,
            op: dfs_filler_cb_t,
            arg: *mut c_void,
        ) -> c_int;
        dfs_mkdir(
            dfs: *mut dfs_t,
            parent: *mut dfs_obj_t,
            name: *const c_char,
            mode: mode_t,
            cid: u32,
        ) -> c_int;
        dfs_remove(
            dfs: *mut dfs_t,
            parent: *mut dfs_obj_t,
            name: *const c_char,
            force: bool,
            oid: *mut daos_obj_id_t,
        ) -> c_int;
        dfs_stat(
            dfs: *mut dfs_t,
            parent: *mut dfs_obj_t,
            name: *const c_char,
            stbuf: *mut stat_t,
        ) -> c_int;
        dfs_ostat(dfs: *mut dfs_t, obj: *mut dfs_obj_t, stbuf: *mut stat_t) -> c_int;
    }
    PoolCont {
        daos_pool_query(
            poh: daos_handle_t,
            ranks: *mut c_void,
            info: *mut daos_pool_info_t,
            pool_prop: *mut daos_prop_t,
            ev: *mut c_void,
        ) -> c_int;
        daos_pool_query_target(
            poh: daos_handle_t,
            tgt: u32,
            rank: d_rank_t,
            info: *mut daos_target_info_t,
            ev: *mut c_void,
        ) -> c_int;
        daos_pool_list_cont(
            poh: daos_handle_t,
            ncont: *mut u64,
            cbuf: *mut daos_pool_cont_info,
            ev: *mut c_void,
        ) -> c_int;
        daos_cont_create2 / daos_cont_create (
            poh: daos_handle_t,
            uuid: *mut uuid_t,
            cont_prop: *mut daos_prop_t,
            ev: *mut c_void,
        ) -> c_int;
        daos_cont_create_with_label(
            poh: daos_handle_t,
            label: *const c_char,
            cont_prop: *mut daos_prop_t,
            uuid: *mut uuid_t,
            ev: *mut c_void,
        ) -> c_int;
        daos_cont_destroy2 / daos_cont_destroy (
            poh: daos_handle_t,
            cont: *const c_char,
            force: c_int,
            ev: *mut c_void,
        ) -> c_int;
        daos_cont_query(
            coh: daos_handle_t,
            info: *mut daos_cont_info_t,
            cont_prop: *mut daos_prop_t,
            ev: *mut c_void,
        ) -> c_int;
        daos_cont_create_snap(
            coh: daos_handle_t,
            epoch: *mut daos_epoch_t,
            name: *mut c_char,
            ev: *mut c_void,
        ) -> c_int;
        daos_cont_list_snap(
            coh: daos_handle_t,
            nr: *mut c_int,
            epochs: *mut daos_epoch_t,
            names: *mut *mut c_char,
            anchor: *mut daos_anchor_t,
            ev: *mut c_void,
        ) -> c_int;
        daos_cont_destroy_snap(coh: daos_handle_t, epr: daos_epoch_range_t, ev: *mut c_void) -> c_int;
        daos_prop_alloc(entries_nr: u32) -> *mut daos_prop_t;
        daos_prop_free(prop: *mut daos_prop_t) -> ();
        dfs_cont_create(
            poh: daos_handle_t,
            uuid: *mut uuid_t,
            attr: *mut dfs_attr_t,
            coh: *mut daos_handle_t,
            dfs: *mut *mut dfs_t,
        ) -> c_int;
        dfs_cont_create_with_label(
            poh: daos_handle_t,
            label: *const c_char,
            attr: *mut dfs_attr_t,
            uuid: *mut uuid_t,
            coh: *mut daos_handle_t,
            dfs: *mut *mut dfs_t,
        ) -> c_int;
    }
    Object {
        daos_cont_alloc_oids(
            coh: daos_handle_t,
            num_oids: u64,
            oid: *mut u64,
            ev: *mut c_void,
        ) -> c_int;
        daos_obj_generate_oid2 / daos_obj_generate_oid (
            coh: daos_handle_t,
            oid: *mut daos_obj_id_t,
            otype: daos_otype_t,
            cid: u32,
            hints: daos_oclass_hints_t,
            args: u32,
        ) -> c_int;
        daos_oclass_name2id(name: *const c_char) -> c_int;
        daos_oclass_id2name(oc_id: u32, name: *mut c_char) -> c_int;
        daos_obj_open(
            coh: daos_handle_t,
            oid: daos_obj_id_t,
            mode: c_uint,
            oh: *mut daos_handle_t,
            ev: *mut c_void,
        ) -> c_int;
        daos_obj_close(oh: daos_handle_t, ev: *mut c_void) -> c_int;
        daos_obj_punch(oh: daos_handle_t, th: daos_handle_t, flags: u64, ev: *mut c_void) -> c_int;
        daos_obj_punch_dkeys(
            oh: daos_handle_t,
            th: daos_handle_t,
            flags: u64,
            nr: c_uint,
            dkeys: *mut daos_key_t,
            ev: *mut c_void,
        ) -> c_int;
        daos_obj_punch_akeys(
            oh: daos_handle_t,
            th: daos_handle_t,
            flags: u64,
            dkey: *mut daos_key_t,
            nr: c_uint,
            akeys: *mut daos_key_t,
            ev: *mut c_void,
        ) -> c_int;
        daos_obj_fetch(
            oh: daos_handle_t,
            th: daos_handle_t,
            flags: u64,
            dkey: *mut daos_key_t,
            nr: c_uint,
            iods: *mut daos_iod_t,
            sgls: *mut d_sg_list_t,
            ioms: *mut c_void,
            ev: *mut c_void,
        ) -> c_int;
        daos_obj_update(
            oh: daos_handle_t,
            th: daos_handle_t,
            flags: u64,
            dkey: *mut daos_key_t,
            nr: c_uint,
            iods: *mut daos_iod_t,
            sgls: *mut d_sg_list_t,
            ev: *mut c_void,
        ) -> c_int;
        daos_obj_list_dkey(
            oh: daos_handle_t,
            th: daos_handle_t,
            nr: *mut u32,
            kds: *mut daos_key_desc_t,
            sgl: *mut d_sg_list_t,
            anchor: *mut daos_anchor_t,
            ev: *mut c_void,
        ) -> c_int;
        daos_obj_list_akey(
            oh: daos_handle_t,
            th: daos_handle_t,
            dkey: *mut daos_key_t,
            nr: *mut u32,
            kds: *mut daos_key_desc_t,
            sgl: *mut d_sg_list_t,
            anchor: *mut daos_anchor_t,
            ev: *mut c_void,
        ) -> c_int;
        daos_obj_query_key(
            oh: daos_handle_t,
            th: daos_handle_t,
            flags: u64,
            dkey: *mut daos_key_t,
            akey: *mut daos_key_t,
            recx: *mut daos_recx_t,
            ev: *mut c_void,
        ) -> c_int;
    }
    Kv {
        daos_kv_open(
            coh: daos_handle_t,
            oid: daos_obj_id_t,
            mode: c_uint,
            oh: *mut daos_handle_t,
            ev: *mut c_void,
        ) -> c_int;
        daos_kv_close(oh: daos_handle_t, ev: *mut c_void) -> c_int;
        daos_kv_destroy(oh: daos_handle_t, th: daos_handle_t, ev: *mut c_void) -> c_int;
        daos_kv_put(
            oh: daos_handle_t,
            th: daos_handle_t,
            flags: u64,
            key: *const c_char,
            size: u64,
            buf: *const c_void,
            ev: *mut c_void,
        ) -> c_int;
        daos_kv_get(
            oh: daos_handle_t,
            th: daos_handle_t,
            flags: u64,
            key: *const c_char,
            size: *mut u64,
            buf: *mut c_void,
            ev: *mut c_void,
        ) -> c_int;
        daos_kv_remove(
            oh: daos_handle_t,
            th: daos_handle_t,
            flags: u64,
            key: *const c_char,
            ev: *mut c_void,
        ) -> c_int;
        daos_kv_list(
            oh: daos_handle_t,
            th: daos_handle_t,
            nr: *mut u32,
            kds: *mut daos_key_desc_t,
            sgl: *mut d_sg_list_t,
            anchor: *mut daos_anchor_t,
            ev: *mut c_void,
        ) -> c_int;
    }
    Array {
        daos_array_create(
            coh: daos_handle_t,
            oid: daos_obj_id_t,
            th: daos_handle_t,
            cell_size: u64,
            chunk_size: u64,
            oh: *mut daos_handle_t,
            ev: *mut c_void,
        ) -> c_int;
        daos_array_open(
            coh: daos_handle_t,
            oid: daos_obj_id_t,
            th: daos_handle_t,
            mode: c_uint,
            cell_size: *mut u64,
            chunk_size: *mut u64,
            oh: *mut daos_handle_t,
            ev: *mut c_void,
        ) -> c_int;
        daos_array_open_with_attr(
            coh: daos_handle_t,
            oid: daos_obj_id_t,
            th: daos_handle_t,
            mode: c_uint,
            cell_size: u64,
            chunk_size: u64,
            oh: *mut daos_handle_t,
            ev: *mut c_void,
        ) -> c_int;
        daos_array_close(oh: daos_handle_t, ev: *mut c_void) -> c_int;
        daos_array_read(
            oh: daos_handle_t,
            th: daos_handle_t,
            iod: *mut daos_array_iod_t,
            sgl: *mut d_sg_list_t,
            ev: *mut c_void,
        ) -> c_int;
        daos_array_write(
            oh: daos_handle_t,
            th: daos_handle_t,
            iod: *mut daos_array_iod_t,
            sgl: *mut d_sg_list_t,
            ev: *mut c_void,
        ) -> c_int;
        daos_array_get_size(oh: daos_handle_t, th: daos_handle_t, size: *mut u64, ev: *mut c_void) -> c_int;
        daos_array_set_size(oh: daos_handle_t, th: daos_handle_t, size: u64, ev: *mut c_void) -> c_int;
        daos_array_stat(
            oh: daos_handle_t,
            th: daos_handle_t,
            stbuf: *mut daos_array_stbuf_t,
            ev: *mut c_void,
        ) -> c_int;
        daos_array_punch(
            oh: daos_handle_t,
            th: daos_handle_t,
            iod: *mut daos_array_iod_t,
            ev: *mut c_void,
        ) -> c_int;
        daos_array_destroy(oh: daos_handle_t, th: daos_handle_t, ev: *mut c_void) -> c_int;
    }
    Tx {
        daos_tx_open(coh: daos_handle_t, th: *mut daos_handle_t, flags: u64, ev: *mut c_void) -> c_int;
        daos_tx_open_snap(
            coh: daos_handle_t,
            epoch: daos_epoch_t,
            th: *mut daos_handle_t,
            ev: *mut c_void,
        ) -> c_int;
        daos_tx_commit(th: daos_handle_t, ev: *mut c_void) -> c_int;
        daos_tx_abort(th: daos_handle_t, ev: *mut c_void) -> c_int;
        daos_tx_close(th: daos_handle_t, ev: *mut c_void) -> c_int;
        daos_tx_restart(th: daos_handle_t, ev: *mut c_void) -> c_int;
        daos_tx_hdl2epoch(th: daos_handle_t, epoch: *mut daos_epoch_t) -> c_int;
    }
}
