use std::{
    collections::BTreeMap,
    io::{Read as _, Write as _},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs as _},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::project_recipe::{ProjectContract, ServiceContract};

use super::ports::{process_alive, PortAllocator, PortAllocatorError, PortLease, PortLeaseOwner};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const PROBE_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub(crate) struct ServiceSet {
    services: BTreeMap<String, ManagedService>,
}

impl ServiceSet {
    pub(crate) fn start(
        allocator: PortAllocator,
        contract: &ProjectContract,
        worktree: &Path,
        mission_id: &str,
        run_id: &str,
        at_millis: u64,
    ) -> Result<Self, ServiceError> {
        let worktree = canonical_worktree(worktree)?;
        let mut services = BTreeMap::new();
        for (service_id, service) in &contract.services {
            let managed = ManagedService::start(
                allocator.clone(),
                service_id,
                service,
                &worktree,
                mission_id,
                run_id,
                at_millis,
            )?;
            services.insert(service_id.clone(), managed);
        }
        Ok(Self { services })
    }

    pub(crate) fn ports(&self) -> BTreeMap<String, u16> {
        self.services
            .iter()
            .map(|(id, service)| (id.clone(), service.lease.port()))
            .collect()
    }

    /// Leave healthy service processes running while retaining their durable
    /// leases. A later Nagi runtime can adopt them by the same mission/run id.
    pub(crate) fn detach(mut self) -> BTreeMap<String, u16> {
        let ports = self.ports();
        for service in self.services.values_mut() {
            service.child.take();
            service.stop_on_drop = false;
        }
        self.services.clear();
        ports
    }

    pub(crate) fn stop_owner(
        allocator: &PortAllocator,
        mission_id: &str,
        run_id: &str,
    ) -> Result<Vec<String>, ServiceError> {
        let leases = allocator.leases_for_owner(mission_id, run_id)?;
        let mut stopped = Vec::new();
        for lease in leases {
            if let Some(pid) = lease.service_pid() {
                terminate_pid(pid)?;
            }
            if allocator.release(&lease)? {
                stopped.push(lease.service_id().to_owned());
            }
        }
        stopped.sort();
        Ok(stopped)
    }
}

#[derive(Debug)]
struct ManagedService {
    child: Option<Child>,
    allocator: PortAllocator,
    lease: PortLease,
    stop_on_drop: bool,
}

impl ManagedService {
    #[allow(clippy::too_many_arguments)]
    fn start(
        allocator: PortAllocator,
        service_id: &str,
        service: &ServiceContract,
        worktree: &Path,
        mission_id: &str,
        run_id: &str,
        at_millis: u64,
    ) -> Result<Self, ServiceError> {
        let owner = PortLeaseOwner::new(mission_id, run_id, service_id)?;
        let request_id = service_request_id(mission_id, run_id, service_id);
        let mut reservation = allocator.reserve(owner, &request_id, at_millis)?;
        let adopted = reservation.is_adopted();
        let lease = reservation.prepare_service_spawn();
        if adopted {
            let mut managed = Self {
                child: None,
                allocator,
                lease,
                stop_on_drop: true,
            };
            if let Err(error) = managed.wait_until_healthy(service_id, service) {
                let _ = managed.stop_inner();
                return Err(error);
            }
            return Ok(managed);
        }
        let mut command = service_command(service, worktree, lease.port())?;
        let child = match command.spawn() {
            Ok(child) => child,
            Err(source) => {
                let _ = allocator.release(&lease);
                return Err(ServiceError::Spawn {
                    service: service_id.to_owned(),
                    source,
                });
            }
        };
        if let Err(error) = allocator.bind_service_process(lease.lease_id(), child.id()) {
            let mut child = child;
            let _ = terminate(&mut child);
            let _ = allocator.release(&lease);
            return Err(error.into());
        }
        let mut managed = Self {
            child: Some(child),
            allocator,
            lease,
            stop_on_drop: true,
        };
        if let Err(error) = managed.wait_until_healthy(service_id, service) {
            let _ = managed.stop_inner();
            return Err(error);
        }
        Ok(managed)
    }

