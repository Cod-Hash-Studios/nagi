use std::{collections::BTreeSet, fmt};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::api::schema::{
    ContractVersionV1, PluginApprovalStateV1, PluginGrantV1, PluginLockEntryV1, PluginRuntimeV2,
};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum PluginCapability {
    NagiStateRead,
    NagiLayoutWrite,
    PaneContentRead,
    PaneInputWrite,
    WorkspaceFilesRead(WorkspaceScope),
    WorkspaceFilesWrite(WorkspaceScope),
    ProcessSpawn(Vec<String>),
    Network(String),
    ClipboardWrite,
    NotificationsSend,
    MissionRead,
    MissionAttentionPropose,
    MissionEvidencePropose,
    SecretsRead(String),
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum WorkspaceScope {
    Changed,
    Worktree,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum CapabilityError {
    #[error("unknown plugin capability: {0}")]
    Unknown(String),
    #[error("plugin capability scope is missing: {0}")]
    MissingScope(String),
    #[error("invalid workspace capability scope: {0}")]
    InvalidWorkspaceScope(String),
    #[error("invalid process allowlist: {0}")]
    InvalidProcessAllowlist(String),
    #[error("invalid network origin: {0}")]
    InvalidNetworkOrigin(String),
    #[error("invalid named secret: {0}")]
    InvalidNamedSecret(String),
    #[error("duplicate plugin capability: {0}")]
    Duplicate(String),
    #[error("too many plugin capabilities; maximum is 64")]
    TooMany,
    #[error("sandbox host binding is unavailable for plugin capability: {0}")]
    RuntimeBindingUnavailable(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GrantEvaluation {
    Allowed,
    ApprovalRequired { added_capabilities: Vec<String> },
    BindingChanged,
    Revoked,
}

impl PluginCapability {
    pub(crate) fn parse(raw: &str) -> Result<Self, CapabilityError> {
        match raw {
            "nagi.state.read" => Ok(Self::NagiStateRead),
            "nagi.layout.write" => Ok(Self::NagiLayoutWrite),
            "pane.content.read" => Ok(Self::PaneContentRead),
            "pane.input.write" => Ok(Self::PaneInputWrite),
            "clipboard.write" => Ok(Self::ClipboardWrite),
            "notifications.send" => Ok(Self::NotificationsSend),
            "mission.read" => Ok(Self::MissionRead),
            "mission.attention.propose" => Ok(Self::MissionAttentionPropose),
            "mission.evidence.propose" => Ok(Self::MissionEvidencePropose),
            _ => {
                let Some((family, scope)) = raw.split_once(':') else {
                    if matches!(
                        raw,
                        "workspace.files.read"
                            | "workspace.files.write"
                            | "process.spawn"
                            | "network"
                            | "secrets.read"
                    ) {
                        return Err(CapabilityError::MissingScope(raw.to_owned()));
                    }
                    return Err(CapabilityError::Unknown(raw.to_owned()));
                };
                match family {
                    "workspace.files.read" => {
                        Ok(Self::WorkspaceFilesRead(parse_workspace_scope(scope)?))
                    }
                    "workspace.files.write" => {
                        Ok(Self::WorkspaceFilesWrite(parse_workspace_scope(scope)?))
                    }
                    "process.spawn" => Ok(Self::ProcessSpawn(parse_process_allowlist(scope)?)),
                    "network" => Ok(Self::Network(parse_network_origin(scope)?)),
                    "secrets.read" => {
                        if valid_id(scope) {
                            Ok(Self::SecretsRead(scope.to_owned()))
                        } else {
                            Err(CapabilityError::InvalidNamedSecret(scope.to_owned()))
                        }
                    }
                    _ => Err(CapabilityError::Unknown(raw.to_owned())),
                }
            }
        }
    }
}

impl fmt::Display for PluginCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NagiStateRead => formatter.write_str("nagi.state.read"),
            Self::NagiLayoutWrite => formatter.write_str("nagi.layout.write"),
            Self::PaneContentRead => formatter.write_str("pane.content.read"),
            Self::PaneInputWrite => formatter.write_str("pane.input.write"),
            Self::WorkspaceFilesRead(scope) => {
                write!(formatter, "workspace.files.read:{scope}")
            }
            Self::WorkspaceFilesWrite(scope) => {
                write!(formatter, "workspace.files.write:{scope}")
            }
            Self::ProcessSpawn(commands) => {
                write!(formatter, "process.spawn:{}", commands.join(","))
            }
            Self::Network(origin) => write!(formatter, "network:{origin}"),
            Self::ClipboardWrite => formatter.write_str("clipboard.write"),
            Self::NotificationsSend => formatter.write_str("notifications.send"),
            Self::MissionRead => formatter.write_str("mission.read"),
            Self::MissionAttentionPropose => formatter.write_str("mission.attention.propose"),
            Self::MissionEvidencePropose => formatter.write_str("mission.evidence.propose"),
            Self::SecretsRead(secret) => write!(formatter, "secrets.read:{secret}"),
        }
    }
}

impl fmt::Display for WorkspaceScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Changed => "changed",
            Self::Worktree => "worktree",
        })
    }
}

