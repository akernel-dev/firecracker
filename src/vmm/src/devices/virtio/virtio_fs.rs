// Copyright 2026 Ant Group Corporation. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A vhost-user virtio-fs frontend.

use std::fs::{File, OpenOptions};
use std::num::Wrapping;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use event_manager::{EventOps, Events, MutEventSubscriber};
use vhost::VhostUserDirtyLogRegion;
use vhost::vhost_user::message::{
    VhostTransferStateDirection, VhostUserProtocolFeatures, VhostUserVirtioFeatures,
};
use vm_memory::{Address, GuestMemory, GuestMemoryRegion};
use vmm_sys_util::eventfd::EventFd;

use crate::devices::virtio::ActivateError;
use crate::devices::virtio::device::{ActiveState, DeviceState, VirtioDevice, VirtioDeviceType};
use crate::devices::virtio::generated::virtio_config::VIRTIO_F_VERSION_1;
use crate::devices::virtio::generated::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
use crate::devices::virtio::queue::Queue;
use crate::devices::virtio::transport::{VirtioInterrupt, VirtioInterruptType};
use crate::devices::virtio::vhost_user::{VhostUserHandle, VhostUserHandleImpl};
use crate::impl_device_type;
use crate::vmm_config::virtio_fs::{FS_TAG_MAX_LEN, FsConfig};
use crate::vstate::memory::GuestMemoryMmap;
use crate::vstate::pagemap_anon::MemoryRange;

pub mod persist;

/// One high-priority queue and one request queue.
pub const FS_NUM_QUEUES: usize = 2;
/// Queue size supported by virtiofsd.
pub const FS_QUEUE_SIZE: u16 = 1024;
const NUM_REQUEST_QUEUES: u32 = 1;
const VHOST_LOG_PAGE_SIZE: u64 = 4096;

const AVAILABLE_FEATURES: u64 = (1 << VIRTIO_F_VERSION_1) | (1 << VIRTIO_RING_F_EVENT_IDX);
const REQUESTED_FEATURES: u64 = AVAILABLE_FEATURES
    | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
    | VhostUserVirtioFeatures::LOG_ALL.bits();
const REQUESTED_PROTOCOL_FEATURES: VhostUserProtocolFeatures =
    VhostUserProtocolFeatures::from_bits_retain(
        VhostUserProtocolFeatures::MQ.bits()
            | VhostUserProtocolFeatures::REPLY_ACK.bits()
            | VhostUserProtocolFeatures::LOG_SHMFD.bits()
            | VhostUserProtocolFeatures::DEVICE_STATE.bits(),
    );
const REQUIRED_PROTOCOL_FEATURES: VhostUserProtocolFeatures =
    VhostUserProtocolFeatures::from_bits_retain(
        VhostUserProtocolFeatures::MQ.bits()
            | VhostUserProtocolFeatures::REPLY_ACK.bits()
            | VhostUserProtocolFeatures::LOG_SHMFD.bits()
            | VhostUserProtocolFeatures::DEVICE_STATE.bits(),
    );

