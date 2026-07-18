use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use sha2::{Digest as _, Sha256};
use thiserror::Error;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct WorktreeKey {
    repository_common_dir: PathBuf,
    checkout_root: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeaseOwner {
    mission_id: String,
    mission_run_id: String,
}

impl LeaseOwner {
    pub fn new(
        mission_id: impl Into<String>,
        mission_run_id: impl Into<String>,
    ) -> Result<Self, WorktreeClaimError> {
        let owner = Self {
            mission_id: mission_id.into(),
            mission_run_id: mission_run_id.into(),
        };
        if owner.mission_id.trim().is_empty() || owner.mission_run_id.trim().is_empty() {
            return Err(WorktreeClaimError::EmptyOwner);
        }
        Ok(owner)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimRequestId(String);

impl ClaimRequestId {
    pub fn new(value: impl Into<String>) -> Result<Self, WorktreeClaimError> {
        let value = value.into();
        if value.trim().is_empty() || value.len() > 128 {
            return Err(WorktreeClaimError::InvalidRequestId);
        }
        Ok(Self(value))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LeaseNonce([u8; 16]);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeLease {
    key: WorktreeKey,
    owner: LeaseOwner,
    request_id: ClaimRequestId,
    nonce: LeaseNonce,
}

impl WorktreeLease {
    #[must_use]
    #[allow(
        dead_code,
        reason = "proof scope inspection is staged until public mission closure"
    )]
    pub fn checkout_root(&self) -> &Path {
        &self.key.checkout_root
    }

    #[allow(
        dead_code,
        reason = "proof scope inspection is staged until public mission closure"
    )]
    pub(crate) fn matches_scope(
        &self,
        mission_id: &str,
        mission_run_id: &str,
        checkout_root: &Path,
    ) -> bool {
        self.owner.mission_id == mission_id
            && self.owner.mission_run_id == mission_run_id
            && self.key.checkout_root == checkout_root
    }

    pub(crate) fn authority_digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"mission-worktree-lease-authority-v1\0");
        hasher.update(
            self.key
                .repository_common_dir
                .as_os_str()
                .as_encoded_bytes(),
        );
        hasher.update([0]);
        hasher.update(self.key.checkout_root.as_os_str().as_encoded_bytes());
        hasher.update([0]);
        hasher.update(self.owner.mission_id.as_bytes());
        hasher.update([0]);
        hasher.update(self.owner.mission_run_id.as_bytes());
        hasher.update([0]);
        hasher.update(self.nonce.0);
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

#[derive(Debug)]
struct ClaimRecord {
    lease: WorktreeLease,
    _process_lock: File,
}

#[derive(Clone, Debug)]
pub struct WorktreeClaimRegistry {
    inner: Arc<Mutex<BTreeMap<WorktreeKey, ClaimRecord>>>,
    lock_directory: Arc<PathBuf>,
}

impl WorktreeClaimRegistry {
    pub fn new(lock_directory: impl Into<PathBuf>) -> Result<Self, WorktreeClaimError> {
        let lock_directory = lock_directory.into();
        reject_symlink(&lock_directory)?;
        std::fs::create_dir_all(&lock_directory)?;
        set_private_directory_permissions(&lock_directory)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(BTreeMap::new())),
            lock_directory: Arc::new(lock_directory),
        })
    }

    pub fn claim(
        &self,
        owner: LeaseOwner,
        mission_repository: &Path,
        requested_checkout: &Path,
        request_id: ClaimRequestId,
    ) -> Result<WorktreeLease, WorktreeClaimError> {
        let key = resolve_worktree_key(mission_repository, requested_checkout)?;
        let mut claims = self
            .inner
            .lock()
            .map_err(|_| WorktreeClaimError::RegistryPoisoned)?;

        if let Some(existing) = claims.get(&key) {
            if existing.lease.owner == owner && existing.lease.request_id == request_id {
                return Ok(existing.lease.clone());
            }
            return Err(WorktreeClaimError::AlreadyOwned {
                mission_id: existing.lease.owner.mission_id.clone(),
                mission_run_id: existing.lease.owner.mission_run_id.clone(),
            });
        }

        let lock_path = self.lock_directory.join(lock_filename(&key));
        let process_lock = open_private_lock_file(&lock_path)?;
        process_lock
            .try_lock()
            .map_err(|_| WorktreeClaimError::OwnedByAnotherProcess)?;
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).map_err(|_| WorktreeClaimError::NonceUnavailable)?;
        let lease = WorktreeLease {
            key: key.clone(),
            owner,
            request_id,
            nonce: LeaseNonce(nonce),
        };
        claims.insert(
            key,
            ClaimRecord {
                lease: lease.clone(),
                _process_lock: process_lock,
            },
        );
        Ok(lease)
    }

    pub fn release(&self, lease: &WorktreeLease) -> Result<ReleaseOutcome, WorktreeClaimError> {
        let mut claims = self
            .inner
            .lock()
            .map_err(|_| WorktreeClaimError::RegistryPoisoned)?;
        match claims.get(&lease.key) {
            Some(record) if record.lease == *lease => {
                claims.remove(&lease.key);
                Ok(ReleaseOutcome::Released)
            }
            Some(_) => Err(WorktreeClaimError::StaleLease),
            None => Ok(ReleaseOutcome::AlreadyReleased),
        }
    }

    #[allow(
        dead_code,
        reason = "lease inspection is staged until public mission closure"
    )]
    pub(crate) fn is_current(&self, lease: &WorktreeLease) -> Result<bool, WorktreeClaimError> {
        let claims = self
            .inner
            .lock()
            .map_err(|_| WorktreeClaimError::RegistryPoisoned)?;
        Ok(claims
            .get(&lease.key)
            .is_some_and(|record| record.lease == *lease))
    }

    pub(crate) fn has_current_authority_digest(
        &self,
        authority_digest: &str,
    ) -> Result<bool, WorktreeClaimError> {
        let claims = self
            .inner
            .lock()
            .map_err(|_| WorktreeClaimError::RegistryPoisoned)?;
        Ok(claims
            .values()
            .any(|record| record.lease.authority_digest() == authority_digest))
    }

    #[must_use]
    #[allow(
        dead_code,
        reason = "lease ownership inspection is staged until the mission cockpit is public"
    )]
    pub fn owner(&self, checkout_root: &Path) -> Option<LeaseOwner> {
        let checkout_root = std::fs::canonicalize(checkout_root).ok()?;
        let claims = self.inner.lock().ok()?;
        claims
            .values()
            .find(|record| record.lease.key.checkout_root == checkout_root)
            .map(|record| record.lease.owner.clone())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReleaseOutcome {
    Released,
    AlreadyReleased,
}

fn resolve_worktree_key(
    mission_repository: &Path,
    requested_checkout: &Path,
) -> Result<WorktreeKey, WorktreeClaimError> {
    let repository = std::fs::canonicalize(mission_repository)
        .map_err(|_| WorktreeClaimError::RepositoryUnavailable)?;
    let checkout = std::fs::canonicalize(requested_checkout)
        .map_err(|_| WorktreeClaimError::CheckoutUnavailable)?;
    let repository_info = crate::workspace::git_worktree_info(&repository)
        .ok_or(WorktreeClaimError::RepositoryNotGit)?;
    let checkout_info =
        crate::workspace::git_worktree_info(&checkout).ok_or(WorktreeClaimError::CheckoutNotGit)?;
    let repository_root = std::fs::canonicalize(&repository_info.repo_root)
        .map_err(|_| WorktreeClaimError::RepositoryUnavailable)?;
    let checkout_root = std::fs::canonicalize(&checkout_info.repo_root)
        .map_err(|_| WorktreeClaimError::CheckoutUnavailable)?;
    let repository_common_dir = std::fs::canonicalize(&repository_info.git_common_dir)
        .map_err(|_| WorktreeClaimError::RepositoryUnavailable)?;
    let checkout_common_dir = std::fs::canonicalize(&checkout_info.git_common_dir)
        .map_err(|_| WorktreeClaimError::CheckoutUnavailable)?;

    if repository != repository_root {
        return Err(WorktreeClaimError::RepositoryMustBeRoot);
    }
    if checkout != checkout_root {
        return Err(WorktreeClaimError::CheckoutMustBeRoot);
    }
    if repository_common_dir != checkout_common_dir {
        return Err(WorktreeClaimError::DifferentRepository);
    }

    Ok(WorktreeKey {
        repository_common_dir,
        checkout_root,
    })
}

fn lock_filename(key: &WorktreeKey) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"mission-worktree-lock-v1\0");
    hasher.update(key.repository_common_dir.as_os_str().as_encoded_bytes());
    hasher.update([0]);
    hasher.update(key.checkout_root.as_os_str().as_encoded_bytes());
    let digest = hasher.finalize();
    let encoded = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{encoded}.lock")
}

