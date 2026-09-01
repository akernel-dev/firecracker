// Copyright 2026 Ant Group Corporation. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Snapshot state for the virtio-fs frontend.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{FS_NUM_QUEUES, FS_QUEUE_SIZE, VirtioFs, VirtioFsError};
use crate::devices::virtio::device::{VirtioDevice, VirtioDeviceType};
use crate::devices::virtio::persist::{PersistError as VirtioPersistError, VirtioDeviceState};
use crate::snapshot::Persist;
use crate::vmm_config::virtio_fs::FsConfig;
use crate::vstate::memory::GuestMemoryMmap;

/// Serializable Firecracker state for one virtio-fs device.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VirtioFsState {
    /// Generic virtio feature and queue state.
    pub virtio_state: VirtioDeviceState,
    /// Device and backend configuration captured at snapshot time.
    pub config: FsConfig,
    /// Backend state sidecar supplied by the load request.
    #[serde(skip)]
    pub backend_state_path: Option<PathBuf>,
}

/// Arguments required to restore a virtio-fs device.
#[derive(Clone, Debug)]
pub struct VirtioFsConstructorArgs {
    /// Restored guest memory.
    pub mem: GuestMemoryMmap,
}

/// Errors raised while restoring virtio-fs frontend state.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum VirtioFsPersistError {
    /// Invalid generic virtio state: {0}
    Virtio(#[from] VirtioPersistError),
    /// Invalid number of queues in virtio-fs state.
    QueueCount,
    /// Failed to reconnect to the virtio-fs backend: {0}
    Backend(#[from] VirtioFsError),
    /// Activated virtio-fs snapshot has no backend state sidecar.
    MissingBackendState,
}

impl<'a> Persist<'a> for VirtioFs {
    type State = VirtioFsState;
    type ConstructorArgs = VirtioFsConstructorArgs;
    type Error = VirtioFsPersistError;

    fn save(&self) -> Self::State {
        VirtioFsState {
            virtio_state: VirtioDeviceState::from_device(self),
            config: self.config(),
            backend_state_path: None,
        }
    }

    fn restore(
        constructor_args: Self::ConstructorArgs,
        state: &Self::State,
    ) -> Result<Self, Self::Error> {
        if state.virtio_state.activated && state.backend_state_path.is_none() {
            return Err(VirtioFsPersistError::MissingBackendState);
        }
        let queues = state.virtio_state.build_queues_checked(
            &constructor_args.mem,
            VirtioDeviceType::Fs,
            FS_NUM_QUEUES,
            FS_QUEUE_SIZE,
        )?;
        let queues = queues
            .try_into()
            .map_err(|_| VirtioFsPersistError::QueueCount)?;
        let mut device = VirtioFs::new_with_state(
            state.config.clone(),
            queues,
            state.virtio_state.acked_features,
            state.backend_state_path.clone(),
        )?;
        device.prepare_dirty_log(&constructor_args.mem)?;
        Ok(device)
    }
}
