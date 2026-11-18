# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Versions before 1.0 may change the API in minor releases. MSRV bumps are a
minor-version change and are listed here.

## [Unreleased]

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
