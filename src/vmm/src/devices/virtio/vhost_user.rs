// Copyright 2023 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

// Portions Copyright 2019 Intel Corporation. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs::File;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use vhost::vhost_user::message::*;
use vhost::vhost_user::{Frontend, VhostUserFrontend};
use vhost::{
    Error as VhostError, VhostBackend, VhostUserDirtyLogRegion, VhostUserMemoryRegionInfo,
    VringConfigData,
};
use vm_memory::{Address, GuestMemory, GuestMemoryError, GuestMemoryRegion};
use vmm_sys_util::eventfd::EventFd;

use crate::devices::virtio::queue::Queue;
use crate::devices::virtio::transport::{VirtioInterrupt, VirtioInterruptType};
use crate::vstate::memory::GuestMemoryMmap;

/// vhost-user error.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum VhostUserError {
    /// Invalid available address
    AvailAddress(GuestMemoryError),
    /// Failed to connect to UDS Unix stream: {0}
    Connect(#[from] std::io::Error),
    /// Invalid descriptor table address
    DescriptorTableAddress(GuestMemoryError),
    /// Get features failed: {0}
    VhostUserGetFeatures(VhostError),
    /// Get protocol features failed: {0}
    VhostUserGetProtocolFeatures(VhostError),
    /// Set owner failed: {0}
    VhostUserSetOwner(VhostError),
    /// Set features failed: {0}
    VhostUserSetFeatures(VhostError),
    /// Set protocol features failed: {0}
    VhostUserSetProtocolFeatures(VhostError),
    /// Set mem table failed: {0}
    VhostUserSetMemTable(VhostError),
    /// Set vring num failed: {0}
    VhostUserSetVringNum(VhostError),
    /// Set vring addr failed: {0}
    VhostUserSetVringAddr(VhostError),
    /// Set vring base failed: {0}
    VhostUserSetVringBase(VhostError),
    /// Set vring call failed: {0}
    VhostUserSetVringCall(VhostError),
    /// Set vring kick failed: {0}
    VhostUserSetVringKick(VhostError),
    /// Set vring enable failed: {0}
    VhostUserSetVringEnable(VhostError),
    /// Get vring base failed: {0}
    VhostUserGetVringBase(VhostError),
    /// Set dirty-log base failed: {0}
    VhostUserSetLogBase(VhostError),
    /// Transfer backend device state failed: {0}
    VhostUserSetDeviceState(VhostError),
    /// Check backend device state failed: {0}
    VhostUserCheckDeviceState(VhostError),
    /// Failed to read vhost eventfd: No memory region found
    VhostUserNoMemoryRegion,
    /// Invalid used address
    UsedAddress(GuestMemoryError),
}

// Trait with all methods we use from `Frontend` from vhost crate.
// It allows us to create a mock implementation of the `Frontend`
// to verify calls to the backend.
// All methods have default impl in order to simplify mock impls.
pub trait VhostUserHandleBackend: Sized {
    /// Constructor of `Frontend`
    fn from_stream(_sock: UnixStream, _max_queue_num: u64) -> Self {
        unimplemented!()
    }

    fn set_hdr_flags(&self, _flags: VhostUserHeaderFlag) {
        unimplemented!()
    }

    /// Get from the underlying vhost implementation the feature bitmask.
    fn get_features(&self) -> Result<u64, vhost::Error> {
        unimplemented!()
    }

    /// Enable features in the underlying vhost implementation using a bitmask.
    fn set_features(&self, _features: u64) -> Result<(), vhost::Error> {
        unimplemented!()
    }

    /// Set the current Frontend as an owner of the session.
    fn set_owner(&self) -> Result<(), vhost::Error> {
        unimplemented!()
    }

    /// Set the memory map regions on the slave so it can translate the vring
    /// addresses. In the ancillary data there is an array of file descriptors
    fn set_mem_table(&self, _regions: &[VhostUserMemoryRegionInfo]) -> Result<(), vhost::Error> {
        unimplemented!()
    }

    /// Set the size of the queue.
    fn set_vring_num(&self, _queue_index: usize, _num: u16) -> Result<(), vhost::Error> {
        unimplemented!()
    }

    /// Sets the addresses of the different aspects of the vring.
    fn set_vring_addr(
        &self,
        _queue_index: usize,
        _config_data: &VringConfigData,
    ) -> Result<(), vhost::Error> {
        unimplemented!()
    }

    /// Sets the base offset in the available vring.
    fn set_vring_base(&self, _queue_index: usize, _base: u16) -> Result<(), vhost::Error> {
        unimplemented!()
    }

    fn get_vring_base(&self, _queue_index: usize) -> Result<u32, vhost::Error> {
        unimplemented!()
    }

    fn set_log_base(
        &self,
        _base: u64,
        _region: Option<VhostUserDirtyLogRegion>,
    ) -> Result<(), vhost::Error> {
        unimplemented!()
    }

    /// Set the event file descriptor to signal when buffers are used.
    /// Bits (0-7) of the payload contain the vring index. Bit 8 is the invalid FD flag. This flag
    /// is set when there is no file descriptor in the ancillary data. This signals that polling
    /// will be used instead of waiting for the call.
    fn set_vring_call(&self, _queue_index: usize, _fd: &EventFd) -> Result<(), vhost::Error> {
        unimplemented!()
    }

    /// Set the event file descriptor for adding buffers to the vring.
    /// Bits (0-7) of the payload contain the vring index. Bit 8 is the invalid FD flag. This flag
    /// is set when there is no file descriptor in the ancillary data. This signals that polling
    /// should be used instead of waiting for a kick.
    fn set_vring_kick(&self, _queue_index: usize, _fd: &EventFd) -> Result<(), vhost::Error> {
        unimplemented!()
    }

