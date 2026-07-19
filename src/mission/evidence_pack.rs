use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use super::{
    evidence::{EvidenceRecord, EvidenceStatus, WorkspaceSnapshot},
    proof::ProofIdentity,
    verifier::TrustedCheckResult,
};

const PACK_VERSION: u32 = 1;
const PACK_DIRECTORY: &str = "evidence";
const MAX_PACK_BYTES: u64 = 64 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum CheckExecutionLog {
    Completed {
        exit_code: Option<i32>,
        started_at_millis: u64,
        finished_at_millis: u64,
        stdout_base64: String,
        stderr_base64: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence_error: Option<String>,
    },
    Rejected {
        error: String,
    },
}

impl CheckExecutionLog {
    pub(crate) fn completed(result: &TrustedCheckResult, evidence_error: Option<String>) -> Self {
        Self::Completed {
            exit_code: result.exit_code(),
            started_at_millis: result.started_at_unix_millis(),
            finished_at_millis: result.finished_at_unix_millis(),
            stdout_base64: BASE64.encode(result.stdout()),
            stderr_base64: BASE64.encode(result.stderr()),
            evidence_error,
        }
    }

    pub(crate) fn rejected(error: impl Into<String>) -> Self {
        Self::Rejected {
            error: error.into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct EvidencePack {
    version: u32,
    mission_id: String,
    run_id: String,
    identity: ProofIdentity,
    current_workspace: WorkspaceSnapshot,
    records: BTreeMap<String, EvidenceRecord>,
    summaries: BTreeMap<String, EvidenceStatus>,
    execution_logs: BTreeMap<String, CheckExecutionLog>,
    created_at_millis: u64,
}

impl EvidencePack {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        mission_id: impl Into<String>,
        run_id: impl Into<String>,
        identity: ProofIdentity,
        current_workspace: WorkspaceSnapshot,
        records: BTreeMap<String, EvidenceRecord>,
        summaries: BTreeMap<String, EvidenceStatus>,
        execution_logs: BTreeMap<String, CheckExecutionLog>,
        created_at_millis: u64,
    ) -> Result<Self, EvidencePackError> {
        let pack = Self {
            version: PACK_VERSION,
            mission_id: mission_id.into(),
            run_id: run_id.into(),
            identity,
            current_workspace,
            records,
            summaries,
            execution_logs,
            created_at_millis,
        };
        pack.validate()?;
        Ok(pack)
    }

    pub(crate) fn mission_id(&self) -> &str {
        &self.mission_id
    }

    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }

    pub(crate) const fn identity(&self) -> &ProofIdentity {
        &self.identity
    }

    pub(crate) const fn current_workspace(&self) -> &WorkspaceSnapshot {
        &self.current_workspace
    }

    pub(crate) const fn records(&self) -> &BTreeMap<String, EvidenceRecord> {
        &self.records
    }

    pub(crate) const fn summaries(&self) -> &BTreeMap<String, EvidenceStatus> {
        &self.summaries
    }

    pub(crate) const fn created_at_millis(&self) -> u64 {
        self.created_at_millis
    }

    fn validate(&self) -> Result<(), EvidencePackError> {
        if self.version != PACK_VERSION
            || !valid_id(&self.mission_id)
            || !valid_id(&self.run_id)
            || self.identity.mission_id() != self.mission_id
            || self.identity.run_id() != self.run_id
            || self.records.len() > 32
            || self.summaries.len() > 32
            || self.execution_logs.len() > 32
        {
            return Err(EvidencePackError::InvalidPack);
        }
        if self
            .records
            .iter()
            .any(|(check_id, record)| !valid_id(check_id) || record.check_id() != check_id)
            || self.summaries.keys().any(|check_id| !valid_id(check_id))
            || self
                .execution_logs
                .iter()
                .any(|(check_id, log)| !valid_id(check_id) || !valid_log(log))
        {
            return Err(EvidencePackError::InvalidPack);
        }
        Ok(())
    }
}

fn valid_log(log: &CheckExecutionLog) -> bool {
    match log {
        CheckExecutionLog::Completed {
            started_at_millis,
            finished_at_millis,
            stdout_base64,
            stderr_base64,
            evidence_error,
            ..
        } => {
            finished_at_millis >= started_at_millis
                && BASE64.decode(stdout_base64).is_ok()
                && BASE64.decode(stderr_base64).is_ok()
                && evidence_error
                    .as_ref()
                    .is_none_or(|error| !error.is_empty() && error.len() <= 8 * 1024)
        }
        CheckExecutionLog::Rejected { error } => !error.is_empty() && error.len() <= 8 * 1024,
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
}

#[derive(Clone, Debug)]
pub(crate) struct EvidencePackStore {
    directory: PathBuf,
}

impl EvidencePackStore {
    pub(crate) fn open(session_data_dir: &Path) -> Result<Self, EvidencePackError> {
        let directory = session_data_dir.join("missions").join(PACK_DIRECTORY);
        ensure_private_directory(&directory)?;
        Ok(Self { directory })
    }

    pub(crate) fn persist(&self, pack: &EvidencePack) -> Result<String, EvidencePackError> {
        pack.validate()?;
        let payload = serde_json::to_vec(pack)?;
        if payload.len() as u64 > MAX_PACK_BYTES {
            return Err(EvidencePackError::PackTooLarge);
        }
        let digest = sha256_hex(&payload);
        let target = self.directory.join(format!("{digest}.json"));
        if target.exists() {
            let existing = read_regular_limited(&target)?;
            if existing == payload {
                return Ok(digest);
            }
            return Err(EvidencePackError::DigestCollision);
        }

        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = self
            .directory
            .join(format!(".evidence-{}-{sequence}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        let write_result = (|| -> Result<(), std::io::Error> {
            file.write_all(&payload)?;
            file.sync_all()?;
            std::fs::rename(&temporary, &target)?;
            sync_directory(&self.directory)
        })();
        if let Err(error) = write_result {
            let _ = std::fs::remove_file(&temporary);
            return Err(error.into());
        }
        Ok(digest)
    }

    pub(crate) fn load(&self, digest: &str) -> Result<EvidencePack, EvidencePackError> {
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(EvidencePackError::InvalidDigest);
        }
        let payload = read_regular_limited(&self.directory.join(format!("{digest}.json")))?;
        if sha256_hex(&payload) != digest.to_ascii_lowercase() {
            return Err(EvidencePackError::DigestMismatch);
        }
        let pack: EvidencePack = serde_json::from_slice(&payload)?;
        pack.validate()?;
        Ok(pack)
    }
}

fn ensure_private_directory(path: &Path) -> Result<(), EvidencePackError> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(EvidencePackError::UnsafePath);
        }
    } else {
        std::fs::create_dir_all(path)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn read_regular_limited(path: &Path) -> Result<Vec<u8>, EvidencePackError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(EvidencePackError::UnsafePath);
    }
    if metadata.len() > MAX_PACK_BYTES {
        return Err(EvidencePackError::PackTooLarge);
    }
    let file = File::open(path)?;
    let mut payload = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_PACK_BYTES + 1).read_to_end(&mut payload)?;
    if payload.len() as u64 > MAX_PACK_BYTES {
        return Err(EvidencePackError::PackTooLarge);
    }
    Ok(payload)
}

