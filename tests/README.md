# Tests

| Binary | Needs | What |
|---|---|---|
| unit tests (`cargo test --lib`) | nothing | Error mapping, ABI comparison with `abi/`, key encoding, builders, oid decoding |
| `mem_backend` | feature `mock` | DFS API over the in-memory backend |
| `mem_daos` | feature `mock` | Pool, container, object, KV, array and transaction API over the in-memory backend |
| `process` | feature `mock` | Fork rules, each case in a subprocess |
| `dlopen_mock` | a C compiler | The dlopen path against the C mock pair in `tests/mock` |
| `real_daos` | `DAOS_POOL`, `DAOS_CONT` (POSIX) | DFS against a real DAOS system |
| `real_daos_obj` | `DAOS_POOL` | libdaos layer against a real DAOS system; creates and destroys `rust-daos-test-*` containers |

## Real-DAOS tests on CI

The `real-daos` job in `.github/workflows/ci.yml` runs `real_daos` and
`real_daos_obj` for DAOS 2.6.4 and 2.8.1 on every push and pull request (and,
through `release.yml`, before a release). Each matrix entry starts a
single-engine DAOS system in a Rocky Linux 9 container on the runner:

- `ci/daos/Dockerfile`: DAOS server, agent, `dmg` and client packages from
  packages.daos.io (`gpgcheck=1` with the key published there, which must be
  a single key with a pinned fingerprint; the repo file is written in the
  Dockerfile, not downloaded) and a stable Rust toolchain; the base image is
  pinned by digest.
- `ci/daos/up.sh <image>`: on the host, checks for 1536 free 2 MiB hugepages
  and transparent hugepages `never` (DAOS 2.8 refuses to start otherwise) and
  stops with a message when either is missing, unless `DAOS_CT_HOST_PREP=1`
  lets it change them (CI sets it); then it starts the container (`--privileged --network host`) and runs
  `ci/daos/start.sh` in it. `start.sh` writes the configuration (one engine,
  2 targets, provider `ofi+tcp`, MD-on-SSD with a 4 GiB RAM tier and a 6 GB
  file bdev, insecure transport, VMD off), formats, waits for the rank to
  join, starts the agent and creates pool `tank` (400 MB metadata, 3 GB data)
  and POSIX container `posix`. Settings a runner needs:
  `DAOS_TARGET_OVERSUBSCRIBE=1` (the runner shows 2 cores), `DAOS_MD_CAP=256`
  (the default 1 GiB pool service blob does not fit the file bdev), 1536
  hugepages (DAOS 2.8 fails to allocate its DMA buffers with 1024), the agent
  pinned to the engine's interface (it offered `docker0` otherwise) and
  `bdev_exclude` for the host's NVMe controllers (the runner's root disk is
  NVMe).
- `ci/daos/test.sh build|real_daos|real_daos_obj`: builds and runs the
  suites in the container with `RUST_DAOS_REQUIRE_REAL=1` and
  `RUST_DAOS_EXPECT_DAOS` set from the DAOS version.
- `ci/daos/down.sh`: stops DAOS, removes the container and puts back the
  host settings `up.sh` changed (see below). On failure the job uploads the
  server, engine, agent and client logs as `daos-logs-<version>`.

What this CI does not cover: one engine with one rank, so no replication,
erasure code or rebuild; the `ofi+tcp` provider only; insecure transport;
the tests run as root and as the pool owner, so no permission-denied paths;
`CRT_MRC_ENABLE=0`; and MD-on-SSD on a file bdev only (no PMem, no real NVMe).

The image is tagged `ghcr.io/potatogim/rust-daos-ci:<DAOS version>-<hash>`,
the hash covering `ci/daos/Dockerfile`, the only input of the image
(`ci/daos/image-tag.sh`; the scripts are mounted at run time). The tag does
not pin the DAOS and EPEL packages or the Rust toolchain, which are the
current ones when the image is built; `test.sh` updates the stable toolchain
before building. Only `.github/workflows/daos-image.yml` pushes the image,
from `main`. Other runs pull that exact tag when it exists and build the
image themselves otherwise; a release always builds it from scratch. The job
runs a privileged container on the host network and changes host hugepage
settings, so it is meant for GitHub-hosted runners only, never a self-hosted
one.

