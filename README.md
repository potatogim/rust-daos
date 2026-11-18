# daos

A Rust client for DAOS: the DAOS File System (DFS, POSIX containers) and the
DAOS object, key-value, array and transaction APIs.

`daos` loads `libdfs.so` at run time with `dlopen` and reaches libdaos through
it. There is no build-time dependency on DAOS: the crate compiles on any Linux
(or other Unix) machine, inside manylinux wheels and on docs.rs, and reports a
clear error when the client libraries are absent.

This is an unofficial project and is not affiliated with or endorsed by the
DAOS Foundation or the daos-stack project. If the DAOS project would like to
take over the `daos` crate name, the maintainer will gladly transfer it.

Scope: DFS files and directories, and the DAOS pool, container, object,
key-value, array and transaction APIs, all with blocking calls. Both layers
share one pool and container connection when used together. The client
libraries are loaded at run time, so nothing of DAOS is needed to build. The
crate is licensed MIT OR Apache-2.0. Other Rust DAOS bindings exist
(`daos-rs`, `daoxide`); they wrap the object API with link-time bindings and
are licensed GPL-3.0.

Status: 0.1, early. The API may change in minor releases before 1.0.

Supported DAOS client libraries:

| Line | DFS | Pool, container, object, KV, array, transaction |
|---|---|---|
| 2.8.x | Tested: full integration suite against a DAOS 2.8.1 system. | Tested on DAOS 2.8.1 (two engines) with non-redundant classes and with `RP_2G1`/`RP_2GX` in a container with redundancy factor 1 at rank level. |
| 2.6.4 | Tested: full integration suite against a DAOS 2.6.4 system (2.6.4 client and server). | Tested on DAOS 2.6.4 (one engine) with non-redundant object classes. |
| 2.6.0 to 2.6.3 | Loading verified with the 2.6.0 client (all symbols resolve, `daos_init` reached; no `daos_reinit` before 2.6.4); layout verified by the ABI probe against the 2.6.0 headers; I/O not run. | Declarations and prototypes verified by the ABI probe and the header check against the 2.6.0 headers; not run. |

Three-way replication (`RP_3*`), erasure-code (`EC_*`) object classes and
redundancy at node level are offered and untested: they need at least three
engines, and the test systems have one (2.6.4) and two (2.8.1). The cases
that need several engines are in the suite and run only with `--ignored`
(see `tests/README.md`).

Differences seen between the 2.6.4 and 2.8.1 runs, none of which needed a
change in the crate: `LibraryInfo::has_connect_snap` is false on 2.6;
with `CRT_MRC_ENABLE` unset, 2.6.4 sets `FI_MR_CACHE_MAX_COUNT=0` during
`daos_init` (client MR cache off for `ofi+tcp`) while 2.8.1 leaves it unset
(cache on), so `InitReport` warns only on 2.8; and libdfs maps
`DER_INPROGRESS` to `EIO` in 2.6 (`EINPROGRESS` in 2.8) and
`DER_NOTSUPPORTED` to `EIO` in 2.6.0 (`ENOTSUP` from 2.6.4), so callers
should not tell those cases apart by errno (from `daos_der2errno` in the
headers; not triggered by the suite).

## Requirements at run time

- The `daos-client` package (it ships `libdfs.so` and `libdaos.so.2`).
- A running `daos_agent` and access to a pool (and a POSIX container for
  DFS).
- `libdfs.so` on the loader path, or its location in `RUST_DAOS_LIBDFS` or
  `InitOptions::library_path`.

Nothing is needed at build time. Targets: 64-bit Linux with glibc at run time.
A statically linked musl binary cannot `dlopen` a glibc library.

## Quick start

```rust,no_run
use std::time::Duration;

fn main() -> Result<(), daos::Error> {
    // First statement of main, before other threads exist (see "Initialisation").
    let report = unsafe { daos::init(&daos::InitOptions::default())? };
    for w in &report.warnings {
        eprintln!("daos: {w}");
    }

    let dfs = daos::Dfs::connect("tank", "data")?;
    dfs.write("/train/sample-0001.bin", &[1, 2, 3, 4])?;

    // Positional read straight into the caller's buffer, no intermediate copy.
    let file = dfs.open("/train/sample-0001.bin")?;
    let mut buf = vec![0u8; file.size()? as usize];
    file.read_exact_at(&mut buf, 0)?;

    for entry in dfs.read_dir("/train")? {
        println!("{}", entry?.path());
    }

    dfs.remove_all("/train")?;
    dfs.close(Duration::from_secs(30))?;
    Ok(())
}
```