/// Errors raised while creating or operating a virtio-fs device.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum VirtioFsError {
    /// Vhost-user operation failed: {0}
    VhostUser(#[from] crate::devices::virtio::vhost_user::VhostUserError),
    /// Failed to create a queue eventfd: {0}
    EventFd(std::io::Error),
    /// Backend does not provide required virtio feature `{0}`.
    MissingVirtioFeature(&'static str),
    /// Backend does not provide required protocol features: {0:?}.
    MissingProtocolFeatures(VhostUserProtocolFeatures),
    /// Backend does not provide VHOST_F_LOG_ALL.
    MissingLogAll,
    /// Snapshot operation requires an activated virtio-fs device.
    NotActivated,
    /// A virtio-fs snapshot operation is already in progress.
    SnapshotAlreadyPrepared,
    /// The virtio-fs dirty log was not armed before snapshot creation.
    DirtyLogNotArmed,
    /// Invalid vring base returned by backend: {0}.
    InvalidVringBase(u32),
    /// Cannot {operation} virtio-fs state file: {source}
    StateFile {
        /// Operation that failed.
        operation: &'static str,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// Restored guest features are not supported by the replacement backend.
    IncompatibleBackendFeatures,
    /// Cannot create the vhost dirty-log memfd: {0}
    DirtyLogMemfd(#[from] memfd::Error),
    /// Cannot resize the vhost dirty-log memfd: {0}
    DirtyLogResize(std::io::Error),
    /// Cannot mmap the vhost dirty-log memfd: {0}
    DirtyLogMmap(std::io::Error),
    /// Guest address space is too large for the host dirty-log bitmap.
    DirtyLogSize,
}

#[derive(Debug)]
struct DirtyLog {
    memfd: memfd::Memfd,
    addr: *mut u8,
    mmap_len: usize,
    guest_pages: usize,
}

// SAFETY: the mapping is owned by this object. It is read or cleared only
// while all backend vrings are stopped.
unsafe impl Send for DirtyLog {}

impl DirtyLog {
    fn new(mem: &GuestMemoryMmap) -> Result<Self, VirtioFsError> {
        let max_end = mem
            .iter()
            .map(|region| region.start_addr().raw_value() + region.len())
            .max()
            .unwrap_or(0);
        let guest_pages = usize::try_from(max_end.div_ceil(VHOST_LOG_PAGE_SIZE))
            .map_err(|_| VirtioFsError::DirtyLogSize)?;
        let bitmap_bytes = guest_pages.div_ceil(8);
        let host_page_size = crate::arch::host_page_size();
        let mmap_len = bitmap_bytes.max(1).div_ceil(host_page_size) * host_page_size;

        let memfd = memfd::MemfdOptions::default()
            .allow_sealing(true)
            .create("virtiofs_dirty_log")?;
        memfd
            .as_file()
            .set_len(mmap_len as u64)
            .map_err(VirtioFsError::DirtyLogResize)?;
        memfd.add_seals(&[
            memfd::FileSeal::SealShrink,
            memfd::FileSeal::SealGrow,
            memfd::FileSeal::SealSeal,
        ])?;

        // SAFETY: the fd has exactly mmap_len bytes and remains owned by
        // this object for at least as long as the mapping.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                mmap_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                memfd.as_file().as_raw_fd(),
                0,
            )
        };
        if addr == libc::MAP_FAILED {
            return Err(VirtioFsError::DirtyLogMmap(std::io::Error::last_os_error()));
        }

        Ok(Self {
            memfd,
            addr: addr.cast(),
            mmap_len,
            guest_pages,
        })
    }

    fn region(&self) -> VhostUserDirtyLogRegion {
        VhostUserDirtyLogRegion {
            mmap_size: self.mmap_len as u64,
            mmap_offset: 0,
            mmap_handle: self.memfd.as_file().as_raw_fd(),
        }
    }

    fn ranges(&self) -> Vec<MemoryRange> {
        let mut ranges: Vec<MemoryRange> = Vec::new();
        for page in 0..self.guest_pages {
            // SAFETY: page/8 is within the mapped bitmap.
            let byte = unsafe { self.addr.add(page / 8).read_volatile() };
            if byte & (1 << (page % 8)) == 0 {
                continue;
            }
            let gpa = page as u64 * VHOST_LOG_PAGE_SIZE;
            if let Some(last) = ranges.last_mut()
                && last.gpa + last.length == gpa
            {
                last.length += VHOST_LOG_PAGE_SIZE;
            } else {
                ranges.push(MemoryRange {
                    gpa,
                    length: VHOST_LOG_PAGE_SIZE,
                });
            }
        }
        ranges
    }

    fn clear(&mut self) {
        // SAFETY: all vrings are stopped and this object owns the mapping.
        unsafe { std::ptr::write_bytes(self.addr, 0, self.mmap_len) };
    }
}

impl Drop for DirtyLog {
    fn drop(&mut self) {
        // SAFETY: addr/mmap_len describe the live mapping owned by self.
        let _ = unsafe { libc::munmap(self.addr.cast(), self.mmap_len) };
    }
}

/// A single virtio-fs device backed by one virtiofsd process.
#[derive(Debug)]
pub struct VirtioFs {
    id: String,
    socket_path: String,
    tag: [u8; FS_TAG_MAX_LEN],
    vu_handle: VhostUserHandle,
    device_state: DeviceState,
    queues: [Queue; FS_NUM_QUEUES],
    queue_evts: [EventFd; FS_NUM_QUEUES],
    avail_features: u64,
    acked_features: u64,
    backend_features: u64,
    protocol_features: u64,
    restore_state_path: Option<PathBuf>,
    snapshot_prepared: bool,
    dirty_log: Option<DirtyLog>,
    dirty_logging_armed: bool,
    snapshot_dirty_ranges: Vec<MemoryRange>,
}

impl VirtioFs {
    /// Connect to virtiofsd and negotiate the migration-capable protocol.
    pub fn new(config: FsConfig) -> Result<Self, VirtioFsError> {
        Self::new_with_state(
            config,
            [Queue::new(FS_QUEUE_SIZE), Queue::new(FS_QUEUE_SIZE)],
            0,
            None,
        )
    }

    pub(crate) fn new_with_state(
        config: FsConfig,
        queues: [Queue; FS_NUM_QUEUES],
        acked_features: u64,
        restore_state_path: Option<PathBuf>,
    ) -> Result<Self, VirtioFsError> {
        config.validate().map_err(|_| VirtioFsError::StateFile {
            operation: "validate configuration",
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid virtio-fs configuration",
            ),
        })?;
        let mut vu_handle = VhostUserHandleImpl::new(&config.socket_path, FS_NUM_QUEUES as u64)?;
        let (backend_features, protocol_features) =
            vu_handle.negotiate_features(REQUESTED_FEATURES, REQUESTED_PROTOCOL_FEATURES)?;
        // The generic negotiation helper enables NEED_REPLY when REPLY_ACK is
        // available. Fresh-device activation happens on a seccomp-confined
        // vCPU thread, which intentionally cannot recvmsg. Keep activation
        // requests one-way and enable acknowledgements only for migration
        // control operations on the VMM thread.
        vu_handle.set_reply_ack_requests(false);

        if backend_features & (1 << VIRTIO_F_VERSION_1) == 0 {
            return Err(VirtioFsError::MissingVirtioFeature("VIRTIO_F_VERSION_1"));
        }
        if backend_features & VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits() == 0 {
            return Err(VirtioFsError::MissingVirtioFeature(
                "VHOST_USER_F_PROTOCOL_FEATURES",
            ));
        }
        let negotiated_protocol = VhostUserProtocolFeatures::from_bits_retain(protocol_features);
        let missing = REQUIRED_PROTOCOL_FEATURES & !negotiated_protocol;
        if !missing.is_empty() {
            return Err(VirtioFsError::MissingProtocolFeatures(missing));
        }
        if backend_features & VhostUserVirtioFeatures::LOG_ALL.bits() == 0 {
            return Err(VirtioFsError::MissingLogAll);
        }

        let avail_features = backend_features
            & AVAILABLE_FEATURES
            & !VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits();
        if acked_features & !avail_features != 0 {
            return Err(VirtioFsError::IncompatibleBackendFeatures);
        }

        let mut tag = [0_u8; FS_TAG_MAX_LEN];
        tag[..config.tag.len()].copy_from_slice(config.tag.as_bytes());

        Ok(Self {
            id: config.fs_id,
            socket_path: config.socket_path,
            tag,
            vu_handle,
            device_state: DeviceState::Inactive,
            queues,
            queue_evts: [
                EventFd::new(libc::EFD_NONBLOCK).map_err(VirtioFsError::EventFd)?,
                EventFd::new(libc::EFD_NONBLOCK).map_err(VirtioFsError::EventFd)?,
            ],
            avail_features,
            acked_features,
            backend_features,
            protocol_features,
            restore_state_path,
            snapshot_prepared: false,
            dirty_log: None,
            dirty_logging_armed: false,
            snapshot_dirty_ranges: Vec::new(),
        })
    }

    /// Return the API configuration represented by this device.
    pub fn config(&self) -> FsConfig {
        let tag_len = self
            .tag
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(FS_TAG_MAX_LEN);
        FsConfig {
            fs_id: self.id.clone(),
            socket_path: self.socket_path.clone(),
            tag: String::from_utf8_lossy(&self.tag[..tag_len]).into_owned(),
        }
    }

    fn session_features(&self) -> u64 {
        self.acked_features
            | (self.backend_features & VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits())
    }

    fn enable_all_vrings(&mut self) -> Result<(), VirtioFsError> {
        let mut first_error = None;
        for index in 0..FS_NUM_QUEUES {
            if let Err(err) = self.vu_handle.set_vring_enabled(index, true)
                && first_error.is_none()
            {
                first_error = Some(err);
            }
        }
        first_error.map_or(Ok(()), |err| Err(err.into()))
    }

    fn restart_all_vrings(&mut self) -> Result<(), VirtioFsError> {
        let interrupt = Arc::clone(
            &self
                .device_state
                .active_state()
                .expect("activated virtio-fs device has active state")
                .interrupt,
        );
        let mut first_error = None;
        for index in 0..FS_NUM_QUEUES {
            let queue_index = u16::try_from(index).expect("virtio-fs queue index fits in u16");
            let notifier = interrupt.notifier(VirtioInterruptType::Queue(queue_index));
            let call_evt = notifier
                .as_ref()
                .expect("virtio-fs queue has an interrupt notifier");
            if let Err(err) = self.vu_handle.restart_vring(
                index,
                self.queues[index].next_avail.0,
                call_evt,
                &self.queue_evts[index],
            ) && first_error.is_none()
            {
                first_error = Some(err);
            }
        }
        first_error.map_or(Ok(()), |err| Err(err.into()))
    }

    fn transfer_state(
        &self,
        direction: VhostTransferStateDirection,
        file: File,
    ) -> Result<(), VirtioFsError> {
        let fd: OwnedFd = file.into();
        self.vu_handle.set_device_state_fd(direction, fd)?;
        self.vu_handle.check_device_state()?;
        Ok(())
    }

    fn install_dirty_log(&mut self, mem: &GuestMemoryMmap) -> Result<(), VirtioFsError> {
        if self.dirty_log.is_none() {
            self.dirty_log = Some(DirtyLog::new(mem)?);
        }
        self.vu_handle
            .set_log_base(0, self.dirty_log.as_ref().unwrap().region())?;
        Ok(())
    }

    /// Configure lifetime dirty logging before the VMM seccomp filter is active.
    pub(crate) fn prepare_dirty_log(&mut self, mem: &GuestMemoryMmap) -> Result<(), VirtioFsError> {
        if self.dirty_log.is_none() {
            self.dirty_log = Some(DirtyLog::new(mem)?);
        }

        // SET_LOG_BASE with LOG_SHMFD always has a protocol reply, regardless
        // of NEED_REPLY. Install the memory table first because virtiofsd
        // attaches the bitmap to the regions that exist at SET_LOG_BASE time.
        // Do both operations while building the VM on the unrestricted VMM
        // thread; fresh activation is later driven by MMIO exits on the
        // seccomp-confined vCPU thread.
        let result: Result<(), VirtioFsError> = (|| {
            let features = self.session_features() | VhostUserVirtioFeatures::LOG_ALL.bits();
            self.vu_handle.set_features(features)?;
            self.vu_handle
                .set_protocol_features(features, self.protocol_features)?;
            self.vu_handle.update_mem_table(mem)?;
            self.install_dirty_log(mem)?;
            Ok(())
        })();
        self.vu_handle.set_reply_ack_requests(false);
        result?;
        self.dirty_logging_armed = true;
        Ok(())
    }

    /// Dirty guest-memory ranges written by virtiofsd in the current window.
    pub fn snapshot_dirty_ranges(&self) -> &[MemoryRange] {
        &self.snapshot_dirty_ranges
    }

    /// Stop and drain vrings, then serialize virtiofsd state into `path`.
    pub fn prepare_snapshot(
        &mut self,
        path: &Path,
        deferred_sync: bool,
    ) -> Result<(), VirtioFsError> {
        if !self.is_activated() {
            return Err(VirtioFsError::NotActivated);
        }
        if self.snapshot_prepared {
            return Err(VirtioFsError::SnapshotAlreadyPrepared);
        }

        if !self.dirty_logging_armed {
            return Err(VirtioFsError::DirtyLogNotArmed);
        }

        self.vu_handle.set_reply_ack_requests(true);
        let result = (|| {
            for index in 0..FS_NUM_QUEUES {
                let base = self.vu_handle.stop_vring(index)?;
                let base =
                    u16::try_from(base).map_err(|_| VirtioFsError::InvalidVringBase(base))?;
                self.queues[index].next_avail = Wrapping(base);
            }

            let file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(path)
                .map_err(|source| VirtioFsError::StateFile {
                    operation: "open",
                    source,
                })?;
            self.transfer_state(VhostTransferStateDirection::SAVE, file)?;
            self.snapshot_dirty_ranges = self
                .dirty_log
                .as_ref()
                .map(DirtyLog::ranges)
                .unwrap_or_default();
            if !deferred_sync {
                // Reopen after the backend has completed its write. Cloning
                // before transfer would require fcntl on the seccomp-confined
                // VMM thread, while reopening has the same durability
                // semantics and uses the already permitted openat path.
                let sync_file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .map_err(|source| VirtioFsError::StateFile {
                        operation: "reopen for sync",
                        source,
                    })?;
                sync_file
                    .sync_all()
                    .map_err(|source| VirtioFsError::StateFile {
                        operation: "sync",
                        source,
                    })?;
            }
            Ok(())
        })();

        if result.is_ok() {
            self.snapshot_prepared = true;
        } else {
            // SET_DEVICE_STATE_FD consumes virtiofsd's premigration worker.
            // Re-announcing LOG_ALL is idempotent when the worker is still
            // present and starts the next worker when state transfer already
            // consumed it.
            let _ = self
                .vu_handle
                .set_features(self.session_features() | VhostUserVirtioFeatures::LOG_ALL.bits());
            let _ = self.restart_all_vrings();
            self.vu_handle.set_reply_ack_requests(false);
        }
        result
    }

    /// Resume the source backend after snapshot creation, successful or not.
    pub fn finish_snapshot(&mut self, snapshot_succeeded: bool) -> Result<(), VirtioFsError> {
        if !self.snapshot_prepared {
            return Ok(());
        }

        if snapshot_succeeded && let Some(log) = self.dirty_log.as_mut() {
            log.clear();
        }
        let mut first_error = None;
        if let Err(err) = self
            .vu_handle
            .set_features(self.session_features() | VhostUserVirtioFeatures::LOG_ALL.bits())
        {
            first_error = Some(err.into());
        }
        if let Err(err) = self.restart_all_vrings()
            && first_error.is_none()
        {
            first_error = Some(err);
        }
        self.snapshot_prepared = false;
        self.snapshot_dirty_ranges.clear();
        self.vu_handle.set_reply_ack_requests(false);
        first_error.map_or(Ok(()), Err)
    }

    fn configure_backend(
        &mut self,
        mem: &GuestMemoryMmap,
        interrupt: Arc<dyn VirtioInterrupt>,
    ) -> Result<(), VirtioFsError> {
        // Vhost-user backends write used rings and payloads without going
        // through KVM, so KVM's dirty bitmap cannot observe those writes.
        // Keep LOG_ALL enabled for the complete device lifetime and clear the
        // shared bitmap only after a successful snapshot while vrings are
        // stopped.
        let restoring = self.restore_state_path.is_some();
        if restoring {
            // Snapshot restore runs while the VMM thread builds the device,
            // so acknowledgements are both permitted and required here.
            self.vu_handle.set_reply_ack_requests(true);
        }
        let result = (|| {
            let features = self.session_features() | VhostUserVirtioFeatures::LOG_ALL.bits();
            // Guest feature acknowledgement happens after device attachment.
            // Re-announce the final feature set here so virtiofsd observes
            // EVENT_IDX and VERSION_1; this request remains one-way on fresh
            // activation and is ordered before all vring configuration.
            self.vu_handle.set_features(features)?;
            let queues = [
                (0, &self.queues[0], &self.queue_evts[0]),
                (1, &self.queues[1], &self.queue_evts[1]),
            ];
            if let Some(path) = self.restore_state_path.take() {
                self.vu_handle
                    .setup_backend_stopped_after_mem_table(mem, &queues, interrupt)?;
                let file = File::open(&path).map_err(|source| VirtioFsError::StateFile {
                    operation: "open restored",
                    source,
                })?;
                self.transfer_state(VhostTransferStateDirection::LOAD, file)?;
                // Loading consumes (and cancels) the premigration worker that
                // SET_FEATURES started. Re-announce the same feature set so the
                // restored VM is immediately ready for its next checkpoint.
                self.vu_handle.set_features(features)?;
                self.enable_all_vrings()?;
            } else {
                self.vu_handle
                    .setup_backend_after_mem_table(mem, &queues, interrupt)?;
            }
            Ok(())
        })();
        if restoring {
            self.vu_handle.set_reply_ack_requests(false);
        }
        result
    }
}

