#!/bin/bash
# Runs inside the container started by ci/daos/up.sh: writes the DAOS
# configuration, starts a single-engine server and the agent, formats the
# storage and creates the pool and the POSIX container the tests use.
#
# Environment (all optional):
#   DAOS_POOL, DAOS_CONT       pool and container labels (tank, posix)
#   DAOS_IFACE                 fabric interface (default: the default route's)
#   DAOS_PORT                  control port (10001)
#   DAOS_FABRIC_PORT           engine fabric port (31416)
#   DAOS_TELEMETRY_PORT        engine telemetry port (9191)
#   DAOS_BDEV_EXCLUDE          space-separated PCI addresses of NVMe
#                              controllers the server must leave alone

set -euo pipefail

: "${DAOS_VERSION:?the image sets DAOS_VERSION}"
pool=${DAOS_POOL:-tank}
cont=${DAOS_CONT:-posix}
port=${DAOS_PORT:-10001}
fabric_port=${DAOS_FABRIC_PORT:-31416}
telemetry_port=${DAOS_TELEMETRY_PORT:-9191}
logs=/var/log/daos
# 2.8 warns below 1536 and its engine fails to allocate DMA buffers with 1024.
hugepages=1536

log() { printf '%s start.sh: %s\n' "$(date +%T)" "$*"; }

diagnose() {
    set +e
    tail -n 60 "$logs"/daos_server.out "$logs"/daos_server.log \
        "$logs"/daos_engine.0.log "$logs"/daos_agent.out "$logs"/daos_agent.log 2>/dev/null
    timeout 30 dmg system query --verbose
    ps -eo pid,stat,rss,etime,args | grep -E 'daos|PID' | grep -v grep
    grep -E '^(MemFree|MemAvailable|HugePages_Total|HugePages_Free)' /proc/meminfo
    dmesg 2>/dev/null | tail -n 30
    set -e
}

# Processes that must stay up while wait_for polls (daos_server, then also
# daos_agent).
watch_pids=()

# wait_for <seconds> <what> <command...>: poll every 2 s until the command
# succeeds; fail with the log tails after the timeout or as soon as a
# process in watch_pids has exited.
wait_for() {
    local limit=$1 what=$2 pid
    shift 2
    local start=$SECONDS
    until "$@" >/dev/null 2>&1; do
        for pid in "${watch_pids[@]}"; do
            if ! kill -0 "$pid" 2>/dev/null; then
                log "process $pid exited while waiting for: $what"
                diagnose
                return 1
            fi
        done
        if ((SECONDS - start >= limit)); then
            log "timed out after ${limit} s waiting for: $what"
            diagnose
            return 1
        fi
        sleep 2
    done
    log "$what (${SECONDS}s since start)"
}

# retry <attempts> <delay> <command...>
retry() {
    local attempts=$1 delay=$2 n=1
    shift 2
    until "$@"; do
        if ((n >= attempts)); then
            log "giving up after $n attempts: $*"
            return 1
        fi
        log "attempt $n failed, retrying in ${delay} s: $*"
        n=$((n + 1))
        sleep "$delay"
    done
}

# The interface is the token after "dev" in the default route.
iface=${DAOS_IFACE:-$(ip -o -4 route show default |
    awk '{for (i = 1; i < NF; i++) if ($i == "dev") {print $(i + 1); exit}}')}
if [ -z "$iface" ]; then
    log "no IPv4 default route; set DAOS_IFACE"
    exit 1
fi
addr=$(ip -o -4 addr show dev "$iface" 2>/dev/null |
    awk '{split($4, a, "/"); print a[1]; exit}' || true)
if [ -z "$addr" ]; then
    log "no IPv4 address on interface '$iface'"
    exit 1
fi
log "DAOS $DAOS_VERSION on $iface ($addr), control port $port"

# A hugetlbfs instance private to this container: SPDK's hugepage files go
# away with the container instead of staying in the host's /dev/hugepages.
mkdir -p /dev/hugepages
mountpoint -q /dev/hugepages || mount -t hugetlbfs nodev /dev/hugepages

# 2.8 renamed access_points to mgmt_svc_replicas and turned NVMe hotplug on
# by default (2.6 has it off and does not know disable_hotplug).
case $DAOS_VERSION in
2.6.*)
    replicas_key=access_points
    hotplug_line=""
    ;;