fn open_private_lock_file(path: &Path) -> Result<File, WorktreeClaimError> {
    reject_symlink(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(WorktreeClaimError::LockPathNotRegular);
    }
    set_private_file_permissions(path)?;
    Ok(file)
}

fn reject_symlink(path: &Path) -> Result<(), WorktreeClaimError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(WorktreeClaimError::SymlinkNotAllowed)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<(), WorktreeClaimError> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<(), WorktreeClaimError> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<(), WorktreeClaimError> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<(), WorktreeClaimError> {
    Ok(())
}

#[derive(Debug, Error)]
pub enum WorktreeClaimError {
    #[error("worktree claim I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("worktree lease owner cannot be empty")]
    EmptyOwner,
    #[error("worktree claim request id is invalid")]
    InvalidRequestId,
    #[error("worktree claim registry is poisoned")]
    RegistryPoisoned,
    #[error("worktree is already owned by mission {mission_id} run {mission_run_id}")]
    AlreadyOwned {
        mission_id: String,
        mission_run_id: String,
    },
    #[error("worktree is already owned by another process")]
    OwnedByAnotherProcess,
    #[error("secure worktree lease nonce is unavailable")]
    NonceUnavailable,
    #[error("worktree lease is stale")]
    StaleLease,
    #[error("mission repository path is unavailable")]
    RepositoryUnavailable,
    #[error("requested worktree path is unavailable")]
    CheckoutUnavailable,
    #[error("mission repository is not a Git checkout")]
    RepositoryNotGit,
    #[error("requested path is not a Git checkout")]
    CheckoutNotGit,
    #[error("mission repository path must be its checkout root")]
    RepositoryMustBeRoot,
    #[error("requested worktree path must be its checkout root")]
    CheckoutMustBeRoot,
    #[error("requested worktree belongs to another repository")]
    DifferentRepository,
    #[error("worktree lock path cannot be a symlink")]
    SymlinkNotAllowed,
    #[error("worktree lock path must be a regular file")]
    LockPathNotRegular,
}
