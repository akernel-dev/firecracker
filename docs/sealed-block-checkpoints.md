# Incremental checkpoints with a sealed virtio-blk upper

This experimental backend targets a single writable Async drive. A restore reads
untouched sectors on demand from an immutable JuiceFS disk. All guest writes
land on XFS. The host resumes the guest before scanning the sealed dirty bank,
compacting the local view, or publishing disk and memory to shared storage.

## How a generation is captured

1. Restore B from G0's vmstate, memory file and read-only disk. Keep a
   generation stamp per 512-byte sector. A completed write stamps that sector
   and marks it in the current dirty bank. Failed or short writes invalidate
   the drive for capture. A zero overwrite is a modification. Guest filesystem
   hole punching is preserved through the filesystem's own metadata writes.
   The Async device does not advertise block DISCARD.
2. Before pausing, prepare an empty local upper and clean mark bank. Only one
   outstanding capture is admitted. Pause vCPUs, drain and fsync the device,
   save vmstate and a sparse KVM dirty-memory diff on local storage, advance
   the write generation, then resume. Local capture and resume are synchronous.
3. In the background, clone the previous cumulative local view into the
   inactive cumulative file with XFS FICLONE. Coalesce dirty sectors and apply
   aligned interiors with FICLONERANGE. Copy only the modified bytes at
   unaligned edges. Fsync before advancing the merged-generation word. Reads
   resolve to the live upper, sealed upper, committed cumulative file, or G0.
   Pending reads keep their registered file slot until the next drain.
   Three upper slots plus two cumulative slots bound local disk file count.
   A 2 GiB disk requires 16 MiB of sector stamps and 8 MiB of mark banks.
4. In parallel, build G1 in a temporary directory on a writable JuiceFS mount.
   Use same-filesystem copy_file_range to clone G0's disk and memory; pack
   frozen dirty sectors and allocated extents of the sparse RAM diff, then
   overlay the ranges from each pack. Copy vmstate, sync and rename the
   directory. G2 uses published G1 as its parent; B keeps reading untouched
   sectors from G0. C opens just its chosen complete generation. JuiceFS
   compaction and object traffic depend on the mount and chunk layout.

## Running the reference coordinator

Use a trusted private workspace on reflink-enabled XFS, an immutable generation
visible on both a read-only JuiceFS mount and a writable publisher mount, and
Linux Python 3.10+ with libatomic. Use the same G0 on both mounts. Configure
the snapshot's drive path as `overlay.ext4`, io_engine Async, cache Writeback,
and enable KVM dirty tracking on restore. Start the VMM with the workspace
as its working directory. Expose all paths inside the VMM's filesystem namespace
when using a jail. Example host commands:

```sh
python3 tools/sealed-block/checkpoint.py init /xfs/B /ro/G0/overlay.ext4 /pub/G0
(cd /xfs/B && FC_SEALED_BLOCK_PATH=overlay.ext4 firecracker --api-sock /xfs/B/api)
# From another shell, load /ro/G0/vmstate and /ro/G0/memory via /snapshot/load.
python3 tools/sealed-block/checkpoint.py capture /xfs/B /xfs/B/api /pub/G1
python3 tools/sealed-block/checkpoint.py capture /xfs/B /xfs/B/api /pub/G2
python3 tools/sealed-block/checkpoint.py init /xfs/C /ro/G2/overlay.ext4 /pub/G2
```

The capture command waits for publication and local merge while the resumed
guest runs. A concurrent capture receives a busy error before pause. If a
post-resume job fails, retain the workspace and run `finish /xfs/B` using
the same script. It checks the pending generation and publication identity
before retrying. A pre-cut failure, ambiguous pause/resume response, or lost
VMM requires operator recovery. Restart from a committed remote generation
in a fresh workspace. Live recovery after host power loss, sandboxd lifecycle
integration, a versioned control API, multi-drive coordination and adaptive
merge scheduling are follow-up work.

## Performance and choice of backend

The following are two separate three-round experiments on one Hong Kong
bare-metal host. Each VM had 2 vCPUs, 1 GiB RAM and a 2 GiB ext4 drive. A, B
and C were independent VMMs in an isolated Pod. JuiceFS 1.4.1, Redis and the
S3 service shared that host. The results exercise the storage protocol;
deployment across physical hosts needs further testing. Durations are
medians in ms. Total pause runs from issuing pause through resume returning.
Stage medians are computed independently.

Under a running writer (at least 128 MiB dirtied), the preliminary prototypes
gave the following breakdown.

| Backend | Device fsync | vmstate | RAM diff | Disk freeze | Total pause |
| --- | ---: | ---: | ---: | ---: | ---: |
| Host FUSE | 58.5 | 0.42 | 53.5 | 18.67 | 132.6 |
| Native full XFS clone | 58.0 | 0.39 | 63.3 | 12.51 | 135.2 |
| Native seal and rotate (fixed number of generations) | 58.3 | 0.38 | 62.8 | 0.16 | 122.9 |

For a long-lived B, option 1 clones the entire cumulative upper while paused,
then rotates between two slots. Option 2 seals immediately, then merges in
the background into bounded storage. In the comparison below G1 wrote 32 MiB
sequentially after random writes; G2 scattered 32 MiB across about 8,194
ranges. Both also dirtied 32 MiB of application RAM per capture.

| Option | G1 pause | G2 pause | G2 device fsync | G2 RAM diff | G2 disk cut | Async local merge | Async G2 publish |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1. Clone then rotate | 70.89 | 482.14 | 287.56 | 30.95 | 161.823 | — | 2,283 |
| 2. Rotate then merge | 47.06 | 262.83 | 228.77 | 30.60 | 0.018 | 207.08 | 2,330 |

Option 2 reduced G2 pause by 45.5%. The device fsync difference also
contributes. In a 100-generation light-write run the median/P99 pauses were
17.26/29.40 ms (option 1) and 3.85/4.43 ms (option 2). Fresh C VMMs restored
G1, G2, G25, G50, G75 and G100 and checked the application RAM, full file,
process identity and hole transitions. Both variants measured zero disk-mount
GET bytes on restore, zero allocated local data blocks after the first 1 GiB
read and zero shared PUT bytes during guest random writes.

The same 32 MiB cost 0.318 ms with one range call, 249.405 ms split into
8,192 contiguous calls and 332.002 ms with 8,192 dispersed calls. The table
shows the impact of sustained background work on prewarmed 4 KiB QD1 writes.
P99 includes the guest fsync in the last column.

| Option 2 background work | Write IOPS | P99 ms | Write + fsync IOPS | P99 ms |
| --- | ---: | ---: | ---: | ---: |
| Idle | 42,346 | 0.033 | 3,134 | 1.234 |
| Local merge | 41,938 | 0.033 | 1,667 | 2.009 |
| Publication | 39,149 | 0.048 | 2,833 | 1.508 |
| Merge + publication | 38,555 | 0.050 | 1,159 | 3.997 |

Prioritize option 2 for pause-sensitive workloads. Option 1 remains a candidate
for infrequent checkpoints with strict synchronous-write latency targets.
Range coalescing, scheduling merge separately from upload, and testing against
the workload's fsync SLO matter. These measurements isolate the cost of range
replay and writeback; attribution to a specific XFS lock would need tracing.
