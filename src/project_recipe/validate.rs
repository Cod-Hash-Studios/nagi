use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};

use thiserror::Error;

use super::model::{
    CheckContract, CommandContract, ProjectContract, ServiceContract, PROJECT_SCHEMA_V1,
};

const MAX_PROJECT_FILE_BYTES: u64 = 256 * 1024;
const MAX_COMMAND_ARGUMENTS: usize = 128;
const MAX_COPY_IGNORED: usize = 32;
const MAX_CHECKS: usize = 32;
const MAX_SERVICES: usize = 16;
const MAX_CLEANUP_COMMANDS: usize = 16;
const MAX_TIMEOUT_SECONDS: u64 = 3_600;

#[derive(Debug, Error)]
pub(crate) enum ProjectContractError {
    #[error("project root is unavailable: {0}")]
    RepositoryUnavailable(std::io::Error),
    #[error("project contract is a symlink; use a regular repository-owned file")]
    SymlinkNotAllowed,
    #[error("project contract exceeds {MAX_PROJECT_FILE_BYTES} bytes")]
    FileTooLarge,
    #[error("project contract cannot be read: {0}")]
    Read(std::io::Error),
    #[error("project contract is invalid TOML: {0}")]
    Parse(toml::de::Error),
    #[error("unsupported project contract schema {actual}; expected {PROJECT_SCHEMA_V1}")]
    UnsupportedSchema { actual: u16 },
    #[error("invalid worktree location: {0}")]
    InvalidWorktreeLocation(String),
    #[error("worktree base must be a non-empty revision")]
    InvalidWorktreeBase,
    #[error("copy_ignored contains too many entries; maximum is {MAX_COPY_IGNORED}")]
    TooManyCopiedFiles,
    #[error("copy_ignored entry must be one exact relative path: {0}")]
    InvalidCopiedPath(String),
    #[error("copy_ignored entry looks secret-bearing and is refused: {0}")]
    SecretCopyRejected(String),
    #[error("{field} command must contain between 1 and {MAX_COMMAND_ARGUMENTS} arguments")]
    InvalidCommandLength { field: String },
    #[error("{field} command contains an empty, NUL, newline, or oversized argument")]
    InvalidCommandArgument { field: String },
    #[error("{field} timeout must be between 1 and {MAX_TIMEOUT_SECONDS} seconds")]
    InvalidTimeout { field: String },
    #[error("project contract has too many services; maximum is {MAX_SERVICES}")]
    TooManyServices,
    #[error("invalid service id: {0}")]
    InvalidServiceId(String),
    #[error("invalid port environment variable for service {service}: {value}")]
    InvalidPortEnvironment { service: String, value: String },
    #[error("service {service} health URL must target loopback and contain {{port}}")]
    InvalidHealthUrl { service: String },
    #[error("project contract has too many checks; maximum is {MAX_CHECKS}")]
    TooManyChecks,
    #[error("invalid check id: {0}")]
    InvalidCheckId(String),
    #[error("duplicate check id: {0}")]
    DuplicateCheckId(String),
    #[error("check {check} contains an empty or oversized coverage label")]
    InvalidCoverage { check: String },
    #[error("project contract has too many cleanup commands; maximum is {MAX_CLEANUP_COMMANDS}")]
    TooManyCleanupCommands,
}

pub(crate) fn load_contract(
    repository: &Path,
) -> Result<Option<ProjectContract>, ProjectContractError> {
    let repository =
        std::fs::canonicalize(repository).map_err(ProjectContractError::RepositoryUnavailable)?;
    let path = repository.join(".nagi/project.toml");
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ProjectContractError::Read(error)),
    };
    if metadata.file_type().is_symlink() {
        return Err(ProjectContractError::SymlinkNotAllowed);
    }
    if !metadata.is_file() {
        return Err(ProjectContractError::Read(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "project contract is not a regular file",
        )));
    }
    if metadata.len() > MAX_PROJECT_FILE_BYTES {
        return Err(ProjectContractError::FileTooLarge);
    }
    let source = std::fs::read_to_string(&path).map_err(ProjectContractError::Read)?;
    let contract =
        toml::from_str::<ProjectContract>(&source).map_err(ProjectContractError::Parse)?;
    validate_contract(&contract)?;
    Ok(Some(contract))
}

