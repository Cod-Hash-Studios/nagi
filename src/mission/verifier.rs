#![allow(
    dead_code,
    reason = "trusted checks are tested but not public until closure execution is wired"
)]

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs::{File, Metadata},
    io::Read as _,
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use sha2::{Digest as _, Sha256};
use thiserror::Error;

use super::{
    digest::CanonicalDigest,
    evidence::{CommandSpec, FileDisposition, FileFingerprint, WorkspaceSnapshot},
};

const MAX_GIT_OUTPUT_BYTES: usize = 256 * 1024 * 1024;
const MAX_GIT_STDERR_BYTES: usize = 1024 * 1024;
const MAX_PATHS: usize = 200_000;
const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_TOTAL_HASHED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_CHECK_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const DEFAULT_CHECK_STDOUT_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_CHECK_STDERR_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct TrustedCheckRunner {
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
}

impl Default for TrustedCheckRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl TrustedCheckRunner {
    pub(crate) const fn new() -> Self {
        Self::with_limits(
            DEFAULT_CHECK_TIMEOUT,
            DEFAULT_CHECK_STDOUT_BYTES,
            DEFAULT_CHECK_STDERR_BYTES,
        )
    }

    pub(crate) const fn with_limits(
        timeout: Duration,
        stdout_limit: usize,
        stderr_limit: usize,
    ) -> Self {
        Self {
            timeout,
            stdout_limit,
            stderr_limit,
        }
    }

    /// Executes exactly one declared command without passing through a shell.
    ///
    /// The command still runs with the current user's operating-system rights.
    /// Ambient credentials are removed, but this is not a filesystem sandbox:
    /// callers must still treat repository code as executable local code.
    pub(crate) fn run(
        &self,
        requested_worktree: &Path,
        spec: &CommandSpec,
    ) -> Result<TrustedCheckResult, TrustedCheckError> {
        let cancelled = AtomicBool::new(false);
        self.run_with_cancel(requested_worktree, spec, &cancelled)
    }

    pub(crate) fn run_with_cancel(
        &self,
        requested_worktree: &Path,
        spec: &CommandSpec,
        cancelled: &AtomicBool,
    ) -> Result<TrustedCheckResult, TrustedCheckError> {
        let worktree = canonical_worktree(requested_worktree)?;
        let cwd = canonical_check_cwd(&worktree, spec.cwd())?;
        let executable = resolve_check_program(spec.program(), cwd.path())?;
        run_check_command(
            &executable,
            spec.args(),
            &cwd,
            self.timeout,
            self.stdout_limit,
            self.stderr_limit,
            cancelled,
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrustedCheckResult {
    exit_code: Option<i32>,
    started_at_unix_millis: u64,
    finished_at_unix_millis: u64,
    duration: Duration,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl TrustedCheckResult {
    pub(crate) const fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    pub(crate) const fn started_at_unix_millis(&self) -> u64 {
        self.started_at_unix_millis
    }

    pub(crate) const fn finished_at_unix_millis(&self) -> u64 {
        self.finished_at_unix_millis
    }

    pub(crate) const fn duration(&self) -> Duration {
        self.duration
    }

    pub(crate) fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    pub(crate) fn stderr(&self) -> &[u8] {
        &self.stderr
    }
}

fn canonical_worktree(requested: &Path) -> Result<PathBuf, TrustedCheckError> {
    let worktree =
        std::fs::canonicalize(requested).map_err(|_| TrustedCheckError::WorktreeUnavailable)?;
    if !worktree.is_dir() {
        return Err(TrustedCheckError::WorktreeUnavailable);
    }
    let info =
        crate::workspace::git_worktree_info(&worktree).ok_or(TrustedCheckError::NotGitWorktree)?;
    let root = std::fs::canonicalize(info.repo_root)
        .map_err(|_| TrustedCheckError::WorktreeUnavailable)?;
    if root != worktree {
        return Err(TrustedCheckError::WorktreeMustBeRoot);
    }
    Ok(worktree)
}

struct CanonicalCheckCwd {
    path: PathBuf,
    #[cfg(unix)]
    handle: File,
}

impl CanonicalCheckCwd {
    fn path(&self) -> &Path {
        &self.path
    }
}

fn canonical_check_cwd(
    worktree: &Path,
    relative: &str,
) -> Result<CanonicalCheckCwd, TrustedCheckError> {
    let relative = Path::new(relative);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::ParentDir
            )
        })
    {
        return Err(TrustedCheckError::InvalidCwd);
    }
    let cwd = std::fs::canonicalize(worktree.join(relative))
        .map_err(|_| TrustedCheckError::CwdUnavailable)?;
    if !cwd.starts_with(worktree) {
        return Err(TrustedCheckError::CwdEscapesWorktree);
    }
    if !cwd.is_dir() {
        return Err(TrustedCheckError::CwdUnavailable);
    }
    #[cfg(unix)]
    let handle = open_canonical_directory_beneath(worktree, &cwd)?;
    Ok(CanonicalCheckCwd {
        path: cwd,
        #[cfg(unix)]
        handle,
    })
}

#[cfg(unix)]
fn open_canonical_directory_beneath(
    worktree: &Path,
    cwd: &Path,
) -> Result<File, TrustedCheckError> {
    use std::os::{
        fd::{AsRawFd as _, FromRawFd as _},
        unix::{ffi::OsStrExt as _, fs::OpenOptionsExt as _},
    };

    let relative = cwd
        .strip_prefix(worktree)
        .map_err(|_| TrustedCheckError::CwdEscapesWorktree)?;
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut current = options
        .open(worktree)
        .map_err(|_| TrustedCheckError::CwdUnavailable)?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let name =
            std::ffi::CString::new(name.as_bytes()).map_err(|_| TrustedCheckError::InvalidCwd)?;
        let descriptor = unsafe {
            libc::openat(
                current.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            return Err(TrustedCheckError::CwdUnavailable);
        }
        // SAFETY: `openat` returned a fresh owned descriptor on success.
        current = unsafe { File::from_raw_fd(descriptor) };
    }
    Ok(current)
}

fn resolve_check_program(program: &str, cwd: &Path) -> Result<PathBuf, TrustedCheckError> {
    if program.trim().is_empty() || program.contains('\0') {
        return Err(TrustedCheckError::InvalidProgram);
    }
    let requested = Path::new(program);
    let explicit_path = requested.is_absolute() || requested.components().count() > 1;
    if explicit_path {
        let candidate = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            cwd.join(requested)
        };
        return canonical_executable(&candidate);
    }