Objects, key-value and arrays:

```rust,no_run
use daos::obj::{ClassHints, ObjectClass, ObjectType, OpenMode};

fn main() -> Result<(), daos::Error> {
    unsafe { daos::init(&daos::InitOptions::default())? };
    let cont = daos::Container::open("tank", "objects")?;
    let ids = cont.oid_allocator();
    let hints = ClassHints::default();

    let kv = cont.kv(ids.next(ObjectType::KvHashed, ObjectClass::UNKNOWN, hints)?, OpenMode::ReadWrite)?;
    kv.put(None, "model", b"resnet50")?;
    assert_eq!(kv.get(None, "model")?.as_deref(), Some(&b"resnet50"[..]));

    let obj = cont.object(ids.next(ObjectType::MultiHashed, ObjectClass::UNKNOWN, hints)?, OpenMode::ReadWrite)?;
    obj.put_single(None, b"sample-1", b"label", b"cat")?;

    let arr = cont.array_create(ids.next(ObjectType::Array, ObjectClass::UNKNOWN, hints)?, 1, 1 << 20)?;
    arr.write_at(None, 0, b"bytes")?;

    cont.transaction(daos::tx::TxMode::ReadWrite, |tx| kv.put(Some(tx), "epoch", b"2"))?;
    cont.close(std::time::Duration::from_secs(30))?;
    Ok(())
}
```

The `examples/` directory has `ls`, `cat` and `put` for DFS, and `kv`,
`object` and `array` for the object API:

```sh
cargo run --example ls -- tank data /
echo hello | cargo run --example put -- tank data /dir/hello.txt
cargo run --example cat -- tank data /dir/hello.txt
cargo run --example kv -- tank objects
```

## API overview

| Item | Purpose |
|---|---|
| `unsafe fn init(&InitOptions)` | Load libdfs, `daos_init`, `dfs_init`. Once per process. |
| `init_report()`, `library_info()`, `is_available()` | What was loaded and observed. |
| `Dfs::connect`, `Dfs::connect_with(&ConnectOptions)` | Mount a container (shared per pool and container). |
| `Dfs::open`, `create`, `open_with(&OpenOptions)` | Open files. |
| `File::read_at`, `read_exact_at`, `write_all_at`, `size`, `metadata` | Positional I/O on `&self`. |
| `Dfs::writer` / `Writer` | Sequential writer, implements `std::io::Write`. |
| `Dfs::read`, `read_range_into`, `write` | Whole-object helpers. |
| `Dfs::metadata`, `try_metadata`, `exists` | `stat`. |
| `Dfs::create_dir`, `create_dir_all`, `read_dir` | Directories. |
| `Dfs::remove_file`, `remove_dir`, `remove_dir_all`, `remove_all` | Removal. |
| `Dfs::close(timeout)` | Release every handle of the mount and unmount. |
| `Dfs::container_handle`, `pool_handle` | The connection a mount uses, for the object API. |
| `Pool::connect`, `query`, `list_containers`, `create_container`, `destroy_container` | Pools and container management. |
| `Container::open`, `query`, `properties`, `mount_dfs`, snapshots, `close` | Containers. |
| `Container::object`, `Object::update`/`fetch` builders, `put_single`, `get_single`, `dkeys`, `punch`, `query_key` | Objects (`daos::obj`). |
| `Container::kv`, `Kv::put`, `get`, `get_into`, `remove`, `keys` | Key-value objects (`daos::kv`). |
| `Container::array_create`, `array_open`, `Array::write_at`, `read_at`, `size`, `punch` | Arrays (`daos::array`). |
| `Container::tx`, `transaction`, `snapshot_tx`, `Tx::commit` | Transactions (`daos::tx`). |

