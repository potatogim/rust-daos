// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pools: [`Pool`], pool information and the container management calls.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::conn::{cstring, require, PoolConn};
use crate::cont::{Container, ContainerCreate};
use crate::error::{Error, Result};
use crate::sys::api::{Group, Raw};
use crate::sys::{self, daos_handle_t};
use crate::uuid::Uuid;

pub use crate::conn::Access;

/// Options of [`Pool::connect_with`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PoolOptions {
    /// Pool label or UUID string.
    pub pool: String,
    /// DAOS system name; `None` = the default system.
    pub sys_name: Option<String>,
    /// Access mode; default read-write.
    pub access: Access,
}

impl PoolOptions {
    /// Options for `pool`: read-write, default system.
    pub fn new(pool: impl Into<String>) -> Self {
        PoolOptions {
            pool: pool.into(),
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

/// A pool connection (`daos_pool_connect`). Cheap to clone; all clones, and
/// every [`Container`] and [`Dfs`](crate::Dfs) opened on the same pool string
/// and access mode, share one pool handle.
///
/// ```no_run
/// # fn main() -> Result<(), daos::Error> {
/// unsafe { daos::init(&daos::InitOptions::default())? };
/// let pool = daos::Pool::connect("tank")?;
/// let info = pool.query()?;
/// println!("{} targets, {} bytes free on NVMe", info.targets, info.space.nvme.free);
/// for c in pool.list_containers()? {
///     println!("{} {}", c.uuid, c.label);
/// }
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct Pool {
    pub(crate) conn: Arc<PoolConn>,
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("name", &self.conn.name)
            .field("uuid", &self.conn.uuid)
            .field("access", &self.conn.access)
            .finish()
    }
}

/// Space of one media class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct MediaSpace {
    /// Total bytes.
    pub total: u64,
    /// Free bytes.
    pub free: u64,
    /// Smallest free space of one target.
    pub free_min: u64,
    /// Largest free space of one target.
    pub free_max: u64,
    /// Mean free space per target.
    pub free_mean: u64,
}

/// Pool space by media class.
///
/// `scm` is the storage-class memory tier; on systems with metadata on SSD
/// (MD-on-SSD) it is the metadata tier held in memory and written to SSD.
/// `nvme` is the data tier on NVMe SSDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct PoolSpace {
    /// Storage-class memory (metadata on MD-on-SSD systems).
    pub scm: MediaSpace,
    /// NVMe.
    pub nvme: MediaSpace,
    /// Targets the space numbers were collected from.
    pub targets: u32,
}

/// Rebuild status of a pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct RebuildStatus {
    /// Pool map version of the rebuild (0: no rebuild has run).
    pub version: u32,
    /// Duration of the rebuild in seconds.
    pub seconds: u32,
    /// Error of the rebuild (a negative DER value), 0 when none.
    pub errno: i32,
    /// `DRS_IN_PROGRESS` (0), `DRS_NOT_STARTED` (1) or `DRS_COMPLETED` (2).
    pub state: i32,
    /// Objects still to be rebuilt.
    pub objects_to_rebuild: u64,
    /// Objects rebuilt.
    pub objects: u64,
    /// Records rebuilt.
    pub records: u64,
    /// Bytes rebuilt.
    pub bytes: u64,
}

impl RebuildStatus {
    /// True when a rebuild is running.
    pub fn in_progress(&self) -> bool {
        self.state == 0
    }
}

/// Result of [`Pool::query`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PoolInfo {
    /// Pool UUID.
    pub uuid: Uuid,
    /// Number of targets.
    pub targets: u32,
    /// Number of engines (`pi_nnodes`).
    pub nodes: u32,
    /// Disabled targets.
    pub disabled_targets: u32,
    /// Pool map version.
    pub map_version: u32,
    /// Rank of the pool service leader.
    pub leader: u32,
    /// Space.
    pub space: PoolSpace,
    /// Rebuild status.
    pub rebuild: RebuildStatus,
}

