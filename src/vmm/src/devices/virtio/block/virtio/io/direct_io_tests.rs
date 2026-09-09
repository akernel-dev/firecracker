// Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;

use vm_memory::bitmap::Bitmap;
use vm_memory::{Bytes, GuestMemoryRegion};
use vmm_sys_util::tempfile::TempFile;

use super::direct_io::{AlignedBuffer, DirectIo, MAX_BOUNCE_BYTES};
use super::*;
use crate::vmm_config::machine_config::HugePageConfig;
use crate::vstate::memory::{self, GuestRegionMmapExt};

fn memory() -> GuestMemoryMmap {
    GuestMemoryMmap::from_regions(
        memory::anonymous(
            [(GuestAddress(0), 1024 * 1024)].into_iter(),
            true,
            HugePageConfig::None,
        )
        .unwrap()
        .into_iter()
        .map(|r| GuestRegionMmapExt::dram_from_mmap_region(r, 0))
        .collect(),
    )
    .unwrap()
}

fn disk() -> (TempFile, File) {
    let path = TempFile::new().unwrap();
    path.as_file().set_len(1024 * 1024).unwrap();
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT)
        .open(path.as_path())
        .unwrap();
    (path, file)
}

fn complete(engine: &mut FileEngine, mem: &GuestMemoryMmap, result: FileEngineOk, count: u32) {
    match result {
        FileEngineOk::Executed(done) => assert_eq!(done.count, count),
        FileEngineOk::Submitted => {
            let FileEngine::Async(e) = engine else {
                panic!("not async")
            };
            e.drain(false).unwrap();
            assert_eq!(e.pop(mem).unwrap().unwrap().result().unwrap(), count);
        }
    }
}

#[test]
fn direct_sync_partial_blocks_and_large_unaligned_buffer() {
    let (_path, file) = disk();
    let geometry = DirectIo::with_alignment(&file, 4096, 4096).unwrap();
    let mem = memory();
    let data: Vec<u8> = (0..3 * MAX_BOUNCE_BYTES + 512)
        .map(|n| u8::try_from(n % 251).unwrap())
        .collect();
    mem.write_slice(&data, GuestAddress(3)).unwrap();
    geometry
        .transfer(
            &file,
            512,
            &mem,
            GuestAddress(3),
            u32::try_from(data.len()).unwrap(),
            true,
        )
        .unwrap();
    let readback = memory();
    geometry
        .transfer(
            &file,
            0,
            &readback,
            GuestAddress(7),
            u32::try_from(data.len() + 1024).unwrap(),
            false,
        )
        .unwrap();
    let mut bytes = vec![0; data.len() + 1024];
    readback.read_slice(&mut bytes, GuestAddress(7)).unwrap();
    assert_eq!(&bytes[..512], &[0; 512]);
    assert_eq!(&bytes[512..512 + data.len()], data);
    assert_eq!(&bytes[512 + data.len()..], &[0; 512]);
    // Last virtual sector must not grow the backing file during read-modify-write.
    geometry
        .transfer(&file, 1024 * 1024 - 512, &mem, GuestAddress(3), 512, true)
        .unwrap();
    assert_eq!(file.metadata().unwrap().len(), 1024 * 1024);
    geometry
        .transfer(&file, 1024 * 1024, &mem, GuestAddress(3), 512, true)
        .unwrap_err();
    geometry
        .transfer(&file, 0, &mem, GuestAddress(1024 * 1024 - 1), 512, true)
        .unwrap_err();
    AlignedBuffer::new(MAX_BOUNCE_BYTES + 1, 4096).unwrap_err();
    DirectIo::with_alignment(&file, 0, 4096).unwrap_err();
    DirectIo::with_alignment(&file, 4096, 0).unwrap_err();
}

#[test]
fn direct_async_bounce_read_write_and_dirty_tracking() {
    let (_path, file) = disk();
    let mut engine = FileEngine::from_file(file, FileEngineType::AsyncDirect).unwrap();
    let mem = memory();
    mem.write_slice(&[0x5a; 8192], GuestAddress(3)).unwrap();
    let op = engine
        .write(0, &mem, GuestAddress(3), 8192, PendingRequest::default())
        .unwrap();
    assert!(matches!(op, FileEngineOk::Submitted));
    complete(&mut engine, &mem, op, 8192);
    let destination = memory();
    let op = engine
        .read(
            0,
            &destination,
            GuestAddress(7),
            8192,
            PendingRequest::default(),
        )
        .unwrap();
    assert!(matches!(op, FileEngineOk::Submitted));
    // Guest data is filled when processing the CQE, before finishing its request.
    complete(&mut engine, &destination, op, 8192);
    let mut result = [0; 8192];
    destination
        .read_slice(&mut result, GuestAddress(7))
        .unwrap();
    assert_eq!(result, [0x5a; 8192]);
    let bitmap = destination.find_region(GuestAddress(7)).unwrap().bitmap();
    assert!(bitmap.dirty_at(7));
    assert!(bitmap.dirty_at(8198));
    assert!(!bitmap.dirty_at(16384));
    let flush = engine.flush(PendingRequest::default()).unwrap();
    complete(&mut engine, &mem, flush, 0);
}

