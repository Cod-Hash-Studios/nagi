use std::{
    fs::{self, OpenOptions},
    io::{Read as _, Write as _},
    path::{Component, Path, PathBuf},
};

use thiserror::Error;

use super::ProjectContract;

const MAX_COPY_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_COPY_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// Copy only the exact repository-owned paths declared by the project recipe.
///
/// Tracked files already created by `git worktree add` are left untouched when
/// their bytes match. Untracked inputs must be ignored by Git. Symlinks,
/// directories, broad patterns and destination overwrites are rejected.
pub(crate) fn copy_declared_ignored_files(
    source_checkout: &Path,
    destination_checkout: &Path,
    contract: &ProjectContract,
) -> Result<Vec<PathBuf>, CopyIgnoredError> {
    let source_checkout = fs::canonicalize(source_checkout).map_err(CopyIgnoredError::Io)?;
    let destination_checkout =
        fs::canonicalize(destination_checkout).map_err(CopyIgnoredError::Io)?;
    let mut copied = Vec::new();
    let mut total = 0_u64;

    for relative in &contract.worktree.copy_ignored {
        let relative = exact_relative_path(relative)?;
        let source = source_checkout.join(&relative);
        reject_symlink_components(&source_checkout, &relative)?;
        let metadata = match fs::symlink_metadata(&source) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(CopyIgnoredError::Io(error)),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(CopyIgnoredError::UnsupportedSource(relative));
        }
        if metadata.len() > MAX_COPY_FILE_BYTES {
            return Err(CopyIgnoredError::FileTooLarge(relative));
        }
        total = total
            .checked_add(metadata.len())
            .ok_or_else(|| CopyIgnoredError::TotalTooLarge(relative.clone()))?;
        if total > MAX_COPY_TOTAL_BYTES {
            return Err(CopyIgnoredError::TotalTooLarge(relative));
        }

        let destination = destination_checkout.join(&relative);
        if destination.exists() {
            if files_equal(&source, &destination)? {
                continue;
            }
            return Err(CopyIgnoredError::DestinationExists(relative));
        }
        if !git_path_is_ignored(&source_checkout, &relative)? {
            return Err(CopyIgnoredError::SourceNotIgnored(relative));
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(CopyIgnoredError::Io)?;
        }
        copy_create_new(&source, &destination, metadata.permissions())?;
        copied.push(relative);
    }

    Ok(copied)
}

fn exact_relative_path(raw: &str) -> Result<PathBuf, CopyIgnoredError> {
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(CopyIgnoredError::InvalidPath(raw.to_owned()));
    }
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            _ => return Err(CopyIgnoredError::InvalidPath(raw.to_owned())),
        }
    }
    if clean.as_os_str().is_empty() {
        Err(CopyIgnoredError::InvalidPath(raw.to_owned()))
    } else {
        Ok(clean)
    }
}

fn reject_symlink_components(root: &Path, relative: &Path) -> Result<(), CopyIgnoredError> {
    let mut current = root.to_owned();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(CopyIgnoredError::InvalidPath(
                relative.to_string_lossy().into_owned(),
            ));
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(CopyIgnoredError::UnsupportedSource(relative.to_owned()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(CopyIgnoredError::Io(error)),
        }
    }
    Ok(())
}

fn git_path_is_ignored(root: &Path, relative: &Path) -> Result<bool, CopyIgnoredError> {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["check-ignore", "--quiet", "--"])
        .arg(relative)
        .status()
        .map_err(CopyIgnoredError::Io)?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(CopyIgnoredError::GitCheckFailed),
    }
}

fn files_equal(left: &Path, right: &Path) -> Result<bool, CopyIgnoredError> {
    let left_metadata = fs::symlink_metadata(left).map_err(CopyIgnoredError::Io)?;
    let right_metadata = fs::symlink_metadata(right).map_err(CopyIgnoredError::Io)?;
    if right_metadata.file_type().is_symlink()
        || !right_metadata.is_file()
        || left_metadata.len() != right_metadata.len()
    {
        return Ok(false);
    }
    let mut left = fs::File::open(left).map_err(CopyIgnoredError::Io)?;
    let mut right = fs::File::open(right).map_err(CopyIgnoredError::Io)?;
    let mut left_buffer = [0_u8; 16 * 1024];
    let mut right_buffer = [0_u8; 16 * 1024];
    loop {
        let left_count = left.read(&mut left_buffer).map_err(CopyIgnoredError::Io)?;
        let right_count = right
            .read(&mut right_buffer)
            .map_err(CopyIgnoredError::Io)?;
        if left_count != right_count || left_buffer[..left_count] != right_buffer[..right_count] {
            return Ok(false);
        }
        if left_count == 0 {
            return Ok(true);
        }
    }
}