/// State of a target (`daos_target_state_t`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TargetState {
    /// Up and in the pool map.
    UpIn,
    /// Up, not yet in.
    Up,
    /// Down, may need rebuild.
    Down,
    /// Down and out.
    DownOut,
    /// Being added.
    New,
    /// Being drained.
    Drain,
    /// Any other value.
    Other(i32),
}

impl TargetState {
    fn from_raw(v: i32) -> TargetState {
        match v {
            1 => TargetState::DownOut,
            2 => TargetState::Down,
            3 => TargetState::Up,
            4 => TargetState::UpIn,
            5 => TargetState::New,
            6 => TargetState::Drain,
            other => TargetState::Other(other),
        }
    }
}

/// Result of [`Pool::query_target`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct TargetInfo {
    /// Target state.
    pub state: TargetState,
    /// Total and free bytes of the target: storage-class memory.
    pub scm: (u64, u64),
    /// Total and free bytes of the target: NVMe.
    pub nvme: (u64, u64),
}

/// One container of [`Pool::list_containers`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ContainerEntry {
    /// Container UUID.
    pub uuid: Uuid,
    /// Container label (empty when the container has none).
    pub label: String,
}

fn media(space: &sys::daos_pool_space, i: usize) -> MediaSpace {
    MediaSpace {
        total: space.ps_space.s_total[i],
        free: space.ps_space.s_free[i],
        free_min: space.ps_free_min[i],
        free_max: space.ps_free_max[i],
        free_mean: space.ps_free_mean[i],
    }
}

impl Pool {
    pub(crate) fn from_conn(conn: Arc<PoolConn>) -> Pool {
        Pool { conn }
    }

    /// Connect read-write to `pool` (label or UUID string) of the default
    /// system. Requires a completed [`init`](crate::init).
    pub fn connect(pool: &str) -> Result<Pool> {
        Self::connect_with(&PoolOptions::new(pool))
    }

    /// Connect with options. A live connection with the same pool string and
    /// access mode is shared. The first successful connect of the process
    /// fixes the DAOS system ([`Error::Conflict`] for another one).
    pub fn connect_with(options: &PoolOptions) -> Result<Pool> {
        let rt = crate::runtime::runtime()?;
        Self::connect_on(&rt.backend, options)
    }

    /// Connect to an in-memory backend (feature `mock`).
    #[cfg(feature = "mock")]
    pub fn connect_mock(backend: Arc<crate::mock::MemDfs>, options: &PoolOptions) -> Result<Pool> {
        backend.stamp().check_cold()?;
        backend.ensure_init()?;
        let raw: Arc<dyn Raw> = backend;
        Self::connect_on(&raw, options)
    }

    fn connect_on(backend: &Arc<dyn Raw>, options: &PoolOptions) -> Result<Pool> {
        let conn = backend.registry().pool(
            backend,
            &options.pool,
            options.sys_name.as_deref(),
            options.access,
        )?;
        Ok(Pool { conn })
    }

    /// The string given at connect (label or UUID string).
    pub fn name(&self) -> &str {
        &self.conn.name
    }

    /// Pool UUID (as reported by the connect call; nil when the library did
    /// not report it).
    pub fn uuid(&self) -> Uuid {
        self.conn.uuid
    }

    /// Access mode of this connection.
    pub fn access(&self) -> Access {
        self.conn.access
    }

    /// The pool handle cookie, for diagnostics and logs. It is valid only
    /// while this connection is open and must not be closed by the caller.
    pub fn as_raw(&self) -> u64 {
        self.conn.poh.cookie
    }

    fn backend(&self) -> &dyn Raw {
        self.conn.backend.as_ref()
    }

    fn poh(&self) -> daos_handle_t {
        self.conn.poh
    }

