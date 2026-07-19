use std::{
    io::Read as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CheckStatus {
    Pass,
    Warning,
    Fail,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct DoctorCheck {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) status: CheckStatus,
    pub(crate) detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) remediation: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct DoctorReport {
    pub(crate) version: String,
    pub(crate) cwd: String,
    pub(crate) ready: bool,
    pub(crate) provider_count: usize,
    pub(crate) checks: Vec<DoctorCheck>,
}

pub(crate) fn inspect(cwd: &Path) -> DoctorReport {
    let mut checks = Vec::new();
    let git = find_executable("git");
    checks.push(match git {
        Some(ref path) => pass("git", "Git", path.display().to_string()),
        None => fail(
            "git",
            "Git",
            "git was not found on PATH",
            "Install Git and reopen the terminal",
        ),
    });

    let repository = crate::workspace::git_worktree_info(cwd);
    checks.push(match repository.as_ref() {
        Some(info) if !info.is_bare => pass(
            "repository",
            "Repository",
            info.repo_root.display().to_string(),
        ),
        Some(_) => fail(
            "repository",
            "Repository",
            "bare repositories cannot host a mission",
            "Run Nagi from a checked-out Git worktree",
        ),
        None => fail(
            "repository",
            "Repository",
            "the current directory is not inside a Git checkout",
            "cd into the project you want to work on",
        ),
    });

    let mut provider_count = 0;
    for (id, label, binary, exact_version) in [
        (
            "codex",
            "Codex",
            "codex",
            Some(crate::managed_provider::CODEX_TESTED_VERSION),
        ),
        (
            "claude",
            "Claude Code",
            "claude",
            Some(crate::managed_provider::CLAUDE_TESTED_VERSION),
        ),
        (
            "opencode",
            "OpenCode",
            "opencode",
            Some(crate::managed_provider::OPENCODE_TESTED_VERSION),
        ),
    ] {
        let Some(path) = find_executable(binary) else {
            checks.push(warning(
                id,
                label,
                format!("{binary} was not found on PATH"),
                format!("Install {label} if you want to use this runtime"),
            ));
            continue;
        };
        let version = command_version(&path, Duration::from_millis(1500));
        let compatible = exact_version.is_none_or(|expected| {
            version
                .as_deref()
                .ok()
                .and_then(extract_version)
                .is_some_and(|actual| actual == expected)
        });
        if compatible {
            provider_count += 1;
            checks.push(pass(
                id,
                label,
                version.unwrap_or_else(|_| path.display().to_string()),
            ));
        } else {
            let expected = exact_version.unwrap();
            checks.push(warning(
                id,
                label,
                format!(
                    "{}; Nagi's tested {label} version is {expected}",
                    version.unwrap_or_else(|error| error)
                ),
                format!("Install {label} {expected} or choose another provider"),
            ));
        }
    }

    let loaded = crate::config::Config::load();
    checks.push(if loaded.diagnostics.is_empty() {
        pass(
            "config",
            "Configuration",
            crate::config::config_path().display().to_string(),
        )
    } else {
        warning(
            "config",
            "Configuration",
            loaded.diagnostics.join("; "),
            "Run `nagi config check` and fix the reported keys",
        )
    });

    let terminal = std::env::var("TERM").unwrap_or_default();
    checks.push(if terminal.is_empty() || terminal == "dumb" {
        warning(
            "terminal",
            "Terminal",
            "TERM is missing or set to dumb",
            "Use a modern terminal with color and mouse reporting enabled",
        )
    } else {
        pass("terminal", "Terminal", terminal)
    });

    if provider_count == 0 {
        checks.push(fail(
            "provider",
            "Managed runtime",
            "no supported provider is ready",
            "Install Codex, Claude Code or the tested OpenCode release",
        ));
    } else {
        checks.push(pass(
            "provider",
            "Managed runtime",
            format!("{provider_count} provider(s) ready"),
        ));
    }

    let ready = !checks.iter().any(|check| check.status == CheckStatus::Fail);
    DoctorReport {
        version: env!("CARGO_PKG_VERSION").into(),
        cwd: cwd.display().to_string(),
        ready,
        provider_count,
        checks,
    }
}

fn pass(id: &str, label: &str, detail: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        id: id.into(),
        label: label.into(),
        status: CheckStatus::Pass,
        detail: detail.into(),
        remediation: None,
    }
}

