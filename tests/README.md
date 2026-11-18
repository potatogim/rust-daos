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

## Cases that run only with `--ignored`

`real_daos_obj` cases named `multi_engine_*` need a DAOS system with more
engines than the single-engine test system has. They are skipped unless the
binary gets `--ignored` (or `--include-ignored`):

```sh
DAOS_POOL=<pool> cargo test --test real_daos_obj -- --ignored
```

| Case | Needs |
|---|---|
| `multi_engine_rp2_objects` | 2 engines (`RP_2G1`, `RP_2GX`; container `rd_fac` 1 at rank level) |
| `multi_engine_rp3_objects` | 3 engines (`RP_3G1`, `RP_3GX`) |
| `multi_engine_ec_objects` | 3 engines for `EC_2P1`, 5 for `EC_4P1`; checks that key enumeration returns `Unsupported` |
| `multi_engine_container_redundancy` | 3 engines on 3 nodes for `rd_fac` 2 at node level |

Kept as a note for the release that adds key enumeration of erasure-code
objects: an exact-fit `DER_KEY2BIG` on an erasure-code object (the mocks
cover the rule itself).
