// Copyright 2021 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::layers::Layers;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::fs::File;
use std::os::fd::RawFd;
use std::os::unix::io::AsRawFd;

use super::direct_io::{AlignedBuffer, DirectIo};
use vm_memory::{Bytes, GuestMemoryBackend, GuestMemoryError};
use vmm_sys_util::eventfd::EventFd;

use crate::devices::virtio::block::virtio::io::RequestError;
use crate::devices::virtio::block::virtio::{IO_URING_NUM_ENTRIES, PendingRequest};
use crate::io_uring::operation::{Cqe, OpCode, Operation};
use crate::io_uring::restriction::Restriction;
use crate::io_uring::{IoUring, IoUringError};
use crate::logger::log_dev_preview_warning;
use crate::vstate::memory::{GuestAddress, GuestMemoryExtension, GuestMemoryMmap};

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum AsyncIoError {
    /// Not implemented
    NotImplemented,
    /// IO: {0}
    IO(std::io::Error),
    /// IoUring: {0}
    IoUring(IoUringError),
    /// Submit: {0}
    Submit(std::io::Error),
    /// SyncAll: {0}
    SyncAll(std::io::Error),
    /// EventFd: {0}
    EventFd(std::io::Error),
    /// GuestMemoryBackend: {0}
    GuestMemoryBackend(GuestMemoryError),
}

#[derive(Debug)]
pub struct AsyncFileEngine {
    file: File,
    pub(super) direct: Option<DirectIo>,
    ring: IoUring<WrappedRequest>,
    completion_evt: EventFd,
    pub(super) layers: Option<Layers>,
    pending_writes: usize,
    ready: VecDeque<Cqe<WrappedRequest>>,
    read_slot: u32,
}

#[derive(Debug)]
pub struct WrappedRequest {
    written: Option<(std::ops::Range<usize>, usize, u32)>,
    addr: Option<GuestAddress>,
    bounce: Option<AlignedBuffer>,
    req: PendingRequest,
}

impl WrappedRequest {
    fn new(req: PendingRequest) -> Self {
        WrappedRequest {
            written: None,
            addr: None,
            bounce: None,
            req,
        }
    }

    fn new_with_dirty_tracking(addr: GuestAddress, req: PendingRequest) -> Self {
        WrappedRequest {
            written: None,
            addr: Some(addr),
            bounce: None,
            req,
        }
    }
}

impl AsyncFileEngine {
    fn new_ring(
        files: Vec<&File>,
        completion_fd: RawFd,
    ) -> Result<IoUring<WrappedRequest>, IoUringError> {
        IoUring::new(
            u32::from(IO_URING_NUM_ENTRIES),
            files,
            vec![
                // Make sure we only allow operations on pre-registered fds.
                Restriction::RequireFixedFds,
                Restriction::AllowFixedFdsAndDrain,
                // Allowlist of opcodes.
                Restriction::AllowOpCode(OpCode::Read),
                Restriction::AllowOpCode(OpCode::Write),
                Restriction::AllowOpCode(OpCode::Fsync),
            ],
            Some(completion_fd),
        )
    }

    pub fn from_file(file: File) -> Result<AsyncFileEngine, AsyncIoError> {
        Self::with_layers(file, None)
    }

    pub(super) fn with_layers(file: File, layers: Option<Layers>) -> Result<Self, AsyncIoError> {
        log_dev_preview_warning("Async file IO", Option::None);

        let completion_evt = EventFd::new(libc::EFD_NONBLOCK).map_err(AsyncIoError::EventFd)?;
        let files = layers
            .as_ref()
            .map_or_else(|| vec![&file], |l| l.files.iter().collect());
        let ring =
            Self::new_ring(files, completion_evt.as_raw_fd()).map_err(AsyncIoError::IoUring)?;

        Ok(AsyncFileEngine {
            file,
            layers,
            pending_writes: 0,
            ready: VecDeque::new(),
            read_slot: 0,
            direct: None,
            ring,
            completion_evt,
        })
    }

    pub fn from_direct_file(file: File) -> Result<Self, AsyncIoError> {
        let direct = DirectIo::new(&file).map_err(AsyncIoError::IO)?;
        let mut engine = Self::from_file(file)?;
        engine.direct = Some(direct);
        Ok(engine)
    }

    pub(super) fn transfer_serialized(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
        write: bool,
    ) -> Result<u32, AsyncIoError> {
        // Complete every earlier write before a partial-block read-modify-write.
        // Keep CQEs for the normal completion path (including bounced reads).
        self.drain(false)?;
        self.direct
            .unwrap()
            .transfer(&self.file, offset, mem, addr, count, write)
            .map_err(AsyncIoError::IO)
    }

