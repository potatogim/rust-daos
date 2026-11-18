// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Table of object, KV and array handles open on one container.
//!
//! Each open handle is registered under a slot. Closing through the handle
//! (its last clone, or `close`) removes the slot first, so a handle is closed
//! exactly once: either by its owner or by `Container::close`, which drains
//! the table while it holds the container gate exclusively.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::error::{Error, Result};
use crate::sys::api::Raw;
use crate::sys::daos_handle_t;

/// Kind of an open handle: decides the close call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `daos_obj_open`, closed with `daos_obj_close`.
    Object,
    /// `daos_kv_open`, closed with `daos_kv_close`.
    Kv,
    /// `daos_array_create`/`open`, closed with `daos_array_close`.
    Array,
}

impl Kind {
    /// Close `oh` with the matching call.
    pub(crate) fn close(self, backend: &dyn Raw, oh: daos_handle_t) -> Result<()> {
        // SAFETY: `oh` came from the open call of this kind and was removed
        // from its table just before, so it is closed once; ev NULL.
        let (op, rc) = unsafe {
            match self {
                Kind::Object => (
                    "daos_obj_close",
                    backend.daos_obj_close(oh, std::ptr::null_mut()),
                ),
                Kind::Kv => (
                    "daos_kv_close",
                    backend.daos_kv_close(oh, std::ptr::null_mut()),
                ),
                Kind::Array => (
                    "daos_array_close",
                    backend.daos_array_close(oh, std::ptr::null_mut()),
                ),
            }
        };
        if rc != 0 {
            return Err(Error::daos(op, rc));
        }
        Ok(())
    }
}

/// Open handles by slot.
#[derive(Default)]
pub(crate) struct Table {
    slots: Mutex<HashMap<u64, (Kind, daos_handle_t)>>,
    next: AtomicU64,
}

impl Table {
    /// Register a handle; returns its slot.
    pub(crate) fn track(&self, kind: Kind, oh: daos_handle_t) -> u64 {
        let slot = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(slot, (kind, oh));
        slot
    }

    /// Remove a slot; returns the handle when it was still registered.
    pub(crate) fn take(&self, slot: u64) -> Option<(Kind, daos_handle_t)> {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&slot)
    }

    /// Close every registered handle (arrays, then KV, then objects);
    /// returns the first error. The caller holds the container gate
    /// exclusively.
    pub(crate) fn close_all(&self, backend: &dyn Raw) -> Result<()> {
        let mut all: Vec<(Kind, daos_handle_t)> = self
            .slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, v)| v)
            .collect();
        all.sort_by_key(|(k, _)| match k {
            Kind::Array => 0,
            Kind::Kv => 1,
            Kind::Object => 2,
        });
        let mut first = None;
        for (kind, oh) in all {
            if let Err(e) = kind.close(backend, oh) {
                first.get_or_insert(e);
            }
        }
        match first {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}
