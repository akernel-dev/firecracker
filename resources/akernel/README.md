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

## Opt-in PVM guest bundle

The default `kvm` profile keeps the Amazon Linux 6.1 guest source and
configuration. Select `pvm` explicitly to build the tested PVM 6.12 guest
instead. `pvm-kernel-versions.env` pins the `virt-pvm/linux` source commit and
archive checksum; `kernel/pvm-guest.config` contains the resolved guest
configuration. The builder merges the common AKernel fragment and verifies its
required built-in filesystem, network, and virtio options as well as
`PVM_GUEST`, `X86_PIE`, and Intel memory protection keys. EROFS LZMA and FUSE
DAX remain disabled.

```sh
AKERNEL_KERNEL_PROFILE=pvm AKERNEL_BUILD_JOBS=8 \
    resources/akernel/build-runtime-bundle.sh \
    v1.16.1-akernel.4 /path/to/new-pvm-candidate
```

Use an unused planned release tag; the tag above is an example, not an existing
release pin. The checkout must have its changes committed before packaging so
the manifest identifies the source that built the VMM. To reuse a downloaded
source archive, set `AKERNEL_KERNEL_SOURCE_ARCHIVE=/path/to/archive.tar.gz`; the
pinned SHA-256 is always verified, including for local archives.
`AKERNEL_BUILD_JOBS` limits kernel compiler parallelism and must be a positive
integer.

The candidate workflow exposes the same `kernel_profile` choice and records it
in the manifest. Promotion verifies the manifest inside the archive matches the
accompanying manifest and publishes the tested bytes without rebuilding. KVM and
PVM candidates must have distinct release tags; do not overwrite or repin the
default runtime merely to enable PVM. Verify both the archive checksum and the
archive's `SHA256SUMS`, then test that exact candidate with the consuming
sandboxd and AKernel revision before promotion.

This is a guest bundle. A PVM node separately requires the matching PVM host
kernel and vendor module, the supported host CPU features, and host boot
settings. It does not provision host kernels, unload modules, reboot nodes, or
provide TSC scaling. Hardware KVM and PVM checkpoints carry different vCPU state
and cannot be interchanged. The sandboxd `firecracker-pvm` runtime profile owns
backend detection and restore restrictions; use its deployment documentation and
validate the target host before advertising the runtime. Build the guest-agent
initrd from the consuming sandboxd revision, as with the default bundle.

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
