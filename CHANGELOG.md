# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Versions before 1.0 may change the API in minor releases. MSRV bumps are a
minor-version change and are listed here.

## [Unreleased]

## [0.1.1] - 2026-11-20
### Fixed
- Fork during initialisation no longer hangs the child. A child forked while
  another thread of the parent was inside `init`, inside the library load of
  `library_info` or `is_available`, or inside the registration of the
  crate's atfork handler, waited for ever on a lock or `Once` that thread
  held. A child forked during `init` before its result was stored, or during
  the library load, now gets `Error::Forked` at once from `init`,
  `library_info` and every connect (`is_available` returns false). A child
  forked while the fork detection was being set up is not refused; it sets
  the detection up itself. A child of a parent that only probed the library,
  or whose `daos_init` failed, may still initialise. A child of a parent
  whose `dfs_init` failed gets `Error::Forked` from `library_info` and from
  `init`, not the error stored in the parent. A child forked after a parent
  thread released one of these locks but before it lowered its counter is
  refused as well.
- A process is no longer taken for the owner of the crate's state, or for
  the maker of a handle, because its process id matches (a child in a new
  PID namespace can have its parent's id), and a child made with a raw
  `clone` is refused on the checked calls that use inherited state (those
  listed under Processes in the README), not only on the calls that
  compared process ids. The crate keeps a token in a page that the kernel
  clears in every child (`MADV_WIPEONFORK`, Linux 4.14 and newer) and reads
  it on the checked calls; `library_info`, `is_available` and `init_report`
  answer from a stored report without reading it. Where that page cannot
  be set up it compares the kernel's process id with the recorded one on
  those calls and uses a
  `pthread_atfork` handler; there a descendant with the recorded process id
  is recognised only if a `fork` of the C library lies between, after the
  handler was installed, and the first use of the crate in a raw `clone`
  child of a multithreaded process may wait on a lock of the C library.
- Process ids are read from the kernel. The C library's `getpid()` could
  return the parent's id in a child made with a raw `clone` (glibc before
  2.25). Where the page cannot be set up, a process whose `getpid` system
  call is denied is refused, and `init` then fails before it loads the
  library; `Error::Forked` carries 0 for an unknown id.
- `ReadDir::next` returns `Error::Forked` once in a fork child, also when
  names were buffered before the fork, then `None`, and frees nothing in
  these calls. An iterator that had already ended with an error before the
  fork keeps returning `None` in the child, as it does in the parent.
- A cached pool, container or mount whose process check fails is no longer
  taken for a closed one: the call returns `Error::Forked` where it could
  open a second connection or let a forced `destroy_container` pass a
  container that is open in this process.
- `close` on an inherited handle returns `Error::Forked` before any DAOS
  call when the process check at entry fails, as before. It now also
  reports that error when the process that owns the last pool or container
  reference cannot be confirmed during cleanup; the native release of that
  handle is skipped, and an unmount or container close already done is not
  undone. A container whose handle was left open this way, or whose
  `daos_cont_close` failed, stays busy for `destroy_container` in this
  process, as does one whose last `daos_cont_close` is still running.
  Connections that a registry lookup or a destroy check found are dropped
  only after the registry lock was released.
- `Container::close` never reaches `daos_cont_close` while a DFS mount of
  the container is still being unmounted, by a dropped `Dfs` or by a
  `Dfs::close` in another thread, or while a directory handle of the mount
  is still being released. It waits for the unmount and returns
  `Error::Busy` when it has not finished by the deadline, with
  `open_connections` giving the number of such mounts; when transactions
  of the container are open as well, the `Busy` reports those instead and
  `open_connections` is 0. In 0.1.0 a container close that overlapped the
  drop of the last `Dfs` could run `daos_cont_close` under the mount and
  leave the container unusable and not closed.
- A container connection with a DFS mount that could not be unmounted,
  with a mount or transaction dropped while the process could not be
  confirmed as its owner, or with a failed `daos_tx_close`, stays open for
  the rest of the process, together with its pool connection: `close`
  returns that error at once and before any `Busy`, no `daos_cont_close` or
  `daos_pool_disconnect` is attempted when its handles are dropped, a call
  that would make a new DFS mount on it returns the same error (a mount of
  it that is still live stays shared, and a mount whose `dfs_mount` was
  already admitted when the error appeared may complete, its retries on
  `ENOENT` included), and `Pool::close` and `destroy_container` for it stay
  `Error::Busy`.
- `Container::mount_dfs` on a connection that is being closed returns
  `Error::Closed` instead of a live mount that a newer connection to the
  same container made.
- The reason in a `Busy` from `Container::close` no longer depends on
  whether calls were running on the container at the deadline, and a reason
  that has gone by the deadline is not reported: the close is tried once
  more.
- A transaction that was closed is no longer looked at again when it is
  dropped.
- A `Busy` from `Container::close` whose mounts could not be examined by
  the deadline carries only `in_flight`, or no count.
