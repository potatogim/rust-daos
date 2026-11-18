// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The open handle shared by [`Object`](super::Object),
//! [`Kv`](crate::kv::Kv) and [`Array`](crate::array::Array).

use std::sync::Arc;

use super::ObjectId;
use crate::conn::ContConn;
use crate::error::Result;
use crate::gate::Pass;
use crate::handles::Kind;
use crate::sys::daos_handle_t;

/// One open object handle registered in its container's handle table. The
/// handle is closed exactly once: by the last owner (drop or `close`) when
/// the slot is still registered, or by `Container::close`.
pub(crate) struct ObjInner {
    pub(crate) cont: Arc<ContConn>,
    pub(crate) oh: daos_handle_t,
    pub(crate) oid: ObjectId,
    slot: u64,
}

impl ObjInner {
    pub(crate) fn new(
        cont: Arc<ContConn>,
        oh: daos_handle_t,
        oid: ObjectId,
        kind: Kind,
    ) -> ObjInner {
        let slot = cont.handles.track(kind, oh);
        ObjInner {
            cont,
            oh,
            oid,
            slot,
        }
    }

    /// Enter a data call: the container gate, held shared.
    pub(crate) fn enter(&self) -> Result<Pass<'_>> {
        self.cont.enter()
    }

    /// Close the handle if it is still registered (reporting the error).
    pub(crate) fn release(&self) -> Result<()> {
        if self.cont.stamp.is_foreign() {
            return Ok(());
        }
        // Releasing is allowed while a container close waits; after the
        // close the slot is gone.
        let _pass = match self.cont.gate.enter(true) {
            Ok(p) => p,
            Err(_) => return Ok(()),
        };
        match self.cont.handles.take(self.slot) {
            Some((kind, oh)) => kind.close(self.cont.backend.as_ref(), oh),
            None => Ok(()),
        }
    }

    /// `close(self)` of the public types: closes when this was the last
    /// clone.
    pub(crate) fn close(this: Arc<ObjInner>) -> Result<()> {
        this.cont.stamp.check_cold()?;
        match Arc::try_unwrap(this) {
            Ok(inner) => {
                let r = inner.release();
                drop(inner);
                r
            }
            Err(_) => Ok(()),
        }
    }
}

impl Drop for ObjInner {
    fn drop(&mut self) {
        // Fork child: release() makes no call. Errors are ignored here.
        let _ = self.release();
    }
}