    fn get_protocol_features(&mut self) -> Result<VhostUserProtocolFeatures, vhost::Error> {
        unimplemented!()
    }

    fn set_protocol_features(
        &mut self,
        _features: VhostUserProtocolFeatures,
    ) -> Result<(), vhost::Error> {
        unimplemented!()
    }

    fn set_vring_enable(&mut self, _queue_index: usize, _enable: bool) -> Result<(), vhost::Error> {
        unimplemented!()
    }

    fn get_config(
        &mut self,
        _offset: u32,
        _size: u32,
        _flags: VhostUserConfigFlags,
        _buf: &[u8],
    ) -> Result<(VhostUserConfig, VhostUserConfigPayload), vhost::Error> {
        unimplemented!()
    }

    fn set_config(
        &mut self,
        _offset: u32,
        _flags: VhostUserConfigFlags,
        _buf: &[u8],
    ) -> Result<(), vhost::Error> {
        unimplemented!()
    }

    fn set_device_state_fd(
        &self,
        _direction: VhostTransferStateDirection,
        _phase: VhostTransferStatePhase,
        _fd: OwnedFd,
    ) -> Result<Option<File>, vhost::Error> {
        unimplemented!()
    }

    fn check_device_state(&self) -> Result<(), vhost::Error> {
        unimplemented!()
    }
}

impl VhostUserHandleBackend for Frontend {
    fn from_stream(sock: UnixStream, max_queue_num: u64) -> Self {
        Frontend::from_stream(sock, max_queue_num)
    }

    fn set_hdr_flags(&self, flags: VhostUserHeaderFlag) {
        self.set_hdr_flags(flags)
    }

    /// Get from the underlying vhost implementation the feature bitmask.
    fn get_features(&self) -> Result<u64, vhost::Error> {
        <Frontend as VhostBackend>::get_features(self)
    }

    /// Enable features in the underlying vhost implementation using a bitmask.
    fn set_features(&self, features: u64) -> Result<(), vhost::Error> {
        <Frontend as VhostBackend>::set_features(self, features)
    }

    /// Set the current Frontend as an owner of the session.
    fn set_owner(&self) -> Result<(), vhost::Error> {
        <Frontend as VhostBackend>::set_owner(self)
    }

    /// Set the memory map regions on the slave so it can translate the vring
    /// addresses. In the ancillary data there is an array of file descriptors
    fn set_mem_table(&self, regions: &[VhostUserMemoryRegionInfo]) -> Result<(), vhost::Error> {
        <Frontend as VhostBackend>::set_mem_table(self, regions)
    }

    /// Set the size of the queue.
    fn set_vring_num(&self, queue_index: usize, num: u16) -> Result<(), vhost::Error> {
        <Frontend as VhostBackend>::set_vring_num(self, queue_index, num)
    }

    /// Sets the addresses of the different aspects of the vring.
    fn set_vring_addr(
        &self,
        queue_index: usize,
        config_data: &VringConfigData,
    ) -> Result<(), vhost::Error> {
        <Frontend as VhostBackend>::set_vring_addr(self, queue_index, config_data)
    }

    /// Sets the base offset in the available vring.
    fn set_vring_base(&self, queue_index: usize, base: u16) -> Result<(), vhost::Error> {
        <Frontend as VhostBackend>::set_vring_base(self, queue_index, base)
    }

    fn get_vring_base(&self, queue_index: usize) -> Result<u32, vhost::Error> {
        <Frontend as VhostBackend>::get_vring_base(self, queue_index)
    }

    fn set_log_base(
        &self,
        base: u64,
        region: Option<VhostUserDirtyLogRegion>,
    ) -> Result<(), vhost::Error> {
        <Frontend as VhostBackend>::set_log_base(self, base, region)
    }

    /// Set the event file descriptor to signal when buffers are used.
    /// Bits (0-7) of the payload contain the vring index. Bit 8 is the invalid FD flag. This flag
    /// is set when there is no file descriptor in the ancillary data. This signals that polling
    /// will be used instead of waiting for the call.
    fn set_vring_call(&self, queue_index: usize, fd: &EventFd) -> Result<(), vhost::Error> {
        <Frontend as VhostBackend>::set_vring_call(self, queue_index, fd)
    }

    /// Set the event file descriptor for adding buffers to the vring.
    /// Bits (0-7) of the payload contain the vring index. Bit 8 is the invalid FD flag. This flag
    /// is set when there is no file descriptor in the ancillary data. This signals that polling
    /// should be used instead of waiting for a kick.
    fn set_vring_kick(&self, queue_index: usize, fd: &EventFd) -> Result<(), vhost::Error> {
        <Frontend as VhostBackend>::set_vring_kick(self, queue_index, fd)
    }

    fn get_protocol_features(&mut self) -> Result<VhostUserProtocolFeatures, vhost::Error> {
        <Frontend as VhostUserFrontend>::get_protocol_features(self)
    }

    fn set_protocol_features(
        &mut self,
        features: VhostUserProtocolFeatures,
    ) -> Result<(), vhost::Error> {
        <Frontend as VhostUserFrontend>::set_protocol_features(self, features)
    }

    fn set_vring_enable(&mut self, queue_index: usize, enable: bool) -> Result<(), vhost::Error> {
        <Frontend as VhostUserFrontend>::set_vring_enable(self, queue_index, enable)
    }