pub(crate) fn validate_contract(contract: &ProjectContract) -> Result<(), ProjectContractError> {
    if contract.schema != PROJECT_SCHEMA_V1 {
        return Err(ProjectContractError::UnsupportedSchema {
            actual: contract.schema,
        });
    }
    validate_relative_path(&contract.worktree.location).map_err(|()| {
        ProjectContractError::InvalidWorktreeLocation(contract.worktree.location.clone())
    })?;
    if contract.worktree.base.trim().is_empty()
        || contract.worktree.base.len() > 256
        || contract.worktree.base.contains(['\0', '\n', '\r'])
    {
        return Err(ProjectContractError::InvalidWorktreeBase);
    }
    if contract.worktree.copy_ignored.len() > MAX_COPY_IGNORED {
        return Err(ProjectContractError::TooManyCopiedFiles);
    }
    let mut copied = BTreeSet::new();
    for path in &contract.worktree.copy_ignored {
        validate_copy_path(path)?;
        if !copied.insert(path) {
            return Err(ProjectContractError::InvalidCopiedPath(path.clone()));
        }
    }

    if let Some(setup) = &contract.setup {
        validate_command("setup", setup)?;
    }
    if contract.services.len() > MAX_SERVICES {
        return Err(ProjectContractError::TooManyServices);
    }
    for (service_id, service) in &contract.services {
        validate_service(service_id, service)?;
    }
    if contract.checks.len() > MAX_CHECKS {
        return Err(ProjectContractError::TooManyChecks);
    }
    let mut check_ids = BTreeSet::new();
    for check in &contract.checks {
        validate_check(check)?;
        if !check_ids.insert(&check.id) {
            return Err(ProjectContractError::DuplicateCheckId(check.id.clone()));
        }
    }
    if contract.cleanup.len() > MAX_CLEANUP_COMMANDS {
        return Err(ProjectContractError::TooManyCleanupCommands);
    }
    for (index, command) in contract.cleanup.iter().enumerate() {
        validate_command(&format!("cleanup[{index}]"), command)?;
    }
    Ok(())
}

fn validate_service(
    service_id: &str,
    service: &ServiceContract,
) -> Result<(), ProjectContractError> {
    if !valid_id(service_id) {
        return Err(ProjectContractError::InvalidServiceId(
            service_id.to_owned(),
        ));
    }
    validate_command_parts(
        &format!("services.{service_id}"),
        &service.command,
        service.timeout_seconds,
    )?;
    if !valid_env_name(&service.port_env) {
        return Err(ProjectContractError::InvalidPortEnvironment {
            service: service_id.to_owned(),
            value: service.port_env.clone(),
        });
    }
    let health = service.health.as_str();
    let loopback = health.starts_with("http://127.0.0.1:{port}")
        || health.starts_with("http://localhost:{port}");
    if !loopback
        || health.matches("{port}").count() != 1
        || health.len() > 2_048
        || health
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(ProjectContractError::InvalidHealthUrl {
            service: service_id.to_owned(),
        });
    }
    Ok(())
}

fn validate_check(check: &CheckContract) -> Result<(), ProjectContractError> {
    if !valid_id(&check.id) {
        return Err(ProjectContractError::InvalidCheckId(check.id.clone()));
    }
    validate_command_parts(
        &format!("checks.{}", check.id),
        &check.command,
        check.timeout_seconds,
    )?;
    if check.covers.len() > 16
        || check
            .covers
            .iter()
            .any(|label| label.trim().is_empty() || label.len() > 1_024)
    {
        return Err(ProjectContractError::InvalidCoverage {
            check: check.id.clone(),
        });
    }
    Ok(())
}

fn validate_command(field: &str, command: &CommandContract) -> Result<(), ProjectContractError> {
    validate_command_parts(field, &command.command, command.timeout_seconds)
}

fn validate_command_parts(
    field: &str,
    command: &[String],
    timeout_seconds: u64,
) -> Result<(), ProjectContractError> {
    if command.is_empty() || command.len() > MAX_COMMAND_ARGUMENTS {
        return Err(ProjectContractError::InvalidCommandLength {
            field: field.to_owned(),
        });
    }
    if command.iter().any(|argument| {
        argument.is_empty() || argument.len() > 4_096 || argument.contains(['\0', '\n', '\r'])
    }) {
        return Err(ProjectContractError::InvalidCommandArgument {
            field: field.to_owned(),
        });
    }
    if !(1..=MAX_TIMEOUT_SECONDS).contains(&timeout_seconds) {
        return Err(ProjectContractError::InvalidTimeout {
            field: field.to_owned(),
        });
    }
    Ok(())
}

fn validate_copy_path(raw: &str) -> Result<(), ProjectContractError> {
    if raw.is_empty()
        || raw.len() > 1_024
        || raw.contains(['*', '?', '[', ']', '{', '}', '\0', '\n', '\r'])
        || validate_relative_path(raw).is_err()
    {
        return Err(ProjectContractError::InvalidCopiedPath(raw.to_owned()));
    }
    let lower = raw.to_ascii_lowercase();
    let name = Path::new(&lower)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let safe_env_template = name == ".env.example"
        || name == ".env.sample"
        || name == ".env.template"
        || name.ends_with(".env.example")
        || name.ends_with(".env.sample")
        || name.ends_with(".env.template");
    let looks_secret = (name.starts_with(".env") && !safe_env_template)
        || matches!(
            name,
            ".npmrc"
                | ".netrc"
                | "credentials"
                | "credentials.json"
                | "id_rsa"
                | "id_ed25519"
                | "secrets"
                | "secrets.json"
        )
        || [".pem", ".key", ".p12", ".pfx"]
            .iter()
            .any(|suffix| name.ends_with(suffix));
    if looks_secret {
        return Err(ProjectContractError::SecretCopyRejected(raw.to_owned()));
    }
    Ok(())
}