    let path = std::env::var_os("PATH").ok_or(TrustedCheckError::ProgramUnavailable)?;
    let mut non_executable_found = false;
    for directory in std::env::split_paths(&path) {
        let directory = if directory.as_os_str().is_empty() {
            cwd.to_path_buf()
        } else if directory.is_absolute() {
            directory
        } else {
            cwd.join(directory)
        };
        for name in executable_names(program) {
            match canonical_executable(&directory.join(name)) {
                Ok(executable) => return Ok(executable),
                Err(TrustedCheckError::ProgramNotExecutable) => non_executable_found = true,
                Err(TrustedCheckError::ProgramUnavailable) => {}
                Err(error) => return Err(error),
            }
        }
    }
    if non_executable_found {
        Err(TrustedCheckError::ProgramNotExecutable)
    } else {
        Err(TrustedCheckError::ProgramUnavailable)
    }
}

#[cfg(not(windows))]
fn executable_names(program: &str) -> Vec<OsString> {
    vec![OsString::from(program)]
}

#[cfg(windows)]
fn executable_names(program: &str) -> Vec<OsString> {
    if Path::new(program).extension().is_some() {
        return vec![OsString::from(program)];
    }
    let extensions =
        std::env::var_os("PATHEXT").unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
    extensions
        .to_string_lossy()
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(|extension| OsString::from(format!("{program}{extension}")))
        .collect()
}

fn canonical_executable(candidate: &Path) -> Result<PathBuf, TrustedCheckError> {
    let executable = match std::fs::canonicalize(candidate) {
        Ok(executable) => executable,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(TrustedCheckError::ProgramUnavailable)
        }
        Err(_) => return Err(TrustedCheckError::ProgramUnavailable),
    };
    let metadata =
        std::fs::metadata(&executable).map_err(|_| TrustedCheckError::ProgramUnavailable)?;
    if !metadata.is_file() || !is_program_executable(&metadata) {
        return Err(TrustedCheckError::ProgramNotExecutable);
    }
    Ok(executable)
}

