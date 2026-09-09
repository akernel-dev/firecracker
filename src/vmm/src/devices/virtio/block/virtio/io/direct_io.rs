// Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Alignment adaptation for private direct-I/O block images.
//! Partial host blocks are handled only after draining the device's async ring.
//! The VMM serializes these read-modify-write operations with later submissions.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::ptr::NonNull;

use vm_memory::Bytes;

use crate::vstate::memory::{GuestAddress, GuestMemory, GuestMemoryExtension, GuestMemoryMmap};

// Linux UAPI <linux/stat.h>, including the Linux 6.1 DIOALIGN fields.
// libc does not expose statx on musl. Keep this fixed kernel ABI local instead
// of depending on a libc-specific wrapper. Unused fields remain explicit so
// that the two queried offsets and the complete kernel output size are clear.
const STATX_DIOALIGN: u32 = 0x2000;

#[repr(C)]
struct StatxTimestamp {
    _seconds: i64,
    _nanoseconds: u32,
    _reserved: i32,
}

#[repr(C)]
struct Statx {
    mask: u32,
    _block_size: u32,
    _attributes: u64,
    _links: u32,
    _uid: u32,
    _gid: u32,
    _mode: u16,
    _spare0: u16,
    _inode: u64,
    _size: u64,
    _blocks: u64,
    _attributes_mask: u64,
    _timestamps: [StatxTimestamp; 4],
    _rdev_major: u32,
    _rdev_minor: u32,
    _dev_major: u32,
    _dev_minor: u32,
    _mount_id: u64,
    dio_memory_alignment: u32,
    dio_offset_alignment: u32,
    _spare3: [u64; 12],
}

const _: () = {
    assert!(std::mem::size_of::<Statx>() == 0x100);
    assert!(std::mem::align_of::<Statx>() == 8);
    assert!(std::mem::offset_of!(Statx, mask) == 0);
    assert!(std::mem::offset_of!(Statx, dio_memory_alignment) == 0x98);
    assert!(std::mem::offset_of!(Statx, dio_offset_alignment) == 0x9c);
};

// Bound memory per bounce request, including requests from an untrusted guest.
// Larger requests use the serialized, chunked path with one reusable buffer.
pub const MAX_BOUNCE_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct AlignedBuffer {
    ptr: NonNull<u8>,
    layout: Layout,
}

// SAFETY: The allocation is exclusively owned and moves with its owner. Async
// requests retain it until completion; they never access it during kernel I/O.
unsafe impl Send for AlignedBuffer {}

impl AlignedBuffer {
    pub fn new(len: usize, alignment: usize) -> io::Result<Self> {
        if len == 0 || len > MAX_BOUNCE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid bounce length",
            ));
        }
        let layout = Layout::from_size_align(len, alignment)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid I/O alignment"))?;
        // SAFETY: Layout is valid and nonempty. A null allocation is reported to
        // the guest as an I/O error instead of aborting the VMM.
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout) })
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOMEM))?;
        Ok(Self { ptr, layout })
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: This object owns the allocation for its entire lifetime.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.layout.size()) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: The mutable borrow excludes concurrent access to the allocation.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        // SAFETY: The allocation was created with this layout and is freed once.
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DirectIo {
    pub memory_alignment: usize,
    pub offset_alignment: usize,
    size: u64,
}

impl DirectIo {
    pub fn new(file: &File) -> io::Result<Self> {
        // SAFETY: statx is a C output structure; zero initialization is valid.
        let mut stat: Statx = unsafe { std::mem::zeroed() };
        // SAFETY: The fd is live, the empty pathname is NUL-terminated, and stat
        // points to writable storage of the correct size.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_statx,
                file.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH,
                STATX_DIOALIGN,
                &mut stat,
            )
        };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
        if stat.mask & STATX_DIOALIGN == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "filesystem does not report direct I/O alignment; select Sync or Async",
            ));
        }
        Self::with_alignment(
            file,
            stat.dio_memory_alignment as usize,
            stat.dio_offset_alignment as usize,
        )
    }

    pub fn with_alignment(
        file: &File,
        memory_alignment: usize,
        offset_alignment: usize,
    ) -> io::Result<Self> {
        let size = file.metadata()?.len();
        if !memory_alignment.is_power_of_two()
            || !offset_alignment.is_power_of_two()
            || memory_alignment > MAX_BOUNCE_BYTES
            || offset_alignment > MAX_BOUNCE_BYTES
            || !size.is_multiple_of(offset_alignment as u64)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported direct I/O alignment or image length",
            ));
        }
        Ok(Self {
            memory_alignment,
            offset_alignment,
            size,
        })
    }

    pub fn range_aligned(&self, offset: u64, count: u32) -> bool {
        offset.is_multiple_of(self.offset_alignment as u64)
            && (count as usize).is_multiple_of(self.offset_alignment)
    }

    pub fn buffer_aligned(&self, ptr: *mut u8) -> bool {
        (ptr as usize).is_multiple_of(self.memory_alignment)
    }

    pub fn validate(&self, offset: u64, count: u32) -> io::Result<()> {
        if offset
            .checked_add(u64::from(count))
            .is_none_or(|end| end > self.size)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "I/O beyond image end",
            ));
        }
        Ok(())
    }

    /// Bounded direct I/O with aligned reads and serialized partial-block writes.
    /// The caller must exclude other in-flight access to this private disk.
    pub fn transfer(
        &self,
        file: &File,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
        write: bool,
    ) -> io::Result<u32> {
        self.validate(offset, count)?;
        mem.get_slice(addr, count as usize)
            .map_err(io::Error::other)?;
        if count == 0 {
            return Ok(0);
        }
        let mut buffer = AlignedBuffer::new(MAX_BOUNCE_BYTES, self.memory_alignment)?;
        let mut done = 0_u32;
        while done < count {
            let position = offset + u64::from(done);
            let start = position / self.offset_alignment as u64 * self.offset_alignment as u64;
            let prefix = usize::try_from(position - start).unwrap();
            let bytes = ((count - done) as usize).min(MAX_BOUNCE_BYTES - prefix);
            let aligned_len = (prefix + bytes).next_multiple_of(self.offset_alignment);
            let data = &mut buffer.as_mut_slice()[..aligned_len];
            if !write || prefix != 0 || bytes != aligned_len {
                direct_read(file, data, start)?;
            }
            let guest = GuestAddress(addr.0 + u64::from(done));
            if write {
                mem.read_slice(&mut data[prefix..prefix + bytes], guest)
                    .map_err(io::Error::other)?;
                direct_write(file, data, start)?;
            } else {
                mem.write_slice(&data[prefix..prefix + bytes], guest)
                    .map_err(io::Error::other)?;
                mem.mark_dirty(guest, bytes);
            }
            done += u32::try_from(bytes).unwrap();
        }
        Ok(done)
    }
}

// Do not retry a short direct transfer at an unaligned continuation offset.
// A short write is an error and may have modified part of the requested data.
fn direct_read(file: &File, data: &mut [u8], offset: u64) -> io::Result<()> {
    loop {
        match file.read_at(data, offset) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Ok(n) if n == data.len() => return Ok(()),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "short direct read",
                ));
            }
            Err(e) => return Err(e),
        }
    }
}

fn direct_write(file: &File, data: &[u8], offset: u64) -> io::Result<()> {
    loop {
        match file.write_at(data, offset) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Ok(n) if n == data.len() => return Ok(()),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "short direct write",
                ));
            }
            Err(e) => return Err(e),
        }
    }
}