    fn get_config(
        &mut self,
        offset: u32,
        size: u32,
        flags: VhostUserConfigFlags,
        buf: &[u8],
    ) -> Result<(VhostUserConfig, VhostUserConfigPayload), vhost::Error> {
        <Frontend as VhostUserFrontend>::get_config(self, offset, size, flags, buf)
    }

    fn set_config(
        &mut self,
        offset: u32,
        flags: VhostUserConfigFlags,
        buf: &[u8],
    ) -> Result<(), vhost::Error> {
        <Frontend as VhostUserFrontend>::set_config(self, offset, flags, buf)
    }

    fn set_device_state_fd(
        &self,
        direction: VhostTransferStateDirection,
        phase: VhostTransferStatePhase,
        fd: OwnedFd,
    ) -> Result<Option<File>, vhost::Error> {
        <Frontend as VhostUserFrontend>::set_device_state_fd(self, direction, phase, fd)
    }

    fn check_device_state(&self) -> Result<(), vhost::Error> {
        <Frontend as VhostUserFrontend>::check_device_state(self)
    }
}

pub type VhostUserHandle = VhostUserHandleImpl<Frontend>;

/// vhost-user socket handle
#[derive(Clone)]
pub struct VhostUserHandleImpl<T: VhostUserHandleBackend> {
    pub vu: T,
    pub socket_path: String,
}

impl<T: VhostUserHandleBackend> std::fmt::Debug for VhostUserHandleImpl<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VhostUserHandle")
            .field("socket_path", &self.socket_path)
            .finish()
    }
}

impl<T: VhostUserHandleBackend> VhostUserHandleImpl<T> {
    /// Connect to the vhost-user backend socket and mark self as an
    /// owner of the session.
    pub fn new(socket_path: &str, num_queues: u64) -> Result<Self, VhostUserError> {
        let stream = UnixStream::connect(socket_path).map_err(VhostUserError::Connect)?;

        let vu = T::from_stream(stream, num_queues);
        vu.set_owner().map_err(VhostUserError::VhostUserSetOwner)?;

        Ok(Self {
            vu,
            socket_path: socket_path.to_string(),
        })
    }

    /// Set vhost-user features to the backend.
    pub fn set_features(&self, features: u64) -> Result<(), VhostUserError> {
        self.vu
            .set_features(features)
            .map_err(VhostUserError::VhostUserSetFeatures)
    }

    /// Select whether ordinary frontend requests ask the backend for a
    /// REPLY_ACK response. Device activation runs on a seccomp-confined vCPU
    /// thread, so migration-capable devices enable replies only around state
    /// transfer on the VMM thread.
    pub fn set_reply_ack_requests(&self, enabled: bool) {
        self.vu.set_hdr_flags(if enabled {
            VhostUserHeaderFlag::NEED_REPLY
        } else {
            VhostUserHeaderFlag::empty()
        });
    }

    /// Set vhost-user protocol features to the backend.
    pub fn set_protocol_features(
        &mut self,
        acked_features: u64,
        acked_protocol_features: u64,
    ) -> Result<(), VhostUserError> {
        if acked_features & VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits() != 0
            && let Some(acked_protocol_features) =
                VhostUserProtocolFeatures::from_bits(acked_protocol_features)
        {
            self.vu
                .set_protocol_features(acked_protocol_features)
                .map_err(VhostUserError::VhostUserSetProtocolFeatures)?;

            if acked_protocol_features.contains(VhostUserProtocolFeatures::REPLY_ACK) {
                self.vu.set_hdr_flags(VhostUserHeaderFlag::NEED_REPLY);
            }
        }

        Ok(())
    }

    /// Enable or disable a backend vring.
    pub fn set_vring_enabled(
        &mut self,
        queue_index: usize,
        enabled: bool,
    ) -> Result<(), VhostUserError> {
        self.vu
            .set_vring_enable(queue_index, enabled)
            .map_err(VhostUserError::VhostUserSetVringEnable)
    }

    /// Stop a vring and return the backend's next available index.
    pub fn stop_vring(&mut self, queue_index: usize) -> Result<u32, VhostUserError> {
        self.set_vring_enabled(queue_index, false)?;
        self.vu
            .get_vring_base(queue_index)
            .map_err(VhostUserError::VhostUserGetVringBase)
    }

    /// Reinstall the state cleared by `GET_VRING_BASE` and restart a vring.
    pub fn restart_vring(
        &mut self,
        queue_index: usize,
        base: u16,
        call_evt: &EventFd,
        kick_evt: &EventFd,
    ) -> Result<(), VhostUserError> {
        self.vu
            .set_vring_base(queue_index, base)
            .map_err(VhostUserError::VhostUserSetVringBase)?;
        self.vu
            .set_vring_call(queue_index, call_evt)
            .map_err(VhostUserError::VhostUserSetVringCall)?;
        self.vu
            .set_vring_kick(queue_index, kick_evt)
            .map_err(VhostUserError::VhostUserSetVringKick)?;
        self.set_vring_enabled(queue_index, true)
    }

    /// Install the shared dirty-log bitmap used by a vhost-user backend.
    pub fn set_log_base(
        &self,
        base: u64,
        region: VhostUserDirtyLogRegion,
    ) -> Result<(), VhostUserError> {
        self.vu
            .set_log_base(base, Some(region))
            .map_err(VhostUserError::VhostUserSetLogBase)
    }

    /// Begin saving or loading backend-internal device state.
    pub fn set_device_state_fd(
        &self,
        direction: VhostTransferStateDirection,
        fd: OwnedFd,
    ) -> Result<(), VhostUserError> {
        self.vu
            .set_device_state_fd(direction, VhostTransferStatePhase::STOPPED, fd)
            .map(|_| ())
            .map_err(VhostUserError::VhostUserSetDeviceState)
    }

