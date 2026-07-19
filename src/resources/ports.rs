use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddrV4, TcpListener},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const REGISTRY_SCHEMA_V1: u16 = 1;
const DEFAULT_PORT_START: u16 = 41_000;
const DEFAULT_PORT_END: u16 = 49_000;
const MAX_REGISTRY_BYTES: u64 = 4 * 1024 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_secs(3);
const LOCK_RETRY: Duration = Duration::from_millis(10);
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PortLeaseOwner {
    mission_id: String,
    run_id: String,
    service_id: String,
}

impl PortLeaseOwner {
    pub(crate) fn new(
        mission_id: impl Into<String>,
        run_id: impl Into<String>,
        service_id: impl Into<String>,
    ) -> Result<Self, PortAllocatorError> {
        let owner = Self {
            mission_id: mission_id.into(),
            run_id: run_id.into(),
            service_id: service_id.into(),
        };
        for (field, value) in [
            ("mission_id", owner.mission_id.as_str()),
            ("run_id", owner.run_id.as_str()),
            ("service_id", owner.service_id.as_str()),
        ] {
            if !valid_id(value) {
                return Err(PortAllocatorError::InvalidOwner(field));
            }
        }
        Ok(owner)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PortLease {
    schema_version: u16,
    lease_id: String,
    request_id: String,
    mission_id: String,
    run_id: String,
    service_id: String,
    port: u16,
    owner_pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    service_pid: Option<u32>,
    acquired_at_millis: u64,
}

impl PortLease {
    pub(crate) fn lease_id(&self) -> &str {
        &self.lease_id
    }

    pub(crate) const fn port(&self) -> u16 {
        self.port
    }

    pub(crate) fn mission_id(&self) -> &str {
        &self.mission_id
    }

    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }

    pub(crate) fn service_id(&self) -> &str {
        &self.service_id
    }

    pub(crate) const fn owner_pid(&self) -> u32 {
        self.owner_pid
    }

    pub(crate) const fn service_pid(&self) -> Option<u32> {
        self.service_pid
    }

    pub(crate) const fn acquired_at_millis(&self) -> u64 {
        self.acquired_at_millis
    }
}

#[derive(Debug)]
pub(crate) struct PortReservation {
    lease: PortLease,
    listener: Option<TcpListener>,
}

impl PortReservation {
    pub(crate) const fn is_adopted(&self) -> bool {
        self.listener.is_none()
    }

    /// Release the OS socket immediately before spawning the service. The
    /// durable logical lease stays active and prevents other Nagi runtimes
    /// from selecting the same port.
    pub(crate) fn prepare_service_spawn(&mut self) -> PortLease {
        self.listener.take();
        self.lease.clone()
    }