Paths are absolute inside the container and are given as `&str` (UTF-8);
names that are not UTF-8 can be listed but not addressed in v0.1. They are
normalised: repeated slashes collapse, `.` is dropped, `..` resolves
lexically and never climbs above `/`. See `daos::path::normalize`. File
operations refuse a path that ends with `/`, `/.` or `/..`. `DirEntry::path`
replaces bytes that are not UTF-8 (lossy); `DirEntry::file_name` keeps the
exact bytes.

### Directory handle cache

Each mount caches up to `dir_cache_capacity` (default 1024) directory handles
from `dfs_lookup`, so that operations on `/a/b/c/f` do not resolve `/a/b/c`
every time. A handle refers to a directory object, not to its path. If another
mount or process removes that directory (and perhaps creates a new one with
the same name), libdfs still accepts creates through the old handle and the
new entries become unreachable. The crate therefore checks a cached handle
with one `dfs_ostat` (which compares the object ID of the entry) before every
create (`dfs_open` with `O_CREAT`, `dfs_mkdir`) and before `read_dir`, and
when another call through a cached handle reports `ENOENT` or `ESTALE`; a
stale handle is dropped and looked up again. Removals through the same mount
drop the affected entries directly.

Measured on a single-engine DAOS 2.8.1 test system (`ofi+tcp`, client and
server on one VM), 300 files at depth 5, per operation, with the cache versus
without it:

| Server storage | Creates | Stats | Whole-file reads |
|---|---|---|---|
| RAM SCM + file bdev | 516 to 586 us vs 884 to 1027 us | 261 to 277 us vs 759 to 852 us | 363 to 412 us vs 867 to 1058 us |
| MD-on-SSD (ram + file bdev with wal, meta, data) | 5810 us vs 6332 us | 237 us vs 817 us | 368 us vs 1063 us |
| DAOS 2.6.4, RAM SCM + file bdev | 515 us vs 1019 us | 234 us vs 853 us | 377 us vs 993 us |

Remaining limit: a rename of an ancestor directory by someone else is not
detected. The cached handle still points at the renamed directory, so paths
below the old name keep resolving to its new place until the handle leaves
the cache. Set `dir_cache_capacity` to 0 when other writers rename
directories that this process uses.

## Pools and containers

`Pool::connect` and `Container::open` share connections through a registry
keyed by the pool and container strings and the access mode, together with
`Dfs::connect`: a `Dfs`, a `Container` and the objects opened through it use
one pool handle and one container handle. Read-only and read-write requests
get separate handles. `Dfs::container_handle()` and `pool_handle()` return
the connection a mount uses; `Container::mount_dfs()` mounts DFS on an open
POSIX container.

`Pool::create_container` creates POSIX containers through libdfs (which
writes the superblock a mount needs) and other layouts through libdaos;
creation does not open the container. Unset properties are left to the
library and the pool, and their defaults differ between DAOS 2.6 and 2.8
(chunk size, default object class, checksums). `Pool::destroy_container`
refuses with `Error::Busy`, without a DAOS call, while this process has the
container open under the same strings; close your handles first.

`close(timeout)` on a `Pool`, `Container` or `Dfs` closes the connection for
every view. Closing a container closes its objects and unmounts its DFS
mounts; it returns `Error::Busy` while transactions of the container are
open or calls of its DFS mount still run at the deadline, and then changes
nothing. A pool closes only when none of its containers is open. Dropping
the last view closes quietly.

A connection is bound to the container that was opened, not to its label:
if another process renames the container, or destroys it and creates a new
one under the same label, a live connection keeps referring to the one it
opened. Label and UUID strings of one container are separate registry keys.

## Objects

An object holds dkeys; each dkey holds akeys; each akey holds a single value
or an array of fixed-size records. Opening any object id succeeds and a
missing key reads as size 0, so the getters return `Option`. Keys are
`&[u8]`; object types with integer keys take 8-byte keys built with
`daos::obj::u64_key` (the crate copies them into aligned storage). The update
and fetch builders take one dkey and up to 64 akeys per call; buffers are
borrowed and passed to libdaos without a copy, which is sound because every
call blocks and the crate never uses `DAOS_TF_ZERO_COPY`.

Things to know:

- Absent records of a record fetch leave the buffer untouched; zero it first
  when that matters.
- An empty destination buffer makes a fetch a size probe; a too small one is
  `Error::BufferTooSmall` with the needed size.