fn sync_directory(path: &Path) -> Result<(), std::io::Error> {
    File::open(path)?.sync_all()
}

fn sha256_hex(payload: &[u8]) -> String {
    Sha256::digest(payload)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Debug, Error)]
pub(crate) enum EvidencePackError {
    #[error("evidence pack is invalid")]
    InvalidPack,
    #[error("evidence pack digest is invalid")]
    InvalidDigest,
    #[error("evidence pack digest does not match its content")]
    DigestMismatch,
    #[error("evidence pack digest collision")]
    DigestCollision,
    #[error("evidence pack exceeds its size limit")]
    PackTooLarge,
    #[error("evidence pack path is unsafe")]
    UnsafePath,
    #[error("evidence pack I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("evidence pack serialization failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission::{
        evidence::{FileDisposition, FileFingerprint},
        proof::ProofIdentity,
    };

    fn pack() -> EvidencePack {
        let identity =
            ProofIdentity::new("mission-1", "run-1", "/repo", "/repo", "a".repeat(40)).unwrap();
        EvidencePack::new(
            "mission-1",
            "run-1",
            identity,
            WorkspaceSnapshot::new(
                "a".repeat(40),
                "b".repeat(64),
                vec![FileFingerprint::new(
                    "src/lib.rs",
                    "c".repeat(64),
                    FileDisposition::Tracked,
                )],
            ),
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
            42,
        )
        .unwrap()
    }

    #[test]
    fn content_addressed_pack_round_trips_without_entering_the_journal() {
        let directory = tempfile::tempdir().unwrap();
        let store = EvidencePackStore::open(directory.path()).unwrap();
        let pack = pack();

        let first = store.persist(&pack).unwrap();
        let second = store.persist(&pack).unwrap();

        assert_eq!(first, second);
        assert_eq!(store.load(&first).unwrap(), pack);
        assert!(
            std::fs::metadata(
                directory
                    .path()
                    .join("missions/evidence")
                    .join(format!("{first}.json"))
            )
            .unwrap()
            .len()
                > 0
        );
    }

    #[test]
    fn pack_reader_rejects_tampered_content() {
        let directory = tempfile::tempdir().unwrap();
        let store = EvidencePackStore::open(directory.path()).unwrap();
        let digest = store.persist(&pack()).unwrap();
        std::fs::write(
            directory
                .path()
                .join("missions/evidence")
                .join(format!("{digest}.json")),
            b"{}",
        )
        .unwrap();

        assert!(matches!(
            store.load(&digest),
            Err(EvidencePackError::DigestMismatch)
        ));
    }
}