fn validate_relative_path(raw: &str) -> Result<PathBuf, ()> {
    let path = Path::new(raw);
    if path.is_absolute() || raw.ends_with(['/', '\\']) {
        return Err(());
    }
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            _ => return Err(()),
        }
    }
    if clean.as_os_str().is_empty() {
        Err(())
    } else {
        Ok(clean)
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}

fn valid_env_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && value.len() <= 128
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn valid_source() -> &'static str {
        r#"
schema = 1

[worktree]
location = ".worktrees"
base = "main"
copy_ignored = [".env.example"]

[setup]
command = ["bun", "install", "--frozen-lockfile"]
timeout_seconds = 180

[services.web]
command = ["bun", "run", "dev"]
port_env = "PORT"
health = "http://127.0.0.1:{port}/health"

[[checks]]
id = "quality"
command = ["bun", "run", "check"]
covers = ["code compiles and tests pass"]

[[cleanup]]
command = ["bun", "run", "cleanup"]
timeout_seconds = 30
"#
    }

    #[test]
    fn loads_the_versioned_project_contract_without_executing_it() {
        let repository = tempfile::tempdir().unwrap();
        fs::create_dir_all(repository.path().join(".nagi")).unwrap();
        fs::write(repository.path().join(".nagi/project.toml"), valid_source()).unwrap();

        let contract = load_contract(repository.path()).unwrap().unwrap();
        assert_eq!(contract.schema, PROJECT_SCHEMA_V1);
        assert_eq!(contract.services["web"].port_env, "PORT");
        assert_eq!(contract.checks[0].id, "quality");
        assert_eq!(contract.cleanup.len(), 1);
    }

    #[test]
    fn missing_project_contract_is_not_an_error() {
        let repository = tempfile::tempdir().unwrap();
        assert_eq!(load_contract(repository.path()).unwrap(), None);
    }

    #[test]
    fn rejects_secret_copy_and_broad_globs_but_allows_templates() {
        let contract = toml::from_str::<ProjectContract>(valid_source()).unwrap();
        validate_contract(&contract).unwrap();

        for rejected in [
            ".env",
            ".env.local",
            "keys/prod.pem",
            "config/*.json",
            "../x",
        ] {
            let mut unsafe_contract = contract.clone();
            unsafe_contract.worktree.copy_ignored = vec![rejected.to_owned()];
            assert!(
                validate_contract(&unsafe_contract).is_err(),
                "accepted {rejected}"
            );
        }
    }

    #[test]
    fn rejects_non_loopback_health_urls_and_invalid_environment_names() {
        let mut contract = toml::from_str::<ProjectContract>(valid_source()).unwrap();
        contract.services.get_mut("web").unwrap().health =
            "https://example.com/{port}/health".to_owned();
        assert!(matches!(
            validate_contract(&contract),
            Err(ProjectContractError::InvalidHealthUrl { .. })
        ));

        let mut contract = toml::from_str::<ProjectContract>(valid_source()).unwrap();
        contract.services.get_mut("web").unwrap().port_env = "9PORT".to_owned();
        assert!(matches!(
            validate_contract(&contract),
            Err(ProjectContractError::InvalidPortEnvironment { .. })
        ));
    }

    #[test]
    fn rejects_unknown_fields_versions_and_symlinks() {
        let unknown = valid_source().replace("schema = 1", "schema = 1\nmagic = true");
        assert!(toml::from_str::<ProjectContract>(&unknown).is_err());

        let unsupported = valid_source().replace("schema = 1", "schema = 2");
        let contract = toml::from_str::<ProjectContract>(&unsupported).unwrap();
        assert!(matches!(
            validate_contract(&contract),
            Err(ProjectContractError::UnsupportedSchema { actual: 2 })
        ));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let repository = tempfile::tempdir().unwrap();
            fs::create_dir_all(repository.path().join(".nagi")).unwrap();
            let outside = tempfile::NamedTempFile::new().unwrap();
            fs::write(outside.path(), valid_source()).unwrap();
            symlink(outside.path(), repository.path().join(".nagi/project.toml")).unwrap();
            assert!(matches!(
                load_contract(repository.path()),
                Err(ProjectContractError::SymlinkNotAllowed)
            ));
        }
    }

    #[test]
    fn duplicate_checks_and_unbounded_commands_are_rejected() {
        let mut contract = toml::from_str::<ProjectContract>(valid_source()).unwrap();
        contract.checks.push(contract.checks[0].clone());
        assert!(matches!(
            validate_contract(&contract),
            Err(ProjectContractError::DuplicateCheckId(_))
        ));

        let mut contract = toml::from_str::<ProjectContract>(valid_source()).unwrap();
        contract.setup.as_mut().unwrap().timeout_seconds = 0;
        assert!(matches!(
            validate_contract(&contract),
            Err(ProjectContractError::InvalidTimeout { .. })
        ));
    }
}