    #[cfg(test)]
    pub(crate) fn take_fixture_listener(&mut self) -> (PortLease, TcpListener) {
        let listener = self
            .listener
            .take()
            .expect("a fresh test reservation must still own its listener");
        (self.lease.clone(), listener)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PortRegistryFile {
    schema_version: u16,
    leases: Vec<PortLease>,
}

impl Default for PortRegistryFile {
    fn default() -> Self {
        Self {
            schema_version: REGISTRY_SCHEMA_V1,
            leases: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PortAllocator {
    state_path: PathBuf,
    lock_path: PathBuf,
    start: u16,
    end: u16,
}

impl PortAllocator {
    pub(crate) fn open(directory: &Path) -> Result<Self, PortAllocatorError> {
        Self::open_with_range(directory, DEFAULT_PORT_START, DEFAULT_PORT_END)
    }

    fn open_with_range(directory: &Path, start: u16, end: u16) -> Result<Self, PortAllocatorError> {
        if start == 0 || start > end {
            return Err(PortAllocatorError::InvalidRange);
        }
        fs::create_dir_all(directory).map_err(PortAllocatorError::Io)?;
        restrict_directory(directory)?;
        Ok(Self {
            state_path: directory.join("ports-v1.json"),
            lock_path: directory.join("ports-v1.lock"),
            start,
            end,
        })
    }

    pub(crate) fn reserve(
        &self,
        owner: PortLeaseOwner,
        request_id: &str,
        acquired_at_millis: u64,
    ) -> Result<PortReservation, PortAllocatorError> {
        if !valid_id(request_id) {
            return Err(PortAllocatorError::InvalidRequestId);
        }
        let _lock = RegistryLock::acquire(&self.lock_path)?;
        let mut registry = self.load_registry()?;
        if let Some(index) = registry
            .leases
            .iter()
            .position(|lease| lease.request_id == request_id)
        {
            let existing = &registry.leases[index];
            if existing.mission_id == owner.mission_id
                && existing.run_id == owner.run_id
                && existing.service_id == owner.service_id
            {
                let service_is_alive = existing.service_pid.is_some_and(process_alive);
                if service_is_alive {
                    if existing.owner_pid != std::process::id() && process_alive(existing.owner_pid)
                    {
                        return Err(PortAllocatorError::LeaseNotOwned);
                    }
                    registry.leases[index].owner_pid = std::process::id();
                    let lease = registry.leases[index].clone();
                    self.save_registry(&registry)?;
                    return Ok(PortReservation {
                        lease,
                        listener: None,
                    });
                }
                let listener = bind_exact(existing.port)?;
                registry.leases[index].owner_pid = std::process::id();
                registry.leases[index].service_pid = None;
                let lease = registry.leases[index].clone();
                self.save_registry(&registry)?;
                return Ok(PortReservation {
                    lease,
                    listener: Some(listener),
                });
            }
            return Err(PortAllocatorError::RequestConflict);
        }
        let occupied = registry
            .leases
            .iter()
            .map(|lease| lease.port)
            .collect::<BTreeSet<_>>();
        let mut selected = None;
        for port in self.start..=self.end {
            if occupied.contains(&port) {
                continue;
            }
            if let Ok(listener) = bind_exact(port) {
                selected = Some((port, listener));
                break;
            }
        }
        let Some((port, listener)) = selected else {
            return Err(PortAllocatorError::NoPortAvailable);
        };
        let lease_id = lease_digest(&owner, request_id, port, acquired_at_millis);
        let lease = PortLease {
            schema_version: REGISTRY_SCHEMA_V1,
            lease_id,
            request_id: request_id.to_owned(),
            mission_id: owner.mission_id,
            run_id: owner.run_id,
            service_id: owner.service_id,
            port,
            owner_pid: std::process::id(),
            service_pid: None,
            acquired_at_millis,
        };
        registry.leases.push(lease.clone());
        registry.leases.sort_by_key(|lease| lease.port);
        self.save_registry(&registry)?;
        Ok(PortReservation {
            lease,
            listener: Some(listener),
        })
    }

    pub(crate) fn bind_service_process(
        &self,
        lease_id: &str,
        service_pid: u32,
    ) -> Result<PortLease, PortAllocatorError> {
        if service_pid == 0 {
            return Err(PortAllocatorError::InvalidProcessId);
        }
        let _lock = RegistryLock::acquire(&self.lock_path)?;
        let mut registry = self.load_registry()?;
        let lease = registry
            .leases
            .iter_mut()
            .find(|lease| lease.lease_id == lease_id)
            .ok_or(PortAllocatorError::LeaseMissing)?;
        if lease.owner_pid != std::process::id() {
            return Err(PortAllocatorError::LeaseNotOwned);
        }
        lease.service_pid = Some(service_pid);
        let result = lease.clone();
        self.save_registry(&registry)?;
        Ok(result)
    }

    pub(crate) fn release(&self, lease: &PortLease) -> Result<bool, PortAllocatorError> {
        let _lock = RegistryLock::acquire(&self.lock_path)?;
        let mut registry = self.load_registry()?;
        let before = registry.leases.len();
        registry.leases.retain(|candidate| {
            candidate.lease_id != lease.lease_id
                || candidate.mission_id != lease.mission_id
                || candidate.run_id != lease.run_id
                || candidate.service_id != lease.service_id
        });
        let released = registry.leases.len() != before;
        if released {
            self.save_registry(&registry)?;
        }
        Ok(released)
    }

    pub(crate) fn orphaned(&self) -> Result<Vec<PortLease>, PortAllocatorError> {
        let _lock = RegistryLock::acquire(&self.lock_path)?;
        let registry = self.load_registry()?;
        Ok(registry
            .leases
            .into_iter()
            .filter(|lease| {
                !process_alive(lease.owner_pid)
                    && lease.service_pid.is_none_or(|pid| !process_alive(pid))
            })
            .collect())
    }

    pub(crate) fn leases_for_owner(
        &self,
        mission_id: &str,
        run_id: &str,
    ) -> Result<Vec<PortLease>, PortAllocatorError> {
        if !valid_id(mission_id) || !valid_id(run_id) {
            return Err(PortAllocatorError::InvalidOwner("mission_id/run_id"));
        }
        let _lock = RegistryLock::acquire(&self.lock_path)?;
        let registry = self.load_registry()?;
        Ok(registry
            .leases
            .into_iter()
            .filter(|lease| lease.mission_id == mission_id && lease.run_id == run_id)
            .collect())
    }

    pub(crate) fn registry_bytes(&self) -> Result<u64, PortAllocatorError> {
        match fs::metadata(&self.state_path) {
            Ok(metadata) => Ok(metadata.len()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(PortAllocatorError::Io(error)),
        }
    }

    /// Remove only the exact leases shown by a previous orphan preview. Every
    /// candidate is rechecked under the registry lock before deletion.
    pub(crate) fn cleanup_orphaned(
        &self,
        lease_ids: &BTreeSet<String>,
    ) -> Result<Vec<PortLease>, PortAllocatorError> {
        let _lock = RegistryLock::acquire(&self.lock_path)?;
        let mut registry = self.load_registry()?;
        let mut removed = Vec::new();
        registry.leases.retain(|lease| {
            let is_orphan = !process_alive(lease.owner_pid)
                && lease.service_pid.is_none_or(|pid| !process_alive(pid));
            if is_orphan && lease_ids.contains(&lease.lease_id) {
                removed.push(lease.clone());
                false
            } else {
                true
            }
        });
        if !removed.is_empty() {
            self.save_registry(&registry)?;
        }
        Ok(removed)
    }

    fn load_registry(&self) -> Result<PortRegistryFile, PortAllocatorError> {
        let mut file = match File::open(&self.state_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PortRegistryFile::default());
            }
            Err(error) => return Err(PortAllocatorError::Io(error)),
        };
        if file.metadata().map_err(PortAllocatorError::Io)?.len() > MAX_REGISTRY_BYTES {
            return Err(PortAllocatorError::RegistryTooLarge);
        }
        let mut source = String::new();
        file.read_to_string(&mut source)
            .map_err(PortAllocatorError::Io)?;
        let registry = serde_json::from_str::<PortRegistryFile>(&source)
            .map_err(PortAllocatorError::CorruptRegistry)?;
        if registry.schema_version != REGISTRY_SCHEMA_V1
            || registry
                .leases
                .iter()
                .any(|lease| lease.schema_version != REGISTRY_SCHEMA_V1)
        {
            return Err(PortAllocatorError::UnsupportedSchema);
        }
        let mut ids = BTreeSet::new();
        let mut ports = BTreeSet::new();
        if registry.leases.iter().any(|lease| {
            !valid_persisted_lease(lease)
                || !ids.insert(&lease.lease_id)
                || !ports.insert(lease.port)
        }) {
            return Err(PortAllocatorError::InvalidRegistry);
        }
        Ok(registry)
    }

    fn save_registry(&self, registry: &PortRegistryFile) -> Result<(), PortAllocatorError> {
        let bytes = serde_json::to_vec_pretty(registry).map_err(PortAllocatorError::Serialize)?;
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_path = self
            .state_path
            .with_extension(format!("tmp-{}-{sequence}", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp_path).map_err(PortAllocatorError::Io)?;
        let write_result = (|| {
            file.write_all(&bytes).map_err(PortAllocatorError::Io)?;
            file.sync_all().map_err(PortAllocatorError::Io)?;
            fs::rename(&temp_path, &self.state_path).map_err(PortAllocatorError::Io)
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        write_result
    }
}

#[derive(Debug, Error)]
pub(crate) enum PortAllocatorError {
    #[error("invalid port allocation range")]
    InvalidRange,
    #[error("invalid port lease owner field {0}")]
    InvalidOwner(&'static str),
    #[error("invalid port allocation request id")]
    InvalidRequestId,
    #[error("port allocation request id is already bound to another owner")]
    RequestConflict,
    #[error("no collision-free loopback port is available")]
    NoPortAvailable,
    #[error("reserved port {0} is no longer available")]
    PortNoLongerAvailable(u16),
    #[error("port registry lock timed out")]
    LockTimeout,
    #[error("port registry is too large")]
    RegistryTooLarge,
    #[error("port registry contains invalid or duplicate leases")]
    InvalidRegistry,
    #[error("port registry schema is unsupported")]
    UnsupportedSchema,
    #[error("port lease does not exist")]
    LeaseMissing,
    #[error("port lease is owned by another runtime")]
    LeaseNotOwned,
    #[error("invalid service process id")]
    InvalidProcessId,
    #[error("port registry is corrupt: {0}")]
    CorruptRegistry(serde_json::Error),
    #[error("port registry serialization failed: {0}")]
    Serialize(serde_json::Error),
    #[error("port registry I/O failed: {0}")]
    Io(std::io::Error),
}

struct RegistryLock {
    path: PathBuf,
}

impl RegistryLock {
    fn acquire(path: &Path) -> Result<Self, PortAllocatorError> {
        let started = Instant::now();
        loop {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id()).map_err(PortAllocatorError::Io)?;
                    file.sync_all().map_err(PortAllocatorError::Io)?;
                    return Ok(Self {
                        path: path.to_owned(),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if stale_lock(path) {
                        let _ = fs::remove_file(path);
                        continue;
                    }
                    if started.elapsed() >= LOCK_TIMEOUT {
                        return Err(PortAllocatorError::LockTimeout);
                    }
                    thread::sleep(LOCK_RETRY);
                }
                Err(error) => return Err(PortAllocatorError::Io(error)),
            }
        }
    }
}

impl Drop for RegistryLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn stale_lock(path: &Path) -> bool {
    let Ok(source) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(pid) = source.trim().parse::<u32>() else {
        return false;
    };
    !process_alive(pid)
}

fn bind_exact(port: u16) -> Result<TcpListener, PortAllocatorError> {
    TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
        .map_err(|_| PortAllocatorError::PortNoLongerAvailable(port))
}

fn lease_digest(
    owner: &PortLeaseOwner,
    request_id: &str,
    port: u16,
    acquired_at_millis: u64,
) -> String {
    let mut digest = Sha256::new();
    for value in [
        b"nagi-port-lease-v1".as_slice(),
        owner.mission_id.as_bytes(),
        owner.run_id.as_bytes(),
        owner.service_id.as_bytes(),
        request_id.as_bytes(),
    ] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value);
    }
    digest.update(port.to_be_bytes());
    digest.update(acquired_at_millis.to_be_bytes());
    format!("{:x}", digest.finalize())
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}

fn valid_persisted_lease(lease: &PortLease) -> bool {
    lease.schema_version == REGISTRY_SCHEMA_V1
        && lease.lease_id.len() == 64
        && lease.lease_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        && valid_id(&lease.request_id)
        && valid_id(&lease.mission_id)
        && valid_id(&lease.run_id)
        && valid_id(&lease.service_id)
        && lease.port != 0
        && lease.owner_pid != 0
}

#[cfg(unix)]
pub(crate) fn process_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub(crate) fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    if pid == 0 {
        return false;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        false
    } else {
        unsafe { CloseHandle(handle) };
        true
    }
}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> Result<(), PortAllocatorError> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)
        .map_err(PortAllocatorError::Io)?
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions).map_err(PortAllocatorError::Io)
}

#[cfg(windows)]
fn restrict_directory(_path: &Path) -> Result<(), PortAllocatorError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    fn owner(index: usize) -> PortLeaseOwner {
        PortLeaseOwner::new("mission", format!("run-{index}"), "web").unwrap()
    }