    /// Space, rebuild status and membership (`daos_pool_query` with
    /// `DPI_SPACE | DPI_REBUILD_STATUS`).
    pub fn query(&self) -> Result<PoolInfo> {
        require(self.backend(), Group::PoolCont)?;
        let _pass = self.conn.enter()?;
        let mut info = sys::daos_pool_info_t {
            pi_bits: sys::DPI_SPACE | sys::DPI_REBUILD_STATUS,
            ..Default::default()
        };
        // SAFETY: live handle (gate held); ranks and pool_prop NULL; info is a
        // local of the header's layout; ev NULL.
        let rc = unsafe {
            self.backend().daos_pool_query(
                self.poh(),
                std::ptr::null_mut(),
                &mut info,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            return Err(Error::daos_at("daos_pool_query", rc, self.name()));
        }
        let rs = &info.pi_rebuild_st;
        Ok(PoolInfo {
            uuid: Uuid::from_bytes(info.pi_uuid),
            targets: info.pi_ntargets,
            nodes: info.pi_nnodes,
            disabled_targets: info.pi_ndisabled,
            map_version: info.pi_map_ver,
            leader: info.pi_leader,
            space: PoolSpace {
                scm: media(&info.pi_space, 0),
                nvme: media(&info.pi_space, 1),
                targets: info.pi_space.ps_ntargets,
            },
            rebuild: RebuildStatus {
                version: rs.rs_version,
                seconds: rs.rs_seconds,
                errno: rs.rs_errno,
                state: rs.rs_state,
                objects_to_rebuild: rs.rs_toberb_obj_nr,
                objects: rs.rs_obj_nr,
                records: rs.rs_rec_nr,
                bytes: rs.rs_size,
            },
        })
    }

    /// State and space of target `target` of engine `rank`
    /// (`daos_pool_query_target`).
    pub fn query_target(&self, rank: u32, target: u32) -> Result<TargetInfo> {
        require(self.backend(), Group::PoolCont)?;
        let _pass = self.conn.enter()?;
        let mut info = sys::daos_target_info_t::default();
        // SAFETY: live handle (gate held); info is a local; ev NULL.
        let rc = unsafe {
            self.backend().daos_pool_query_target(
                self.poh(),
                target,
                rank,
                &mut info,
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            return Err(Error::daos_at("daos_pool_query_target", rc, self.name()));
        }
        Ok(TargetInfo {
            state: TargetState::from_raw(info.ta_state),
            scm: (info.ta_space.s_total[0], info.ta_space.s_free[0]),
            nvme: (info.ta_space.s_total[1], info.ta_space.s_free[1]),
        })
    }

    /// UUIDs and labels of the pool's containers (`daos_pool_list_cont`).
    pub fn list_containers(&self) -> Result<Vec<ContainerEntry>> {
        require(self.backend(), Group::PoolCont)?;
        let _pass = self.conn.enter()?;
        let mut cap: u64 = 64;
        // The list can grow between calls: retry with the reported count.
        for _ in 0..8 {
            let mut buf = vec![
                sys::daos_pool_cont_info {
                    pci_uuid: [0; 16],
                    pci_label: [0; sys::DAOS_PROP_LABEL_MAX_LEN + 1],
                };
                cap as usize
            ];
            let mut n = cap;
            // SAFETY: live handle; buf holds `n` entries; ev NULL.
            let rc = unsafe {
                self.backend().daos_pool_list_cont(
                    self.poh(),
                    &mut n,
                    buf.as_mut_ptr(),
                    std::ptr::null_mut(),
                )
            };
            if rc == -(crate::DerCode::TRUNC.value()) || (rc == 0 && n > cap) {
                cap = n.max(cap * 2);
                continue;
            }
            if rc != 0 {
                return Err(Error::daos_at("daos_pool_list_cont", rc, self.name()));
            }
            buf.truncate(n as usize);
            return Ok(buf
                .iter()
                .map(|e| ContainerEntry {
                    uuid: Uuid::from_bytes(e.pci_uuid),
                    label: c_label(&e.pci_label),
                })
                .collect());
        }
        Err(Error::daos_at(
            "daos_pool_list_cont",
            -(crate::DerCode::TRUNC.value()),
            self.name(),
        ))
    }

    /// Create a container; returns its UUID. Does not open it. See
    /// [`ContainerCreate`] for the routes (POSIX containers through libdfs,
    /// others through libdaos).
    pub fn create_container(&self, spec: &ContainerCreate) -> Result<Uuid> {
        require(self.backend(), Group::PoolCont)?;
        spec.validate()?;
        let _pass = self.conn.enter()?;
        spec.create(self.backend(), self.poh(), self.name())
    }

    /// Open (or share) a container of this pool, with the pool's access mode.
    pub fn open_container(&self, cont: &str) -> Result<Container> {
        self.conn.stamp.check()?;
        let c = self
            .conn
            .backend
            .registry()
            .container_on(&self.conn, cont)?;
        Ok(Container::from_conn(c))
    }

    /// Destroy a container (`daos_cont_destroy`).
    ///
    /// Returns [`Error::Busy`] (with `open_connections` 1) without any DAOS
    /// call when this process has the container open (or opening) under the
    /// same pool and container strings, in any access mode, even with
    /// `force`: close your own handles first. An open handle under another
    /// string (UUID versus label) is not detected; DAOS then refuses the
    /// destroy without `force`.
    ///
    /// The same `Busy` holds while the last close of such a handle runs, and
    /// for the rest of the process when that close failed or was skipped
    /// (a close whose process check failed): the crate cannot show that the
    /// handle is closed and never closes it later. The block is by pool and
    /// container string, so a container that another process destroys and
    /// creates again under the same label stays blocked in this process as
    /// well.
    pub fn destroy_container(&self, cont: &str, force: bool) -> Result<()> {
        // Before any lock: a fork child must not wait for a lock a parent
        // thread held at the fork.
        self.conn.stamp.check()?;
        require(self.backend(), Group::PoolCont)?;
        if cont.is_empty() {
            return Err(Error::InvalidArgument("container is empty"));
        }
        let c = cstring(cont, "container names must not contain NUL")?;
        // Opens of the same pool and container strings wait until the
        // destroy has returned.
        let _guard = self
            .conn
            .backend
            .registry()
            .reserve_destroy(&self.conn.name, cont)?;
        let _pass = self.conn.enter()?;
        // SAFETY: live handle (gate held); NUL-terminated name; ev NULL.
        let rc = unsafe {
            self.backend().daos_cont_destroy2(
                self.poh(),
                c.as_ptr(),
                i32::from(force),
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            return Err(Error::daos_at(
                "daos_cont_destroy",
                rc,
                format!("{}/{}", self.name(), cont),
            ));
        }
        Ok(())
    }

    /// Close this pool connection for every view: refuse new pool calls,
    /// wait up to `timeout` until running ones end and no container
    /// connection of the pool is open, then `daos_pool_disconnect`.
    ///
    /// [`Error::Busy`] (with the open container count as `open_connections`) when
    /// containers or DFS mounts of the pool are still open at the deadline;
    /// nothing is closed then. Idempotent. Dropping every view without
    /// `close` disconnects quietly.
    ///
    /// On an inherited handle `close` returns [`Error::Forked`] before any
    /// DAOS call when the process check at entry fails.
    pub fn close(&self, timeout: Duration) -> Result<()> {
        self.conn.close(timeout)
    }

    /// True after a successful [`Pool::close`], and in a fork child.
    pub fn is_closed(&self) -> bool {
        !self.conn.is_usable()
    }
}

/// A NUL-terminated label from a fixed C array.
pub(crate) fn c_label(raw: &[std::ffi::c_char]) -> String {
    let bytes: Vec<u8> = raw
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}