impl VirtioDevice for VirtioFs {
    impl_device_type!(VirtioDeviceType::Fs);

    fn id(&self) -> &str {
        &self.id
    }

    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features;
    }

    fn queues(&self) -> &[Queue] {
        &self.queues
    }

    fn queues_mut(&mut self) -> &mut [Queue] {
        &mut self.queues
    }

    fn queue_events(&self) -> &[EventFd] {
        &self.queue_evts
    }

    fn interrupt_trigger(&self) -> &dyn VirtioInterrupt {
        self.device_state
            .active_state()
            .expect("virtio-fs is not activated")
            .interrupt
            .as_ref()
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut config = [0_u8; FS_TAG_MAX_LEN + std::mem::size_of::<u32>()];
        config[..FS_TAG_MAX_LEN].copy_from_slice(&self.tag);
        config[FS_TAG_MAX_LEN..].copy_from_slice(&NUM_REQUEST_QUEUES.to_le_bytes());

        let Ok(start) = usize::try_from(offset) else {
            return;
        };
        if start >= config.len() {
            return;
        }
        let count = data.len().min(config.len() - start);
        data[..count].copy_from_slice(&config[start..start + count]);
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: Arc<dyn VirtioInterrupt>,
    ) -> Result<(), ActivateError> {
        for queue in &mut self.queues {
            queue
                .initialize(&mem)
                .map_err(ActivateError::QueueMemoryError)?;
        }

        self.configure_backend(&mem, interrupt.clone())
            .map_err(ActivateError::VirtioFs)?;
        self.device_state = DeviceState::Activated(ActiveState { mem, interrupt });
        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }
}