    fn prepare_buffer(
        &self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
        ptr: *mut u8,
        read: bool,
    ) -> Result<Option<AlignedBuffer>, AsyncIoError> {
        let Some(direct) = self.direct else {
            return Ok(None);
        };
        direct.validate(offset, count).map_err(AsyncIoError::IO)?;
        if !direct.range_aligned(offset, count) {
            return Err(AsyncIoError::IO(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "partial host block must use serialized direct I/O",
            )));
        }
        if count == 0 || direct.buffer_aligned(ptr) {
            return Ok(None);
        }
        let mut bounce = AlignedBuffer::new(count as usize, direct.memory_alignment)
            .map_err(AsyncIoError::IO)?;
        if !read {
            mem.read_slice(bounce.as_mut_slice(), addr)
                .map_err(AsyncIoError::GuestMemoryBackend)?;
        }
        Ok(Some(bounce))
    }

    pub fn update_file(&mut self, file: File) -> Result<(), AsyncIoError> {
        if !self.ring.is_empty() {
            return Err(AsyncIoError::IO(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "drain and consume requests before replacing the backing file",
            )));
        }
        let direct = self
            .direct
            .map(|_| DirectIo::new(&file))
            .transpose()
            .map_err(AsyncIoError::IO)?;
        if self.layers.is_some() {
            return Err(AsyncIoError::NotImplemented);
        }
        let ring = Self::new_ring(vec![&file], self.completion_evt.as_raw_fd())
            .map_err(AsyncIoError::IoUring)?;

        self.file = file;
        self.direct = direct;
        self.ring = ring;
        Ok(())
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn completion_evt(&self) -> &EventFd {
        &self.completion_evt
    }

    pub(super) fn read_layer(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
    ) -> Result<bool, AsyncIoError> {
        if self.pending_writes != 0 {
            self.drain(false)?;
            while let Some(cqe) = self.reap()? {
                self.ready.push_back(cqe);
            }
        }
        let layer = self.layers.as_ref().unwrap();
        let range = layer.sectors(offset, count).map_err(AsyncIoError::IO)?;
        if let Some(slot) = layer.source(range.clone()) {
            self.read_slot = slot;
            return Ok(false);
        }
        // Validate the entire destination before issuing any partial transfer.
        mem.get_slice(addr, count as usize)
            .map_err(AsyncIoError::GuestMemoryBackend)?;
        let mut buf = vec![0; (count as usize).min(64 * 1024)];
        for start in (range.start..range.end).step_by(128) {
            let end = (start + 128).min(range.end);
            let bytes = &mut buf[..(end - start) * 512];
            layer
                .read_mixed(start..end, bytes)
                .map_err(AsyncIoError::IO)?;
            mem.write_slice(
                bytes,
                GuestAddress(addr.0 + ((start - range.start) * 512) as u64),
            )
            .map_err(AsyncIoError::GuestMemoryBackend)?;
        }
        Ok(true)
    }
    pub fn push_read(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
        req: PendingRequest,
    ) -> Result<(), RequestError<AsyncIoError>> {
        let buf = match mem.get_slice(addr, count as usize) {
            Ok(slice) => slice.ptr_guard_mut().as_ptr(),
            Err(err) => {
                return Err(RequestError {
                    req,
                    error: AsyncIoError::GuestMemoryBackend(err),
                });
            }
        };

        let bounce = match self.prepare_buffer(offset, mem, addr, count, buf, true) {
            Ok(bounce) => bounce,
            Err(error) => return Err(RequestError { req, error }),
        };
        let buf = bounce.as_ref().map_or(buf, AlignedBuffer::as_ptr);
        let mut wrapped_user_data = WrappedRequest::new_with_dirty_tracking(addr, req);
        wrapped_user_data.bounce = bounce;

        self.ring
            .push(Operation::read(
                self.read_slot,
                buf as usize,
                count,
                offset,
                wrapped_user_data,
            ))
            .map_err(|(io_uring_error, data)| RequestError {
                req: data.req,
                error: AsyncIoError::IoUring(io_uring_error),
            })
    }

    pub fn push_write(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
        req: PendingRequest,
    ) -> Result<(), RequestError<AsyncIoError>> {
        let buf = match mem.get_slice(addr, count as usize) {
            Ok(slice) => slice.ptr_guard_mut().as_ptr(),
            Err(err) => {
                return Err(RequestError {
                    req,
                    error: AsyncIoError::GuestMemoryBackend(err),
                });
            }
        };

        let bounce = match self.prepare_buffer(offset, mem, addr, count, buf, false) {
            Ok(bounce) => bounce,
            Err(error) => return Err(RequestError { req, error }),
        };
        let buf = bounce.as_ref().map_or(buf, AlignedBuffer::as_ptr);
        let mut slot = 0;
        let mut written = None;
        if let Some(layer) = &self.layers {
            let range = match layer.sectors(offset, count) {
                Ok(range) => range,
                Err(e) => {
                    return Err(RequestError {
                        req,
                        error: AsyncIoError::IO(e),
                    });
                }
            };
            slot = layer.active();
            written = Some((range, layer.epoch(), slot));
        }
        let mut wrapped_user_data = WrappedRequest::new(req);
        wrapped_user_data.written = written;
        wrapped_user_data.bounce = bounce;

        self.ring
            .push(Operation::write(
                slot,
                buf as usize,
                count,
                offset,
                wrapped_user_data,
            ))
            .map_err(|(io_uring_error, data)| RequestError {
                req: data.req,
                error: AsyncIoError::IoUring(io_uring_error),
            })?;
        if self.layers.is_some() {
            self.pending_writes += 1;
        }
        Ok(())
    }

    pub fn push_flush(&mut self, req: PendingRequest) -> Result<(), RequestError<AsyncIoError>> {
        let wrapped_user_data = WrappedRequest::new(req);

        self.ring
            .push(
                Operation::fsync(
                    self.layers.as_ref().map_or(0, Layers::active),
                    wrapped_user_data,
                )
                .drained(),
            )
            .map_err(|(io_uring_error, data)| RequestError {
                req: data.req,
                error: AsyncIoError::IoUring(io_uring_error),
            })
    }

    pub fn discard(&mut self, _range: (u64, u32)) -> Result<u32, AsyncIoError> {
        Err(AsyncIoError::NotImplemented)
    }

    pub fn kick_submission_queue(&mut self) -> Result<(), AsyncIoError> {
        self.ring
            .submit()
            .map(|_| ())
            .map_err(AsyncIoError::IoUring)
    }

    pub fn drain(&mut self, discard_cqes: bool) -> Result<(), AsyncIoError> {
        self.ring
            .submit_and_wait_all()
            .map(|_| ())
            .map_err(AsyncIoError::IoUring)?;

        if discard_cqes {
            // Drain the completion queue so that we may deallocate the user_data fields.
            while self.do_pop()?.is_some() {}
        }

        Ok(())
    }

    pub fn drain_and_flush(&mut self, discard_cqes: bool) -> Result<(), AsyncIoError> {
        let result = self.drain(discard_cqes).and_then(|()| {
            let file = self
                .layers
                .as_ref()
                .map_or(&self.file, |layers| &layers.files[layers.active() as usize]);
            file.sync_all().map_err(AsyncIoError::SyncAll)
        });
        if result.is_err()
            && let Some(layers) = &self.layers
        {
            layers.fail();
        }
        result
    }

    fn do_pop(&mut self) -> Result<Option<Cqe<WrappedRequest>>, AsyncIoError> {
        if let Some(cqe) = self.ready.pop_front() {
            return Ok(Some(cqe));
        }
        self.reap()
    }
    fn reap(&mut self) -> Result<Option<Cqe<WrappedRequest>>, AsyncIoError> {
        Ok(self.ring.pop().map_err(AsyncIoError::IoUring)?.map(|cqe| {
            let count = cqe.count() as usize;
            cqe.map_user_data(|mut data| {
                if let Some((range, epoch, slot)) = data.written.take() {
                    self.pending_writes -= 1;
                    if count == range.len() * 512 {
                        self.layers.as_mut().unwrap().complete(range, epoch, slot);
                    } else {
                        self.layers.as_ref().unwrap().fail();
                    }
                }
                data
            })
        }))
    }

    pub fn pop(
        &mut self,
        mem: &GuestMemoryMmap,
    ) -> Result<Option<Cqe<PendingRequest>>, AsyncIoError> {
        let cqe = self.do_pop()?.map(|cqe| {
            let count = cqe.count();
            let mut copy_failed = false;
            let mapped = cqe.map_user_data(|wrapped| {
                if let Some(addr) = wrapped.addr {
                    if let Some(bounce) = wrapped.bounce.as_ref() {
                        copy_failed = count as usize > bounce.as_slice().len()
                            || mem
                                .write_slice(
                                    &bounce.as_slice()
                                        [..(count as usize).min(bounce.as_slice().len())],
                                    addr,
                                )
                                .is_err();
                    }
                    mem.mark_dirty(addr, count as usize);
                }
                wrapped.req
            });
            if copy_failed {
                Cqe::new(-libc::EFAULT, mapped.user_data())
            } else {
                mapped
            }
        });

        Ok(cqe)
    }
}

impl Drop for AsyncFileEngine {
    fn drop(&mut self) {
        // The kernel must finish using owned bounce buffers before they are freed.
        // Block::drop normally already drains the device.
        if self.drain(true).is_err() {
            self.ring.abandon_user_data();
        }
    }
}