#[test]
fn direct_async_partial_blocks_preserve_earlier_inflight_writes() {
    let (_path, file) = disk();
    let mut engine = FileEngine::from_file(file, FileEngineType::AsyncDirect).unwrap();
    if let FileEngine::Async(e) = &mut engine {
        e.direct = Some(DirectIo::with_alignment(e.file(), 4096, 4096).unwrap());
    }
    let mem = memory();
    mem.write_slice(&[0x11; 4096], GuestAddress(16384)).unwrap();
    mem.write_slice(&[0x22; 512], GuestAddress(3)).unwrap();
    mem.write_slice(&[0x33; 512], GuestAddress(8195)).unwrap();
    assert!(matches!(
        engine
            .write(
                0,
                &mem,
                GuestAddress(16384),
                4096,
                PendingRequest::default()
            )
            .unwrap(),
        FileEngineOk::Submitted
    ));
    // Force 512-byte guest writes inside one 4096-byte host block. Both must
    // preserve the aligned write already queued, and each other's sectors.
    for (offset, addr) in [(512, 3), (1024, 8195)] {
        let result = engine
            .write(
                offset,
                &mem,
                GuestAddress(addr),
                512,
                PendingRequest::default(),
            )
            .unwrap();
        assert!(matches!(result, FileEngineOk::Executed(_)));
    }
    if let FileEngine::Async(e) = &mut engine {
        assert_eq!(e.pop(&mem).unwrap().unwrap().result().unwrap(), 4096);
        assert!(e.pop(&mem).unwrap().is_none());
    }
    let op = engine
        .read(
            0,
            &mem,
            GuestAddress(32771),
            4096,
            PendingRequest::default(),
        )
        .unwrap();
    complete(&mut engine, &mem, op, 4096);
    let mut result = [0; 4096];
    mem.read_slice(&mut result, GuestAddress(32771)).unwrap();
    assert_eq!(&result[..512], &[0x11; 512]);
    assert_eq!(&result[512..1024], &[0x22; 512]);
    assert_eq!(&result[1024..1536], &[0x33; 512]);
    assert_eq!(&result[1536..], &[0x11; 2560]);
}

#[test]
fn direct_async_queue_pressure_and_large_buffer() {
    let (_path, file) = disk();
    let mut engine = FileEngine::from_file(file, FileEngineType::AsyncDirect).unwrap();
    let mem = memory();
    mem.write_slice(&vec![0x65; 2 * MAX_BOUNCE_BYTES], GuestAddress(3))
        .unwrap();
    let mut submitted = 0;
    loop {
        match engine.write(0, &mem, GuestAddress(3), 4096, PendingRequest::default()) {
            Ok(FileEngineOk::Submitted) => submitted += 1,
            Err(error) => {
                assert!(error.error.is_throttling_err());
                break;
            }
            _ => panic!("unexpected synchronous request"),
        }
        assert!(submitted <= 1024);
    }
    if let FileEngine::Async(e) = &mut engine {
        e.drain(false).unwrap();
        for _ in 0..submitted {
            assert_eq!(e.pop(&mem).unwrap().unwrap().result().unwrap(), 4096);
        }
        assert!(e.pop(&mem).unwrap().is_none());
    }
    let n = u32::try_from(2 * MAX_BOUNCE_BYTES).unwrap();
    let op = engine
        .write(4096, &mem, GuestAddress(3), n, PendingRequest::default())
        .unwrap();
    assert!(matches!(op, FileEngineOk::Executed(_)));
    let readback = memory();
    let op = engine
        .read(
            4096,
            &readback,
            GuestAddress(3),
            n,
            PendingRequest::default(),
        )
        .unwrap();
    complete(&mut engine, &readback, op, n);
    let mut result = vec![0; n as usize];
    readback.read_slice(&mut result, GuestAddress(3)).unwrap();
    assert_eq!(result, vec![0x65; n as usize]);
}

