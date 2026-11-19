#!/bin/bash
# Runs inside the container: builds and runs the real-DAOS test suites.
#
#   ci/daos/test.sh build              compile both test binaries and the
#                                      unit tests
#   ci/daos/test.sh fork               run the fork detection unit tests
#   ci/daos/test.sh real_daos          run one suite
#   ci/daos/test.sh real_daos_obj
#   ci/daos/test.sh                    all of the above

set -euo pipefail

: "${DAOS_VERSION:?the image sets DAOS_VERSION}"
# The checkout is mounted read-only and has no Cargo.lock; build from a copy.
# A fresh copy each time, so files deleted from the checkout do not linger
# (the build output is in /build/target and stays).
rm -rf /build/src
mkdir -p /build/src
tar -C /src --exclude=./target --exclude=./.git -cf - . | tar -C /build/src -xf -
cd /build/src
export CARGO_TARGET_DIR=/build/target
export CRT_MRC_ENABLE=0
export DAOS_POOL=${DAOS_POOL:-tank}
export DAOS_CONT=${DAOS_CONT:-posix}
export RUST_DAOS_REQUIRE_REAL=1
# 2.6.4 -> 2.6, 2.8.1 -> 2.8
export RUST_DAOS_EXPECT_DAOS=${DAOS_VERSION%.*}
export D_LOG_FILE=/var/log/daos/client.log
export D_LOG_MASK=${D_LOG_MASK:-ERR}
# The fork detection must use the page the kernel clears in children.
export RUST_DAOS_REQUIRE_WIPE=1

# The image is rebuilt only when its Dockerfile changes, so its stable
# toolchain can be months old; the other CI jobs use the current stable.
build() {
    rustup update --no-self-update stable
    cargo test --no-run --lib --test real_daos --test real_daos_obj
}
fork() { cargo test --lib fork::tests::; }
suite() { cargo test --test "$1"; }

case ${1:-all} in
build) build ;;
fork) fork ;;
real_daos | real_daos_obj) suite "$1" ;;
all)
    build
    fork
    suite real_daos
    suite real_daos_obj
    ;;
*)
    echo "unknown step: $1" >&2
    exit 2
    ;;
esac
