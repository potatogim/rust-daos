#!/bin/sh
# Copyright (c) 2026 Jihyeon Gim
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Compile tools/abi-probe/probe.c against real DAOS headers and write
# abi/layout-<label>.txt.
#
# Usage:
#   tools/abi-probe/run.sh <daos-checkout> <tag>...
#       headers are taken from the checkout with `git archive <tag> src/include`
#   tools/abi-probe/run.sh --dir <include-dir> <label>
#       headers are taken from an include directory (for example /usr/include
#       of an installed daos-devel package, or src/include of an extracted tag)
#
# Needs only a C compiler and the uuid development header; no DAOS build.

set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
cc=${CC:-cc}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

probe_dir() {
	inc=$1
	label=$2
	gen="$work/gen-$label"
	mkdir -p "$gen"
	# daos_version.h is generated at DAOS build time; provide it from the
	# template when the include directory has only the .in file.
	if [ ! -f "$inc/daos_version.h" ] && [ -f "$inc/daos_version.h.in" ]; then
		sed 's/@[A-Za-z_]*@/0/g' "$inc/daos_version.h.in" >"$gen/daos_version.h"
	fi
	"$cc" -std=gnu11 -Wall -Werror -I"$gen" -I"$inc" -o "$work/probe-$label" "$here/probe.c"
	"$work/probe-$label" >"$root/abi/layout-$label.txt"
	echo "wrote abi/layout-$label.txt ($(wc -l <"$root/abi/layout-$label.txt") lines)"
}

if [ "${1:-}" = "--dir" ]; then
	[ $# -eq 3 ] || { echo "usage: $0 --dir <include-dir> <label>" >&2; exit 2; }
	probe_dir "$2" "$3"
	exit 0
fi

[ $# -ge 2 ] || { echo "usage: $0 <daos-checkout> <tag>... | --dir <include-dir> <label>" >&2; exit 2; }
checkout=$1
shift
for tag in "$@"; do
	mkdir -p "$work/$tag"
	git -C "$checkout" archive "$tag" src/include | tar -x -C "$work/$tag"
	probe_dir "$work/$tag/src/include" "$tag"
done
