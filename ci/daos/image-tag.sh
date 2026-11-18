#!/bin/bash
# Print the tag of the CI image for a DAOS version:
#   <DAOS version>-<first 12 hex digits of the SHA-256 of ci/daos/Dockerfile>
# The Dockerfile is the only input of the image (it copies nothing from the
# build context; the scripts are mounted at run time), so a change to it gives
# a new tag and a change to the scripts does not. The tag does not pin what
# the build downloads: DAOS and EPEL packages and the Rust toolchain are the
# current ones at build time, so two builds of one tag can differ.
#
#   ci/daos/image-tag.sh 2.8.1

set -euo pipefail

version=${1:?usage: ci/daos/image-tag.sh <DAOS version>}
dir=$(cd "$(dirname "$0")" && pwd)
hash=$(sha256sum "$dir/Dockerfile" | cut -c1-12)
echo "$version-$hash"