    /// Wait for an asynchronous backend state transfer and report its result.
    pub fn check_device_state(&self) -> Result<(), VhostUserError> {
        self.vu
            .check_device_state()
            .map_err(VhostUserError::VhostUserCheckDeviceState)
    }

    /// Negotiate virtio and protocol features with the backend.
    pub fn negotiate_features(
        &mut self,
        avail_features: u64,
        avail_protocol_features: VhostUserProtocolFeatures,
    ) -> Result<(u64, u64), VhostUserError> {
        // Get features from backend, do negotiation to get a feature collection which
        // both VMM and backend support.
        let backend_features = self
            .vu
            .get_features()
            .map_err(VhostUserError::VhostUserGetFeatures)?;
        let acked_features = avail_features & backend_features;

        let acked_protocol_features =
            // If frontend can negotiate protocol features.
            if acked_features & VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits() != 0 {
                let backend_protocol_features = self
                    .vu
                    .get_protocol_features()
                    .map_err(VhostUserError::VhostUserGetProtocolFeatures)?;

                let acked_protocol_features = avail_protocol_features & backend_protocol_features;

                self.vu
                    .set_protocol_features(acked_protocol_features)
                    .map_err(VhostUserError::VhostUserSetProtocolFeatures)?;

                acked_protocol_features
            } else {
                VhostUserProtocolFeatures::empty()
            };

        if acked_protocol_features.contains(VhostUserProtocolFeatures::REPLY_ACK) {
            self.vu.set_hdr_flags(VhostUserHeaderFlag::NEED_REPLY);
        }

        Ok((acked_features, acked_protocol_features.bits()))
    }

    /// Update guest memory table to the backend.
    pub(crate) fn update_mem_table(&self, mem: &GuestMemoryMmap) -> Result<(), VhostUserError> {
        let mut regions: Vec<VhostUserMemoryRegionInfo> = Vec::new();

        for region in mem.iter() {
            let (mmap_handle, mmap_offset) = match region.file_offset() {
                Some(_file_offset) => (_file_offset.file().as_raw_fd(), _file_offset.start()),
                None => {
                    return Err(VhostUserError::VhostUserNoMemoryRegion);
                }
            };

            let vhost_user_net_reg = VhostUserMemoryRegionInfo {
                guest_phys_addr: region.start_addr().raw_value(),
                memory_size: region.len(),
                userspace_addr: region.inner.as_ptr() as u64,
                mmap_offset,
                mmap_handle,
            };
            regions.push(vhost_user_net_reg);
        }

        self.vu
            .set_mem_table(regions.as_slice())
            .map_err(VhostUserError::VhostUserSetMemTable)?;

        Ok(())
    }

    /// Set up vhost-user backend. This includes updating memory table,
    /// sending information about virtio rings and enabling them.
    pub fn setup_backend(
        &mut self,
        mem: &GuestMemoryMmap,
        queues: &[(usize, &Queue, &EventFd)],
        interrupt: Arc<dyn VirtioInterrupt>,
    ) -> Result<(), VhostUserError> {
        self.setup_backend_stopped(mem, queues, interrupt)?;
        for (queue_index, _, _) in queues {
            self.set_vring_enabled(*queue_index, true)?;
        }
        Ok(())
    }

    /// Set up and enable vrings after the caller has already installed the
    /// backend memory table.
    pub(crate) fn setup_backend_after_mem_table(
        &mut self,
        mem: &GuestMemoryMmap,
        queues: &[(usize, &Queue, &EventFd)],
        interrupt: Arc<dyn VirtioInterrupt>,
    ) -> Result<(), VhostUserError> {
        self.setup_backend_stopped_after_mem_table(mem, queues, interrupt)?;
        for (queue_index, _, _) in queues {
            self.set_vring_enabled(*queue_index, true)?;
        }
        Ok(())
    }

    /// Configure all backend vrings but leave them disabled.
    pub fn setup_backend_stopped(
        &mut self,
        mem: &GuestMemoryMmap,
        queues: &[(usize, &Queue, &EventFd)],
        interrupt: Arc<dyn VirtioInterrupt>,
    ) -> Result<(), VhostUserError> {
        // Provide the memory table to the backend.
        self.update_mem_table(mem)?;

        self.setup_backend_stopped_after_mem_table(mem, queues, interrupt)
    }

