use std::{path::Path, time::Duration};

use crate::mission::{
    evidence::CommandSpec,
    verifier::{TrustedCheckError, TrustedCheckResult, TrustedCheckRunner},
};

use super::{CheckContract, CommandContract};

pub(crate) fn run_setup(
    repository: &Path,
    setup: &CommandContract,
) -> Result<ProjectCommandResult, TrustedCheckError> {
    run_parts(repository, "setup", &setup.command, setup.timeout_seconds)
}

pub(crate) fn run_check(
    repository: &Path,
    check: &CheckContract,
) -> Result<ProjectCommandResult, TrustedCheckError> {
    run_parts(repository, &check.id, &check.command, check.timeout_seconds)
}

pub(crate) fn run_cleanup(
    repository: &Path,
    index: usize,
    cleanup: &CommandContract,
) -> Result<ProjectCommandResult, TrustedCheckError> {
    run_parts(
        repository,
        &format!("cleanup[{index}]"),
        &cleanup.command,
        cleanup.timeout_seconds,
    )
}

fn run_parts(
    repository: &Path,
    id: &str,
    command: &[String],
    timeout_seconds: u64,
) -> Result<ProjectCommandResult, TrustedCheckError> {
    let (program, args) = command
        .split_first()
        .expect("validated project commands are non-empty");
    let spec = CommandSpec::new(program, args.iter().cloned(), ".");
    let result = TrustedCheckRunner::with_limits(
        Duration::from_secs(timeout_seconds),
        16 * 1024 * 1024,
        8 * 1024 * 1024,
    )
    .run(repository, &spec)?;
    Ok(ProjectCommandResult::from_trusted(id, result))
}

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct ProjectCommandResult {
    pub(crate) id: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) duration_millis: u64,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl ProjectCommandResult {
    fn from_trusted(id: &str, result: TrustedCheckResult) -> Self {
        Self {
            id: id.to_owned(),
            exit_code: result.exit_code(),
            duration_millis: u64::try_from(result.duration().as_millis()).unwrap_or(u64::MAX),
            stdout: String::from_utf8_lossy(result.stdout()).into_owned(),
            stderr: String::from_utf8_lossy(result.stderr()).into_owned(),
        }
    }

    pub(crate) const fn succeeded(&self) -> bool {
        matches!(self.exit_code, Some(0))
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn git(root: &Path, args: &[&str]) {
        assert!(Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .status()
            .unwrap()
            .success());
    }

    #[cfg(unix)]
    #[test]
    fn setup_runs_bounded_argv_without_a_shell() {
        use std::os::unix::fs::PermissionsExt as _;

        let repository = tempfile::tempdir().unwrap();
        git(repository.path(), &["init", "-q"]);
        let script = repository.path().join("setup-script");
        std::fs::write(&script, "#!/bin/sh\nprintf '%s' \"$1\"\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let result = run_setup(
            repository.path(),
            &CommandContract {
                command: vec!["./setup-script".into(), "hello; touch nope".into()],
                timeout_seconds: 10,
            },
        )
        .unwrap();
        assert!(result.succeeded());
        assert_eq!(result.stdout, "hello; touch nope");
        assert!(!repository.path().join("nope").exists());
    }
}