#[cfg(unix)]
fn is_program_executable(metadata: &Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_program_executable(_metadata: &Metadata) -> bool {
    true
}

fn run_check_command(
    executable: &Path,
    args: &[String],
    cwd: &CanonicalCheckCwd,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
    cancelled: &AtomicBool,
) -> Result<TrustedCheckResult, TrustedCheckError> {
    let mut command = Command::new(executable);
    command
        .args(args)
        .env_clear()
        .env("PWD", cwd.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    copy_trusted_check_environment(&mut command);

    #[cfg(unix)]
    {
        use std::os::{fd::AsRawFd as _, unix::process::CommandExt as _};

        let cwd_fd = cwd.handle.as_raw_fd();
        command.process_group(0);
        // SAFETY: `fchdir` is async-signal-safe and the captured descriptor is
        // kept open until `spawn` has completed. The closure allocates nothing.
        unsafe {
            command.pre_exec(move || {
                if libc::fchdir(cwd_fd) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
    }

    #[cfg(not(unix))]
    command.current_dir(cwd.path());

    let started_at_unix_millis = unix_millis(SystemTime::now());
    let started = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|_| TrustedCheckError::SpawnFailed)?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
        terminate_check_process_group(&mut child);
        let _ = child.wait();
        return Err(TrustedCheckError::PipeUnavailable);
    };
    let stdout_overflowed = Arc::new(AtomicBool::new(false));
    let stderr_overflowed = Arc::new(AtomicBool::new(false));
    let stdout_reader = {
        let overflowed = stdout_overflowed.clone();
        thread::spawn(move || drain_bounded(stdout, stdout_limit, overflowed))
    };
    let stderr_reader = {
        let overflowed = stderr_overflowed.clone();
        thread::spawn(move || drain_bounded(stderr, stderr_limit, overflowed))
    };

    enum StopReason {
        Exited(ExitStatus),
        TimedOut,
        OutputLimit,
        Cancelled,
        WaitFailed,
    }

    let reason = loop {
        if stdout_overflowed.load(Ordering::Acquire) || stderr_overflowed.load(Ordering::Acquire) {
            break StopReason::OutputLimit;
        }
        if cancelled.load(Ordering::Acquire) {
            break StopReason::Cancelled;
        }
        match child.try_wait() {
            Ok(Some(status)) => break StopReason::Exited(status),
            Ok(None) => {}
            Err(_) => break StopReason::WaitFailed,
        }
        if started.elapsed() >= timeout {
            break StopReason::TimedOut;
        }
        thread::sleep(Duration::from_millis(5));
    };

    // A successful parent may have left children holding the output pipes.
    // Terminating the dedicated group here prevents both leaks and reader hangs.
    terminate_check_process_group(&mut child);
    let (status, wait_failed) = match reason {
        StopReason::Exited(status) => (Some(status), false),
        StopReason::TimedOut
        | StopReason::OutputLimit
        | StopReason::Cancelled
        | StopReason::WaitFailed => (None, child.wait().is_err()),
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| TrustedCheckError::ReaderFailed)?
        .map_err(|_| TrustedCheckError::ReaderFailed)?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| TrustedCheckError::ReaderFailed)?
        .map_err(|_| TrustedCheckError::ReaderFailed)?;

    if stdout_overflowed.load(Ordering::Acquire) {
        return Err(TrustedCheckError::StdoutLimitExceeded);
    }
    if stderr_overflowed.load(Ordering::Acquire) {
        return Err(TrustedCheckError::StderrLimitExceeded);
    }
    if wait_failed || matches!(reason, StopReason::WaitFailed) {
        return Err(TrustedCheckError::WaitFailed);
    }
    match reason {
        StopReason::TimedOut => return Err(TrustedCheckError::TimedOut),
        StopReason::OutputLimit => return Err(TrustedCheckError::OutputLimitExceeded),
        StopReason::Cancelled => return Err(TrustedCheckError::Cancelled),
        StopReason::Exited(_) | StopReason::WaitFailed => {}
    }

    let duration = started.elapsed();
    let duration_millis = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    Ok(TrustedCheckResult {
        exit_code: status.and_then(|status| status.code()),
        started_at_unix_millis,
        finished_at_unix_millis: started_at_unix_millis.saturating_add(duration_millis),
        duration,
        stdout,
        stderr,
    })
}

fn copy_trusted_check_environment(command: &mut Command) {
    const ALLOWED: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "TMPDIR",
        "TEMP",
        "TMP",
        "TERM",
        "COLORTERM",
        "LANG",
        "LC_ALL",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "BUN_INSTALL",
        "NVM_DIR",
        "GOPATH",
        "GOROOT",
        "SYSTEMROOT",
        "WINDIR",
        "PATHEXT",
    ];
    for key in ALLOWED {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    if std::env::var_os("LANG").is_none() && std::env::var_os("LC_ALL").is_none() {
        command.env("LC_ALL", "C");
    }
}

#[cfg(unix)]
fn terminate_check_process_group(child: &mut std::process::Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        // The child is placed in a group whose id is its pid before exec.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

#[cfg(not(unix))]
fn terminate_check_process_group(child: &mut std::process::Child) {
    let _ = child.kill();
}

fn unix_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

#[derive(Debug, Error)]
pub(crate) enum TrustedCheckError {
    #[error("check worktree is unavailable")]
    WorktreeUnavailable,
    #[error("check worktree is not a Git checkout")]
    NotGitWorktree,
    #[error("check worktree path must be its checkout root")]
    WorktreeMustBeRoot,
    #[error("check working directory is invalid")]
    InvalidCwd,
    #[error("check working directory is unavailable")]
    CwdUnavailable,
    #[error("check working directory escapes the worktree")]
    CwdEscapesWorktree,
    #[error("check executable declaration is invalid")]
    InvalidProgram,
    #[error("check executable is unavailable")]
    ProgramUnavailable,
    #[error("check executable is not executable")]
    ProgramNotExecutable,
    #[error("check process could not be started")]
    SpawnFailed,
    #[error("check output pipe is unavailable")]
    PipeUnavailable,
    #[error("check process wait failed")]
    WaitFailed,
    #[error("check output reader failed")]
    ReaderFailed,
    #[error("check command timed out")]
    TimedOut,
    #[error("check stdout exceeded its limit")]
    StdoutLimitExceeded,
    #[error("check stderr exceeded its limit")]
    StderrLimitExceeded,
    #[error("check output exceeded its limit")]
    OutputLimitExceeded,
    #[error("check execution was cancelled")]
    Cancelled,
}

#[derive(Clone, Debug)]
pub(crate) struct TrustedGit {
    executable: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitWorkspaceSnapshot {
    head_revision: String,
    digest: String,
    path_count: usize,
    evidence_snapshot: WorkspaceSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitHandoffSummary {
    pub(crate) head_revision: String,
    pub(crate) workspace_digest: String,
    pub(crate) changed_paths: Vec<String>,
    pub(crate) diff_stat: String,
}

impl GitWorkspaceSnapshot {
    pub(crate) fn head_revision(&self) -> &str {
        &self.head_revision
    }

    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }

    pub(crate) const fn path_count(&self) -> usize {
        self.path_count
    }

    pub(crate) fn evidence_snapshot(&self) -> &WorkspaceSnapshot {
        &self.evidence_snapshot
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitManifest {
    head: Vec<u8>,
    index: Vec<u8>,
    flags: Vec<u8>,
    status: Vec<u8>,
    staged_paths: Vec<u8>,
    unstaged_paths: Vec<u8>,
    paths: Vec<u8>,
    ignored_paths: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WorktreeNode {
    Missing,
    Regular {
        sha256: String,
        executable: bool,
        size: u64,
    },
    Symlink {
        target_sha256: String,
    },
}

fn node_fingerprint(node: &WorktreeNode) -> String {
    let mut digest = CanonicalDigest::new(b"trusted-git-worktree-node-v1");
    match node {
        WorktreeNode::Missing => digest.u8(0),
        WorktreeNode::Regular {
            sha256,
            executable,
            size,
        } => {
            digest.u8(1);
            digest.string(sha256);
            digest.bool(*executable);
            digest.u64(*size);
        }
        WorktreeNode::Symlink { target_sha256 } => {
            digest.u8(2);
            digest.string(target_sha256);
        }
    }
    digest.finish()
}

fn node_artifact_hash(node: &WorktreeNode) -> Option<&str> {
    match node {
        WorktreeNode::Regular { sha256, .. } => Some(sha256),
        WorktreeNode::Symlink { target_sha256 } => Some(target_sha256),
        WorktreeNode::Missing => None,
    }
}

impl TrustedGit {
    pub(crate) fn discover() -> Result<Self, VerificationError> {
        for candidate in system_git_candidates()
            .into_iter()
            .chain(path_git_candidates())
        {
            if candidate.is_file() {
                let executable = std::fs::canonicalize(&candidate)
                    .map_err(|_| VerificationError::GitUnavailable)?;
                return Ok(Self { executable });
            }
        }
        Err(VerificationError::GitUnavailable)
    }

    pub(crate) fn head_revision(
        &self,
        requested_worktree: &Path,
    ) -> Result<String, VerificationError> {
        let worktree = std::fs::canonicalize(requested_worktree)
            .map_err(|_| VerificationError::WorktreeUnavailable)?;
        let info = crate::workspace::git_worktree_info(&worktree)
            .ok_or(VerificationError::NotGitWorktree)?;
        let root = std::fs::canonicalize(info.repo_root)
            .map_err(|_| VerificationError::WorktreeUnavailable)?;
        if worktree != root {
            return Err(VerificationError::WorktreeMustBeRoot);
        }
        parse_head_revision(&self.run(&worktree, &["rev-parse", "--verify", "HEAD^{commit}"])?)
    }

    pub(crate) fn scan(
        &self,
        requested_worktree: &Path,
        include_ignored: bool,
    ) -> Result<GitWorkspaceSnapshot, VerificationError> {
        let worktree = std::fs::canonicalize(requested_worktree)
            .map_err(|_| VerificationError::WorktreeUnavailable)?;
        let info = crate::workspace::git_worktree_info(&worktree)
            .ok_or(VerificationError::NotGitWorktree)?;
        let root = std::fs::canonicalize(info.repo_root)
            .map_err(|_| VerificationError::WorktreeUnavailable)?;
        if worktree != root {
            return Err(VerificationError::WorktreeMustBeRoot);
        }

        let before = self.manifest(&worktree, include_ignored)?;
        validate_index(&before.index)?;
        validate_flags(&before.flags)?;
        let paths = manifest_paths(&before, include_ignored)?;
        if paths.len() > MAX_PATHS {
            return Err(VerificationError::PathLimitExceeded);
        }

        let mut total_hashed = 0_u64;
        let mut nodes = BTreeMap::new();
        for path in paths {
            validate_repo_path(&path)?;
            let node = hash_worktree_node(&worktree, &path, &mut total_hashed)?;
            nodes.insert(path, node);
        }
        let after = self.manifest(&worktree, include_ignored)?;
        if before != after {
            return Err(VerificationError::WorkspaceChangedDuringScan);
        }

        let head_revision = parse_head_revision(&before.head)?;
        let tracked_paths = index_paths(&before.index)?;
        let staged_paths = repo_path_set(&before.staged_paths)?;
        let unstaged_paths = repo_path_set(&before.unstaged_paths)?;
        let ignored_paths = repo_path_set(&before.ignored_paths)?;
        let mut digest = CanonicalDigest::new(b"trusted-git-workspace-v1");
        digest.bool(include_ignored);
        digest.bytes(&before.head);
        digest.bytes(&before.index);
        digest.bytes(&before.flags);
        digest.bytes(&before.status);
        digest.bytes(&before.staged_paths);
        digest.bytes(&before.unstaged_paths);
        digest.bytes(&before.paths);
        digest.bytes(&before.ignored_paths);
        digest.u64(nodes.len() as u64);
        for (path, node) in &nodes {
            digest.string(path);
            match node {
                WorktreeNode::Missing => digest.u8(0),
                WorktreeNode::Regular {
                    sha256,
                    executable,
                    size,
                } => {
                    digest.u8(1);
                    digest.string(sha256);
                    digest.bool(*executable);
                    digest.u64(*size);
                }
                WorktreeNode::Symlink { target_sha256 } => {
                    digest.u8(2);
                    digest.string(target_sha256);
                }
            }
        }
        let workspace_digest = digest.finish();
        let files = nodes
            .iter()
            .map(|(path, node)| {
                let disposition = if ignored_paths.contains(path) {
                    FileDisposition::Ignored
                } else if !tracked_paths.contains(path) {
                    FileDisposition::Untracked
                } else if unstaged_paths.contains(path) {
                    FileDisposition::Unstaged
                } else if staged_paths.contains(path) {
                    FileDisposition::Staged
                } else {
                    FileDisposition::Tracked
                };
                FileFingerprint::new(path, node_fingerprint(node), disposition)
            })
            .collect::<Vec<_>>();
        let artifacts = nodes.iter().filter_map(|(path, node)| {
            node_artifact_hash(node).map(|hash| (path.clone(), hash.to_owned()))
        });
        let evidence_snapshot =
            WorkspaceSnapshot::new(head_revision.clone(), workspace_digest.clone(), files)
                .with_artifacts(artifacts);
        Ok(GitWorkspaceSnapshot {
            head_revision,
            digest: workspace_digest,
            path_count: nodes.len(),
            evidence_snapshot,
        })
    }

    pub(crate) fn handoff_summary(
        &self,
        requested_worktree: &Path,
        base_revision: &str,
    ) -> Result<GitHandoffSummary, VerificationError> {
        if !matches!(base_revision.len(), 40 | 64)
            || !base_revision.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(VerificationError::InvalidRevision);
        }
        let worktree = std::fs::canonicalize(requested_worktree)
            .map_err(|_| VerificationError::WorktreeUnavailable)?;
        let info = crate::workspace::git_worktree_info(&worktree)
            .ok_or(VerificationError::NotGitWorktree)?;
        let root = std::fs::canonicalize(info.repo_root)
            .map_err(|_| VerificationError::WorktreeUnavailable)?;
        if worktree != root {
            return Err(VerificationError::WorktreeMustBeRoot);
        }
        let snapshot = self.scan(&worktree, false)?;
        let changed = self.run(
            &worktree,
            &[
                "diff",
                "--name-only",
                "-z",
                "--ignore-submodules=none",
                base_revision,
                "--",
            ],
        )?;
        let untracked = self.run(
            &worktree,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )?;
        let mut changed_paths = repo_path_set(&changed)?;
        changed_paths.extend(repo_path_set(&untracked)?);
        if changed_paths.len() > MAX_PATHS {
            return Err(VerificationError::PathLimitExceeded);
        }
        let stat = self.run(
            &worktree,
            &[
                "diff",
                "--stat",
                "--no-ext-diff",
                "--no-color",
                "--ignore-submodules=none",
                base_revision,
                "--",
            ],
        )?;
        let mut diff_stat = String::from_utf8_lossy(&stat).trim().to_owned();
        let untracked_count = repo_path_set(&untracked)?.len();
        if untracked_count > 0 {
            if !diff_stat.is_empty() {
                diff_stat.push('\n');
            }
            diff_stat.push_str(&format!("{untracked_count} untracked path(s)"));
        }
        Ok(GitHandoffSummary {
            head_revision: snapshot.head_revision,
            workspace_digest: snapshot.digest,
            changed_paths: changed_paths.into_iter().collect(),
            diff_stat,
        })
    }

    fn manifest(
        &self,
        worktree: &Path,
        include_ignored: bool,
    ) -> Result<GitManifest, VerificationError> {
        let head = self.run(worktree, &["rev-parse", "--verify", "HEAD^{commit}"])?;
        let index = self.run(worktree, &["ls-files", "--stage", "-z"])?;
        let flags = self.run(worktree, &["ls-files", "-v", "-z"])?;
        let status = self.run(
            worktree,
            &[
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=all",
                "--ignore-submodules=none",
            ],
        )?;
        let staged_paths = self.run(
            worktree,
            &[
                "diff",
                "--cached",
                "--name-only",
                "-z",
                "--ignore-submodules=none",
            ],
        )?;
        let unstaged_paths = self.run(
            worktree,
            &["diff", "--name-only", "-z", "--ignore-submodules=none"],
        )?;
        let paths = self.run(
            worktree,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ],
        )?;
        let ignored_paths = if include_ignored {
            self.run(
                worktree,
                &[
                    "ls-files",
                    "-z",
                    "--others",
                    "--ignored",
                    "--exclude-standard",
                ],
            )?
        } else {
            Vec::new()
        };
        Ok(GitManifest {
            head,
            index,
            flags,
            status,
            staged_paths,
            unstaged_paths,
            paths,
            ignored_paths,
        })
    }

    fn run(&self, worktree: &Path, args: &[&str]) -> Result<Vec<u8>, VerificationError> {
        let mut command = Command::new(&self.executable);
        command
            .env_clear()
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args([
                "--no-pager",
                "--literal-pathspecs",
                "--no-optional-locks",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.untrackedCache=false",
                "-c",
                "submodule.recurse=false",
                "-C",
            ])
            .arg(worktree)
            .args(args);
        let output = run_bounded(command, GIT_TIMEOUT)?;
        if output.overflowed {
            return Err(VerificationError::GitOutputLimitExceeded);
        }
        if !output.status.success() {
            return Err(VerificationError::GitCommandFailed);
        }
        Ok(output.stdout)
    }
}

fn system_git_candidates() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        Vec::new()
    }
    #[cfg(not(windows))]
    {
        vec![
            PathBuf::from("/usr/bin/git"),
            PathBuf::from("/opt/homebrew/bin/git"),
        ]
    }
}

fn path_git_candidates() -> impl Iterator<Item = PathBuf> {
    let executable = if cfg!(windows) { "git.exe" } else { "git" };
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(move |directory| directory.join(executable))
}

fn validate_index(bytes: &[u8]) -> Result<(), VerificationError> {
    for record in nul_records(bytes) {
        let (metadata, _) = split_once(record, b'\t').ok_or(VerificationError::InvalidGitOutput)?;
        let fields = metadata
            .split(|byte| *byte == b' ')
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        if fields.len() != 3 {
            return Err(VerificationError::InvalidGitOutput);
        }
        if fields[0] == b"160000" {
            return Err(VerificationError::SubmoduleUnsupported);
        }
        if fields[0] == b"000000" {
            return Err(VerificationError::IntentToAddUnsupported);
        }
        if fields[2] != b"0" {
            return Err(VerificationError::ConflictUnsupported);
        }
    }
    Ok(())
}

fn validate_flags(bytes: &[u8]) -> Result<(), VerificationError> {
    for record in nul_records(bytes) {
        let Some(flag) = record.first().copied() else {
            return Err(VerificationError::InvalidGitOutput);
        };
        if flag.is_ascii_lowercase() {
            return Err(VerificationError::AssumeUnchangedUnsupported);
        }
        if flag == b'S' {
            return Err(VerificationError::SkipWorktreeUnsupported);
        }
    }
    Ok(())
}

fn manifest_paths(
    manifest: &GitManifest,
    include_ignored: bool,
) -> Result<Vec<String>, VerificationError> {
    let mut paths = BTreeMap::new();
    for record in nul_records(&manifest.paths).chain(
        include_ignored
            .then_some(manifest.ignored_paths.as_slice())
            .into_iter()
            .flat_map(nul_records),
    ) {
        let path = std::str::from_utf8(record)
            .map_err(|_| VerificationError::NonUtf8Path)?
            .to_owned();
        paths.insert(path, ());
    }
    Ok(paths.into_keys().collect())
}

fn index_paths(bytes: &[u8]) -> Result<BTreeSet<String>, VerificationError> {
    nul_records(bytes)
        .map(|record| {
            let (_, path) = split_once(record, b'\t').ok_or(VerificationError::InvalidGitOutput)?;
            std::str::from_utf8(path)
                .map(str::to_owned)
                .map_err(|_| VerificationError::NonUtf8Path)
        })
        .collect()
}

fn repo_path_set(bytes: &[u8]) -> Result<BTreeSet<String>, VerificationError> {
    nul_records(bytes)
        .map(|record| {
            std::str::from_utf8(record)
                .map(str::to_owned)
                .map_err(|_| VerificationError::NonUtf8Path)
        })
        .collect()
}

fn nul_records(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
}

fn split_once(bytes: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    let index = bytes.iter().position(|byte| *byte == delimiter)?;
    Some((&bytes[..index], &bytes[index + 1..]))
}

fn validate_repo_path(path: &str) -> Result<(), VerificationError> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::ParentDir
            )
        })
        || matches!(path.components().next(), Some(Component::Normal(value)) if value == OsStr::new(".git"))
    {
        return Err(VerificationError::UnsafePath);
    }
    Ok(())
}

