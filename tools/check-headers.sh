#!/bin/sh
# Copyright (c) 2026 Jihyeon Gim
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Prototype snapshot check. Extracts the prototypes of every C function the
# crate calls or resolves, the dfs_filler_cb_t typedef, the bodies of the
# transcribed structures and the renaming macros (daos_pool_connect ->
# daos_pool_connect2, ...) from DAOS headers, normalises whitespace, and diffs
# the result against abi/prototypes.txt. Argument lists cannot be checked
# with sizeof, so this text comparison complements tools/abi-probe.
#
# Usage:
#   tools/check-headers.sh <daos-checkout> <tag>...     headers via git show
#   tools/check-headers.sh --dir <include-dir> <label>  headers from a directory
#   tools/check-headers.sh --print --dir <include-dir> <label>
#       print the normalised extraction (to regenerate the snapshot)
#
# Exit status: 0 when every source matches the snapshot, 1 otherwise.

set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/.." && pwd)
snapshot="$root/abi/prototypes.txt"

# file:function pairs. daos_reinit is optional (absent before DAOS 2.6.4).
# Functions renamed by a macro are listed under their header name; the
# macros themselves are checked through MACROS.
FUNCS="daos.h:daos_init daos.h:daos_fini daos.h:daos_reinit
daos_fs.h:dfs_init daos_fs.h:dfs_fini daos_fs.h:dfs_mount daos_fs.h:dfs_umount
daos_fs.h:dfs_cont_create daos_fs.h:dfs_cont_create_with_label
daos_fs.h:dfs_lookup daos_fs.h:dfs_open daos_fs.h:dfs_release daos_fs.h:dfs_read
daos_fs.h:dfs_write daos_fs.h:dfs_get_size daos_fs.h:dfs_iterate daos_fs.h:dfs_mkdir
daos_fs.h:dfs_remove daos_fs.h:dfs_stat daos_fs.h:dfs_ostat
daos_pool.h:daos_pool_connect daos_pool.h:daos_pool_disconnect daos_pool.h:daos_pool_query
daos_pool.h:daos_pool_query_target daos_pool.h:daos_pool_list_cont
daos_cont.h:daos_cont_open daos_cont.h:daos_cont_close daos_cont.h:daos_cont_create
daos_cont.h:daos_cont_create_with_label daos_cont.h:daos_cont_destroy daos_cont.h:daos_cont_query
daos_cont.h:daos_cont_alloc_oids daos_cont.h:daos_cont_create_snap
daos_cont.h:daos_cont_list_snap daos_cont.h:daos_cont_destroy_snap
daos_prop.h:daos_prop_alloc daos_prop.h:daos_prop_free
daos_obj.h:daos_obj_generate_oid daos_obj.h:daos_obj_open daos_obj.h:daos_obj_close
daos_obj.h:daos_obj_punch daos_obj.h:daos_obj_punch_dkeys daos_obj.h:daos_obj_punch_akeys
daos_obj.h:daos_obj_fetch daos_obj.h:daos_obj_update daos_obj.h:daos_obj_list_dkey
daos_obj.h:daos_obj_list_akey daos_obj.h:daos_obj_query_key
daos_obj_class.h:daos_oclass_name2id daos_obj_class.h:daos_oclass_id2name
daos_kv.h:daos_kv_open daos_kv.h:daos_kv_close daos_kv.h:daos_kv_destroy daos_kv.h:daos_kv_put
daos_kv.h:daos_kv_get daos_kv.h:daos_kv_remove daos_kv.h:daos_kv_list
daos_array.h:daos_array_create daos_array.h:daos_array_open daos_array.h:daos_array_open_with_attr
daos_array.h:daos_array_close daos_array.h:daos_array_read daos_array.h:daos_array_write
daos_array.h:daos_array_get_size daos_array.h:daos_array_set_size daos_array.h:daos_array_stat
daos_array.h:daos_array_punch daos_array.h:daos_array_destroy
daos_api.h:daos_tx_open daos_api.h:daos_tx_open_snap daos_api.h:daos_tx_commit
daos_api.h:daos_tx_abort daos_api.h:daos_tx_close daos_api.h:daos_tx_restart
daos_api.h:daos_tx_hdl2epoch"
# Structures whose text is identical in every supported tag. Structures
# whose fields were renamed at the same offset are checked by the ABI probe.
STRUCTS="gurt/types.h:d_iov_t gurt/types.h:d_sg_list_t daos_types.h:daos_anchor_t
daos_types.h:daos_obj_id_t daos_types.h:daos_handle_t daos_obj.h:daos_recx_t
daos_obj.h:daos_iod_t daos_array.h:daos_array_iod_t daos_array.h:daos_array_stbuf_t
daos_cont.h:daos_cont_info_t daos_prop.h:daos_prop_t daos_fs.h:dfs_attr_t"
# Renaming macros: a future *3 name shows up here.
MACROS="daos_pool.h:daos_pool_connect daos_cont.h:daos_cont_open daos_cont.h:daos_cont_destroy
daos_cont.h:daos_cont_create daos_obj.h:daos_obj_generate_oid"
OPTIONAL="daos_reinit"