- Key enumeration order is unspecified; collect and sort when order matters.
  `query_key` finds the largest or smallest integer key. It takes integer
  keys only, so the largest record extent of a byte akey (on `DkeyUint64`,
  `KvUint64` and array objects) cannot be queried in this version.
- Keys of a `Kv` that are not UTF-8 come back from its iterators as
  `Error::NotUtf8` with the bytes; use `next_batch` or the object API for
  them.
- `ObjectClass::from_name` and `name` use the library loaded by `init`;
  `Container::class_from_name` and `class_name` use the container's own
  library or in-memory backend.
- Key enumeration of objects of an erasure-code class returns
  `Error::Unsupported` in this version (see "Known limitations").
- Object ids come from `Container::oid_allocator()` (`daos_cont_alloc_oids`
  plus `daos_obj_generate_oid`). Objects created through `Container` in a
  POSIX container are invisible to DFS, and a DFS consistency check may report
  or remove them.

## Key-value objects and arrays

`daos::kv::Kv` wraps `daos_kv_*` for `KvHashed` and `KvLexical` objects: keys
are strings without NUL. An absent key and an empty value both read as
`None`, so empty values are refused. Integer-keyed KV objects (`KvUint64`)
are used through the object API with `u64_key` and `daos::kv::KV_AKEY`.

`daos::array::Array` works in cells. A request of zero cells makes no DAOS
call. Byte arrays (`ArrayByte`) report exact read counters and zero-fill
holes; other arrays report no counters (`ArrayRead::counts()` is `None`) and
leave holes untouched. `array_open_with_attr` needs the cell and chunk size
the array was always used with; other values corrupt the view of the data
and cannot be detected.

## Transactions

`Container::transaction(mode, |tx| ...)` opens a transaction, runs the
closure, commits, and restarts on `DER_TX_RESTART` (without a limit unless
`transaction_with` sets one). Every data call takes `tx: Option<&Tx>`. A `Tx`
is neither `Send` nor `Sync`: DAOS does not support the operations of one
transaction from several threads. `Container::snapshot_tx(epoch)` reads a
snapshot made with `create_snapshot`; named snapshots are not offered (the
DAOS client returns `DER_NOSYS` for them).

## Initialisation

`daos::init` is `unsafe` because `daos_init` writes the process environment
(it applies client variables pushed by the servers and provider settings).
The caller must make sure no other thread reads or writes the environment
while it runs; the simplest way is to call it first in `main`. Hosts that
already run threads (for example a Python process) can call it at import time
of their extension module, before starting worker threads, or do DAOS I/O in a
dedicated process started with spawn.

`Dfs::connect`, `Pool::connect` and `Container::open` never initialise DAOS;
without a completed `init` they return `Error::NotInitialized`.

The crate never calls `daos_fini` or `dfs_fini`. libdaos keeps a per-thread
event for blocking calls whose "initialised" flag is thread local, and
finalisation clears it only on the calling thread; a finalised and
re-initialised library would let other threads use stale events. DAOS stays
initialised until the process exits. Closing a mount, container or pool only
returns its handles. Close them before exit; a value stored in a `static` is
never dropped, so call `close` on it yourself.

## Errors

`daos::Error` keeps the two DAOS conventions apart:

- `Error::Dfs { op, errno, path }`: a `dfs_*` call failed with an errno (DFS
  converts its internal DER codes to errno values itself).
- `Error::Daos { op, code, target }`: a libdaos call failed with a DER code,
  printed with its name and the pool or container it was about, for example
  `daos_pool_connect("tank") failed: DER_NONEXIST(-1005): The specified entity
  does not exist`. `Dfs::connect` reports pool and container errors this way
  (`op` is `daos_pool_connect` or `daos_cont_open`) and mount errors as
  `Error::Dfs { op: "dfs_mount", .. }`.
- `Error::BufferTooSmall`, `Error::Unsupported` and `Error::Busy` for the
  object API cases described above.

The struct-like variants are `#[non_exhaustive]`: match them with `..`.

`Error::raw_os_error()`, `kind()`, `is_not_found()`, `is_already_exists()` and
`is_permission_denied()` work on both. DFS reports permission problems as
`EPERM`. `From<Error> for std::io::Error` builds the `io::Error` with
`from_raw_os_error`, so `raw_os_error()` works on it; the operation name, path
and DER code stay only on `daos::Error`, so convert at the last moment.

