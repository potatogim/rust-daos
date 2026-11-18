// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions transcribed from DAOS headers, BSD-2-Clause-Patent, see LICENSE-THIRD-PARTY

//! dlopen of libdfs and its symbol table.

use std::ffi::c_void;

use libloading::os::unix::{Library, RTLD_LOCAL, RTLD_NOW};

use super::api::{Group, RawFns};
use crate::error::{Error, Result};

/// The loaded libdfs. Never unloaded: libdaos keeps thread-local state
/// (`static __thread daos_event_t ev_thpriv`, `client/api/event.c`) that must
/// not disappear under running threads.
pub(crate) struct LibDfs {
    pub(crate) path: String,
    pub(crate) has_reinit: bool,
    pub(crate) has_connect_snap: bool,
    pub(crate) fns: RawFns,
    /// Library the `daos_*` names were resolved through.
    daos_library: String,
    /// Pool, container and mount registry of this library.
    pub(crate) registry: crate::conn::Registry,
}

/// Fallback for the `daos_*` symbols when they are not reachable through the
/// libdfs handle. Never the unversioned `libdaos.so`, which is only in
/// daos-devel.
const LIBDAOS_FALLBACK: &str = "libdaos.so.2";

/// Address of `name` in `lib`, or NULL.
fn addr(lib: &Library, name: &str) -> *mut c_void {
    // SAFETY: the symbol is only read as an address here; it is converted to
    // a function pointer of its C prototype by RawFns::resolve.
    unsafe { lib.get::<*mut c_void>(name.as_bytes()) }
        .map(|s| *s)
        .unwrap_or(std::ptr::null_mut())
}

fn open_lib(name: &str) -> std::result::Result<&'static Library, String> {
    // SAFETY: loading libdfs runs its ELF constructors and those of its
    // dependencies (libdaos, libgurt, libcart). They only set up library
    // state; no Rust invariants depend on them.
    let lib =
        unsafe { Library::open(Some(name), RTLD_NOW | RTLD_LOCAL) }.map_err(|e| e.to_string())?;
    // Never unloaded (see LibDfs).
    Ok(Box::leak(Box::new(lib)))
}

impl LibDfs {
    /// dlopen `name` (a path or soname) and resolve every symbol.
    pub(crate) fn load(name: &str) -> Result<LibDfs> {
        let dfs = open_lib(name).map_err(|reason| Error::LibraryNotFound {
            tried: vec![name.to_string()],
            reason,
        })?;
        let missing = |sym: &'static str, library: &str| Error::MissingSymbol {
            name: sym,
            library: library.to_string(),
        };

        // daos_* live in libdaos, a dependency of libdfs. dlsym on the libdfs
        // handle searches its dependency tree; fall back to libdaos.so.2.
        let (daos_lib, daos_name) = if !addr(dfs, "daos_init").is_null() {
            (dfs, name.to_string())
        } else {
            let l = open_lib(LIBDAOS_FALLBACK).map_err(|_| missing("daos_init", name))?;
            (l, LIBDAOS_FALLBACK.to_string())
        };
        // Required for completeness of the client library, never called.
        if addr(daos_lib, "daos_fini").is_null() {
            return Err(missing("daos_fini", &daos_name));
        }
        if addr(dfs, "dfs_fini").is_null() {
            return Err(missing("dfs_fini", name));
        }
        let has_reinit = !addr(daos_lib, "daos_reinit").is_null();
        let has_connect_snap = !addr(dfs, "dfs_connect_snap").is_null();

        // daos_* and the libraries libdaos depends on (daos_common, gurt)
        // through the daos handle, dfs_* through the libdfs handle.
        let fns = RawFns::resolve(|sym| {
            let lib = if sym.starts_with("dfs_") {
                dfs
            } else {
                daos_lib
            };
            addr(lib, sym)
        });
        let lib = LibDfs {
            path: name.to_string(),
            has_reinit,
            has_connect_snap,
            fns,
            daos_library: daos_name,
            registry: crate::conn::Registry::default(),
        };
        // Core and DFS symbols must be present at load.
        for g in [Group::Core, Group::Dfs] {
            if let Some(e) = lib.missing_error(g) {
                return Err(e);
            }
        }
        Ok(lib)
    }

    /// `MissingSymbol` for the first absent symbol of `group`.
    pub(crate) fn missing_error(&self, group: Group) -> Option<Error> {
        self.fns.missing[group as usize].map(|name| Error::MissingSymbol {
            name,
            library: if name.starts_with("dfs_") {
                self.path.clone()
            } else {
                self.daos_library.clone()
            },
        })
    }
}