    /// Configure vrings while preserving a memory table that the caller has
    /// already installed. This is required when protocol state tied to the
    /// memory regions, such as LOG_SHMFD bitmaps, was attached after
    /// SET_MEM_TABLE and must not be discarded by replacing the regions.
    pub(crate) fn setup_backend_stopped_after_mem_table(
        &mut self,
        mem: &GuestMemoryMmap,
        queues: &[(usize, &Queue, &EventFd)],
        interrupt: Arc<dyn VirtioInterrupt>,
    ) -> Result<(), VhostUserError> {
        // Send set_vring_num here, since it could tell backends, like SPDK,
        // how many virt queues to be handled, which backend required to know
        // at early stage.
        for (queue_index, queue, _) in queues.iter() {
            self.vu
                .set_vring_num(*queue_index, queue.size)
                .map_err(VhostUserError::VhostUserSetVringNum)?;
        }

        for (queue_index, queue, queue_evt) in queues.iter() {
            let config_data = VringConfigData {
                queue_max_size: queue.max_size,
                queue_size: queue.size,
                flags: 0u32,
                desc_table_addr: mem
                    .get_host_address(queue.desc_table_address)
                    .map_err(VhostUserError::DescriptorTableAddress)?
                    as u64,
                used_ring_addr: mem
                    .get_host_address(queue.used_ring_address)
                    .map_err(VhostUserError::UsedAddress)? as u64,
                avail_ring_addr: mem
                    .get_host_address(queue.avail_ring_address)
                    .map_err(VhostUserError::AvailAddress)? as u64,
                log_addr: None,
            };

            self.vu
                .set_vring_addr(*queue_index, &config_data)
                .map_err(VhostUserError::VhostUserSetVringAddr)?;
            self.vu
                .set_vring_base(*queue_index, queue.next_avail.0)
                .map_err(VhostUserError::VhostUserSetVringBase)?;

            // No matter the queue, we set irq_evt for signaling the guest that buffers were
            // consumed.
            self.vu
                .set_vring_call(
                    *queue_index,
                    interrupt
                        .notifier(VirtioInterruptType::Queue(
                            (*queue_index).try_into().unwrap_or_else(|_| {
                                panic!("vhost-user: invalid queue index: {}", *queue_index)
                            }),
                        ))
                        .as_ref()
                        .unwrap(),
                )
                .map_err(VhostUserError::VhostUserSetVringCall)?;

            self.vu
                .set_vring_kick(*queue_index, queue_evt)
                .map_err(VhostUserError::VhostUserSetVringKick)?;
        }

        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::undocumented_unsafe_blocks)]

    use std::fs::File;

    use vmm_sys_util::tempfile::TempFile;

    use super::*;
    use crate::devices::virtio::test_utils::default_interrupt;
    use crate::test_utils::create_tmp_socket;
    use crate::vstate::memory;
    use crate::vstate::memory::{GuestAddress, GuestRegionMmapExt};

    pub(crate) fn create_mem(file: File, regions: &[(GuestAddress, usize)]) -> GuestMemoryMmap {
        GuestMemoryMmap::from_regions(
            memory::create(
                regions.iter().copied(),
                libc::MAP_PRIVATE,
                Some(file),
                false,
            )
            .unwrap()
            .into_iter()
            .map(|region| GuestRegionMmapExt::dram_from_mmap_region(region, 0))
            .collect(),
        )
        .unwrap()
    }

    #[test]
    fn test_new() {
        struct MockFrontend {
            sock: UnixStream,
            max_queue_num: u64,
            is_owner: std::cell::UnsafeCell<bool>,
        }

        impl VhostUserHandleBackend for MockFrontend {
            fn from_stream(sock: UnixStream, max_queue_num: u64) -> Self {
                Self {
                    sock,
                    max_queue_num,
                    is_owner: std::cell::UnsafeCell::new(false),
                }
            }

            fn set_owner(&self) -> Result<(), vhost::Error> {
                unsafe { *self.is_owner.get() = true };
                Ok(())
            }
        }

        let max_queue_num = 69;

        let (_tmp_dir, tmp_socket_path) = create_tmp_socket();

        // Creation of the VhostUserHandleImpl correctly connects to the socket, sets the maximum
        // number of queues and sets itself as an owner of the session.
        let vuh =
            VhostUserHandleImpl::<MockFrontend>::new(&tmp_socket_path, max_queue_num).unwrap();
        assert_eq!(
            vuh.vu
                .sock
                .peer_addr()
                .unwrap()
                .as_pathname()
                .unwrap()
                .to_str()
                .unwrap(),
            &tmp_socket_path,
        );
        assert_eq!(vuh.vu.max_queue_num, max_queue_num);
        assert!(unsafe { *vuh.vu.is_owner.get() });
    }

    #[test]
    fn test_set_features() {
        struct MockFrontend {
            features: std::cell::UnsafeCell<u64>,
        }

        impl VhostUserHandleBackend for MockFrontend {
            fn set_features(&self, features: u64) -> Result<(), vhost::Error> {
                unsafe { *self.features.get() = features };
                Ok(())
            }
        }

        // VhostUserHandleImpl can correctly set backend features.
        let vuh = VhostUserHandleImpl {
            vu: MockFrontend { features: 0.into() },
            socket_path: "".to_string(),
        };
        vuh.set_features(0x69).unwrap();
        assert_eq!(unsafe { *vuh.vu.features.get() }, 0x69);
    }

    #[test]
    fn test_set_reply_ack_requests() {
        struct MockFrontend {
            hdr_flags: std::cell::UnsafeCell<VhostUserHeaderFlag>,
        }

        impl VhostUserHandleBackend for MockFrontend {
            fn set_hdr_flags(&self, flags: VhostUserHeaderFlag) {
                unsafe { *self.hdr_flags.get() = flags };
            }
        }

        let vuh = VhostUserHandleImpl {
            vu: MockFrontend {
                hdr_flags: std::cell::UnsafeCell::new(VhostUserHeaderFlag::empty()),
            },
            socket_path: String::new(),
        };
        vuh.set_reply_ack_requests(true);
        assert_eq!(
            unsafe { &*vuh.vu.hdr_flags.get() }.bits(),
            VhostUserHeaderFlag::NEED_REPLY.bits()
        );
        vuh.set_reply_ack_requests(false);
        assert_eq!(
            unsafe { &*vuh.vu.hdr_flags.get() }.bits(),
            VhostUserHeaderFlag::empty().bits()
        );
    }

    #[test]
    fn test_set_protocol_features() {
        struct MockFrontend {
            protocol_features: VhostUserProtocolFeatures,
            hdr_flags: std::cell::UnsafeCell<VhostUserHeaderFlag>,
        }

        impl VhostUserHandleBackend for MockFrontend {
            fn set_hdr_flags(&self, flags: VhostUserHeaderFlag) {
                unsafe { *self.hdr_flags.get() = flags };
            }

            fn set_protocol_features(
                &mut self,
                features: VhostUserProtocolFeatures,
            ) -> Result<(), vhost::Error> {
                self.protocol_features = features;
                Ok(())
            }
        }

        let mut vuh = VhostUserHandleImpl {
            vu: MockFrontend {
                protocol_features: VhostUserProtocolFeatures::empty(),
                hdr_flags: std::cell::UnsafeCell::new(VhostUserHeaderFlag::empty()),
            },
            socket_path: "".to_string(),
        };

        // No protocol features are set if acked_features do not have PROTOCOL_FEATURES bit
        let acked_features = 0;
        let acked_protocol_features = VhostUserProtocolFeatures::empty();
        vuh.set_protocol_features(acked_features, acked_protocol_features.bits())
            .unwrap();
        assert_eq!(vuh.vu.protocol_features, VhostUserProtocolFeatures::empty());
        assert_eq!(
            unsafe { &*vuh.vu.hdr_flags.get() }.bits(),
            VhostUserHeaderFlag::empty().bits()
        );

        // No protocol features are set if acked_features do not have PROTOCOL_FEATURES bit
        let acked_features = 0;
        let acked_protocol_features = VhostUserProtocolFeatures::all();
        vuh.set_protocol_features(acked_features, acked_protocol_features.bits())
            .unwrap();
        assert_eq!(vuh.vu.protocol_features, VhostUserProtocolFeatures::empty());
        assert_eq!(
            unsafe { &*vuh.vu.hdr_flags.get() }.bits(),
            VhostUserHeaderFlag::empty().bits()
        );

        // If not REPLY_ACK present, no header is set
        let acked_features = VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits();
        let mut acked_protocol_features = VhostUserProtocolFeatures::all();
        acked_protocol_features.set(VhostUserProtocolFeatures::REPLY_ACK, false);
        vuh.set_protocol_features(acked_features, acked_protocol_features.bits())
            .unwrap();
        assert_eq!(vuh.vu.protocol_features, acked_protocol_features);
        assert_eq!(
            unsafe { &*vuh.vu.hdr_flags.get() }.bits(),
            VhostUserHeaderFlag::empty().bits()
        );

        // If REPLY_ACK present, header is set
        let acked_features = VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits();
        let acked_protocol_features = VhostUserProtocolFeatures::all();
        vuh.set_protocol_features(acked_features, acked_protocol_features.bits())
            .unwrap();
        assert_eq!(vuh.vu.protocol_features, acked_protocol_features);
        assert_eq!(
            unsafe { &*vuh.vu.hdr_flags.get() }.bits(),
            VhostUserHeaderFlag::NEED_REPLY.bits()
        );
    }

    #[test]
    fn test_negotiate_features() {
        struct MockFrontend {
            features: u64,
            protocol_features: VhostUserProtocolFeatures,
            hdr_flags: std::cell::UnsafeCell<VhostUserHeaderFlag>,
        }

        impl VhostUserHandleBackend for MockFrontend {
            fn set_hdr_flags(&self, flags: VhostUserHeaderFlag) {
                unsafe { *self.hdr_flags.get() = flags };
            }

            fn get_features(&self) -> Result<u64, vhost::Error> {
                Ok(self.features)
            }

            fn get_protocol_features(&mut self) -> Result<VhostUserProtocolFeatures, vhost::Error> {
                Ok(self.protocol_features)
            }

            fn set_protocol_features(
                &mut self,
                features: VhostUserProtocolFeatures,
            ) -> Result<(), vhost::Error> {
                self.protocol_features = features;
                Ok(())
            }
        }

        let mut vuh = VhostUserHandleImpl {
            vu: MockFrontend {
                features: 0,
                protocol_features: VhostUserProtocolFeatures::empty(),
                hdr_flags: std::cell::UnsafeCell::new(VhostUserHeaderFlag::empty()),
            },
            socket_path: "".to_string(),
        };

        // If nothing is available, nothing is negotiated
        let avail_features = 0;
        let avail_protocol_features = VhostUserProtocolFeatures::empty();
        let (acked_features, acked_protocol_features) = vuh
            .negotiate_features(avail_features, avail_protocol_features)
            .unwrap();
        assert_eq!(acked_features, avail_features);
        assert_eq!(acked_protocol_features, avail_protocol_features.bits());
        assert_eq!(vuh.vu.protocol_features, VhostUserProtocolFeatures::empty());
        assert_eq!(
            unsafe { &*vuh.vu.hdr_flags.get() }.bits(),
            VhostUserHeaderFlag::empty().bits()
        );

        // If neither frontend avail_features nor backend avail_features contain PROTOCOL_FEATURES
        // bit, only features are negotiated
        let mut avail_features = VhostUserVirtioFeatures::all();
        avail_features.set(VhostUserVirtioFeatures::PROTOCOL_FEATURES, false);

        // Pretend backend has same features as frontend
        vuh.vu.features = avail_features.bits();

        let avail_protocol_features = VhostUserProtocolFeatures::empty();
        let (acked_features, acked_protocol_features) = vuh
            .negotiate_features(avail_features.bits(), avail_protocol_features)
            .unwrap();
        assert_eq!(acked_features, avail_features.bits());
        assert_eq!(acked_protocol_features, avail_protocol_features.bits());
        assert_eq!(vuh.vu.protocol_features, VhostUserProtocolFeatures::empty());
        assert_eq!(
            unsafe { &*vuh.vu.hdr_flags.get() }.bits(),
            VhostUserHeaderFlag::empty().bits()
        );

        // If PROTOCOL_FEATURES is negotiated, but REPLY_ACK is not, headers are not set
        let avail_features = VhostUserVirtioFeatures::all();
        // Pretend backend has same features as frontend
        vuh.vu.features = avail_features.bits();

        let mut avail_protocol_features = VhostUserProtocolFeatures::empty();
        avail_protocol_features.set(VhostUserProtocolFeatures::CONFIG, true);

        let mut backend_protocol_features = VhostUserProtocolFeatures::empty();
        backend_protocol_features.set(VhostUserProtocolFeatures::CONFIG, true);
        backend_protocol_features.set(VhostUserProtocolFeatures::PAGEFAULT, true);
        vuh.vu.protocol_features = backend_protocol_features;

        let (acked_features, acked_protocol_features) = vuh
            .negotiate_features(avail_features.bits(), avail_protocol_features)
            .unwrap();
        assert_eq!(acked_features, avail_features.bits());
        assert_eq!(acked_protocol_features, avail_protocol_features.bits());
        assert_eq!(vuh.vu.protocol_features, avail_protocol_features);
        assert_eq!(
            unsafe { &*vuh.vu.hdr_flags.get() }.bits(),
            VhostUserHeaderFlag::empty().bits()
        );

        // If PROTOCOL_FEATURES and REPLY_ACK are negotiated
        let avail_features = VhostUserVirtioFeatures::all();
        // Pretend backend has same features as frontend
        vuh.vu.features = avail_features.bits();

        let mut avail_protocol_features = VhostUserProtocolFeatures::empty();
        avail_protocol_features.set(VhostUserProtocolFeatures::REPLY_ACK, true);

        // Pretend backend has same features as frontend
        vuh.vu.protocol_features = avail_protocol_features;

        let (acked_features, acked_protocol_features) = vuh
            .negotiate_features(avail_features.bits(), avail_protocol_features)
            .unwrap();
        assert_eq!(acked_features, avail_features.bits());
        assert_eq!(acked_protocol_features, avail_protocol_features.bits());
        assert_eq!(vuh.vu.protocol_features, avail_protocol_features);
        assert_eq!(
            unsafe { &*vuh.vu.hdr_flags.get() }.bits(),
            VhostUserHeaderFlag::NEED_REPLY.bits(),
        );
    }

    #[test]
    fn test_update_mem_table() {
        struct MockFrontend {
            regions: std::cell::UnsafeCell<Vec<VhostUserMemoryRegionInfo>>,
        }

        impl VhostUserHandleBackend for MockFrontend {
            fn set_mem_table(
                &self,
                regions: &[VhostUserMemoryRegionInfo],
            ) -> Result<(), vhost::Error> {
                unsafe { (*self.regions.get()).extend_from_slice(regions) }
                Ok(())
            }
        }

        let vuh = VhostUserHandleImpl {
            vu: MockFrontend {
                regions: std::cell::UnsafeCell::new(vec![]),
            },
            socket_path: "".to_string(),
        };

        let region_size = 0x10000;
        let file = TempFile::new().unwrap().into_file();
        let file_size = 2 * region_size;
        file.set_len(file_size as u64).unwrap();
        let regions = vec![
            (GuestAddress(0x0), region_size),
            (GuestAddress(0x10000), region_size),
        ];

        let guest_memory = create_mem(file, &regions);

        vuh.update_mem_table(&guest_memory).unwrap();

        // VhostUserMemoryRegionInfo should be correctly set by the VhostUserHandleImpl
        let expected_regions = guest_memory
            .iter()
            .map(|region| VhostUserMemoryRegionInfo {
                guest_phys_addr: region.start_addr().raw_value(),
                memory_size: region.len(),
                userspace_addr: region.inner.as_ptr() as u64,
                mmap_offset: region.file_offset().unwrap().start(),
                mmap_handle: region.file_offset().unwrap().file().as_raw_fd(),
            })
            .collect::<Vec<_>>();

        for (region, expected) in (unsafe { &*vuh.vu.regions.get() })
            .iter()
            .zip(expected_regions)
        {
            // VhostUserMemoryRegionInfo does not implement Eq.
            assert_eq!(region.guest_phys_addr, expected.guest_phys_addr);
            assert_eq!(region.memory_size, expected.memory_size);
            assert_eq!(region.userspace_addr, expected.userspace_addr);
            assert_eq!(region.mmap_offset, expected.mmap_offset);
            assert_eq!(region.mmap_handle, expected.mmap_handle);
        }
    }

    #[test]
    fn test_setup_backend() {
        #[derive(Default)]
        struct VringData {
            index: usize,
            size: u16,
            config: VringConfigData,
            base: u16,
            call: i32,
            kick: i32,
            enable: bool,
        }

        struct MockFrontend {
            vrings: std::cell::UnsafeCell<Vec<VringData>>,
            mem_table_updates: std::cell::UnsafeCell<usize>,
        }

        impl VhostUserHandleBackend for MockFrontend {
            fn set_mem_table(
                &self,
                _regions: &[VhostUserMemoryRegionInfo],
            ) -> Result<(), vhost::Error> {
                unsafe { *self.mem_table_updates.get() += 1 };
                Ok(())
            }

            fn set_vring_num(&self, queue_index: usize, num: u16) -> Result<(), vhost::Error> {
                unsafe {
                    (*self.vrings.get()).push(VringData {
                        index: queue_index,
                        size: num,
                        ..Default::default()
                    })
                };
                Ok(())
            }

            fn set_vring_addr(
                &self,
                queue_index: usize,
                config_data: &VringConfigData,
            ) -> Result<(), vhost::Error> {
                unsafe { (&mut (*self.vrings.get()))[queue_index].config = *config_data };
                Ok(())
            }

            fn set_vring_base(&self, queue_index: usize, base: u16) -> Result<(), vhost::Error> {
                unsafe { (&mut (*self.vrings.get()))[queue_index].base = base };
                Ok(())
            }

            fn set_vring_call(&self, queue_index: usize, fd: &EventFd) -> Result<(), vhost::Error> {
                unsafe { (&mut (*self.vrings.get()))[queue_index].call = fd.as_raw_fd() };
                Ok(())
            }

            fn set_vring_kick(&self, queue_index: usize, fd: &EventFd) -> Result<(), vhost::Error> {
                unsafe { (&mut (*self.vrings.get()))[queue_index].kick = fd.as_raw_fd() };
                Ok(())
            }

            fn set_vring_enable(
                &mut self,
                queue_index: usize,
                enable: bool,
            ) -> Result<(), vhost::Error> {
                unsafe { &mut *self.vrings.get() }
                    .get_mut(queue_index)
                    .unwrap()
                    .enable = enable;
                Ok(())
            }
        }

        let mut vuh = VhostUserHandleImpl {
            vu: MockFrontend {
                vrings: std::cell::UnsafeCell::new(vec![]),
                mem_table_updates: std::cell::UnsafeCell::new(0),
            },
            socket_path: "".to_string(),
        };

        let region_size = 0x10000;
        let file = TempFile::new().unwrap().into_file();
        file.set_len(region_size as u64).unwrap();
        let regions = vec![(GuestAddress(0x0), region_size)];

        let guest_memory = create_mem(file, &regions);

        let mut queue = Queue::new(128);
        queue.ready = true;
        queue.size = queue.max_size;
        queue.initialize(&guest_memory).unwrap();

        let event_fd = EventFd::new(0).unwrap();

        let queues = [(0, &queue, &event_fd)];

        let interrupt = default_interrupt();
        vuh.setup_backend(&guest_memory, &queues, interrupt.clone())
            .unwrap();
        assert_eq!(unsafe { *vuh.vu.mem_table_updates.get() }, 1);

        // VhostUserHandleImpl should correctly send memory and queues information to
        // the backend.
        let expected_config = VringData {
            index: 0,
            size: 128,
            config: VringConfigData {
                queue_max_size: 128,
                queue_size: 128,
                flags: 0,
                desc_table_addr: guest_memory
                    .get_host_address(queue.desc_table_address)
                    .unwrap() as u64,
                used_ring_addr: guest_memory
                    .get_host_address(queue.used_ring_address)
                    .unwrap() as u64,
                avail_ring_addr: guest_memory
                    .get_host_address(queue.avail_ring_address)
                    .unwrap() as u64,
                log_addr: None,
            },
            base: queue.avail_ring_idx_get(),
            call: interrupt
                .notifier(VirtioInterruptType::Queue(0u16))
                .as_ref()
                .unwrap()
                .as_raw_fd(),
            kick: event_fd.as_raw_fd(),
            enable: true,
        };

        let result = unsafe { &*vuh.vu.vrings.get() };
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].index, expected_config.index);
        assert_eq!(result[0].size, expected_config.size);

        // VringConfigData does not implement Eq.
        assert_eq!(
            result[0].config.queue_max_size,
            expected_config.config.queue_max_size
        );
        assert_eq!(
            result[0].config.queue_size,
            expected_config.config.queue_size
        );
        assert_eq!(result[0].config.flags, expected_config.config.flags);
        assert_eq!(
            result[0].config.desc_table_addr,
            expected_config.config.desc_table_addr
        );
        assert_eq!(
            result[0].config.used_ring_addr,
            expected_config.config.used_ring_addr
        );
        assert_eq!(
            result[0].config.avail_ring_addr,
            expected_config.config.avail_ring_addr
        );
        assert_eq!(result[0].config.log_addr, expected_config.config.log_addr);

        assert_eq!(result[0].base, expected_config.base);
        assert_eq!(result[0].call, expected_config.call);
        assert_eq!(result[0].kick, expected_config.kick);
        assert_eq!(result[0].enable, expected_config.enable);

        // A caller that installed SET_MEM_TABLE followed by memory-bound
        // protocol state (for example LOG_SHMFD) can configure the same
        // vrings without replacing those backend regions.
        unsafe { (*vuh.vu.vrings.get()).clear() };
        vuh.setup_backend_after_mem_table(&guest_memory, &queues, interrupt.clone())
            .unwrap();
        assert_eq!(unsafe { *vuh.vu.mem_table_updates.get() }, 1);
        assert_eq!(unsafe { &*vuh.vu.vrings.get() }.len(), 1);

        // GET_VRING_BASE stops the ring and clears call/kick in a real
        // backend. Restart must therefore reinstall more than the enable bit.
        {
            let result = unsafe { &mut *vuh.vu.vrings.get() };
            result[0].base = 0;
            result[0].call = -1;
            result[0].kick = -1;
            result[0].enable = false;
        }
        let restart_base = 37;
        vuh.restart_vring(
            0,
            restart_base,
            interrupt
                .notifier(VirtioInterruptType::Queue(0u16))
                .as_ref()
                .unwrap(),
            &event_fd,
        )
        .unwrap();
        let result = unsafe { &*vuh.vu.vrings.get() };
        assert_eq!(result[0].base, restart_base);
        assert_eq!(result[0].call, expected_config.call);
        assert_eq!(result[0].kick, expected_config.kick);
        assert!(result[0].enable);
    }
}