- `Container::close` no longer holds the container while it waits for
  calls of its DFS mount. New calls of the container get `Error::Closed`
  while the close waits; new calls of the mount get it from when the close
  installs its hold, after its first successful process check of the
  mount, until it drops that hold on return (in 0.1.0 only while it waited
  on the mount itself); after a `Busy` each of them works again when it is
  still open and its last waiting close has returned.
- The result of `pthread_atfork` is checked; a failed installation is tried
  again later.
- Docs: in a refused fork child `library_info`, `is_available` and
  `init_report` answer with the parent's data when the parent's `init` had
  stored its report.
- The docs build without features (the crate-level text named
  `mock::MemDfs` as a link).
- `path::MAX_PATH` docs state the longest accepted normalised path:
  `MAX_PATH - 1` = 4095 bytes, as in `dfs_lookup`; tests at 4095 and 4096
  bytes.

### Tests
- `multi_engine_ec_objects` is split by class (`_ec_2p1` needs 3 engines,
  `_ec_4p1` needs 5) and uses `rd_fac` 1 at rank level; every multi-engine
  case skips with its reason when the pool spans too few engines, and the
  node-level redundancy case runs only with `RUST_DAOS_TEST_NODES` of 3 or
  more. The runner lists skips by reason.
- The `enumeration` case inserts 2,000 keys per class on 8 threads sharing
  one object (`RUST_DAOS_ENUM_KEYS` raises the count) and times each
  listing pass.

## [0.1.0] - 2026-11-18
### Added
- DFS client: connect, open/create, read_at, read_exact_at, write_all_at,
  size, metadata, create_dir(_all), remove_file/dir(_all), remove_all,
  read_dir, Writer, close with timeout.
- Pools and containers (`daos::pool`, `daos::cont`): connect, query, target
  query, list, create (POSIX through libdfs, other layouts through libdaos,
  with or without a label), open, query, properties, destroy, unnamed
  snapshots, `close` with timeout, `as_raw`.
- Objects (`daos::obj`): object ids, types and classes, an id allocator,
  update and fetch builders with single values and records, size probes,
  conditional flags, punch, key enumeration, `query_key`.
- Key-value objects (`daos::kv`) and arrays (`daos::array`).
- Transactions (`daos::tx`): `Tx`, the `Container::transaction` closure API
  with restart, snapshot transactions.
- `Dfs::container_handle`, `Dfs::pool_handle` and `Container::mount_dfs`:
  DFS and the object API share one pool and one container connection.
- Run-time loading of libdfs; libdaos is reached through it. Symbols are
  grouped: DFS and connect symbols are required at load, the others are
  reported as `Error::MissingSymbol` at first use. `LibraryInfo` reports
  names found only without their `*2` export and missing optional symbols.
- DFS integration suite passes against DAOS 2.8.1 and DAOS 2.6.4 systems;
  the 2.6.0 client library loads (I/O not run). The object, key-value, array
  and transaction API is tested on DAOS 2.8.1 (two engines, also with
  `RP_2` classes) and 2.6.4 (one engine) with non-redundant classes;
  three-way replication and erasure-code classes are offered and untested
  on multi-engine systems.
- Explicit `unsafe fn init` with an `InitReport` of the MR cache environment.
- Typed errors with DER names; conversion to `std::io::Error` keeping errno;
  `Error::BufferTooSmall`, `Error::Unsupported`, `Error::NotUtf8`;
  `is_tx_restart`, `is_busy`, `is_no_handle`.
- `Container::class_from_name` and `Container::class_name` (object classes
  through the container's own library or backend).
- Fork guard: every use in a fork child returns `Error::Forked`.
- One DAOS system per process, enforced with `Error::Conflict`.
- Directory handle cache, checked with `dfs_ostat` before creates and
  listings so that directories removed or recreated elsewhere are noticed.
- `mock` feature with the in-memory `MemDfs` (`MemDaos`) backend for DFS
  and the libdaos layer.
- MSRV 1.77.

### Changed
- `Dfs::connect` opens the pool and container itself and mounts with
  `dfs_mount` instead of `dfs_connect`; it opens existing containers only.
  Connect errors are `Error::Daos` with `op` `daos_pool_connect` or
  `daos_cont_open`, or `Error::Dfs` with `op` `dfs_mount`.
- Read-only and read-write connections to the same pool are allowed side by
  side (separate handles); the "one access mode per pool" conflict is gone.
- `Error::Daos` has a `target` field, `Error::Busy` the fields
  `transactions` and `open_connections` (open connections no longer count as
  `in_flight`); struct-like variants are `#[non_exhaustive]`.

### Known limitations
- Key enumeration of objects of an erasure-code class returns
  `Error::Unsupported`.
- Dropping a `ReadDir` early on a directory of an erasure-code class leaves
  enumeration memory allocated inside libdfs.
- No `Container::set_label`, named snapshots, event queues.

[Unreleased]: https://github.com/potatogim/rust-daos/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/potatogim/rust-daos/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/potatogim/rust-daos/releases/tag/v0.1.0
