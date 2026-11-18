#!/bin/bash
# Start the DAOS test container and the DAOS system inside it.
#
#   ci/daos/up.sh <image>
#
# Environment (all optional):
#   DAOS_CT_NAME        container name (rust-daos-ci)
#   DAOS_CT_WORK        host directory for the file bdev and the logs
#                       ($RUNNER_TEMP/daos-ct on CI, else target/daos-ct)
#   DAOS_CT_HOST_PREP   1: raise the host's 2 MiB hugepages and set
#                       transparent hugepages to never when needed (CI sets
#                       it). The values it replaces go to $work/host-state
#                       and down.sh puts them back. Otherwise the script
#                       only checks and stops with a message.
#   DAOS_PORT, DAOS_FABRIC_PORT, DAOS_TELEMETRY_PORT, DAOS_IFACE,
#   DAOS_POOL, DAOS_CONT: passed to start.sh. The container uses the host
#                       network, so change the ports when the host runs DAOS.

set -euo pipefail

image=${1:?usage: ci/daos/up.sh <image>}
name=${DAOS_CT_NAME:-rust-daos-ci}
repo=$(cd "$(dirname "$0")/../.." && pwd)
work=${DAOS_CT_WORK:-${RUNNER_TEMP:-$repo/target}/daos-ct}
need_hugepages=1536

log() { printf '%s up.sh: %s\n' "$(date +%T)" "$*"; }

as_root() {
    if [ "$(id -u)" -eq 0 ]; then "$@"; else sudo "$@"; fi
}

free_hugepages() { awk '/^HugePages_Free:/ {print $2}' /proc/meminfo; }

# save_host_state <key> <value>: remember a host setting before changing it,
# for down.sh. A key already saved by an earlier up.sh (without down.sh in
# between) keeps its first value, which is the one from before any change.
state=$work/host-state
save_host_state() {
    mkdir -p "$work"
    if ! grep -q "^$1=" "$state" 2>/dev/null; then
        echo "$1=$2" >>"$state"
    fi
}

# Hugepages come from the host's pool even though the container mounts its
# own hugetlbfs.
free=$(free_hugepages)
if [ "$free" -lt "$need_hugepages" ]; then
    if [ "${DAOS_CT_HOST_PREP:-0}" != 1 ]; then
        log "$free free 2 MiB hugepages, DAOS needs $need_hugepages; set DAOS_CT_HOST_PREP=1 or raise vm.nr_hugepages"
        exit 1
    fi
    total=$(cat /proc/sys/vm/nr_hugepages)
    want=$((total + need_hugepages - free))
    save_host_state nr_hugepages "$total"
    log "raising vm.nr_hugepages from $total to $want (down.sh restores $total)"
    echo "$want" | as_root tee /proc/sys/vm/nr_hugepages >/dev/null
    free=$(free_hugepages)
    if [ "$free" -lt "$need_hugepages" ]; then
        log "only $free hugepages free after raising vm.nr_hugepages"
        exit 1
    fi
fi
log "$free free 2 MiB hugepages"

# DAOS 2.8 refuses to start while transparent hugepages are enabled.
thp=/sys/kernel/mm/transparent_hugepage/enabled
if [ -r "$thp" ] && ! grep -q '\[never\]' "$thp"; then
    mode=$(sed 's/.*\[\([a-z]*\)\].*/\1/' "$thp")
    if [ "${DAOS_CT_HOST_PREP:-0}" != 1 ]; then
        log "transparent hugepages are '$mode'; DAOS 2.8 needs 'never'. Set DAOS_CT_HOST_PREP=1 or run: echo never | sudo tee $thp"
        exit 1
    fi
    save_host_state thp "$mode"
    log "setting transparent hugepages from $mode to never (down.sh restores $mode)"
    echo never | as_root tee "$thp" >/dev/null
fi
log "transparent hugepages: $(cat "$thp" 2>/dev/null || echo unknown)"

# NVMe controllers on the host: the server's SPDK setup would otherwise try
# to bind every unmounted one to a user-space driver.
exclude=""
for dev in /sys/bus/pci/devices/*; do
    [ -r "$dev/class" ] || continue
    if [ "$(cat "$dev/class")" = 0x010802 ]; then
        exclude="$exclude ${dev##*/}"
    fi
done
exclude=${exclude# }
[ -z "$exclude" ] || log "excluding NVMe controllers: $exclude"

mkdir -p "$work/logs" "$work/bdev"
docker rm -f "$name" >/dev/null 2>&1 || true
docker run -d --name "$name" --init --privileged --network host \
    --ulimit memlock=-1:-1 \
    -e DAOS_BDEV_EXCLUDE="$exclude" \
    -e DAOS_PORT -e DAOS_FABRIC_PORT -e DAOS_TELEMETRY_PORT -e DAOS_IFACE \
    -e DAOS_POOL -e DAOS_CONT \
    -v "$repo:/src:ro" \
    -v "$work/logs:/var/log/daos" \
    -v "$work/bdev:/var/lib/daos-bdev" \
    "$image" sleep infinity >/dev/null
log "container $name started from $image"

docker exec "$name" /src/ci/daos/start.sh