fn hash_worktree_node(
    root: &Path,
    relative: &str,
    total_hashed: &mut u64,
) -> Result<WorktreeNode, VerificationError> {
    let path = root.join(relative);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorktreeNode::Missing)
        }
        Err(error) => return Err(VerificationError::Io(error)),
    };
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(&path)?;
        let mut hasher = Sha256::new();
        hasher.update(target.as_os_str().as_encoded_bytes());
        return Ok(WorktreeNode::Symlink {
            target_sha256: hex_digest(hasher.finalize()),
        });
    }
    if !metadata.is_file() {
        return Err(VerificationError::SpecialFileUnsupported);
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(VerificationError::FileLimitExceeded);
    }
    *total_hashed = total_hashed
        .checked_add(metadata.len())
        .ok_or(VerificationError::HashLimitExceeded)?;
    if *total_hashed > MAX_TOTAL_HASHED_BYTES {
        return Err(VerificationError::HashLimitExceeded);
    }

    let mut file = open_regular_without_following(&path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || !same_file_identity(&metadata, &opened) {
        return Err(VerificationError::WorkspaceChangedDuringScan);
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let after = file.metadata()?;
    if !same_file_identity(&opened, &after) {
        return Err(VerificationError::WorkspaceChangedDuringScan);
    }
    Ok(WorktreeNode::Regular {
        sha256: hex_digest(hasher.finalize()),
        executable: is_executable(&opened),
        size: opened.len(),
    })
}

fn open_regular_without_following(path: &Path) -> Result<File, VerificationError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    Ok(options.open(path)?)
}

