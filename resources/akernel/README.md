# AKernel Firecracker runtime bundle

This directory contains the AKernel Firecracker release inputs: the fork built
VMM, the customized guest kernel, and the packaging script that assembles them
into a checksum-pinned runtime bundle.

The VMM is built from this repository (`build-runtime-bundle.sh` runs
`cargo build --release --target x86_64-unknown-linux-musl -p firecracker`),
because the AKernel snapshot APIs (`snapshot_type`, `deferred_sync`,
`mem_backend: Uffd`) and migration-capable virtio-fs frontend only exist in this
fork. The bundle manifest records `vmm.source: fork-source-build`, the exact
source commit, and the binary digest. Only the guest kernel is built separately
from the Amazon Linux sources.

`runtime-versions.env` is the single source of truth for the upstream VMM
version and the Amazon Linux kernel inputs. `kernel/akernel.config` is appended
to Firecracker's upstream 6.1 guest configuration before `make olddefconfig`.

Candidate and promotion workflows deliberately form a two-step release:

1. `AKernel Firecracker candidate` builds and uploads an expiring artifact.
1. The candidate is tested with sandboxd and AKernel.
1. `Promote AKernel Firecracker candidate` publishes those exact bytes without
   rebuilding them.

Release tags use `vX.Y.Z-akernel.N`. A release archive contains the fork built
VMM, the AKernel guest kernel, the resolved kernel configuration, licenses,
checksums, and a provenance manifest. The sandboxd-coupled guest agent and
initrd are intentionally excluded.

Before promotion, run the privileged virtio-fs checkpoint test on an XFS host
with KVM. It builds a small BusyBox initramfs, verifies the host and guest
exports are read-only, and forces virtio-fs I/O after each snapshot boundary. It
takes a Full snapshot followed by a SoftDirty snapshot, restores with a
replacement virtiofsd and `SharedFile` memory, then takes and restores another
SoftDirty generation. The test also requires nonzero vhost-user dirty pages. The
new work directory must not already exist; test artifacts are retained for
inspection.

```sh
resources/akernel/test-virtiofs-snapshot.sh \
    /path/to/firecracker \
    /path/to/vmlinux \
    /path/to/virtiofsd \
    /xfs/path/new-test-directory
```

## Writable virtio-fs host directories

The existing frontend can transport writable filesystem requests; its
[`FsConfig`](../../src/vmm/src/vmm_config/virtio_fs.rs) selects the backend
socket and guest tag, not a read-only or read-write policy. Enabling writable
host directories does not require a new VMM API or runtime bundle. The companion
[sandboxd change](https://github.com/inclusionAI/sandboxd/pull/57) configures
host exports, virtiofsd, and its separately built guest agent/initrd.

Access control belongs to those layers:

- The host export must allow writes for an explicitly writable directory. Image
  roots and read-only exports must remain protected on the host, including
  nested mounts; guest mount flags alone are insufficient protection against a
  guest remounting its view.
- A virtiofsd serving writable exports cannot use the global `--readonly` flag.
  With mixed RO/RW exports, enforce each export's permissions on its host
  staging mount and protect the staging directory layout. Keep `--readonly` for
  an entirely read-only export.
- The guest agent must mount the shared filesystem and requested writable bind
  views with write access. Retain the default write-through policy rather than
  enabling virtiofsd `--writeback` for directories also accessed on the host.

### Local checkpoint and restore

The VMM snapshot and virtiofsd device-state sidecar preserve execution and
device state. They do not snapshot the contents of an external host directory.
Local restore therefore requires the original backing directories and referenced
files, with the same export layout and access modes. The orchestrator owns their
retention policy; deleting a sandbox need not delete caller-owned host data.

Do not interpret restoring virtiofsd state as recreating files that disappeared
after checkpoint, restoring into an empty directory, or starting a new log
segment. Those behaviors require additional filesystem recovery semantics.
Cross-node data migration is also outside this contract. For ordinary append
logs, use `O_APPEND`: non-append writes retain their saved offsets and may
overwrite data written after checkpoint. Replayed application execution can
still produce duplicate logs.

The sandboxd integration was tested with the existing `v1.16.1-akernel.3` bundle
and virtiofsd 1.14.0 on Linux 6.8, with both EROFS and directory roots. Coverage
included mixed RO/RW protection, host/guest file visibility, rotation of an open
append FD, local checkpoint/delete/restore, preserved read offsets, append after
host-side writes, daemon crash recovery, and rejection of missing backing files.
The RW scenario lives in sandboxd's `test/e2e/firecracker-rw.sh`; the release
test above remains a read-only frontend snapshot regression. No VMM or virtiofsd
binary was modified for this integration.
