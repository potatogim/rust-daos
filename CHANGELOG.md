# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Versions before 1.0 may change the API in minor releases. MSRV bumps are a
minor-version change and are listed here.

## [Unreleased]
### Fixed
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

### Changed (relative to earlier development snapshots)
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

[Unreleased]: https://github.com/potatogim/rust-daos/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/potatogim/rust-daos/releases/tag/v0.1.0
