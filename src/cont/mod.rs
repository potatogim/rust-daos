// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Containers: [`Container`], creation ([`ContainerCreate`]), properties and
//! snapshots.

mod create;
mod prop;

pub use create::{ContainerCreate, Layout, RedundancyLevel};
pub use prop::Properties;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::conn::{require, Access, ContConn};
use crate::dfs::{Dfs, MountOptions};
use crate::error::{Error, Result};
use crate::pool::Pool;
use crate::sys::api::{Group, Raw};
use crate::sys::{self, daos_anchor_t, daos_handle_t};
use crate::uuid::Uuid;

/// Options of [`Container::open_with`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ContainerOptions {
    /// Pool label or UUID string.
    pub pool: String,
    /// Container label or UUID string.
    pub container: String,
    /// DAOS system name; `None` = the default system.
    pub sys_name: Option<String>,
    /// Access mode of the container and of its pool connection; default
    /// read-write.
    pub access: Access,
}

impl ContainerOptions {
    /// Options for `pool` and `container`: read-write, default system.
    pub fn new(pool: impl Into<String>, container: impl Into<String>) -> Self {
        ContainerOptions {
            pool: pool.into(),
            container: container.into(),
            sys_name: None,
            access: Access::ReadWrite,
        }
    }

    /// Set the DAOS system name.
    pub fn sys_name(mut self, name: impl Into<String>) -> Self {
        self.sys_name = Some(name.into());
        self
    }

    /// Set the access mode.
    pub fn access(mut self, access: Access) -> Self {
        self.access = access;
        self
    }
}

/// A container connection (`daos_cont_open`). Cheap to clone; all clones, a
/// [`Dfs`] mount of the same container and access mode, and every object,
/// key-value and array handle opened through it share one container handle.
///
/// A connection is bound to the container that was opened, not to its label:
/// if another process renames the container, or destroys it and creates a
/// new one under the same label, this connection still refers to the one it
/// opened.
///
/// Objects created through a `Container` in a POSIX container are invisible
/// to DFS, and a DFS consistency check may report or remove them. Use a
/// separate container for object data unless you know what you are doing.
///
/// ```no_run
/// # fn main() -> Result<(), daos::Error> {
/// unsafe { daos::init(&daos::InitOptions::default())? };
/// let cont = daos::Container::open("tank", "data")?;
/// let info = cont.query()?;
/// println!("{} has {} open handles", cont.uuid(), info.open_handles);
/// cont.close(std::time::Duration::from_secs(10))?;
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct Container {
    pub(crate) conn: Arc<ContConn>,
}

impl fmt::Debug for Container {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Container")
            .field("pool", &self.conn.pool_name)
            .field("name", &self.conn.name)
            .field("uuid", &self.conn.uuid)
            .field("access", &self.conn.access)
            .finish()
    }
}

/// Result of [`Container::query`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ContainerInfo {
    /// Container UUID.
    pub uuid: Uuid,
    /// Epoch of the latest snapshot (0 when there is none).
    pub latest_snapshot: u64,
    /// Open handles of the container, over all clients.
    pub open_handles: u32,
    /// Number of snapshots.
    pub snapshots: u32,
    /// Hybrid logical clock time of the last metadata open.
    pub metadata_open_time: u64,
    /// Hybrid logical clock time of the last metadata change.
    pub metadata_modify_time: u64,
}

/// A container snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub struct Snapshot {
    /// Epoch of the snapshot.
    pub epoch: u64,
}

impl Container {
    pub(crate) fn from_conn(conn: Arc<ContConn>) -> Container {
        Container { conn }
    }

    /// Open (or share) a container read-write on the default system.
    /// Requires a completed [`init`](crate::init). The container must exist.
    pub fn open(pool: &str, container: &str) -> Result<Container> {
        Self::open_with(&ContainerOptions::new(pool, container))
    }

    /// Open with options. A live connection with the same pool string,
    /// container string and access mode is shared, also with a [`Dfs`] mount
    /// of that key.
    pub fn open_with(options: &ContainerOptions) -> Result<Container> {
        let rt = crate::runtime::runtime()?;
        Self::open_on(&rt.backend, options)
    }

