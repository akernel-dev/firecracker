#!/usr/bin/env bash
# Copyright 2026 Ant Group Corporation. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

set -Eeuo pipefail

fail() {
    printf '[virtiofs-snapshot-test][error] %s\n' "$*" >&2
    exit 1
}

log() {
    printf '[virtiofs-snapshot-test] %s\n' "$*"
}

[ "$#" -eq 4 ] || fail \
    "usage: $0 FIRECRACKER VMLINUX VIRTIOFSD NEW_XFS_WORK_DIR"

firecracker="$(readlink -f "$1")"
kernel="$(readlink -f "$2")"
virtiofsd="$(readlink -f "$3")"
work_dir="$4"

[ "$(id -u)" -eq 0 ] || fail "the test requires root"
[ -c /dev/kvm ] || fail "/dev/kvm is unavailable"
[ -x "${firecracker}" ] || fail "Firecracker is not executable: ${firecracker}"
[ -f "${kernel}" ] || fail "guest kernel is missing: ${kernel}"
[ -x "${virtiofsd}" ] || fail "virtiofsd is not executable: ${virtiofsd}"
[[ "${work_dir}" = /* ]] || fail "work directory must be absolute"
[ ! -e "${work_dir}" ] || fail "work directory already exists: ${work_dir}"
[ -d "$(dirname "${work_dir}")" ] || fail "work directory parent does not exist"
[ "$(stat -f -c %T "$(dirname "${work_dir}")")" = xfs ] ||
    fail "work directory parent must be XFS"

for command_name in cpio curl findmnt gzip jq mount mountpoint sha256sum; do
    command -v "${command_name}" >/dev/null 2>&1 ||
        fail "missing command: ${command_name}"
done

mkdir "${work_dir}"
source_dir="${work_dir}/source"
shared_dir="${work_dir}/shared"
initrd_root="${work_dir}/initrd-root"
initrd="${work_dir}/initrd.img"
mkdir "${source_dir}" "${shared_dir}" "${initrd_root}"
printf 'checkpoint-marker\n' >"${source_dir}/marker"

firecracker_pids=()
virtiofsd_pids=()
shared_mounted=false

stop_process() {
    local pid="$1"
    [ "${pid}" -gt 1 ] || return 0
    if kill -0 "${pid}" 2>/dev/null; then
        kill "${pid}" 2>/dev/null || true
        for _ in $(seq 1 50); do
            kill -0 "${pid}" 2>/dev/null || break
            sleep 0.02
        done
        if kill -0 "${pid}" 2>/dev/null; then
            kill -KILL "${pid}" 2>/dev/null || true
        fi
    fi
    wait "${pid}" 2>/dev/null || true
}

cleanup() {
    set +e
    local pid
    for pid in "${firecracker_pids[@]}"; do
        stop_process "${pid}"
    done
    for pid in "${virtiofsd_pids[@]}"; do
        stop_process "${pid}"
    done
    if ${shared_mounted} && mountpoint -q "${shared_dir}"; then
        umount -l "${shared_dir}"
    fi
}
trap cleanup EXIT

mount -t tmpfs -o mode=0700,size=1m tmpfs "${shared_dir}"
shared_mounted=true
mount --make-rprivate "${shared_dir}"
mkdir "${shared_dir}/rootfs"
mount --rbind "${source_dir}" "${shared_dir}/rootfs"
mount -o remount,bind,ro,nodev "${shared_dir}/rootfs"
if touch "${shared_dir}/rootfs/host-write-must-fail" 2>/dev/null; then
    fail "staging bind is writable"
fi
printf 'host-source-stays-writable\n' >"${source_dir}/host-write"

install -D -m 0755 /usr/bin/busybox "${initrd_root}/bin/busybox"
mkdir -p \
    "${initrd_root}/dev" \
    "${initrd_root}/proc" \
    "${initrd_root}/shared" \
    "${initrd_root}/sys"
cat >"${initrd_root}/init" <<'EOF'
#!/bin/busybox sh
bb=/bin/busybox
$bb mount -t devtmpfs devtmpfs /dev
$bb mount -t proc proc /proc
$bb mount -t sysfs sysfs /sys
if ! $bb mount -t virtiofs -o ro,nodev sandboxfs /shared; then
    echo VIRTIOFS_E2E_MOUNT_FAILED
    exec $bb sh
fi
marker="$($bb cat /shared/rootfs/marker 2>/dev/null)"
if [ "$marker" != checkpoint-marker ]; then
    echo "VIRTIOFS_E2E_READ_FAILED:$marker"
    exec $bb sh
fi
echo VIRTIOFS_E2E_BOOT_OK
if $bb touch /shared/rootfs/guest-write-must-fail 2>/dev/null; then
    echo VIRTIOFS_E2E_READONLY_FAILED
    exec $bb sh
fi
echo VIRTIOFS_E2E_READONLY_OK
counter=0
after_full_seen=0
after_restore_seen=0
while true; do
    marker="$($bb cat /shared/rootfs/marker 2>/dev/null)"
    if [ "$marker" != checkpoint-marker ]; then
        echo "VIRTIOFS_E2E_RUNTIME_READ_FAILED:$marker"
        exec $bb sh
    fi
    if $bb touch /shared/rootfs/guest-write-must-fail 2>/dev/null; then
        echo VIRTIOFS_E2E_RUNTIME_READONLY_FAILED
        exec $bb sh
    fi
    if [ -f /shared/rootfs/payload-after-full ]; then
        payload="$($bb cat /shared/rootfs/payload-after-full 2>/dev/null)"
        if [ "$payload" != virtiofs-after-full-payload ]; then
            echo "VIRTIOFS_E2E_AFTER_FULL_READ_FAILED:$payload"
            exec $bb sh
        fi
        if [ "$after_full_seen" -eq 0 ]; then
            echo VIRTIOFS_E2E_AFTER_FULL_OK
            after_full_seen=1
        fi
    fi
    if [ -f /shared/rootfs/payload-after-restore ]; then
        payload="$($bb cat /shared/rootfs/payload-after-restore 2>/dev/null)"
        if [ "$payload" != virtiofs-after-restore-payload ]; then
            echo "VIRTIOFS_E2E_AFTER_RESTORE_READ_FAILED:$payload"
            exec $bb sh
        fi
        if [ "$after_restore_seen" -eq 0 ]; then
            echo VIRTIOFS_E2E_AFTER_RESTORE_OK
            after_restore_seen=1
        fi
    fi
    echo "VIRTIOFS_E2E_HEARTBEAT:$counter"
    counter=$((counter + 1))
    $bb sleep 1
done
EOF
chmod 0755 "${initrd_root}/init"
(
    cd "${initrd_root}"
    find . -print0 | cpio --null --create --format=newc --quiet
) | gzip -9 >"${initrd}"

wait_for_socket() {
    local socket="$1"
    local pid="$2"
    local log_file="$3"
    for _ in $(seq 1 300); do
        [ -S "${socket}" ] && return 0
        if ! kill -0 "${pid}" 2>/dev/null; then
            tail -100 "${log_file}" >&2 || true
            fail "process ${pid} exited before creating ${socket}"
        fi
        sleep 0.02
    done
    tail -100 "${log_file}" >&2 || true
    fail "timed out waiting for socket ${socket}"
}

wait_for_log() {
    local log_file="$1"
    local pattern="$2"
    local pid="$3"
    for _ in $(seq 1 300); do
        grep -qF "${pattern}" "${log_file}" && return 0
        if ! kill -0 "${pid}" 2>/dev/null; then
            tail -100 "${log_file}" >&2 || true
            fail "Firecracker ${pid} exited before logging ${pattern}"
        fi
        sleep 0.05
    done
    tail -100 "${log_file}" >&2 || true
    fail "timed out waiting for ${pattern} in ${log_file}"
}

wait_for_new_log_line() {
    local log_file="$1"
    local pattern="$2"
    local previous_count="$3"
    local pid="$4"
    local current_count
    for _ in $(seq 1 300); do
        current_count="$(grep -cF "${pattern}" "${log_file}" || true)"
        [ "${current_count}" -gt "${previous_count}" ] && return 0
        if ! kill -0 "${pid}" 2>/dev/null; then
            tail -100 "${log_file}" >&2 || true
            fail "Firecracker ${pid} exited before logging another ${pattern}"
        fi
        sleep 0.05
    done
    tail -100 "${log_file}" >&2 || true
    fail "timed out waiting for another ${pattern} in ${log_file}"
}

api_request() {
    local method="$1"
    local socket="$2"
    local path="$3"
    local payload="$4"
    curl --silent --show-error --fail-with-body \
        --unix-socket "${socket}" \
        -X "${method}" \
        -H 'Accept: application/json' \
        -H 'Content-Type: application/json' \
        --data "${payload}" \
        "http://localhost${path}"
}

start_virtiofsd() {
    local socket="$1"
    local log_file="$2"
    [ "${#socket}" -lt 100 ] || fail "virtiofsd socket path is too long: ${socket}"
    "${virtiofsd}" \
        --shared-dir "${shared_dir}" \
        --socket-path "${socket}" \
        --readonly \
        --no-announce-submounts \
        --sandbox namespace \
        --inode-file-handles=never \
        --migration-mode find-paths \
        --migration-on-error abort \
        >"${log_file}" 2>&1 &
    started_pid=$!
    virtiofsd_pids+=("${started_pid}")
    wait_for_socket "${socket}" "${started_pid}" "${log_file}"
}

start_firecracker() {
    local socket="$1"
    local log_file="$2"
    local instance_id="$3"
    [ "${#socket}" -lt 100 ] || fail "Firecracker socket path is too long: ${socket}"
    "${firecracker}" --api-sock "${socket}" --id "${instance_id}" \
        >"${log_file}" 2>&1 &
    started_pid=$!
    firecracker_pids+=("${started_pid}")
    wait_for_socket "${socket}" "${started_pid}" "${log_file}"
}

configure_and_start() {
    local api_socket="$1"
    local fs_socket="$2"
    api_request PUT "${api_socket}" /machine-config \
        '{"vcpu_count":1,"mem_size_mib":128,"track_dirty_pages":true}'
    api_request PUT "${api_socket}" /boot-source "$(jq -cn \
        --arg kernel "${kernel}" \
        --arg initrd "${initrd}" \
        '{kernel_image_path:$kernel,initrd_path:$initrd,boot_args:"console=ttyS0 reboot=k panic=1 pci=off nomodules rdinit=/init"}')"
    api_request PUT "${api_socket}" /fs/root "$(jq -cn \
        --arg socket "${fs_socket}" \
        '{fs_id:"root",socket_path:$socket,tag:"sandboxfs"}')"
    api_request PUT "${api_socket}" /actions \
        '{"action_type":"InstanceStart"}'
}

create_snapshot() {
    local api_socket="$1"
    local snapshot_type="$2"
    local vmstate="$3"
    local memory="$4"
    local fs_state="$5"
    api_request PATCH "${api_socket}" /vm '{"state":"Paused"}'
    api_request PUT "${api_socket}" /snapshot/create "$(jq -cn \
        --arg type "${snapshot_type}" \
        --arg vmstate "${vmstate}" \
        --arg memory "${memory}" \
        --arg fs_state "${fs_state}" \
        '{snapshot_type:$type,snapshot_path:$vmstate,mem_file_path:$memory,fs_state_path:$fs_state}')"
    [ -s "${vmstate}" ] || fail "empty VM state: ${vmstate}"
    [ -s "${memory}" ] || fail "empty memory snapshot: ${memory}"
    [ -s "${fs_state}" ] || fail "empty virtiofsd state: ${fs_state}"
}

assert_external_dirty_pages() {
    local log_file="$1"
    if ! grep -Eq \
        'supplemental ledgers: [0-9]+ KVM pages, [1-9][0-9]* external pages in [1-9][0-9]* ranges' \
        "${log_file}"; then
        tail -100 "${log_file}" >&2 || true
        fail "virtio-fs backend reported no externally dirtied guest pages"
    fi
}

load_snapshot() {
    local api_socket="$1"
    local fs_socket="$2"
    local vmstate="$3"
    local memory="$4"
    local live_memory="$5"
    local fs_state="$6"
    api_request PUT "${api_socket}" /snapshot/load "$(jq -cn \
        --arg vmstate "${vmstate}" \
        --arg memory "${memory}" \
        --arg live_memory "${live_memory}" \
        --arg fs_socket "${fs_socket}" \
        --arg fs_state "${fs_state}" \
        '{snapshot_path:$vmstate,
          mem_backend:{backend_type:"SharedFile",backend_path:$live_memory,source_path:$memory},
          track_dirty_pages:true,resume_vm:true,network_overrides:[],
          fs_override:{fs_id:"root",socket_path:$fs_socket,state_path:$fs_state}}')"
    [ -s "${live_memory}" ] || fail "SharedFile restore did not create ${live_memory}"
    [ "$(stat -c %i "${live_memory}")" != "$(stat -c %i "${memory}")" ] ||
        fail "live memory aliases checkpoint memory"
}

fs_socket_1="${work_dir}/vfs1.sock"
api_socket_1="${work_dir}/fc1.sock"
vfs_log_1="${work_dir}/virtiofsd-1.log"
fc_log_1="${work_dir}/firecracker-1.log"
vmstate_1="${work_dir}/vmstate-1"
memory_1="${work_dir}/memory-1"
fs_state_1="${work_dir}/virtiofs-1.state"

log "booting source microVM"
start_virtiofsd "${fs_socket_1}" "${vfs_log_1}"
vfs_pid_1="${started_pid}"
start_firecracker "${api_socket_1}" "${fc_log_1}" virtiofs-source
fc_pid_1="${started_pid}"
configure_and_start "${api_socket_1}" "${fs_socket_1}"
wait_for_log "${fc_log_1}" VIRTIOFS_E2E_READONLY_OK "${fc_pid_1}"
wait_for_log "${fc_log_1}" VIRTIOFS_E2E_HEARTBEAT: "${fc_pid_1}"
source_heartbeat_count="$(grep -cF VIRTIOFS_E2E_HEARTBEAT: "${fc_log_1}")"
create_snapshot "${api_socket_1}" Full "${vmstate_1}" "${memory_1}" "${fs_state_1}"
full_checkpoint_hash="$(sha256sum "${memory_1}" | awk '{print $1}')"
printf 'virtiofs-after-full-payload\n' >"${source_dir}/payload-after-full"
api_request PATCH "${api_socket_1}" /vm '{"state":"Resumed"}'
wait_for_log "${fc_log_1}" VIRTIOFS_E2E_AFTER_FULL_OK "${fc_pid_1}"
wait_for_new_log_line \
    "${fc_log_1}" VIRTIOFS_E2E_HEARTBEAT: \
    "${source_heartbeat_count}" "${fc_pid_1}"

vmstate_2="${work_dir}/vmstate-2"
memory_2="${work_dir}/memory-2"
fs_state_2="${work_dir}/virtiofs-2.state"
cp --reflink=always "${memory_1}" "${memory_2}"
source_heartbeat_count="$(grep -cF VIRTIOFS_E2E_HEARTBEAT: "${fc_log_1}")"
create_snapshot \
    "${api_socket_1}" SoftDirty "${vmstate_2}" "${memory_2}" "${fs_state_2}"
assert_external_dirty_pages "${fc_log_1}"
softdirty_checkpoint_hash="$(sha256sum "${memory_2}" | awk '{print $1}')"
api_request PATCH "${api_socket_1}" /vm '{"state":"Resumed"}'
wait_for_new_log_line \
    "${fc_log_1}" VIRTIOFS_E2E_HEARTBEAT: \
    "${source_heartbeat_count}" "${fc_pid_1}"
stop_process "${fc_pid_1}"
stop_process "${vfs_pid_1}"

fs_socket_2="${work_dir}/vfs2.sock"
api_socket_2="${work_dir}/fc2.sock"
vfs_log_2="${work_dir}/virtiofsd-2.log"
fc_log_2="${work_dir}/firecracker-2.log"
live_memory_1="${work_dir}/memory-live-1"

log "restoring the source SoftDirty checkpoint with a replacement backend"
start_virtiofsd "${fs_socket_2}" "${vfs_log_2}"
vfs_pid_2="${started_pid}"
start_firecracker "${api_socket_2}" "${fc_log_2}" virtiofs-restore-one
fc_pid_2="${started_pid}"
load_snapshot \
    "${api_socket_2}" "${fs_socket_2}" "${vmstate_2}" "${memory_2}" \
    "${live_memory_1}" "${fs_state_2}"
wait_for_log "${fc_log_2}" VIRTIOFS_E2E_HEARTBEAT: "${fc_pid_2}"
if grep -qF VIRTIOFS_E2E_RUNTIME_READONLY_FAILED "${fc_log_2}"; then
    fail "restored guest wrote through the read-only export"
fi

printf 'virtiofs-after-restore-payload\n' >"${source_dir}/payload-after-restore"
wait_for_log "${fc_log_2}" VIRTIOFS_E2E_AFTER_RESTORE_OK "${fc_pid_2}"
vmstate_3="${work_dir}/vmstate-3"
memory_3="${work_dir}/memory-3"
fs_state_3="${work_dir}/virtiofs-3.state"
cp --reflink=always "${memory_2}" "${memory_3}"
restored_heartbeat_count="$(grep -cF VIRTIOFS_E2E_HEARTBEAT: "${fc_log_2}")"
create_snapshot \
    "${api_socket_2}" SoftDirty "${vmstate_3}" "${memory_3}" "${fs_state_3}"
assert_external_dirty_pages "${fc_log_2}"
second_softdirty_checkpoint_hash="$(sha256sum "${memory_3}" | awk '{print $1}')"
api_request PATCH "${api_socket_2}" /vm '{"state":"Resumed"}'
wait_for_new_log_line \
    "${fc_log_2}" VIRTIOFS_E2E_HEARTBEAT: \
    "${restored_heartbeat_count}" "${fc_pid_2}"
stop_process "${fc_pid_2}"
stop_process "${vfs_pid_2}"

fs_socket_3="${work_dir}/vfs3.sock"
api_socket_3="${work_dir}/fc3.sock"
vfs_log_3="${work_dir}/virtiofsd-3.log"
fc_log_3="${work_dir}/firecracker-3.log"
live_memory_2="${work_dir}/memory-live-2"

log "restoring the post-restore SoftDirty checkpoint"
start_virtiofsd "${fs_socket_3}" "${vfs_log_3}"
vfs_pid_3="${started_pid}"
start_firecracker "${api_socket_3}" "${fc_log_3}" virtiofs-restore-two
fc_pid_3="${started_pid}"
load_snapshot \
    "${api_socket_3}" "${fs_socket_3}" "${vmstate_3}" "${memory_3}" \
    "${live_memory_2}" "${fs_state_3}"
wait_for_log "${fc_log_3}" VIRTIOFS_E2E_HEARTBEAT: "${fc_pid_3}"
if grep -qF VIRTIOFS_E2E_RUNTIME_READONLY_FAILED "${fc_log_3}"; then
    fail "second restored guest wrote through the read-only export"
fi

restored_full_hash="$(sha256sum "${memory_1}" | awk '{print $1}')"
restored_softdirty_hash="$(sha256sum "${memory_2}" | awk '{print $1}')"
restored_second_softdirty_hash="$(sha256sum "${memory_3}" | awk '{print $1}')"
[ "${restored_full_hash}" = "${full_checkpoint_hash}" ] ||
    fail "restore modified immutable Full checkpoint memory"
[ "${restored_softdirty_hash}" = "${softdirty_checkpoint_hash}" ] ||
    fail "restore modified immutable source SoftDirty checkpoint memory"
[ "${restored_second_softdirty_hash}" = "${second_softdirty_checkpoint_hash}" ] ||
    fail "restore modified immutable post-restore SoftDirty checkpoint memory"

log "PASS: read-only virtio-fs survived source resume and Full/SoftDirty/SoftDirty restore"
log "final checkpoint memory sha256: ${second_softdirty_checkpoint_hash}"
log "artifacts retained at ${work_dir}"
