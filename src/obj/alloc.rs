// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`OidAllocator`]: unique object ids from `daos_cont_alloc_oids`.

use std::fmt;
use std::sync::Mutex;

use super::{ClassHints, ObjectClass, ObjectId, ObjectType};
use crate::cont::Container;
use crate::error::{Error, Result};

/// Hands out object ids that are unique in one container: `lo` values come
/// from `daos_cont_alloc_oids` in batches, and
/// [`Container::generate_oid`] encodes type and class.
///
/// The range of the current batch lives in this process. A child that the
/// crate recognises as such cannot use it ([`next`](Self::next) returns
/// [`Error::Forked`] there), so a parent and such a child never draw from
/// one range. Where the kernel cannot clear the crate's token page in
/// children, a descendant with the parent's process id (possible with PID
/// namespaces) is recognised only when a `fork` of the C library lies
/// between, after the crate's atfork handler was installed; see the README
/// section Processes. Ids drawn and not used are lost.
pub struct OidAllocator {
    cont: Container,
    /// (next, end) of the current range.
    range: Mutex<(u64, u64)>,
    batch: u64,
}

impl fmt::Debug for OidAllocator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OidAllocator")
            .field("container", &self.cont.name())
            .field("batch", &self.batch)
            .finish()
    }
}

impl OidAllocator {
    pub(crate) fn new(cont: Container) -> OidAllocator {
        OidAllocator {
            cont,
            range: Mutex::new((0, 0)),
            batch: 1024,
        }
    }

    /// Ids reserved per `daos_cont_alloc_oids` call (default 1024).
    pub fn batch_size(mut self, n: u64) -> Self {
        self.batch = n.max(1);
        self
    }

    /// The next unique id of type `ty` and class `class`.
    pub fn next(&self, ty: ObjectType, class: ObjectClass, hints: ClassHints) -> Result<ObjectId> {
        // Before the range lock: a fork child must not wait for a lock a
        // parent thread held at the fork.
        self.cont.conn.stamp.check()?;
        let lo = {
            let mut r = self.range.lock().unwrap_or_else(|e| e.into_inner());
            if r.0 == r.1 {
                let first = self.cont.alloc_oids(self.batch)?;
                let end = first
                    .checked_add(self.batch)
                    .ok_or(Error::InvalidArgument("object id range exhausted"))?;
                *r = (first, end);
            }
            let lo = r.0;
            r.0 += 1;
            lo
        };
        self.cont.generate_oid(lo, 0, ty, class, hints)
    }
}