    /// Open on an in-memory backend (feature `mock`).
    #[cfg(feature = "mock")]
    pub fn open_mock(
        backend: Arc<crate::mock::MemDfs>,
        options: &ContainerOptions,
    ) -> Result<Container> {
        backend.stamp().check_cold()?;
        backend.ensure_init()?;
        let raw: Arc<dyn Raw> = backend;
        Self::open_on(&raw, options)
    }

    fn open_on(backend: &Arc<dyn Raw>, options: &ContainerOptions) -> Result<Container> {
        let has_nul = |s: &str| s.as_bytes().contains(&0);
        if has_nul(&options.pool)
            || has_nul(&options.container)
            || options.sys_name.as_deref().is_some_and(has_nul)
        {
            return Err(Error::InvalidArgument(
                "pool, container and system names must not contain NUL",
            ));
        }
        let conn = backend.registry().container(
            backend,
            &options.pool,
            &options.container,
            options.sys_name.as_deref(),
            options.access,
        )?;
        Ok(Container { conn })
    }

    /// The pool connection this container was opened on.
    pub fn pool(&self) -> Result<Pool> {
        self.conn.stamp.check()?;
        Ok(Pool::from_conn(self.conn.pool()?))
    }

    /// The string given at open (label or UUID string).
    pub fn name(&self) -> &str {
        &self.conn.name
    }

    /// Container UUID (the stable identity).
    pub fn uuid(&self) -> Uuid {
        self.conn.uuid
    }

    /// Access mode of this connection.
    pub fn access(&self) -> Access {
        self.conn.access
    }

    /// The container handle cookie, for diagnostics and logs. It is valid
    /// only while this connection is open and must not be closed by the
    /// caller.
    pub fn as_raw(&self) -> u64 {
        self.conn.coh.cookie
    }

    pub(crate) fn backend(&self) -> &dyn Raw {
        self.conn.backend.as_ref()
    }

    pub(crate) fn coh(&self) -> daos_handle_t {
        self.conn.coh
    }

