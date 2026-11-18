# Contributing

Thank you for helping. This file says how contributions are accepted and how
to run the checks.

## License of contributions

By submitting a contribution you agree that it is licensed under
MIT OR Apache-2.0, and you additionally consent to the project
relicensing your contribution under the BSD-2-Clause-Patent license, so
that the code can be offered to the DAOS project.

## Sources you may use

This crate is written from the DAOS headers and sources only
(https://github.com/daos-stack/daos). Do not read,
copy or adapt code from other Rust DAOS bindings (for example `daos-rs`,
`daoxide`, `daos-rust-bind`), which are licensed GPL-3.0, when you work on
this crate. Say in a pull request which DAOS files and tags you used.

## Developer Certificate of Origin

Every commit must carry a sign-off line certifying the
[Developer Certificate of Origin 1.1](https://developercertificate.org/):

```
Signed-off-by: Your Name <you@example.com>
```

`git commit -s` adds it. This applies from the first commit of the
repository; CI rejects pull requests with unsigned commits.

## Checks to run locally

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
RUST_DAOS_REQUIRE_MOCK=1 cargo test --all-features   # unit, mem backends, C mock, subprocess tests
cargo test --no-default-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
cargo +1.77 check --all-features --all-targets     # MSRV
```

With a DAOS system:

```sh
CRT_MRC_ENABLE=0 DAOS_POOL=<pool> DAOS_CONT=<posix container> \
    RUST_DAOS_REQUIRE_REAL=1 cargo test --test real_daos
# creates and destroys rust-daos-test-* containers in <pool>
CRT_MRC_ENABLE=0 DAOS_POOL=<pool> RUST_DAOS_REQUIRE_REAL=1 \
    cargo test --test real_daos_obj
```

CI runs both suites against DAOS 2.6.4 and 2.8.1 in a container (job
`real-daos`); [tests/README.md](tests/README.md#real-daos-tests-on-ci) says
how it works and how to start the same container locally.

The C mock (`tests/mock`) needs a C compiler (`cc`, or `CC`).

## Changes to the FFI declarations

Any new or changed declaration transcribed from DAOS headers needs, in the
pull request text, the header `file:line` at a 2.6 tag and at a 2.8 tag. Update
in the same pull request:

- `abi/prototypes.txt` (`tools/check-headers.sh --print --dir <include> <tag>`),
- the ABI probe and its outputs (`tools/abi-probe/run.sh`, `abi/layout-*.txt`),
- `abi/symbols.txt` when a symbol is added to the `raw_api!` list,
- the C mock (`tests/mock/libdfs_mock.c`, `tests/mock/libdaos_mock.c`),
- `src/der_table.rs` and `abi/der2errno-*.txt` when error codes change
  (`tools/gen-der-table.sh`).

## Style

- No `unsafe` block without a `// SAFETY:` comment; the crate denies
  `unsafe_op_in_unsafe_fn`.
- Source files start with the copyright and SPDX lines used in `src/lib.rs`;
  files with transcribed declarations add the DAOS notice line.
- Public API changes are noted in `CHANGELOG.md`.