    #[test]
    fn reservation_is_persisted_idempotent_and_holds_the_socket() {
        let directory = tempfile::tempdir().unwrap();
        let allocator = PortAllocator::open_with_range(directory.path(), 45_100, 45_110).unwrap();
        let mut first = allocator.reserve(owner(1), "request-1", 10).unwrap();
        assert!(TcpListener::bind((Ipv4Addr::LOCALHOST, first.lease.port())).is_err());
        let port = first.lease.port();
        first.prepare_service_spawn();
        drop(first);

        let second = allocator.reserve(owner(1), "request-1", 999).unwrap();
        assert_eq!(second.lease.port(), port);
        assert_eq!(second.lease.mission_id(), "mission");
    }

    #[test]
    fn concurrent_allocators_never_return_the_same_port() {
        let directory = tempfile::tempdir().unwrap();
        let allocator =
            Arc::new(PortAllocator::open_with_range(directory.path(), 45_120, 45_140).unwrap());
        let barrier = Arc::new(Barrier::new(8));
        let threads = (0..8)
            .map(|index| {
                let allocator = Arc::clone(&allocator);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    allocator
                        .reserve(owner(index), &format!("request-{index}"), index as u64)
                        .unwrap()
                })
            })
            .collect::<Vec<_>>();
        let reservations = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        let ports = reservations
            .iter()
            .map(|reservation| reservation.lease.port())
            .collect::<BTreeSet<_>>();
        assert_eq!(ports.len(), reservations.len());
    }

    #[test]
    fn request_id_cannot_be_rebound_to_another_owner() {
        let directory = tempfile::tempdir().unwrap();
        let allocator = PortAllocator::open_with_range(directory.path(), 45_150, 45_155).unwrap();
        let _reservation = allocator.reserve(owner(1), "same-request", 1).unwrap();
        assert!(matches!(
            allocator.reserve(owner(2), "same-request", 2),
            Err(PortAllocatorError::RequestConflict)
        ));
    }

    #[test]
    fn corrupt_registry_is_never_silently_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let allocator = PortAllocator::open_with_range(directory.path(), 45_160, 45_165).unwrap();
        fs::write(directory.path().join("ports-v1.json"), "not json").unwrap();
        assert!(matches!(
            allocator.reserve(owner(1), "request", 1),
            Err(PortAllocatorError::CorruptRegistry(_))
        ));
        assert_eq!(
            fs::read_to_string(directory.path().join("ports-v1.json")).unwrap(),
            "not json"
        );
    }

    #[test]
    fn release_requires_the_exact_lease_identity() {
        let directory = tempfile::tempdir().unwrap();
        let allocator = PortAllocator::open_with_range(directory.path(), 45_170, 45_175).unwrap();
        let reservation = allocator.reserve(owner(1), "request", 1).unwrap();
        let mut forged = reservation.lease.clone();
        forged.service_id = "other".into();
        assert!(!allocator.release(&forged).unwrap());
        assert!(allocator.release(&reservation.lease).unwrap());
        assert!(!allocator.release(&reservation.lease).unwrap());
    }

    #[test]
    fn live_process_leases_are_not_presented_as_orphans() {
        let directory = tempfile::tempdir().unwrap();
        let allocator = PortAllocator::open_with_range(directory.path(), 45_180, 45_185).unwrap();
        let reservation = allocator.reserve(owner(1), "request", 1).unwrap();
        assert!(allocator.orphaned().unwrap().is_empty());
        assert!(allocator
            .cleanup_orphaned(&BTreeSet::from([reservation.lease.lease_id().to_owned()]))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn dead_runtime_can_adopt_a_live_service_without_rebinding_its_port() {
        let directory = tempfile::tempdir().unwrap();
        let allocator = PortAllocator::open_with_range(directory.path(), 45_190, 45_195).unwrap();
        let reservation = allocator.reserve(owner(1), "request", 1).unwrap();
        let mut lease = reservation.lease.clone();
        lease.owner_pid = u32::MAX;
        lease.service_pid = Some(std::process::id());
        allocator
            .save_registry(&PortRegistryFile {
                schema_version: REGISTRY_SCHEMA_V1,
                leases: vec![lease],
            })
            .unwrap();

        let adopted = allocator.reserve(owner(1), "request", 2).unwrap();
        assert!(adopted.is_adopted());
        assert_eq!(adopted.lease.owner_pid(), std::process::id());
        assert_eq!(adopted.lease.service_pid(), Some(std::process::id()));
        assert_eq!(adopted.lease.port(), reservation.lease.port());
    }
}
