#!/bin/bash
# Stop and remove the DAOS test container. Safe to run when up.sh failed
# half way, and after a failed up.sh it is the way to clean up. Puts back the
# host settings that up.sh changed with DAOS_CT_HOST_PREP=1 (listed in
# $DAOS_CT_WORK/host-state). Leaves the logs in $DAOS_CT_WORK/logs.

set -uo pipefail

name=${DAOS_CT_NAME:-rust-daos-ci}
repo=$(cd "$(dirname "$0")/../.." && pwd)
work=${DAOS_CT_WORK:-${RUNNER_TEMP:-$repo/target}/daos-ct}

log() { printf '%s down.sh: %s\n' "$(date +%T)" "$*"; }

as_root() {
    if [ "$(id -u)" -eq 0 ]; then "$@"; else sudo "$@"; fi
}

if docker inspect "$name" >/dev/null 2>&1; then
    timeout 180 docker exec "$name" sh -c '
        timeout 30 dmg system query --verbose >/var/log/daos/system_query.txt 2>&1
        timeout 30 dmg pool list --verbose >/var/log/daos/pool_list.txt 2>&1
        timeout 60 dmg system stop --force
        pkill daos_agent
        sleep 2
        pkill daos_server
        sleep 3
        rm -f /var/lib/daos-bdev/bdev0
        chmod -R a+rX /var/log/daos' || true
    docker rm -f "$name" >/dev/null
    log "container $name removed"
fi
rm -rf "$work/bdev" 2>/dev/null || true

# Host settings changed by up.sh, restored now that the container is gone.
state=$work/host-state
if [ -f "$state" ]; then
    failed=0
    while IFS='=' read -r key value; do
        case $key in
        nr_hugepages) file=/proc/sys/vm/nr_hugepages ;;
        thp) file=/sys/kernel/mm/transparent_hugepage/enabled ;;
        *)
            log "unknown entry in $state: $key=$value"
            failed=1
            continue
            ;;
        esac
        if echo "$value" | as_root tee "$file" >/dev/null; then
            log "restored $file to $value"
        else
            log "could not restore $file to $value"
            failed=1
        fi
    done <"$state"
    if [ "$failed" = 0 ]; then
        rm -f "$state"
    else
        log "kept $state; restore the rest by hand"
    fi
else
    log "no host settings to restore"
fi

# The container had its own hugetlbfs mount and IPC namespace, so nothing
# should be left; print the host state for the log.
awk '/^HugePages_(Total|Free):/' /proc/meminfo
log "SysV shared memory segments on the host: $(($(wc -l </proc/sysvipc/shm) - 1))"
