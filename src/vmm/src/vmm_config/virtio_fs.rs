// Copyright 2026 Ant Group Corporation. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

/// The fixed virtio-fs tag used by the sandbox guest.
pub const DEFAULT_FS_TAG: &str = "sandboxfs";
/// Maximum tag length defined by the virtio-fs device configuration.
pub const FS_TAG_MAX_LEN: usize = 36;

fn default_tag() -> String {
    DEFAULT_FS_TAG.to_string()
}

/// Configuration for the single vhost-user virtio-fs device.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FsConfig {
    /// Device identifier. It must match the identifier in the API path.
    pub fs_id: String,
    /// Path to the vhost-user Unix domain socket served by virtiofsd.
    pub socket_path: String,
    /// Filesystem tag exposed to the guest.
    #[serde(default = "default_tag")]
    pub tag: String,
}

impl FsConfig {
    /// Validate fields that cannot be checked by serde.
    pub fn validate(&self) -> Result<(), FsConfigError> {
        if self.socket_path.is_empty() {
            return Err(FsConfigError::EmptySocketPath);
        }
        if self.tag.is_empty() || self.tag.len() > FS_TAG_MAX_LEN || self.tag.contains('\0') {
            return Err(FsConfigError::InvalidTag);
        }
        Ok(())
    }
}

/// Errors associated with virtio-fs configuration.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum FsConfigError {
    /// The virtio-fs socket path cannot be empty.
    EmptySocketPath,
    /// The virtio-fs tag must contain between 1 and 36 non-NUL bytes.
    InvalidTag,
    /// Only one virtio-fs device is supported; already configured as `{0}`.
    DeviceAlreadyExists(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(tag: &str) -> FsConfig {
        FsConfig {
            fs_id: "root".to_string(),
            socket_path: "/run/virtiofsd.sock".to_string(),
            tag: tag.to_string(),
        }
    }

    #[test]
    fn test_validate_tag_and_socket() {
        config(DEFAULT_FS_TAG).validate().unwrap();
        config(&"x".repeat(FS_TAG_MAX_LEN)).validate().unwrap();
        assert!(matches!(
            config(&"x".repeat(FS_TAG_MAX_LEN + 1)).validate(),
            Err(FsConfigError::InvalidTag)
        ));
        assert!(matches!(
            config("bad\0tag").validate(),
            Err(FsConfigError::InvalidTag)
        ));
        let mut cfg = config(DEFAULT_FS_TAG);
        cfg.socket_path.clear();
        assert!(matches!(
            cfg.validate(),
            Err(FsConfigError::EmptySocketPath)
        ));
    }
}