fn warning(
    id: &str,
    label: &str,
    detail: impl Into<String>,
    remediation: impl Into<String>,
) -> DoctorCheck {
    DoctorCheck {
        id: id.into(),
        label: label.into(),
        status: CheckStatus::Warning,
        detail: detail.into(),
        remediation: Some(remediation.into()),
    }
}

fn fail(
    id: &str,
    label: &str,
    detail: impl Into<String>,
    remediation: impl Into<String>,
) -> DoctorCheck {
    DoctorCheck {
        id: id.into(),
        label: label.into(),
        status: CheckStatus::Fail,
        detail: detail.into(),
        remediation: Some(remediation.into()),
    }
}

fn find_executable(binary: &str) -> Option<PathBuf> {
    let path = Path::new(binary);
    if path.components().count() > 1 {
        return executable_file(path).then(|| path.to_path_buf());
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|directory| directory.join(binary))
        .find(|candidate| executable_file(candidate))
}

#[cfg(unix)]
fn executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable_file(path: &Path) -> bool {
    path.is_file()
}

fn command_version(executable: &Path, timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(executable)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut output = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    let _ = stdout.read_to_string(&mut output);
                }
                if output.trim().is_empty() {
                    if let Some(mut stderr) = child.stderr.take() {
                        let _ = stderr.read_to_string(&mut output);
                    }
                }
                let output = output.lines().next().unwrap_or_default().trim();
                return if status.success() && !output.is_empty() {
                    Ok(output.to_owned())
                } else {
                    Err(format!("version command exited with {status}"))
                };
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("version command timed out".into());
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn extract_version(output: &str) -> Option<&str> {
    output
        .split(|character: char| character.is_whitespace() || character == 'v')
        .find(|part| {
            part.as_bytes().first().is_some_and(u8::is_ascii_digit)
                && part.chars().all(|character| {
                    character.is_ascii_digit() || matches!(character, '.' | '-' | '+')
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn write_version_fixture(directory: &Path, binary: &str, version: &str) {
        use std::os::unix::fs::PermissionsExt as _;

        let executable = directory.join(binary);
        std::fs::write(
            &executable,
            format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n"),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(executable, permissions).unwrap();
    }

    #[test]
    fn version_extraction_accepts_common_provider_outputs() {
        assert_eq!(extract_version("opencode 1.18.3"), Some("1.18.3"));
        assert_eq!(extract_version("claude v2.1.0"), Some("2.1.0"));
        assert_eq!(extract_version("version unavailable"), None);
    }

    #[cfg(unix)]
    #[test]
    fn doctor_accepts_only_pinned_first_party_provider_versions() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        for binary in ["codex", "claude", "opencode"] {
            write_version_fixture(directory.path(), binary, "0.0.0");
        }
        let previous_path = std::env::var_os("PATH");
        let previous_term = std::env::var_os("TERM");
        unsafe {
            std::env::set_var("PATH", directory.path());
            std::env::set_var("TERM", "xterm-256color");
        }

        let unsupported = inspect(directory.path());

        write_version_fixture(directory.path(), "codex", "codex-cli 0.144.5");
        write_version_fixture(directory.path(), "claude", "2.1.212 (Claude Code)");
        write_version_fixture(directory.path(), "opencode", "opencode 1.18.3");
        let supported = inspect(directory.path());

        unsafe {
            match previous_path {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
            match previous_term {
                Some(term) => std::env::set_var("TERM", term),
                None => std::env::remove_var("TERM"),
            }
        }

        assert_eq!(unsupported.provider_count, 0);
        assert_eq!(supported.provider_count, 3);
    }

    #[cfg(unix)]
    #[test]
    fn version_probe_is_bounded() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("slow");
        std::fs::write(&executable, "#!/bin/sh\nwhile :; do :; done\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();
        let started = Instant::now();
        assert_eq!(
            command_version(&executable, Duration::from_millis(30)).unwrap_err(),
            "version command timed out"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
