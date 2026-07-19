#[cfg(any(windows, test))]
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
use std::process::Command;

pub(crate) const SAFE_PLUGIN_ENV_KEYS: &[&str] = &[
    "PATH",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "TEMP",
    "TMP",
    "SYSTEMROOT",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
];
pub(crate) const PLUGIN_ISOLATED_PANE_ENV_MARKER: &str = "NAGI_INTERNAL_ISOLATED_PLUGIN_PANE";

/// Remove ambient credentials and restore only the minimal cross-platform
/// environment needed to locate and launch a plugin toolchain.
pub(crate) fn scrub_plugin_environment(command: &mut Command) {
    command.env_clear();
    for key in SAFE_PLUGIN_ENV_KEYS {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
}

pub(crate) fn command_for_argv(program: &str, args: &[String]) -> Command {
    let mut command = command_for_program(program);
    command.args(args);
    command
}

#[cfg(not(windows))]
fn command_for_program(program: &str) -> Command {
    Command::new(program)
}

#[cfg(windows)]
fn command_for_program(program: &str) -> Command {
    let resolved = resolve_windows_program(program);
    let command_program = resolved.as_ref().map_or_else(
        || std::ffi::OsString::from(program),
        |path| path.as_os_str().to_os_string(),
    );
    if is_windows_batch_file_name(program)
        || resolved
            .as_ref()
            .is_some_and(|path| is_windows_batch_path(path))
    {
        let shell =
            std::env::var_os("ComSpec").unwrap_or_else(|| r"C:\Windows\System32\cmd.exe".into());
        let mut command = Command::new(shell);
        command.arg("/d").arg("/c").arg(command_program);
        command
    } else {
        Command::new(command_program)
    }
}

#[cfg(windows)]
fn resolve_windows_program(program: &str) -> Option<PathBuf> {
    if has_path_separator(program) {
        return None;
    }
    let path = Path::new(program);
    if path.extension().is_some() {
        return std::env::var_os("PATH").and_then(|path_var| {
            std::env::split_paths(&path_var)
                .map(|dir| dir.join(program))
                .find(|candidate| candidate.is_file())
        });
    }
    let extensions = windows_path_extensions();
    std::env::var_os("PATH").and_then(|path_var| {
        std::env::split_paths(&path_var).find_map(|dir| {
            extensions
                .iter()
                .map(|extension| dir.join(format!("{program}{extension}")))
                .find(|candidate| candidate.is_file())
        })
    })
}

#[cfg(windows)]
fn windows_path_extensions() -> Vec<String> {
    std::env::var_os("PATHEXT")
        .map(|value| {
            value
                .to_string_lossy()
                .split(';')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(|part| {
                    if part.starts_with('.') {
                        part.to_string()
                    } else {
                        format!(".{part}")
                    }
                })
                .collect::<Vec<_>>()
        })
        .filter(|extensions| !extensions.is_empty())
        .unwrap_or_else(|| {
            vec![
                ".COM".to_string(),
                ".EXE".to_string(),
                ".BAT".to_string(),
                ".CMD".to_string(),
            ]
        })
}

#[cfg(windows)]
fn has_path_separator(program: &str) -> bool {
    program.contains(['/', '\\'])
}

#[cfg(windows)]
fn is_windows_batch_path(path: &Path) -> bool {
    path.extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(is_windows_batch_extension)
}

#[cfg(any(windows, test))]
fn is_windows_batch_file_name(program: &str) -> bool {
    Path::new(program)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(is_windows_batch_extension)
}

#[cfg(any(windows, test))]
fn is_windows_batch_extension(extension: &str) -> bool {
    extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_windows_batch_extensions_case_insensitively() {
        assert!(is_windows_batch_file_name("npm.cmd"));
        assert!(is_windows_batch_file_name("script.BAT"));
        assert!(!is_windows_batch_file_name("node.exe"));
        assert!(!is_windows_batch_file_name("node"));
    }

    #[cfg(unix)]
    #[test]
    fn plugin_environment_scrub_keeps_only_the_runtime_allowlist() {
        let mut command = Command::new("/usr/bin/env");
        command.env("NAGI_TEST_SECRET", "must-not-leak");
        scrub_plugin_environment(&mut command);

        let output = command.output().unwrap();
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).unwrap();
        assert!(!output.contains("NAGI_TEST_SECRET="));
        assert!(output.lines().any(|line| line.starts_with("PATH=")));
    }
}