### The same container locally

Needs Docker, root (or sudo) and about 8 GB of free memory. The container
uses the host network: on a host that runs its own DAOS, pick other ports.
The container is privileged and its DAOS server runs SPDK setup, which
skips the NVMe controllers `up.sh` puts in `bdev_exclude`; even so, do not
run it on a machine whose NVMe devices are in use (LVM, md, ZFS or other
mounted or claimed devices), and prefer a VM or a spare machine.

```sh
docker build --build-arg DAOS_VERSION=2.8.1 -t rust-daos-ci:2.8.1 ci/daos
# DAOS_CT_HOST_PREP=1 lets up.sh raise vm.nr_hugepages and set
# transparent hugepages to never; without it the script only checks.
# On a host with its own DAOS: DAOS_PORT=10101 DAOS_FABRIC_PORT=31516
# DAOS_TELEMETRY_PORT=9291
DAOS_CT_HOST_PREP=1 ci/daos/up.sh rust-daos-ci:2.8.1
docker exec rust-daos-ci /src/ci/daos/test.sh        # build and both suites
ci/daos/down.sh
```

Run `ci/daos/down.sh` also when `up.sh` failed: it removes a half-started
container, the file bdev and the changed host settings.

With `DAOS_CT_HOST_PREP=1`, `up.sh` writes each host setting it changes
(`vm.nr_hugepages`, transparent hugepages) with its previous value to
`target/daos-ct/host-state`, and `down.sh` writes those values back and
says so in its output. A second `up.sh` before `down.sh` keeps the first
saved values.

The file bdev and the logs go to `target/daos-ct` (`DAOS_CT_WORK`). The
container mounts its own hugetlbfs and has its own IPC namespace, so it
leaves no hugepage files or SysV shared memory on the host.

## Cases that run only with `--ignored`

`real_daos_obj` cases named `multi_engine_*` need a DAOS system with more
engines than the single-engine test system has. They are skipped unless the
binary gets `--ignored` (or `--include-ignored`):

```sh
DAOS_POOL=<pool> cargo test --test real_daos_obj -- --ignored
```

Containers of these cases use `rd_fac` 1 at rank level (the default level,
node, has a single fault domain on one host). Each case reads the engine
count of the pool (`PoolInfo::nodes`) and is skipped, with the reason, when
the pool spans fewer engines than its class needs. The runner lists skips by
reason at the end, so a skipped width is not counted as a pass.

| Case | Needs |
|---|---|
| `multi_engine_rp2_objects` | 2 engines (`RP_2G1`, `RP_2GX`) |
| `multi_engine_rp3_objects` | 3 engines (`RP_3G1`, `RP_3GX`) |
| `multi_engine_ec_objects_ec_2p1` | 3 engines (`EC_2P1G1`); key enumeration of the object and of a KV returns `Unsupported` |
| `multi_engine_ec_objects_ec_4p1` | 5 engines (`EC_4P1G1`); as above |
| `multi_engine_container_redundancy_rank` | 2 engines, `rd_fac` 1 at rank level |
| `multi_engine_container_redundancy_node` | `rd_fac` 2 at node level: 3 engines on 3 nodes; runs only with `RUST_DAOS_TEST_NODES=3` (or more) |

The `enumeration` case inserts 2,000 keys per class on 8 threads sharing one
object and times each listing pass; `RUST_DAOS_ENUM_KEYS=20000` gives the
long variant.

Kept as a note for the release that adds key enumeration of erasure-code
objects: an exact-fit `DER_KEY2BIG` on an erasure-code object (the mocks
cover the rule itself).