pub(crate) fn normalize_capabilities(raw: &[String]) -> Result<Vec<String>, CapabilityError> {
    if raw.len() > 64 {
        return Err(CapabilityError::TooMany);
    }
    let mut normalized = BTreeSet::new();
    for capability in raw {
        let parsed = PluginCapability::parse(capability)?;
        let canonical = parsed.to_string();
        if !normalized.insert(canonical.clone()) {
            return Err(CapabilityError::Duplicate(canonical));
        }
    }
    Ok(normalized.into_iter().collect())
}

/// Reject capability grants that the current sandbox host cannot enforce.
///
/// Parsing a capability and approving it are deliberately separate steps: the
/// manifest schema can remain forward-compatible, while the local user cannot
/// be misled into granting a permission that has no runtime binding yet.
pub(crate) fn ensure_runtime_bindings_available(raw: &[String]) -> Result<(), CapabilityError> {
    for capability in normalize_capabilities(raw)? {
        match PluginCapability::parse(&capability)? {
            PluginCapability::WorkspaceFilesRead(WorkspaceScope::Worktree)
            | PluginCapability::WorkspaceFilesWrite(WorkspaceScope::Worktree)
            | PluginCapability::MissionRead => {}
            unavailable => {
                return Err(CapabilityError::RuntimeBindingUnavailable(
                    unavailable.to_string(),
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn added_capabilities(
    granted: &[String],
    requested: &[String],
) -> Result<Vec<String>, CapabilityError> {
    let granted = normalize_capabilities(granted)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    Ok(normalize_capabilities(requested)?
        .into_iter()
        .filter(|capability| !granted.contains(capability))
        .collect())
}

#[derive(Clone, Copy)]
pub(crate) struct PluginBinding<'a> {
    pub(crate) plugin_id: &'a str,
    pub(crate) plugin_version: &'a str,
    pub(crate) runtime: PluginRuntimeV2,
    pub(crate) manifest_sha256: &'a str,
    pub(crate) package_sha256: &'a str,
    pub(crate) resolved_commit: Option<&'a str>,
    pub(crate) capabilities: &'a [String],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OwnedPluginBinding {
    pub(crate) plugin_id: String,
    pub(crate) plugin_version: String,
    pub(crate) runtime: PluginRuntimeV2,
    pub(crate) manifest_sha256: String,
    pub(crate) package_sha256: String,
    pub(crate) resolved_commit: Option<String>,
    pub(crate) capabilities: Vec<String>,
}

impl OwnedPluginBinding {
    pub(crate) fn as_binding(&self) -> PluginBinding<'_> {
        PluginBinding {
            plugin_id: &self.plugin_id,
            plugin_version: &self.plugin_version,
            runtime: self.runtime,
            manifest_sha256: &self.manifest_sha256,
            package_sha256: &self.package_sha256,
            resolved_commit: self.resolved_commit.as_deref(),
            capabilities: &self.capabilities,
        }
    }
}

pub(crate) fn installed_plugin_binding(
    plugin: &crate::api::schema::InstalledPluginInfo,
) -> Result<OwnedPluginBinding, String> {
    let entrypoint = plugin
        .entrypoint
        .as_deref()
        .ok_or_else(|| "sandboxed plugin entrypoint is missing".to_owned())?;
    let manifest = std::fs::read(&plugin.manifest_path)
        .map_err(|error| format!("plugin manifest digest failed: {error}"))?;
    let package = std::fs::read(entrypoint)
        .map_err(|error| format!("plugin package digest failed: {error}"))?;
    let binding = OwnedPluginBinding {
        plugin_id: plugin.plugin_id.clone(),
        plugin_version: plugin.version.clone(),
        runtime: plugin.runtime,
        manifest_sha256: sha256_hex(&manifest),
        package_sha256: sha256_hex(&package),
        resolved_commit: plugin.source.resolved_commit.clone(),
        capabilities: normalize_capabilities(&plugin.requested_capabilities)
            .map_err(|error| error.to_string())?,
    };
    validate_binding(&binding.as_binding())?;
    Ok(binding)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn new_grant(
    binding: &PluginBinding<'_>,
    approved_by: &str,
    approved_at_millis: u64,
) -> Result<PluginGrantV1, String> {
    validate_binding(binding)?;
    if !valid_id(approved_by) {
        return Err("invalid plugin grant approver".to_owned());
    }
    Ok(PluginGrantV1 {
        schema_version: ContractVersionV1,
        plugin_id: binding.plugin_id.to_owned(),
        plugin_version: binding.plugin_version.to_owned(),
        runtime: binding.runtime,
        manifest_sha256: binding.manifest_sha256.to_owned(),
        capabilities: normalize_capabilities(binding.capabilities)
            .map_err(|error| error.to_string())?,
        approved_by: approved_by.to_owned(),
        approved_at_millis,
        revoked_at_millis: None,
    })
}

pub(crate) fn new_lock_entry(
    binding: &PluginBinding<'_>,
    approval: PluginApprovalStateV1,
) -> Result<PluginLockEntryV1, String> {
    validate_binding(binding)?;
    Ok(PluginLockEntryV1 {
        schema_version: ContractVersionV1,
        plugin_id: binding.plugin_id.to_owned(),
        plugin_version: binding.plugin_version.to_owned(),
        runtime: binding.runtime,
        manifest_sha256: binding.manifest_sha256.to_owned(),
        package_sha256: binding.package_sha256.to_owned(),
        resolved_commit: binding.resolved_commit.map(str::to_owned),
        requested_capabilities: normalize_capabilities(binding.capabilities)
            .map_err(|error| error.to_string())?,
        approval,
    })
}

pub(crate) fn evaluate_grant(
    grant: &PluginGrantV1,
    binding: &PluginBinding<'_>,
) -> Result<GrantEvaluation, String> {
    validate_binding(binding)?;
    if grant.revoked_at_millis.is_some() {
        return Ok(GrantEvaluation::Revoked);
    }
    if grant.plugin_id != binding.plugin_id
        || grant.plugin_version != binding.plugin_version
        || grant.runtime != binding.runtime
        || grant.manifest_sha256 != binding.manifest_sha256
    {
        return Ok(GrantEvaluation::BindingChanged);
    }
    let added = added_capabilities(&grant.capabilities, binding.capabilities)
        .map_err(|error| error.to_string())?;
    if added.is_empty() {
        Ok(GrantEvaluation::Allowed)
    } else {
        Ok(GrantEvaluation::ApprovalRequired {
            added_capabilities: added,
        })
    }
}

pub(crate) fn evaluate_security_binding(
    lock: &PluginLockEntryV1,
    grant: &PluginGrantV1,
    binding: &PluginBinding<'_>,
) -> Result<GrantEvaluation, String> {
    validate_binding(binding)?;
    if lock.plugin_id != binding.plugin_id
        || lock.plugin_version != binding.plugin_version
        || lock.runtime != binding.runtime
        || lock.manifest_sha256 != binding.manifest_sha256
        || lock.package_sha256 != binding.package_sha256
        || lock.resolved_commit.as_deref() != binding.resolved_commit
    {
        return Ok(GrantEvaluation::BindingChanged);
    }
    match lock.approval {
        PluginApprovalStateV1::Revoked => return Ok(GrantEvaluation::Revoked),
        PluginApprovalStateV1::Pending | PluginApprovalStateV1::EscalationBlocked => {
            return Ok(GrantEvaluation::ApprovalRequired {
                added_capabilities: added_capabilities(&grant.capabilities, binding.capabilities)
                    .map_err(|error| error.to_string())?,
            })
        }
        PluginApprovalStateV1::Approved => {}
    }
    evaluate_grant(grant, binding)
}

fn validate_binding(binding: &PluginBinding<'_>) -> Result<(), String> {
    if !valid_id(binding.plugin_id) || binding.plugin_version.trim().is_empty() {
        return Err("invalid plugin binding identity".to_owned());
    }
    if !valid_sha256(binding.manifest_sha256) || !valid_sha256(binding.package_sha256) {
        return Err("plugin binding digests must be lowercase SHA-256".to_owned());
    }
    if binding.resolved_commit.is_some_and(|commit| {
        commit.len() != 40
            || !commit
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    }) {
        return Err("plugin binding commit must be a lowercase 40-byte Git object id".to_owned());
    }
    normalize_capabilities(binding.capabilities).map_err(|error| error.to_string())?;
    Ok(())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn parse_workspace_scope(raw: &str) -> Result<WorkspaceScope, CapabilityError> {
    match raw {
        "changed" => Ok(WorkspaceScope::Changed),
        "worktree" => Ok(WorkspaceScope::Worktree),
        _ => Err(CapabilityError::InvalidWorkspaceScope(raw.to_owned())),
    }
}

fn parse_process_allowlist(raw: &str) -> Result<Vec<String>, CapabilityError> {
    let commands = raw.split(',').map(str::trim).collect::<Vec<_>>();
    if commands.is_empty()
        || commands.len() > 16
        || commands.iter().any(|command| {
            !valid_command_name(command)
                || command.contains('/')
                || command.contains('\\')
                || *command == "."
                || *command == ".."
        })
    {
        return Err(CapabilityError::InvalidProcessAllowlist(raw.to_owned()));
    }
    let mut unique = commands
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if unique.is_empty() {
        return Err(CapabilityError::InvalidProcessAllowlist(raw.to_owned()));
    }
    Ok(std::mem::take(&mut unique).into_iter().collect())
}

fn parse_network_origin(raw: &str) -> Result<String, CapabilityError> {
    let url = reqwest::Url::parse(raw)
        .map_err(|_| CapabilityError::InvalidNetworkOrigin(raw.to_owned()))?;
    let scheme_allowed = url.scheme() == "https"
        || (url.scheme() == "http"
            && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1")));
    if !scheme_allowed
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(CapabilityError::InvalidNetworkOrigin(raw.to_owned()));
    }
    let mut origin = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
    if let Some(port) = url.port() {
        origin.push(':');
        origin.push_str(&port.to_string());
    }
    Ok(origin)
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn valid_command_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'+'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_canonicalizes_every_capability_family() {
        let capabilities = [
            "nagi.state.read",
            "nagi.layout.write",
            "pane.content.read",
            "pane.input.write",
            "workspace.files.read:changed",
            "workspace.files.write:worktree",
            "process.spawn:git,cargo",
            "network:https://api.github.com",
            "clipboard.write",
            "notifications.send",
            "mission.read",
            "mission.attention.propose",
            "mission.evidence.propose",
            "secrets.read:github_token",
        ]
        .map(str::to_owned);
        let normalized = normalize_capabilities(&capabilities).unwrap();
        assert_eq!(normalized.len(), capabilities.len());
        assert!(normalized.contains(&"process.spawn:cargo,git".to_owned()));
    }

    #[test]
    fn rejects_invalid_scopes_origins_and_processes() {
        for capability in [
            "workspace.files.read:../../home",
            "network:http://example.com",
            "network:https://user@example.com",
            "network:https://example.com/path",
            "process.spawn:/bin/sh",
            "process.spawn:git,../sh",
            "secrets.read:*",
        ] {
            assert!(
                PluginCapability::parse(capability).is_err(),
                "accepted {capability}"
            );
        }
    }

    #[test]
    fn capability_escalation_is_set_based_and_strict() {
        let granted = vec!["mission.read".to_owned()];
        let requested = vec![
            "mission.read".to_owned(),
            "workspace.files.read:changed".to_owned(),
        ];
        assert_eq!(
            added_capabilities(&granted, &requested).unwrap(),
            ["workspace.files.read:changed"]
        );
        assert!(
            normalize_capabilities(&["mission.read".to_owned(), "mission.read".to_owned()])
                .is_err()
        );
    }

    #[test]
    fn runtime_binding_availability_is_fail_closed() {
        ensure_runtime_bindings_available(&[
            "workspace.files.read:worktree".to_owned(),
            "workspace.files.write:worktree".to_owned(),
            "mission.read".to_owned(),
        ])
        .unwrap();

        for capability in [
            "workspace.files.read:changed",
            "network:https://api.github.com",
        ] {
            let error = ensure_runtime_bindings_available(&[capability.to_owned()]).unwrap_err();
            assert_eq!(
                error,
                CapabilityError::RuntimeBindingUnavailable(capability.to_owned())
            );
        }
    }

    #[test]
    fn grants_bind_version_runtime_digest_and_capability_set() {
        let capabilities = vec!["mission.read".to_owned()];
        let binding = PluginBinding {
            plugin_id: "example.review",
            plugin_version: "1.0.0",
            runtime: PluginRuntimeV2::WasiComponent,
            manifest_sha256: &"a".repeat(64),
            package_sha256: &"b".repeat(64),
            resolved_commit: Some("0123456789012345678901234567890123456789"),
            capabilities: &capabilities,
        };
        let grant = new_grant(&binding, "local-user", 10).unwrap();
        assert_eq!(
            evaluate_grant(&grant, &binding).unwrap(),
            GrantEvaluation::Allowed
        );

        let escalated_capabilities = vec![
            "mission.read".to_owned(),
            "workspace.files.read:changed".to_owned(),
        ];
        let escalated = PluginBinding {
            capabilities: &escalated_capabilities,
            ..binding
        };
        assert!(matches!(
            evaluate_grant(&grant, &escalated).unwrap(),
            GrantEvaluation::ApprovalRequired { ref added_capabilities }
                if added_capabilities == &["workspace.files.read:changed"]
        ));
        let changed_version = PluginBinding {
            plugin_version: "1.0.1",
            ..binding
        };
        assert_eq!(
            evaluate_grant(&grant, &changed_version).unwrap(),
            GrantEvaluation::BindingChanged
        );
        let mut revoked = grant;
        revoked.revoked_at_millis = Some(11);
        assert_eq!(
            evaluate_grant(&revoked, &binding).unwrap(),
            GrantEvaluation::Revoked
        );
    }

    #[test]
    fn installed_binding_detects_package_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let manifest = directory.path().join("nagi-plugin.toml");
        let package = directory.path().join("plugin.wasm");
        std::fs::write(&package, b"first package").unwrap();
        std::fs::write(
            &manifest,
            format!(
                r#"
manifest_version = 2
id = "example.binding"
name = "Binding"
version = "1.0.0"
min_nagi_version = "{}"
runtime = "wasi-component"
entrypoint = "plugin.wasm"
capabilities = ["mission.read"]
"#,
                crate::build_info::BASE_VERSION
            ),
        )
        .unwrap();
        let plugin =
            crate::app::load_plugin_manifest(&manifest.display().to_string(), false).unwrap();
        let before = installed_plugin_binding(&plugin).unwrap();
        let grant = new_grant(&before.as_binding(), "local-user", 1).unwrap();
        let lock = new_lock_entry(&before.as_binding(), PluginApprovalStateV1::Approved).unwrap();
        assert_eq!(
            evaluate_security_binding(&lock, &grant, &before.as_binding()).unwrap(),
            GrantEvaluation::Allowed
        );

        std::fs::write(&package, b"mutated package").unwrap();
        let after = installed_plugin_binding(&plugin).unwrap();
        assert_ne!(before.package_sha256, after.package_sha256);
        assert_eq!(
            evaluate_security_binding(&lock, &grant, &after.as_binding()).unwrap(),
            GrantEvaluation::BindingChanged
        );
    }
}