normalise() {
	tr '\t\n' '  ' | sed -e 's/  */ /g' -e 's/( /(/g' -e 's/ )/)/g' -e 's/ ;/;/g' -e 's/^ //' -e 's/ $//'
}

# extract <reader> <label>; <reader> is a command that prints a header given
# its path relative to src/include.
extract() {
	reader=$1
	for fp in $FUNCS; do
		file=${fp%%:*}
		fn=${fp#*:}
		text=$($reader "$file" | awk -v fn="$fn" '
			$0 ~ "^" fn "\\(" { on = 1; printf "%s ", prev }
			on { print; if ($0 ~ /\);/) exit }
			{ prev = $0 }')
		if [ -z "$text" ]; then
			case " $OPTIONAL " in
			*" $fn "*) echo "$file: $fn absent (optional)" ;;
			*) echo "$file: $fn MISSING" ;;
			esac
		else
			echo "$file: $(printf '%s' "$text" | normalise)"
		fi
	done
	text=$($reader daos_fs.h | awk '/typedef int \(\*dfs_filler_cb_t\)/ { on = 1 } on { print; if ($0 ~ /;/) exit }')
	echo "daos_fs.h: $(printf '%s' "$text" | normalise)"
	for sp in $STRUCTS; do
		file=${sp%%:*}
		name=${sp#*:}
		text=$($reader "$file" | awk -v name="$name" '
			/^typedef struct/ { buf = ""; on = 1 }
			on { buf = buf "\n" $0 }
			on && $0 ~ "^} *" name ";" { print buf; exit }
			/^}/ { on = 0 }' |
			sed -e 's|/\*.*\*/||g' -e '/^[[:space:]]*\*/d' -e '/^[[:space:]]*\/\*\*/d' -e '/^[[:space:]]*$/d')
		echo "$file: $(printf '%s' "$text" | normalise)"
	done
	for mp in $MACROS; do
		file=${mp%%:*}
		name=${mp#*:}
		text=$($reader "$file" | grep -E "^#define[[:space:]]+$name[[:space:]]" || echo "MISSING #define $name")
		echo "$file: $(printf '%s' "$text" | normalise)"
	done
}

check() {
	label=$1
	out=$2
	if diff -u "$snapshot" "$out" >"$out.diff"; then
		echo "$label: matches abi/prototypes.txt"
		return 0
	fi
	# A missing optional symbol is reported, not a failure.
	if grep -v '^[-+][-+]' "$out.diff" | grep '^[-+]' | grep -qv 'daos_reinit'; then
		echo "$label: DIFFERS from abi/prototypes.txt"
		cat "$out.diff"
		return 1
	fi
	echo "$label: matches abi/prototypes.txt (optional daos_reinit absent)"
	return 0
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

print=0
if [ "${1:-}" = "--print" ]; then
	print=1
	shift
fi

status=0
if [ "${1:-}" = "--dir" ]; then
	[ $# -eq 3 ] || { echo "usage: $0 [--print] --dir <include-dir> <label>" >&2; exit 2; }
	INC=$2
	read_dir() { cat "$INC/$1"; }
	extract read_dir >"$work/out"
	if [ $print -eq 1 ]; then
		cat "$work/out"
		exit 0
	fi
	check "$3" "$work/out" || status=1
	exit $status
fi

[ $# -ge 2 ] || { echo "usage: $0 <daos-checkout> <tag>... | [--print] --dir <include-dir> <label>" >&2; exit 2; }
CHECKOUT=$1
shift
for TAG in "$@"; do
	read_git() { git -C "$CHECKOUT" show "$TAG:src/include/$1"; }
	extract read_git >"$work/out-$TAG"
	check "$TAG" "$work/out-$TAG" || status=1
done
exit $status