impl MutEventSubscriber for VirtioFs {
    fn init(&mut self, _ops: &mut EventOps) {}

    fn process(&mut self, _event: Events, _ops: &mut EventOps) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::single_region_mem;

    #[test]
    fn test_features_are_not_leaked_to_guest() {
        assert_eq!(
            AVAILABLE_FEATURES & VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits(),
            0
        );
        assert_eq!(
            AVAILABLE_FEATURES & VhostUserVirtioFeatures::LOG_ALL.bits(),
            0
        );
        assert_ne!(
            REQUESTED_FEATURES & VhostUserVirtioFeatures::LOG_ALL.bits(),
            0
        );
        assert!(REQUIRED_PROTOCOL_FEATURES.contains(VhostUserProtocolFeatures::MQ));
        assert!(REQUIRED_PROTOCOL_FEATURES.contains(VhostUserProtocolFeatures::REPLY_ACK));
        assert!(REQUIRED_PROTOCOL_FEATURES.contains(VhostUserProtocolFeatures::DEVICE_STATE));
        assert!(REQUIRED_PROTOCOL_FEATURES.contains(VhostUserProtocolFeatures::LOG_SHMFD));
    }

    #[test]
    fn test_dirty_log_ranges_and_clear() {
        let mem = single_region_mem(4 * usize::try_from(VHOST_LOG_PAGE_SIZE).unwrap());
        let mut log = DirtyLog::new(&mem).unwrap();

        // Pages 0, 1 and 3: the first two coalesce, the final page does not.
        // SAFETY: the one-byte write is within the mapped dirty bitmap.
        unsafe {
            log.addr.write_volatile(0b0000_1011);
        }
        assert_eq!(
            log.ranges(),
            vec![
                MemoryRange {
                    gpa: 0,
                    length: 2 * VHOST_LOG_PAGE_SIZE,
                },
                MemoryRange {
                    gpa: 3 * VHOST_LOG_PAGE_SIZE,
                    length: VHOST_LOG_PAGE_SIZE,
                },
            ]
        );

        log.clear();
        assert!(log.ranges().is_empty());
    }
}