fn copy_create_new(
    source: &Path,
    destination: &Path,
    permissions: fs::Permissions,
) -> Result<(), CopyIgnoredError> {
    let mut input = fs::File::open(source).map_err(CopyIgnoredError::Io)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut output = options.open(destination).map_err(CopyIgnoredError::Io)?;
    let result = std::io::copy(&mut input, &mut output)
        .and_then(|_| output.flush())
        .and_then(|_| output.sync_all());
    if let Err(error) = result {
        let _ = fs::remove_file(destination);
        return Err(CopyIgnoredError::Io(error));
    }
    fs::set_permissions(destination, permissions).map_err(CopyIgnoredError::Io)
}

#[derive(Debug, Error)]
pub(crate) enum CopyIgnoredError {
    #[error("copy_ignored path is not one exact relative path: {0}")]
    InvalidPath(String),
    #[error("copy_ignored source must be a regular non-symlink file: {}", .0.display())]
    UnsupportedSource(PathBuf),
    #[error("copy_ignored source is not ignored by Git: {}", .0.display())]
    SourceNotIgnored(PathBuf),
    #[error("copy_ignored destination already exists with different contents: {}", .0.display())]
    DestinationExists(PathBuf),
    #[error("copy_ignored source exceeds 16 MiB: {}", .0.display())]
    FileTooLarge(PathBuf),
    #[error("copy_ignored files exceed the 64 MiB total limit near {}", .0.display())]
    TotalTooLarge(PathBuf),
    #[error("git check-ignore failed")]
    GitCheckFailed,
    #[error("copy_ignored I/O failed: {0}")]
    Io(std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::project_recipe::model::{ProjectContract, WorktreeContract};

    fn git(root: &Path, args: &[&str]) {
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .status()
            .unwrap()
            .success());
    }

    fn contract(paths: &[&str]) -> ProjectContract {
        ProjectContract {
            schema: 1,
            worktree: WorktreeContract {
                copy_ignored: paths.iter().map(|path| (*path).to_owned()).collect(),
                ..Default::default()
            },
            setup: None,
            services: BTreeMap::new(),
            checks: Vec::new(),
            cleanup: Vec::new(),
        }
    }

    #[test]
    fn copies_only_explicit_git_ignored_files() {
        let source = tempfile::tempdir().unwrap();
        git(source.path(), &["init", "-q"]);
        fs::write(source.path().join(".gitignore"), "local.txt\n").unwrap();
        fs::write(source.path().join("local.txt"), "local\n").unwrap();
        let destination = tempfile::tempdir().unwrap();

        let copied = copy_declared_ignored_files(
            source.path(),
            destination.path(),
            &contract(&["local.txt"]),
        )
        .unwrap();
        assert_eq!(copied, vec![PathBuf::from("local.txt")]);
        assert_eq!(
            fs::read_to_string(destination.path().join("local.txt")).unwrap(),
            "local\n"
        );
    }

    #[test]
    fn refuses_unignored_symlink_and_destination_overwrite() {
        let source = tempfile::tempdir().unwrap();
        git(source.path(), &["init", "-q"]);
        fs::write(source.path().join("plain.txt"), "source").unwrap();
        let destination = tempfile::tempdir().unwrap();
        assert!(matches!(
            copy_declared_ignored_files(
                source.path(),
                destination.path(),
                &contract(&["plain.txt"]),
            ),
            Err(CopyIgnoredError::SourceNotIgnored(_))
        ));

        fs::write(source.path().join(".gitignore"), "plain.txt\n").unwrap();
        fs::write(destination.path().join("plain.txt"), "destination").unwrap();
        assert!(matches!(
            copy_declared_ignored_files(
                source.path(),
                destination.path(),
                &contract(&["plain.txt"]),
            ),
            Err(CopyIgnoredError::DestinationExists(_))
        ));

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("plain.txt", source.path().join("linked.txt")).unwrap();
            assert!(matches!(
                copy_declared_ignored_files(
                    source.path(),
                    destination.path(),
                    &contract(&["linked.txt"]),
                ),
                Err(CopyIgnoredError::UnsupportedSource(_))
            ));
        }
    }

    #[test]
    fn identical_tracked_destination_is_not_overwritten() {
        let source = tempfile::tempdir().unwrap();
        git(source.path(), &["init", "-q"]);
        fs::write(source.path().join("tracked.txt"), "same").unwrap();
        let destination = tempfile::tempdir().unwrap();
        fs::write(destination.path().join("tracked.txt"), "same").unwrap();

        assert!(copy_declared_ignored_files(
            source.path(),
            destination.path(),
            &contract(&["tracked.txt"]),
        )
        .unwrap()
        .is_empty());
    }
}