#[test]
fn direct_async_short_read_only_copies_completed_bytes() {
    let (_path, file) = disk();
    let mut engine = FileEngine::from_file(file, FileEngineType::AsyncDirect).unwrap();
    // Simulate external truncation after opening the image, forcing a short read.
    engine.file().set_len(4096).unwrap();
    let mem = memory();
    mem.write_slice(&[0x7a; 8192], GuestAddress(3)).unwrap();
    let op = engine
        .read(0, &mem, GuestAddress(3), 8192, PendingRequest::default())
        .unwrap();
    complete(&mut engine, &mem, op, 4096);
    let mut result = [0; 8192];
    mem.read_slice(&mut result, GuestAddress(3)).unwrap();
    assert_eq!(&result[..4096], &[0; 4096]);
    assert_eq!(&result[4096..], &[0x7a; 4096]);
}

#[test]
fn direct_async_rejects_invalid_requests_before_submission() {
    let (_path, file) = disk();
    let mut engine = FileEngine::from_file(file, FileEngineType::AsyncDirect).unwrap();
    let FileEngine::Async(e) = &mut engine else {
        unreachable!()
    };
    let geometry = e.direct.unwrap();
    let mem = memory();
    // Validation rejects the invalid address before enqueueing anything.
    e.push_read(
        0,
        &mem,
        GuestAddress(1024 * 1024),
        4096,
        PendingRequest::default(),
    )
    .unwrap_err();
    e.push_read(
        u64::MAX,
        &mem,
        GuestAddress(3),
        4096,
        PendingRequest::default(),
    )
    .unwrap_err();
    assert!(!geometry.range_aligned(1, 4096));
    assert!(e.pop(&mem).unwrap().is_none());
}

#[test]
fn direct_async_flush_waits_for_prior_writes() {
    let (_path, file) = disk();
    let mut engine = FileEngine::from_file(file, FileEngineType::AsyncDirect).unwrap();
    let mem = memory();
    mem.write_slice(&[0x3e; 4096], GuestAddress(3)).unwrap();
    for index in 0..8 {
        engine
            .write(
                index * 4096,
                &mem,
                GuestAddress(3),
                4096,
                PendingRequest::default(),
            )
            .unwrap();
    }
    engine.flush(PendingRequest::default()).unwrap();
    let FileEngine::Async(e) = &mut engine else {
        unreachable!()
    };
    e.drain(false).unwrap();
    for _ in 0..8 {
        assert_eq!(e.pop(&mem).unwrap().unwrap().result().unwrap(), 4096);
    }
    assert_eq!(e.pop(&mem).unwrap().unwrap().result().unwrap(), 0);
    assert!(e.pop(&mem).unwrap().is_none());
}

#[test]
fn direct_async_failed_write_and_pending_update() {
    let (path, file) = disk();
    drop(file);
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path.as_path())
        .unwrap();
    let mut engine = FileEngine::from_file(file, FileEngineType::AsyncDirect).unwrap();
    let mem = memory();
    mem.write_slice(&[0x4f; 4096], GuestAddress(3)).unwrap();
    engine
        .write(0, &mem, GuestAddress(3), 4096, PendingRequest::default())
        .unwrap();
    let (_replacement_path, replacement) = disk();
    let FileEngine::Async(e) = &mut engine else {
        unreachable!()
    };
    e.update_file(replacement).unwrap_err();
    e.drain(false).unwrap();
    e.pop(&mem).unwrap().unwrap().result().unwrap_err();
    assert!(e.pop(&mem).unwrap().is_none());
    let (_replacement_path, replacement) = disk();
    e.update_file(replacement).unwrap();
    // A failed submission/completion must not poison later I/O or lose O_DIRECT.
    let op = engine
        .write(0, &mem, GuestAddress(3), 4096, PendingRequest::default())
        .unwrap();
    complete(&mut engine, &mem, op, 4096);
}

#[test]
fn direct_sync_engine_preserves_policy_on_update() {
    let (_path, file) = disk();
    let mut engine = FileEngine::from_file(file, FileEngineType::SyncDirect).unwrap();
    let mem = memory();
    mem.write_slice(&[0x9f; 512], GuestAddress(3)).unwrap();
    let op = engine
        .write(512, &mem, GuestAddress(3), 512, PendingRequest::default())
        .unwrap();
    complete(&mut engine, &mem, op, 512);
    let (_new_path, new_file) = disk();
    engine.update_file_path(new_file).unwrap();
    let op = engine
        .write(512, &mem, GuestAddress(3), 512, PendingRequest::default())
        .unwrap();
    complete(&mut engine, &mem, op, 512);
    let out = memory();
    let op = engine
        .read(512, &out, GuestAddress(7), 512, PendingRequest::default())
        .unwrap();
    complete(&mut engine, &out, op, 512);
    let mut data = [0; 512];
    out.read_slice(&mut data, GuestAddress(7)).unwrap();
    assert_eq!(data, [0x9f; 512]);
}
