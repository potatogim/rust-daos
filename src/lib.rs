// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Client for DAOS: the DAOS File System (DFS, POSIX containers) and the
//! pool, container, object, key-value, array and transaction APIs.
//!
//! `daos` loads `libdfs.so` at run time with `dlopen` and reaches libdaos
//! through it, so it has no build-time dependency on DAOS: it compiles on any
//! Unix machine and reports a clear error when the libraries are absent. One
//! binary works with the DAOS 2.6 and 2.8 client libraries: tested against
//! DAOS 2.8.1 and 2.6.4 systems; the 2.6.0 client library loads (I/O not run
//! before 2.6.4). Two-way replicated classes are tested on two engines;
//! three-way replicated and erasure-code classes are offered and untested.
//!
//! # Quick start
//!
//! ```no_run
//! fn main() -> Result<(), daos::Error> {
//!     // SAFETY: first statement of main, no other thread exists yet.
//!     let report = unsafe { daos::init(&daos::InitOptions::default())? };
//!     for w in &report.warnings {
//!         eprintln!("daos: {w}");
//!     }
//!     let dfs = daos::Dfs::connect("tank", "data")?;
//!     dfs.write("/train/sample.bin", &[1, 2, 3])?;
//!     let file = dfs.open("/train/sample.bin")?;
//!     let mut buf = vec![0u8; file.size()? as usize];
//!     file.read_exact_at(&mut buf, 0)?; // straight into buf, no copy
//!     for entry in dfs.read_dir("/train")? {
//!         println!("{}", entry?.path());
//!     }
//!     dfs.remove_all("/train")?;
//!     dfs.close(std::time::Duration::from_secs(30))?;
//!     Ok(())
//! }
//! ```
//!
//! # Object API
//!
//! ```no_run
//! # fn main() -> Result<(), daos::Error> {
//! use daos::obj::{ClassHints, ObjectClass, ObjectType, OpenMode};
//! unsafe { daos::init(&daos::InitOptions::default())? };
//! let cont = daos::Container::open("tank", "objects")?;
//! let ids = cont.oid_allocator();
//! let kv = cont.kv(
//!     ids.next(ObjectType::KvHashed, ObjectClass::UNKNOWN, ClassHints::default())?,
//!     OpenMode::ReadWrite,
//! )?;
//! kv.put(None, "model", b"resnet50")?;
//! cont.transaction(daos::tx::TxMode::ReadWrite, |tx| kv.put(Some(tx), "epoch", b"2"))?;
//! # Ok(()) }
//! ```
//!
//! [`Pool`], [`Container`] and [`Dfs`] share pool and container handles
//! through a process-wide registry; [`Dfs::container_handle`] gives the
//! connection of a mount to the object API. Modules: [`pool`], [`cont`],
//! [`obj`], [`kv`], [`array`](mod@array), [`tx`].
//!
//! # Transactions
//!
//! Every data call takes `tx: Option<&Tx>`. A [`Tx`] is neither `Send` nor
//! `Sync`; [`Container::transaction`] runs a closure and restarts it on
//! `DER_TX_RESTART`.
//!
//! # Initialisation
//!
//! [`init`] is `unsafe` because `daos_init` writes the process environment
//! (server-provided client variables, provider settings). Call it before other
//! threads start. [`Dfs::connect`], [`Pool::connect`] and [`Container::open`]
//! never initialise; without [`init`] they return [`Error::NotInitialized`].
//! The crate never finalises DAOS: the runtime stays until the process exits,
//! and closing a mount, container or pool only returns its handles.
//!
//! # Errors
//!
//! [`Error::Dfs`] carries the errno libdfs returned, [`Error::Daos`] the DER
//! code of a libdaos call. [`Error::raw_os_error`] and [`Error::kind`] work on
//! both; `From<Error> for std::io::Error` keeps the errno for those two.
//!
//! # Threads
//!
//! [`Dfs`], [`File`], [`Pool`], [`Container`], [`Object`], [`Kv`] and
//! [`Array`] are `Send + Sync`; [`Writer`], [`ReadDir`] and the key
//! iterators are `Send`; [`Tx`] and what borrows it are neither. All calls
//! block (no event queues). From async code use `spawn_blocking`. libdfs and
//! libdaos protect shared state with their own locks and run blocking calls
//! on per-thread events.
//!
//! # Processes
//!
//! libdaos and DFS mount state do not survive `fork`. Every use of the crate
//! in a fork child of a process that called [`init`] returns
//! [`Error::Forked`], and dropping inherited handles in the child does
//! nothing. The same holds when another thread of the parent was inside
//! [`init`] or inside the library load of [`library_info`] at the moment of
//! the fork: the child gets [`Error::Forked`] at once instead of waiting for
//! a lock that thread held. Start workers with spawn (or fork then exec) and
//! call [`init`] and connect in each process. Forking before [`init`] is
//! fine, also after a [`library_info`] probe in the parent.
//!
//! # One system per process
//!
//! The crate opens pool and container handles itself and shares them through
//! a registry keyed by the pool and container strings and the access mode.
//! The first successful connect fixes the DAOS system for the life of the
//! process; a request for another system returns [`Error::Conflict`].
//! Read-only and read-write connections to the same pool or container are
//! separate handles.
//!
//! # Known issue: MR cache in multi-threaded processes
//!
//! Multi-threaded processes without MPI have crashed in the fabric provider's
//! memory-registration cache (`ofi_mr_cache_search`) when freeing buffers used
//! for reads. Set `CRT_MRC_ENABLE=0` in the launcher (shell, `mpirun -x`,
//! systemd unit, container spec) before the process starts. The crate never
//! writes the environment; [`InitReport`] shows what it found and warns.
//!
//! # Testing without DAOS
//!
//! Feature `mock` adds [`mock::MemDfs`] (also named `MemDaos`), an in-memory
//! backend, with `Dfs::connect_mock`, `Pool::connect_mock` and
//! `Container::open_mock`.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

pub mod array;
mod conn;
pub mod cont;
mod dfs;
mod error;
mod fork;
mod gate;
mod handles;
pub mod kv;
pub mod obj;
pub mod path;
pub mod pool;
mod runtime;
mod sys;
pub mod tx;
mod uuid;

#[rustfmt::skip]
mod der_table;

#[cfg(doctest)]
mod doc_checks;

#[cfg(feature = "mock")]
pub mod mock;

pub use array::Array;
pub use conn::Access;
pub use cont::Container;
pub use dfs::{
    ConnectOptions, Dfs, DirEntry, File, FileType, Metadata, MountOptions, OpenOptions, ReadDir,
    Writer, DEFAULT_DIR_CACHE_CAPACITY,
};
pub use error::{DerCode, Error, Result};
pub use kv::Kv;
pub use obj::{Object, ObjectClass, ObjectId, ObjectType};
pub use pool::Pool;
pub use runtime::{
    init, init_report, is_available, library_info, InitOptions, InitReport, LibraryInfo,
    DEFAULT_LIBDFS, LIBDFS_ENV,
};
pub use tx::Tx;
pub use uuid::Uuid;