```rust,no_run
# fn f(dfs: &daos::Dfs) -> Result<(), daos::Error> {
match dfs.metadata("/maybe") {
    Ok(md) => println!("{} bytes", md.len()),
    Err(e) if e.is_not_found() => println!("absent"),
    Err(e) => return Err(e),
}
# Ok(()) }
```

## Threads

`Dfs`, `File`, `Pool`, `Container`, `Object`, `Kv` and `Array` are
`Send + Sync`; `Writer`, `ReadDir`, `KeyIter` and `KvKeys` are `Send`; `Tx`
and everything that borrows one is neither. All calls are blocking (no DAOS
event queues). From async code, call them inside
`spawn_blocking` (or `block_in_place` when a borrowed buffer is involved).
libdfs protects mount state with its own lock and runs blocking calls on
per-thread events. Concurrent positional I/O on one `File` is fine;
overlapping writes to the same range have no defined order.

`Dfs::close(timeout)` refuses new operations from the moment it starts (they
return `Error::Closed`), waits for running operations, then releases every
open handle of the mount, including those held by `File`, `Writer` and
`ReadDir` values elsewhere; their later use returns `Error::Closed`. If
operations are still running after the timeout, `close` returns
`Error::Busy`, leaves the mount connected, and the mount accepts operations
again. If a `dfs_release` fails during close, libdfs keeps that object
allocated; the crate reports the first such error and forgets the object,
which then leaks.

## Processes

libdaos and DFS mount state do not survive `fork`: a read-write mount keeps
its object ID allocator in process memory, so a parent and a child would hand
out the same IDs. Every use of the crate in a fork child of a process that
called `init` returns `Error::Forked`, and dropping inherited handles in the
child does nothing (no locks, no library calls).

Supported models: start workers with spawn (or fork then exec), and call
`init` and `connect` in each process. Forking before `init` is fine.

## One system per process

The crate opens pool and container handles itself (`daos_pool_connect`,
`daos_cont_open`, then `dfs_mount`) and does not use libdfs's handle caches.
The first successful connect fixes the DAOS system for the process (no name
and `daos_server`, the DAOS default, are the same system); a request for
another system returns `Error::Conflict`. Read-only and read-write requests
for the same pool or container get separate handles. A pool or container
given once by label and once by UUID counts as two names. Two copies of this
crate in one process (two different versions in one dependency graph) each
have their own registry.

## Known limitations

- Key enumeration of objects of an erasure-code class (`Object::dkeys`,
  `akeys`, `Kv::keys`) returns `Error::Unsupported`. libdaos attaches memory
  to the enumeration anchor on such objects and frees it only when the
  enumeration reaches its end; there is no public call to free it when an
  iterator is dropped early. Update, fetch, punch, `query_key`, KV get and put
  and all array calls on such objects are offered.
- The same applies to `Dfs::read_dir` on a directory whose object class is an
  erasure-code class: dropping the `ReadDir` before its end leaves that memory
  allocated inside libdfs. Directories get replicated classes by default, so
  this needs an erasure-code class chosen for directories on purpose. Iterate
  such directories to the end.
- Three-way replication, erasure-code classes and redundancy at node level
  are untested (see the table above).
- Not offered: event queues and asynchronous calls, container attributes,
  ACLs, pool properties, renaming containers (`set_label`), named snapshots,
  extent maps (`daos_iom_t`), handle sharing between processes, management
  calls (pool create and destroy), `query_key` on byte akeys, conditional
  punch (`DAOS_COND_PUNCH`).

## Known issue: MR cache in multi-threaded processes

Multi-threaded processes that do not load MPI have hung or crashed inside the
fabric provider's memory-registration cache (`ofi_mr_cache_search`,
`ofi_insert_rebalance`) after freeing large buffers used for reads. This was
seen with the verbs provider and a server configuration that pushed
`CRT_MRC_ENABLE=1` to clients. Setting `CRT_MRC_ENABLE=0` avoids it, at the
cost of a memory registration per I/O.

The object and array read paths register their buffers the same way; the
real-DAOS suite repeats the stress test on `Object::read_records` and
`Array::read_at`.