#[cfg(unix)]
fn same_file_identity(left: &Metadata, right: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.mode() == right.mode()
}

#[cfg(not(unix))]
fn same_file_identity(left: &Metadata, right: &Metadata) -> bool {
    left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && left.permissions().readonly() == right.permissions().readonly()
}

#[cfg(unix)]
fn is_executable(metadata: &Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &Metadata) -> bool {
    false
}

fn parse_head_revision(bytes: &[u8]) -> Result<String, VerificationError> {
    let revision = std::str::from_utf8(bytes)
        .map_err(|_| VerificationError::InvalidGitOutput)?
        .trim();
    if !(40..=64).contains(&revision.len())
        || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(VerificationError::InvalidGitOutput);
    }
    Ok(revision.to_owned())
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct BoundedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    overflowed: bool,
}

fn run_bounded(
    mut command: Command,
    timeout: Duration,
) -> Result<BoundedOutput, VerificationError> {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let mut child = command.spawn().map_err(VerificationError::Io)?;
    let stdout = child
        .stdout
        .take()
        .ok_or(VerificationError::GitUnavailable)?;
    let stderr = child
        .stderr
        .take()
        .ok_or(VerificationError::GitUnavailable)?;
    let overflowed = Arc::new(AtomicBool::new(false));
    let stdout_overflowed = overflowed.clone();
    let stdout_reader =
        thread::spawn(move || drain_bounded(stdout, MAX_GIT_OUTPUT_BYTES, stdout_overflowed));
    let stderr_overflowed = overflowed.clone();
    let stderr_reader =
        thread::spawn(move || drain_bounded(stderr, MAX_GIT_STDERR_BYTES, stderr_overflowed));

    let deadline = Instant::now() + timeout;
    let status = loop {
        if overflowed.load(Ordering::Acquire) {
            let _ = child.kill();
            break child.wait()?;
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(VerificationError::GitTimeout);
        }
        thread::sleep(Duration::from_millis(5));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| VerificationError::ReaderFailed)??;
    let _stderr = stderr_reader
        .join()
        .map_err(|_| VerificationError::ReaderFailed)??;
    Ok(BoundedOutput {
        status,
        stdout,
        overflowed: overflowed.load(Ordering::Acquire),
    })
}