    fn wait_until_healthy(
        &mut self,
        service_id: &str,
        service: &ServiceContract,
    ) -> Result<(), ServiceError> {
        let endpoint = HealthEndpoint::parse(&service.health, self.lease.port())?;
        let deadline = Instant::now() + Duration::from_secs(service.timeout_seconds);
        loop {
            if let Some(child) = self.child.as_mut() {
                if let Some(status) = child.try_wait().map_err(ServiceError::Io)? {
                    return Err(ServiceError::Exited {
                        service: service_id.to_owned(),
                        code: status.code(),
                    });
                }
            } else if self
                .lease
                .service_pid()
                .is_none_or(|pid| !process_alive(pid))
            {
                return Err(ServiceError::Exited {
                    service: service_id.to_owned(),
                    code: None,
                });
            }
            if endpoint.probe()? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(ServiceError::HealthTimeout {
                    service: service_id.to_owned(),
                    timeout_seconds: service.timeout_seconds,
                });
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn stop_inner(&mut self) -> Result<(), ServiceError> {
        let process_result = if let Some(child) = self.child.as_mut() {
            terminate(child)
        } else if let Some(pid) = self.lease.service_pid() {
            terminate_pid(pid)
        } else {
            Ok(())
        };
        self.child = None;
        let release_result = self.allocator.release(&self.lease).map(|_| ());
        process_result?;
        release_result?;
        Ok(())
    }
}

impl Drop for ManagedService {
    fn drop(&mut self) {
        if self.stop_on_drop {
            let _ = self.stop_inner();
        }
    }
}

fn service_command(
    service: &ServiceContract,
    worktree: &Path,
    port: u16,
) -> Result<Command, ServiceError> {
    let (program, args) = service
        .command
        .split_first()
        .ok_or(ServiceError::EmptyCommand)?;
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(worktree)
        .env(&service.port_env, port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    Ok(command)
}

fn canonical_worktree(path: &Path) -> Result<PathBuf, ServiceError> {
    let canonical = std::fs::canonicalize(path).map_err(ServiceError::Io)?;
    if !canonical.is_dir() {
        return Err(ServiceError::InvalidWorktree);
    }
    let info =
        crate::workspace::git_worktree_info(&canonical).ok_or(ServiceError::InvalidWorktree)?;
    let root = std::fs::canonicalize(info.repo_root).map_err(ServiceError::Io)?;
    if root != canonical {
        return Err(ServiceError::InvalidWorktree);
    }
    Ok(canonical)
}

fn terminate(child: &mut Child) -> Result<(), ServiceError> {
    if child.try_wait().map_err(ServiceError::Io)?.is_some() {
        return Ok(());
    }
    terminate_pid(child.id())?;
    let _ = child.wait();
    Ok(())
}

#[cfg(unix)]
fn terminate_pid(pid: u32) -> Result<(), ServiceError> {
    let pid = i32::try_from(pid).map_err(|_| ServiceError::InvalidProcessId)?;
    if !process_alive(pid as u32) {
        return Ok(());
    }
    signal_process_group_or_pid(pid, libc::SIGTERM);
    let deadline = Instant::now() + Duration::from_secs(2);
    while process_alive(pid as u32) && Instant::now() < deadline {
        thread::sleep(POLL_INTERVAL);
    }
    if process_alive(pid as u32) {
        signal_process_group_or_pid(pid, libc::SIGKILL);
    }
    Ok(())
}

#[cfg(unix)]
fn signal_process_group_or_pid(pid: i32, signal: i32) {
    let group_result = unsafe { libc::kill(-pid, signal) };
    if group_result != 0 {
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

#[cfg(windows)]
fn terminate_pid(pid: u32) -> Result<(), ServiceError> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE},
    };
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
        return if process_alive(pid) {
            Err(ServiceError::InvalidProcessId)
        } else {
            Ok(())
        };
    }
    let terminated = unsafe { TerminateProcess(handle, 1) };
    unsafe { CloseHandle(handle) };
    if terminated == 0 {
        Err(ServiceError::Io(std::io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct HealthEndpoint {
    address: SocketAddr,
    authority: String,
    path: String,
}

impl HealthEndpoint {
    fn parse(template: &str, port: u16) -> Result<Self, ServiceError> {
        if template
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        {
            return Err(ServiceError::InvalidHealthEndpoint);
        }
        let rendered = template.replace("{port}", &port.to_string());
        let rest = rendered
            .strip_prefix("http://")
            .ok_or(ServiceError::UnsupportedHealthScheme)?;
        let (authority, path) = rest
            .split_once('/')
            .map_or((rest, "/"), |(authority, path)| (authority, path));
        let (host, parsed_port) = authority
            .rsplit_once(':')
            .ok_or(ServiceError::InvalidHealthEndpoint)?;
        if parsed_port.parse::<u16>().ok() != Some(port)
            || !matches!(host, "127.0.0.1" | "localhost")
        {
            return Err(ServiceError::InvalidHealthEndpoint);
        }
        let address = (host, port)
            .to_socket_addrs()
            .map_err(ServiceError::Io)?
            .find(|address| matches!(address.ip(), IpAddr::V4(ip) if ip == Ipv4Addr::LOCALHOST))
            .ok_or(ServiceError::InvalidHealthEndpoint)?;
        Ok(Self {
            address,
            authority: authority.to_owned(),
            path: format!("/{path}"),
        })
    }

    fn probe(&self) -> Result<bool, ServiceError> {
        let Ok(mut stream) = TcpStream::connect_timeout(&self.address, PROBE_TIMEOUT) else {
            return Ok(false);
        };
        stream
            .set_read_timeout(Some(PROBE_TIMEOUT))
            .map_err(ServiceError::Io)?;
        stream
            .set_write_timeout(Some(PROBE_TIMEOUT))
            .map_err(ServiceError::Io)?;
        write!(
            stream,
            "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            self.path, self.authority
        )
        .map_err(ServiceError::Io)?;
        let mut response = [0_u8; 64];
        let count = match stream.read(&mut response) {
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(ServiceError::Io(error)),
        };
        let status = String::from_utf8_lossy(&response[..count]);
        Ok(status.starts_with("HTTP/1.0 2") || status.starts_with("HTTP/1.1 2"))
    }
}

fn service_request_id(mission_id: &str, run_id: &str, service_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"nagi-service-request-v1");
    for value in [mission_id, run_id, service_id] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
    }
    format!("service-{:x}", digest.finalize())
}

#[derive(Debug, Error)]
pub(crate) enum ServiceError {
    #[error("project service command cannot be empty")]
    EmptyCommand,
    #[error("project service worktree must be a Git checkout root")]
    InvalidWorktree,
    #[error("project service health endpoint must be loopback HTTP")]
    InvalidHealthEndpoint,
    #[error("HTTPS health probes are not supported by the local service runner")]
    UnsupportedHealthScheme,
    #[error("project service {service} failed to spawn: {source}")]
    Spawn {
        service: String,
        #[source]
        source: std::io::Error,
    },
    #[error("project service {service} exited before health check with code {code:?}")]
    Exited { service: String, code: Option<i32> },
    #[error("project service {service} did not become healthy within {timeout_seconds}s")]
    HealthTimeout {
        service: String,
        timeout_seconds: u64,
    },
    #[error(transparent)]
    Port(#[from] PortAllocatorError),
    #[error("project service I/O failed: {0}")]
    Io(std::io::Error),
    #[error("project service process id is invalid")]
    InvalidProcessId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_endpoint_rejects_remote_https_and_wrong_ports() {
        assert!(matches!(
            HealthEndpoint::parse("https://localhost:{port}/health", 42_001),
            Err(ServiceError::UnsupportedHealthScheme)
        ));
        assert!(matches!(
            HealthEndpoint::parse("http://example.com:{port}/health", 42_001),
            Err(ServiceError::InvalidHealthEndpoint)
        ));
        assert!(HealthEndpoint::parse("http://127.0.0.1:99/health", 42_001).is_err());
        assert!(HealthEndpoint::parse(
            "http://127.0.0.1:{port}/health\r\nHost:example.com",
            42_001
        )
        .is_err());
    }

    #[test]
    fn health_probe_requires_a_successful_http_status() {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request).unwrap();
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        let endpoint = HealthEndpoint::parse("http://localhost:{port}/health", port).unwrap();
        assert!(endpoint.probe().unwrap());
        worker.join().unwrap();
    }

    #[test]
    fn failed_spawn_releases_the_reserved_port() {
        let repository = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(repository.path())
            .status()
            .unwrap();
        assert!(status.success());
        let state = tempfile::tempdir().unwrap();
        let allocator = PortAllocator::open(state.path()).unwrap();
        let contract = ProjectContract {
            schema: 1,
            worktree: Default::default(),
            setup: None,
            services: BTreeMap::from([(
                "web".into(),
                ServiceContract {
                    command: vec!["/definitely/missing/nagi-service".into()],
                    port_env: "PORT".into(),
                    health: "http://127.0.0.1:{port}/health".into(),
                    timeout_seconds: 1,
                },
            )]),
            checks: Vec::new(),
            cleanup: Vec::new(),
        };
        assert!(matches!(
            ServiceSet::start(
                allocator.clone(),
                &contract,
                repository.path(),
                "mission",
                "run",
                1,
            ),
            Err(ServiceError::Spawn { .. })
        ));
        assert!(allocator.orphaned().unwrap().is_empty());
        let mut reservation = allocator
            .reserve(
                PortLeaseOwner::new("mission", "next", "web").unwrap(),
                "next-request",
                2,
            )
            .unwrap();
        assert!(reservation.prepare_service_spawn().port() >= 41_000);
    }

    #[test]
    fn service_request_ids_are_bounded_even_for_maximal_owner_ids() {
        let id = service_request_id(&"m".repeat(128), &"r".repeat(128), &"s".repeat(128));
        assert!(id.len() <= 128);
        assert!(id.starts_with("service-"));
    }

    #[test]
    fn service_set_adopts_a_healthy_process_after_runtime_restart() {
        let repository = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(repository.path())
            .status()
            .unwrap();
        assert!(status.success());
        let state = tempfile::tempdir().unwrap();
        let allocator = PortAllocator::open(state.path()).unwrap();
        let owner = PortLeaseOwner::new("mission", "run", "web").unwrap();
        let request_id = service_request_id("mission", "run", "web");
        let mut reservation = allocator.reserve(owner, &request_id, 1).unwrap();
        let lease = reservation.prepare_service_spawn();
        let mut service_process = Command::new("git")
            .args(["cat-file", "--batch"])
            .current_dir(repository.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        allocator
            .bind_service_process(lease.lease_id(), service_process.id())
            .unwrap();

        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, lease.port())).unwrap();
        let worker = thread::spawn(move || loop {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 256];
            let count = stream.read(&mut request).unwrap();
            if count == 0 || !request[..count].starts_with(b"GET ") {
                continue;
            }
            if stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .is_ok()
            {
                break;
            }
        });
        let stray = TcpStream::connect((Ipv4Addr::LOCALHOST, lease.port())).unwrap();
        drop(stray);
        let contract = ProjectContract {
            schema: 1,
            worktree: Default::default(),
            setup: None,
            services: BTreeMap::from([(
                "web".into(),
                ServiceContract {
                    command: vec!["/this/must/not/be/spawned".into()],
                    port_env: "PORT".into(),
                    health: "http://127.0.0.1:{port}/health".into(),
                    timeout_seconds: 1,
                },
            )]),
            checks: Vec::new(),
            cleanup: Vec::new(),
        };

        let set = ServiceSet::start(
            allocator.clone(),
            &contract,
            repository.path(),
            "mission",
            "run",
            2,
        )
        .unwrap();
        assert_eq!(set.ports().get("web"), Some(&lease.port()));
        set.detach();
        worker.join().unwrap();
        terminate(&mut service_process).unwrap();
        assert!(allocator.release(&lease).unwrap());
    }
}
