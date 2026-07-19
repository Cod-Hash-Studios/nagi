use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::ports::{PortAllocator, PortAllocatorError, PortLease};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CleanupPreview {
    pub(crate) schema_version: u16,
    pub(crate) digest: String,
    pub(crate) registry_bytes: u64,
    pub(crate) orphaned_ports: Vec<CleanupPortLease>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CleanupPortLease {
    pub(crate) lease_id: String,
    pub(crate) mission_id: String,
    pub(crate) run_id: String,
    pub(crate) service_id: String,
    pub(crate) port: u16,
    pub(crate) acquired_at_millis: u64,
}

pub(crate) fn preview(allocator: &PortAllocator) -> Result<CleanupPreview, CleanupError> {
    let mut leases = allocator
        .orphaned()?
        .into_iter()
        .map(CleanupPortLease::from)
        .collect::<Vec<_>>();
    leases.sort_by(|left, right| left.lease_id.cmp(&right.lease_id));
    let registry_bytes = allocator.registry_bytes()?;
    Ok(CleanupPreview {
        schema_version: 1,
        digest: preview_digest(&leases, registry_bytes),
        registry_bytes,
        orphaned_ports: leases,
    })
}

/// Apply only an exact preview the caller has already inspected. The port
/// registry rechecks process liveness under its lock, so a recovered resource
/// is never removed merely because it appeared in an older preview.
pub(crate) fn apply(
    allocator: &PortAllocator,
    inspected: &CleanupPreview,
) -> Result<Vec<CleanupPortLease>, CleanupError> {
    if inspected.schema_version != 1
        || inspected.digest != preview_digest(&inspected.orphaned_ports, inspected.registry_bytes)
    {
        return Err(CleanupError::PreviewMismatch);
    }
    let ids = inspected
        .orphaned_ports
        .iter()
        .map(|lease| lease.lease_id.clone())
        .collect::<BTreeSet<_>>();
    let mut removed = allocator
        .cleanup_orphaned(&ids)?
        .into_iter()
        .map(CleanupPortLease::from)
        .collect::<Vec<_>>();
    removed.sort_by(|left, right| left.lease_id.cmp(&right.lease_id));
    Ok(removed)
}

impl From<PortLease> for CleanupPortLease {
    fn from(lease: PortLease) -> Self {
        Self {
            lease_id: lease.lease_id().to_owned(),
            mission_id: lease.mission_id().to_owned(),
            run_id: lease.run_id().to_owned(),
            service_id: lease.service_id().to_owned(),
            port: lease.port(),
            acquired_at_millis: lease.acquired_at_millis(),
        }
    }
}

fn preview_digest(leases: &[CleanupPortLease], registry_bytes: u64) -> String {
    let mut digest = Sha256::new();
    digest.update(b"nagi-resource-cleanup-preview-v1");
    digest.update(registry_bytes.to_be_bytes());
    for lease in leases {
        for value in [
            lease.lease_id.as_bytes(),
            lease.mission_id.as_bytes(),
            lease.run_id.as_bytes(),
            lease.service_id.as_bytes(),
        ] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value);
        }
        digest.update(lease.port.to_be_bytes());
        digest.update(lease.acquired_at_millis.to_be_bytes());
    }
    format!("{:x}", digest.finalize())
}

#[derive(Debug, Error)]
pub(crate) enum CleanupError {
    #[error("resource cleanup preview was modified or has an unsupported schema")]
    PreviewMismatch,
    #[error(transparent)]
    Port(#[from] PortAllocatorError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_preview_is_stable_and_tampering_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let allocator = PortAllocator::open(directory.path()).unwrap();
        let clean = preview(&allocator).unwrap();
        assert!(clean.orphaned_ports.is_empty());
        assert_eq!(clean.digest.len(), 64);
        assert!(apply(&allocator, &clean).unwrap().is_empty());

        let mut tampered = clean;
        tampered.digest = "0".repeat(64);
        assert!(matches!(
            apply(&allocator, &tampered),
            Err(CleanupError::PreviewMismatch)
        ));
    }
}