    /// Container information (`daos_cont_query`).
    pub fn query(&self) -> Result<ContainerInfo> {
        require(self.backend(), Group::PoolCont)?;
        let _pass = self.conn.enter()?;
        let mut info = sys::daos_cont_info_t::default();
        // SAFETY: live handle (gate held); info is a local; prop NULL; ev NULL.
        let rc = unsafe {
            self.backend().daos_cont_query(
                self.coh(),
                &mut info,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            return Err(Error::daos_at("daos_cont_query", rc, self.conn.target()));
        }
        Ok(ContainerInfo {
            uuid: Uuid::from_bytes(info.ci_uuid),
            latest_snapshot: info.ci_lsnapshot,
            open_handles: info.ci_nhandles,
            snapshots: info.ci_nsnapshots,
            metadata_open_time: info.ci_md_otime,
            metadata_modify_time: info.ci_md_mtime,
        })
    }

    /// Container properties (`daos_cont_query` with a property list
    /// allocated by libdaos; copied out and freed before returning).
    pub fn properties(&self) -> Result<Properties> {
        require(self.backend(), Group::PoolCont)?;
        let _pass = self.conn.enter()?;
        prop::query(self.backend(), self.coh(), &self.conn.target())
    }

    /// Create an unnamed snapshot of the current state
    /// (`daos_cont_create_snap`). Named snapshots are not offered: the DAOS
    /// client returns `DER_NOSYS` for them.
    pub fn create_snapshot(&self) -> Result<Snapshot> {
        require(self.backend(), Group::PoolCont)?;
        let _pass = self.conn.enter()?;
        let mut epoch: u64 = 0;
        // SAFETY: live handle; out-pointer to a local; name NULL; ev NULL.
        let rc = unsafe {
            self.backend().daos_cont_create_snap(
                self.coh(),
                &mut epoch,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            return Err(Error::daos_at(
                "daos_cont_create_snap",
                rc,
                self.conn.target(),
            ));
        }
        Ok(Snapshot { epoch })
    }

    /// Epochs of the container's snapshots (`daos_cont_list_snap`).
    pub fn list_snapshots(&self) -> Result<Vec<Snapshot>> {
        require(self.backend(), Group::PoolCont)?;
        let _pass = self.conn.enter()?;
        let mut cap: usize = 16;
        for _ in 0..8 {
            let mut epochs = vec![0u64; cap];
            let mut nr = cap as i32;
            let mut anchor = daos_anchor_t::new();
            // SAFETY: live handle; `epochs` holds `nr` entries; names NULL;
            // the anchor is a local; ev NULL.
            let rc = unsafe {
                self.backend().daos_cont_list_snap(
                    self.coh(),
                    &mut nr,
                    epochs.as_mut_ptr(),
                    std::ptr::null_mut(),
                    &mut anchor,
                    std::ptr::null_mut(),
                )
            };
            if rc != 0 {
                return Err(Error::daos_at(
                    "daos_cont_list_snap",
                    rc,
                    self.conn.target(),
                ));
            }
            let n = usize::try_from(nr).unwrap_or(0);
            if n > cap {
                // More snapshots than room: retry with the reported count.
                cap = n;
                continue;
            }
            epochs.truncate(n);
            let mut v: Vec<Snapshot> = epochs.into_iter().map(|epoch| Snapshot { epoch }).collect();
            v.sort();
            return Ok(v);
        }
        Err(Error::daos_at(
            "daos_cont_list_snap",
            -(crate::DerCode::TRUNC.value()),
            self.conn.target(),
        ))
    }

    /// Destroy the snapshot at `epoch` (`daos_cont_destroy_snap`).
    pub fn destroy_snapshot(&self, epoch: u64) -> Result<()> {
        require(self.backend(), Group::PoolCont)?;
        let _pass = self.conn.enter()?;
        let epr = sys::daos_epoch_range_t {
            epr_lo: epoch,
            epr_hi: epoch,
        };
        // SAFETY: live handle; epr by value; ev NULL.
        let rc = unsafe {
            self.backend()
                .daos_cont_destroy_snap(self.coh(), epr, std::ptr::null_mut())
        };
        if rc != 0 {
            return Err(Error::daos_at(
                "daos_cont_destroy_snap",
                rc,
                self.conn.target(),
            ));
        }
        Ok(())
    }

    /// Mount DFS on this container (a POSIX container) with default options.
    /// Returns the shared mount when one exists for this container and
    /// access mode.
    pub fn mount_dfs(&self) -> Result<Dfs> {
        self.mount_dfs_with(&MountOptions::default())
    }

    /// Mount DFS with options. The options apply only when a new mount is
    /// made.
    pub fn mount_dfs_with(&self, options: &MountOptions) -> Result<Dfs> {
        self.conn.stamp.check()?;
        let sys = self.conn.pool()?.sys_name.clone();
        let mount = crate::dfs::mount_on(&self.conn, sys, options)?;
        Ok(Dfs { mount })
    }

    /// Close this container connection for every view: refuse new calls,
    /// wait up to `timeout` for running ones, close every object, key-value
    /// and array handle opened through it, unmount DFS mounts of this
    /// container and access mode, then `daos_cont_close` and give up the
    /// pool lease. Later calls on any of them return [`Error::Closed`].
    ///
    /// [`Error::Busy`] when calls still run or transactions of this
    /// container are open at the deadline; nothing is closed then and the
    /// connection accepts calls again. Idempotent. Dropping every view
    /// without `close` closes quietly.
    ///
    /// On an inherited handle `close` returns [`Error::Forked`] before any
    /// DAOS call when the process check at entry fails. It also returns that
    /// error when, during cleanup, the container held the last reference of
    /// its pool connection and this process cannot be confirmed as the one
    /// that connected it: the disconnect is skipped, and the container close
    /// already done is not undone. When several cleanup steps fail, the
    /// error returned is the first of: the `daos_cont_close` error, the error
    /// of the pool release (this one or a `daos_pool_disconnect` error), an
    /// error closing an object handle, an error unmounting a DFS mount of the
    /// container.
    pub fn close(&self, timeout: Duration) -> Result<()> {
        self.conn.close(timeout)
    }

    /// True after a successful [`Container::close`], and in a fork child.
    pub fn is_closed(&self) -> bool {
        !self.conn.is_usable()
    }
}