The variable must be in the environment before `daos_init`, so set it in the
launcher: the shell, `mpirun -x`, a systemd unit, a container spec, or the
parent that spawns the worker. A value the launcher sets wins over a value
pushed by the servers. The crate never writes the environment;
`InitReport` shows `CRT_MRC_ENABLE` before and after `daos_init` and
`FI_MR_CACHE_MAX_COUNT` after it, and `InitReport::warnings` explains when the
cache appears enabled.

DAOS 2.6.5 and later and 2.8 enable the client MR cache by default for every
provider except verbs. The crate's stress test did not reproduce the problem
under these conditions: DAOS 2.8.1 client and single-engine server on one VM
(16 cores, 15 GiB), provider `ofi+tcp`, servers pushing no client variables,
one process with 16 threads each reading one 64 MiB file 40 times into a
freshly allocated 64 MiB buffer that is freed after each read (640 reads,
about 0.7 to 0.8 GiB/s), run once with `CRT_MRC_ENABLE=0` and once with the
variable unset (`FI_MR_CACHE_MAX_COUNT` was then unset after `daos_init`). Setting `CRT_MRC_ENABLE=0`
is recommended for multi-threaded processes on verbs and wherever the servers
push `CRT_MRC_ENABLE=1`.

## Testing

Without DAOS:

```sh
cargo test --all-features
```

This runs the unit tests (including the ABI comparison with `abi/`), the
in-memory backend tests (`--features mock`: `mem_backend` for DFS and
`mem_daos` for the libdaos layer), the dlopen tests against a C mock pair
built at test time with `cc` (`tests/mock`), and the fork and process-state
tests, each in its own subprocess. Set `RUST_DAOS_REQUIRE_MOCK=1` to fail
instead of skip when no C compiler is present.

The `mock` feature exposes `daos::mock::MemDfs` (also named `MemDaos`),
`Dfs::connect_mock`, `Pool::connect_mock` and `Container::open_mock` so that
dependent crates can test their DAOS code paths without DAOS.

Against a real DAOS system (each case in its own subprocess, working below
`/daos-crate-test` in the container, removed at the end):

```sh
CRT_MRC_ENABLE=0 DAOS_POOL=tank DAOS_CONT=posix \
    cargo test --test real_daos
```

Optional: `DAOS_SYS`, `RUST_DAOS_EXPECT_DAOS=2.6` or `2.8` (also check the
version-specific capabilities), `RUST_DAOS_REQUIRE_REAL=1` (fail when the variables are
missing), `RUST_DAOS_SHOW_OUTPUT=1`.

The libdaos layer has its own suite. It creates containers labelled
`rust-daos-test-<pid>-<case>` in `DAOS_POOL` and destroys them, also when a
case fails; its first case removes leftovers with that prefix:

```sh
CRT_MRC_ENABLE=0 DAOS_POOL=tank cargo test --test real_daos_obj
cargo test --test real_daos_obj -- --ignored    # plus the multi-engine cases
```

Header checks against a DAOS checkout:

```sh
tools/check-headers.sh ~/src/daos v2.6.4 v2.8.1      # prototype and macro snapshot
tools/abi-probe/run.sh ~/src/daos v2.6.4 v2.8.1      # sizes, offsets, constants, DER codes
```

## Scope

v0.1 covers DFS files and directories and the DAOS pool, container, object,
key-value, array and transaction APIs, with blocking calls. Not covered:
event queues and asynchronous I/O; unified namespace paths; ACLs,
ownership, xattrs, symlink creation, rename and punch in DFS; pool creation
and other management calls; handle sharing between processes; non-Linux
platforms at run time; the items under "Known limitations".

Roadmap: `read_dir_plus`, `rename`, `set_len`, vectored I/O and directory
handles; extent maps and key enumeration of erasure-code objects; container
and pool attributes; symlinks and xattrs; unified namespace paths; an
event-queue layer for asynchronous calls; fork support that reconnects in the
child; several systems per process.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Declarations transcribed from the DAOS headers are covered by the
BSD-2-Clause-Patent license of DAOS; see
[LICENSE-THIRD-PARTY](LICENSE-THIRD-PARTY).

Contributions are accepted under the terms in
[CONTRIBUTING.md](CONTRIBUTING.md), including the Developer Certificate of
Origin sign-off.
