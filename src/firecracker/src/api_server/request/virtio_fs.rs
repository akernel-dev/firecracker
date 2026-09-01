// Copyright 2026 Ant Group Corporation. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use vmm::rpc_interface::VmmAction;
use vmm::vmm_config::virtio_fs::FsConfig;

use super::super::parsed_request::{ParsedRequest, RequestError, checked_id};
use super::{Body, StatusCode};

pub(crate) fn parse_put_fs(
    body: &Body,
    id_from_path: Option<&str>,
) -> Result<ParsedRequest, RequestError> {
    let id = id_from_path
        .ok_or(RequestError::EmptyID)
        .and_then(checked_id)?;
    let config = serde_json::from_slice::<FsConfig>(body.raw())?;
    if id != config.fs_id {
        return Err(RequestError::Generic(
            StatusCode::BadRequest,
            "The id from the path does not match the id from the body!".to_string(),
        ));
    }
    config
        .validate()
        .map_err(|err| RequestError::Generic(StatusCode::BadRequest, err.to_string()))?;
    Ok(ParsedRequest::new_sync(VmmAction::SetFsDevice(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_put_fs() {
        let body =
            Body::new(r#"{"fs_id":"root","socket_path":"/run/virtiofsd.sock","tag":"sandboxfs"}"#);
        parse_put_fs(&body, Some("root")).unwrap();
        parse_put_fs(&body, Some("other")).unwrap_err();
        parse_put_fs(&body, None).unwrap_err();
    }

    #[test]
    fn test_reject_invalid_tag_without_truncating() {
        let body = Body::new(
            r#"{"fs_id":"root","socket_path":"/run/virtiofsd.sock","tag":"1234567890123456789012345678901234567"}"#,
        );
        parse_put_fs(&body, Some("root")).unwrap_err();
    }
}