fn drain_bounded<R: std::io::Read>(
    mut reader: R,
    limit: usize,
    overflowed: Arc<AtomicBool>,
) -> Result<Vec<u8>, std::io::Error> {
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(retained);
        }
        let remaining = limit.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..read.min(remaining)]);
        if read > remaining {
            overflowed.store(true, Ordering::Release);
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum VerificationError {
    #[error("Git executable is unavailable")]
    GitUnavailable,
    #[error("worktree is unavailable")]
    WorktreeUnavailable,
    #[error("path is not a Git worktree")]
    NotGitWorktree,
    #[error("worktree path must be its checkout root")]
    WorktreeMustBeRoot,
    #[error("Git command failed")]
    GitCommandFailed,
    #[error("Git command timed out")]
    GitTimeout,
    #[error("Git output exceeded its limit")]
    GitOutputLimitExceeded,
    #[error("Git output is invalid")]
    InvalidGitOutput,
    #[error("Git revision is invalid")]
    InvalidRevision,
    #[error("repository path is not valid UTF-8")]
    NonUtf8Path,
    #[error("repository path is unsafe")]
    UnsafePath,
    #[error("repository contains too many paths")]
    PathLimitExceeded,
    #[error("repository file exceeds the verifier limit")]
    FileLimitExceeded,
    #[error("repository content exceeds the verifier hash limit")]
    HashLimitExceeded,
    #[error("workspace changed while it was being scanned")]
    WorkspaceChangedDuringScan,
    #[error("submodules are not supported by trusted verification yet")]
    SubmoduleUnsupported,
    #[error("intent-to-add index entries are not supported")]
    IntentToAddUnsupported,
    #[error("conflicted index entries are not supported")]
    ConflictUnsupported,
    #[error("assume-unchanged index entries are not supported")]
    AssumeUnchangedUnsupported,
    #[error("skip-worktree index entries are not supported")]
    SkipWorktreeUnsupported,
    #[error("special files are not supported")]
    SpecialFileUnsupported,
    #[error("verifier output reader failed")]
    ReaderFailed,
    #[error("verifier I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .expect("run git fixture command");
        assert!(status.success(), "git fixture command failed: {args:?}");
    }

    fn repository() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init", "-q"]);
        git(directory.path(), &["config", "user.name", "Verifier Test"]);
        git(
            directory.path(),
            &["config", "user.email", "verifier@example.invalid"],
        );
        std::fs::write(directory.path().join("tracked.txt"), "first\n").unwrap();
        git(directory.path(), &["add", "tracked.txt"]);
        git(directory.path(), &["commit", "-qm", "fixture"]);
        directory
    }

    #[cfg(unix)]
    fn executable_script(path: &Path, source: &str) {
        std::fs::write(path, source).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn trusted_check_runs_once_with_literal_args_and_a_relative_cwd() {
        let directory = repository();
        let nested = directory.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        executable_script(
            &nested.join("probe"),
            "#!/bin/sh\nprintf 'run\\n' >> run.log\nprintf '%s|%s' \"$1\" \"$(pwd)\"\nexit 7\n",
        );
        let runner = TrustedCheckRunner::with_limits(Duration::from_secs(10), 1024, 1024);

        let result = runner
            .run(
                directory.path(),
                &super::super::evidence::CommandSpec::new(
                    "./probe",
                    ["value with spaces; $(not-executed)"],
                    "nested",
                ),
            )
            .unwrap();

        assert_eq!(result.exit_code(), Some(7));
        assert_eq!(
            result.stdout(),
            format!(
                "value with spaces; $(not-executed)|{}",
                nested.canonicalize().unwrap().display()
            )
            .as_bytes()
        );
        assert!(result.stderr().is_empty());
        assert_eq!(
            std::fs::read_to_string(nested.join("run.log")).unwrap(),
            "run\n"
        );
        assert!(result.finished_at_unix_millis() >= result.started_at_unix_millis());
        assert!(result.duration() <= Duration::from_secs(10));
    }

    #[test]
    fn trusted_check_rejects_parent_traversal_cwd() {
        let directory = repository();
        let runner = TrustedCheckRunner::with_limits(Duration::from_secs(2), 1024, 1024);

        assert!(matches!(
            runner.run(
                directory.path(),
                &super::super::evidence::CommandSpec::new("git", ["--version"], "../"),
            ),
            Err(TrustedCheckError::InvalidCwd)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn trusted_check_rejects_a_cwd_symlink_that_escapes_the_worktree() {
        use std::os::unix::fs::symlink;

        let directory = repository();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), directory.path().join("outside")).unwrap();
        let runner = TrustedCheckRunner::with_limits(Duration::from_secs(2), 1024, 1024);

        assert!(matches!(
            runner.run(
                directory.path(),
                &super::super::evidence::CommandSpec::new("git", ["--version"], "outside"),
            ),
            Err(TrustedCheckError::CwdEscapesWorktree)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn trusted_check_allows_a_cwd_symlink_that_resolves_inside_the_worktree() {
        use std::os::unix::fs::symlink;

        let directory = repository();
        let nested = directory.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        executable_script(&nested.join("probe"), "#!/bin/sh\nprintf 'inside'\n");
        symlink("nested", directory.path().join("inside-link")).unwrap();
        let runner = TrustedCheckRunner::with_limits(Duration::from_secs(10), 1024, 1024);

        let result = runner
            .run(
                directory.path(),
                &super::super::evidence::CommandSpec::new(
                    "./probe",
                    [] as [&str; 0],
                    "inside-link",
                ),
            )
            .unwrap();
        assert_eq!(result.stdout(), b"inside");
    }

    #[cfg(unix)]
    #[test]
    fn trusted_check_times_out_a_command_with_a_descendant() {
        let directory = repository();
        let script = directory.path().join("spawn-descendant");
        executable_script(&script, "#!/bin/sh\nsleep 30 &\nwait\n");
        let runner = TrustedCheckRunner::with_limits(Duration::from_millis(150), 1024, 1024);

        assert!(matches!(
            runner.run(
                directory.path(),
                &super::super::evidence::CommandSpec::new(
                    "./spawn-descendant",
                    [] as [&str; 0],
                    "."
                ),
            ),
            Err(TrustedCheckError::TimedOut)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn trusted_check_cancellation_terminates_the_process_group() {
        let directory = repository();
        let script = directory.path().join("cancel-check");
        executable_script(&script, "#!/bin/sh\nsleep 30 &\nwait\n");
        let runner = TrustedCheckRunner::with_limits(Duration::from_secs(30), 1024, 1024);
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = cancelled.clone();
        let setter = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            signal.store(true, Ordering::Release);
        });
        let started = Instant::now();

        let result = runner.run_with_cancel(
            directory.path(),
            &super::super::evidence::CommandSpec::new("./cancel-check", [] as [&str; 0], "."),
            &cancelled,
        );
        setter.join().unwrap();

        assert!(matches!(result, Err(TrustedCheckError::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn trusted_check_kills_descendants_when_the_parent_exits() {
        let directory = repository();
        let script = directory.path().join("leave-descendant");
        executable_script(
            &script,
            "#!/bin/sh\nsleep 30 &\nprintf '%s' \"$!\"\nexit 0\n",
        );
        let runner = TrustedCheckRunner::with_limits(Duration::from_secs(10), 1024, 1024);

        let result = runner
            .run(
                directory.path(),
                &super::super::evidence::CommandSpec::new(
                    "./leave-descendant",
                    [] as [&str; 0],
                    ".",
                ),
            )
            .unwrap();

        let pid: i32 = std::str::from_utf8(result.stdout())
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            if !alive || Instant::now() >= deadline {
                assert!(!alive, "descendant process survived timeout");
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn trusted_check_enforces_stdout_and_stderr_limits_independently() {
        let directory = repository();
        executable_script(
            &directory.path().join("overflow-stdout"),
            "#!/bin/sh\nprintf '123456789'\n",
        );
        executable_script(
            &directory.path().join("overflow-stderr"),
            "#!/bin/sh\nprintf '12345' >&2\n",
        );
        let runner = TrustedCheckRunner::with_limits(Duration::from_secs(10), 8, 4);

        let stdout_error = runner
            .run(
                directory.path(),
                &super::super::evidence::CommandSpec::new(
                    "./overflow-stdout",
                    [] as [&str; 0],
                    ".",
                ),
            )
            .unwrap_err();
        assert!(matches!(
            &stdout_error,
            TrustedCheckError::StdoutLimitExceeded
        ));
        assert_eq!(stdout_error.to_string(), "check stdout exceeded its limit");
        assert!(matches!(
            runner.run(
                directory.path(),
                &super::super::evidence::CommandSpec::new(
                    "./overflow-stderr",
                    [] as [&str; 0],
                    "."
                ),
            ),
            Err(TrustedCheckError::StderrLimitExceeded)
        ));
    }

    #[test]
    fn trusted_check_reports_a_missing_program_without_spawning() {
        let directory = repository();
        let runner = TrustedCheckRunner::with_limits(Duration::from_secs(2), 1024, 1024);

        let error = runner
            .run(
                directory.path(),
                &super::super::evidence::CommandSpec::new(
                    "program-that-does-not-exist-8ea9b51a",
                    [] as [&str; 0],
                    ".",
                ),
            )
            .unwrap_err();
        assert!(matches!(error, TrustedCheckError::ProgramUnavailable));
        assert_eq!(error.to_string(), "check executable is unavailable");
    }

    #[test]
    fn tracked_staged_unstaged_deleted_and_untracked_changes_alter_the_snapshot() {
        let directory = repository();
        let scanner = TrustedGit::discover().unwrap();
        let clean = scanner.scan(directory.path(), false).unwrap();
        assert_eq!(
            clean.head_revision(),
            scanner.head_revision(directory.path()).unwrap()
        );
        assert_eq!(clean.path_count(), 1);

        std::fs::write(directory.path().join("tracked.txt"), "staged\n").unwrap();
        git(directory.path(), &["add", "tracked.txt"]);
        std::fs::write(directory.path().join("tracked.txt"), "unstaged\n").unwrap();
        std::fs::write(directory.path().join("untracked.txt"), "new\n").unwrap();
        let changed = scanner.scan(directory.path(), false).unwrap();
        assert_ne!(clean.digest(), changed.digest());

        std::fs::remove_file(directory.path().join("tracked.txt")).unwrap();
        let deleted = scanner.scan(directory.path(), false).unwrap();
        assert_ne!(changed.digest(), deleted.digest());
    }

    #[test]
    fn trusted_scan_exports_exact_evidence_file_dispositions() {
        let directory = repository();
        std::fs::write(directory.path().join(".gitignore"), "ignored.log\n").unwrap();
        git(directory.path(), &["add", ".gitignore"]);
        git(directory.path(), &["commit", "-qm", "ignore fixture"]);

        std::fs::write(directory.path().join("tracked.txt"), "unstaged\n").unwrap();
        std::fs::write(directory.path().join("staged.txt"), "staged\n").unwrap();
        git(directory.path(), &["add", "staged.txt"]);
        std::fs::write(directory.path().join("untracked.txt"), "untracked\n").unwrap();
        std::fs::write(directory.path().join("ignored.log"), "ignored\n").unwrap();

        let snapshot = TrustedGit::discover()
            .unwrap()
            .scan(directory.path(), true)
            .unwrap();
        let evidence = snapshot.evidence_snapshot();

        assert_eq!(
            evidence.file_disposition(".gitignore"),
            Some(super::super::evidence::FileDisposition::Tracked)
        );
        assert_eq!(
            evidence.file_disposition("tracked.txt"),
            Some(super::super::evidence::FileDisposition::Unstaged)
        );
        assert_eq!(
            evidence.file_disposition("staged.txt"),
            Some(super::super::evidence::FileDisposition::Staged)
        );
        assert_eq!(
            evidence.file_disposition("untracked.txt"),
            Some(super::super::evidence::FileDisposition::Untracked)
        );
        assert_eq!(
            evidence.file_disposition("ignored.log"),
            Some(super::super::evidence::FileDisposition::Ignored)
        );
    }

    #[test]
    fn ignored_content_is_excluded_by_default_and_included_explicitly() {
        let directory = repository();
        std::fs::write(directory.path().join(".gitignore"), "ignored.log\n").unwrap();
        git(directory.path(), &["add", ".gitignore"]);
        git(directory.path(), &["commit", "-qm", "ignore fixture"]);
        std::fs::write(directory.path().join("ignored.log"), "first\n").unwrap();
        let scanner = TrustedGit::discover().unwrap();
        let default_before = scanner.scan(directory.path(), false).unwrap();
        let strict_before = scanner.scan(directory.path(), true).unwrap();

        std::fs::write(directory.path().join("ignored.log"), "second\n").unwrap();
        let default_after = scanner.scan(directory.path(), false).unwrap();
        let strict_after = scanner.scan(directory.path(), true).unwrap();
        assert_eq!(default_before.digest(), default_after.digest());
        assert_ne!(strict_before.digest(), strict_after.digest());
    }

    #[test]
    fn unsafe_index_visibility_flags_fail_closed() {
        let directory = repository();
        let scanner = TrustedGit::discover().unwrap();

        git(
            directory.path(),
            &["update-index", "--assume-unchanged", "tracked.txt"],
        );
        assert!(matches!(
            scanner.scan(directory.path(), false),
            Err(VerificationError::AssumeUnchangedUnsupported)
        ));
        git(
            directory.path(),
            &["update-index", "--no-assume-unchanged", "tracked.txt"],
        );
        git(
            directory.path(),
            &["update-index", "--skip-worktree", "tracked.txt"],
        );
        assert!(matches!(
            scanner.scan(directory.path(), false),
            Err(VerificationError::SkipWorktreeUnsupported)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_hashes_its_target_text_without_following_external_content() {
        use std::os::unix::fs::symlink;

        let directory = repository();
        let external = tempfile::tempdir().unwrap();
        let external_file = external.path().join("outside.txt");
        std::fs::write(&external_file, "secret-one\n").unwrap();
        symlink(&external_file, directory.path().join("link.txt")).unwrap();
        let scanner = TrustedGit::discover().unwrap();
        let before = scanner.scan(directory.path(), false).unwrap();

        std::fs::write(&external_file, "secret-two\n").unwrap();
        let after_external_change = scanner.scan(directory.path(), false).unwrap();
        assert_eq!(before.digest(), after_external_change.digest());

        std::fs::remove_file(directory.path().join("link.txt")).unwrap();
        symlink("tracked.txt", directory.path().join("link.txt")).unwrap();
        let changed_target = scanner.scan(directory.path(), false).unwrap();
        assert_ne!(before.digest(), changed_target.digest());
    }
}