*)
    replicas_key=mgmt_svc_replicas
    hotplug_line="disable_hotplug: true"
    ;;
esac
exclude_line=""
if [ -n "${DAOS_BDEV_EXCLUDE:-}" ]; then
    # Quoted: a PCI address has colons.
    exclude_line="bdev_exclude: [$(printf '"%s", ' $DAOS_BDEV_EXCLUDE | sed 's/, $//')]"
fi

mkdir -p /etc/daos "$logs" /var/lib/daos-bdev
# MD-on-SSD with a RAM tier and a 6 GB file bdev holding all three roles.
# Two targets: the per-target meta blob and reservations leave no usable
# space on a small file with more targets.
cat >/etc/daos/daos_server.yml <<EOF
name: daos_server
$replicas_key: ["$addr"]
port: $port
provider: ofi+tcp
socket_dir: /var/run/daos_server
transport_config:
  allow_insecure: true
control_log_mask: INFO
control_log_file: $logs/daos_server.log
helper_log_file: $logs/daos_server_helper.log
telemetry_port: $telemetry_port
system_ram_reserved: 2
nr_hugepages: $hugepages
# The file bdev needs no VMD; keeps SPDK away from VMD domains on the host.
disable_vmd: true
$hotplug_line
$exclude_line
control_metadata:
  path: /var/db/daos_server
engines:
- targets: 2
  nr_xs_helpers: 0
  fabric_iface: $iface
  fabric_iface_port: $fabric_port
  log_mask: ERR
  log_file: $logs/daos_engine.0.log
  env_vars:
  - CRT_MRC_ENABLE=0
  # GitHub runners show 4 vCPUs as 2 cores; the engine reserves cores for
  # its system service and refuses 2 targets without this.
  - DAOS_TARGET_OVERSUBSCRIBE=1
  # Pool service metadata (rdb-pool) is 1 GiB per pool by default; on this
  # small meta device that fails with DER_NOSPACE.
  - DAOS_MD_CAP=256
  storage:
  - class: ram
    scm_mount: /mnt/daos
    scm_size: 4
  - class: file
    bdev_list: [/var/lib/daos-bdev/bdev0]
    bdev_size: 6
    bdev_roles: [wal, meta, data]
EOF

cat >/etc/daos/daos_agent.yml <<EOF
name: daos_server
access_points: ["$addr"]
port: $port
runtime_dir: /var/run/daos_agent
# Without this the agent hands clients docker0 (the host network has it).
fabric_ifaces:
- numa_node: 0
  devices:
  - iface: $iface
    domain: $iface
transport_config:
  allow_insecure: true
log_file: $logs/daos_agent.log
EOF

cat >/etc/daos/daos_control.yml <<EOF
name: daos_server
port: $port
hostlist: ["$addr"]
transport_config:
  allow_insecure: true
EOF

log "starting daos_server"
setsid daos_server start -o /etc/daos/daos_server.yml >"$logs/daos_server.out" 2>&1 </dev/null &
watch_pids+=($!)

wait_for 120 "control plane answers" timeout 30 dmg storage scan
retry 3 10 timeout 120 dmg storage format
joined() { timeout 30 dmg system query 2>/dev/null | grep -q Joined; }
wait_for 180 "rank joined" joined
timeout 30 dmg system query --verbose

log "starting daos_agent"
setsid daos_agent -o /etc/daos/daos_agent.yml >"$logs/daos_agent.out" 2>&1 </dev/null &
watch_pids+=($!)
wait_for 60 "agent socket" test -S /var/run/daos_agent/daos_agent.sock

timeout 60 dmg storage query usage

pool_exists() { timeout 30 dmg pool list 2>/dev/null | grep -qw "$pool"; }
create_pool() {
    pool_exists && return 0
    timeout 180 dmg pool create --scm-size=400M --nvme-size=3G "$pool"
}
retry 3 15 create_pool
timeout 60 dmg pool query "$pool"

retry 3 10 timeout 120 daos cont create "$pool" "$cont" --type POSIX
timeout 60 daos cont query "$pool" "$cont"
log "ready: DAOS_POOL=$pool DAOS_CONT=$cont"
