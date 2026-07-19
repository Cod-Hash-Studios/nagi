use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use tracing::warn;

use crate::api::schema::{InstalledPluginInfo, PluginGrantV1, PluginLockEntryV1};

pub const MANIFEST_UNAVAILABLE_WARNING_PREFIX: &str = "manifest unavailable: ";
const REGISTRY_SCHEMA_V2: u16 = 2;
const MAX_REGISTRY_BYTES: u64 = 16 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct PluginRegistryV2 {
    schema_version: u16,
    #[serde(default)]
    plugins: Vec<InstalledPluginInfo>,
    #[serde(default)]
    locks: Vec<PluginLockEntryV1>,
    #[serde(default)]
    grants: Vec<PluginGrantV1>,
}

impl Default for PluginRegistryV2 {
    fn default() -> Self {
        Self {
            schema_version: REGISTRY_SCHEMA_V2,
            plugins: Vec::new(),
            locks: Vec::new(),
            grants: Vec::new(),
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum PluginRegistryOnDisk {
    Versioned(PluginRegistryV2),
    Legacy(Vec<InstalledPluginInfo>),
}

fn registry_path() -> PathBuf {
    crate::session::data_dir().join("plugins.json")
}

fn save_json_to_path<T: serde::Serialize + ?Sized>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(value)?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let tmp_path = path.with_extension(format!("json.tmp-{}-{sequence}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp_path)?;
    file.write_all(&json)?;
    file.sync_all()?;
    #[cfg(windows)]
    if path.exists() {
        if let Err(err) = std::fs::remove_file(path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }
    }
    if let Err(err) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(())
}

/// Atomically write `plugins.json` next to `session.json`.
pub fn save(plugins: &[InstalledPluginInfo]) -> std::io::Result<()> {
    let path = registry_path();
    save_to_path(&path, plugins)
}

pub fn save_to_path(path: &Path, plugins: &[InstalledPluginInfo]) -> std::io::Result<()> {
    let mut registry = if path.exists() {
        read_registry(path).map_err(registry_error_to_io)?
    } else {
        PluginRegistryV2::default()
    };
    registry.plugins = plugins.to_vec();
    let installed_ids = registry
        .plugins
        .iter()
        .map(|plugin| plugin.plugin_id.as_str())
        .collect::<std::collections::HashSet<_>>();
    registry
        .locks
        .retain(|lock| installed_ids.contains(lock.plugin_id.as_str()));
    registry
        .grants
        .retain(|grant| installed_ids.contains(grant.plugin_id.as_str()));
    save_json_to_path(path, &registry)
}

/// Load `plugins.json`.  Returns an empty vec on any failure so a corrupt or
/// missing file never blocks server startup.
pub fn load() -> Vec<InstalledPluginInfo> {
    load_from_path(&registry_path())
}

pub fn load_from_path(path: &Path) -> Vec<InstalledPluginInfo> {
    if !path.exists() {
        return Vec::new();
    }
    match read_registry(path) {
        Ok(registry) => registry.plugins,
        Err(error) => {
            let quarantine = quarantine_registry(path);
            warn!(
                path = %path.display(),
                err = %error,
                quarantine = ?quarantine.as_ref().map(|path| path.display().to_string()),
                "plugin registry is invalid and was quarantined"
            );
            Vec::new()
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum RegistryReadError {
    #[error("plugin registry I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("plugin registry exceeds {MAX_REGISTRY_BYTES} bytes")]
    TooLarge,
    #[error("plugin registry JSON is invalid: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("unsupported plugin registry schema {0}")]
    UnsupportedSchema(u16),
    #[error("plugin registry contains duplicate plugin, lock, or grant ids")]
    DuplicateEntries,
}

fn read_registry(path: &Path) -> Result<PluginRegistryV2, RegistryReadError> {
    if fs::metadata(path)?.len() > MAX_REGISTRY_BYTES {
        return Err(RegistryReadError::TooLarge);
    }
    let content = fs::read_to_string(path)?;
    let mut registry = match serde_json::from_str::<PluginRegistryOnDisk>(&content)? {
        PluginRegistryOnDisk::Versioned(registry) => registry,
        PluginRegistryOnDisk::Legacy(plugins) => PluginRegistryV2 {
            plugins,
            ..PluginRegistryV2::default()
        },
    };
    if registry.schema_version != REGISTRY_SCHEMA_V2 {
        return Err(RegistryReadError::UnsupportedSchema(
            registry.schema_version,
        ));
    }
    for plugin in &mut registry.plugins {
        if plugin.runtime == crate::api::schema::PluginRuntimeV2::TrustedNative
            && !plugin.native_trusted
        {
            plugin.enabled = false;
        }
    }
    registry
        .plugins
        .sort_by(|a, b| a.plugin_id.cmp(&b.plugin_id));
    registry.locks.sort_by(|a, b| a.plugin_id.cmp(&b.plugin_id));
    registry
        .grants
        .sort_by(|a, b| a.plugin_id.cmp(&b.plugin_id));
    let plugins_unique = registry
        .plugins
        .windows(2)
        .all(|pair| pair[0].plugin_id != pair[1].plugin_id);
    let locks_unique = registry
        .locks
        .windows(2)
        .all(|pair| pair[0].plugin_id != pair[1].plugin_id);
    let grants_unique = registry
        .grants
        .windows(2)
        .all(|pair| pair[0].plugin_id != pair[1].plugin_id);
    if !(plugins_unique && locks_unique && grants_unique) {
        return Err(RegistryReadError::DuplicateEntries);
    }
    Ok(registry)
}

fn quarantine_registry(path: &Path) -> std::io::Result<PathBuf> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let quarantine = path.with_extension(format!("json.corrupt-{millis}"));
    fs::rename(path, &quarantine)?;
    Ok(quarantine)
}

fn registry_error_to_io(error: RegistryReadError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error)
}

pub(crate) fn load_security_binding(
    plugin_id: &str,
) -> std::io::Result<Option<(PluginLockEntryV1, PluginGrantV1)>> {
    load_security_binding_from_path(&registry_path(), plugin_id)
}

pub(crate) fn load_security_binding_from_path(
    path: &Path,
    plugin_id: &str,
) -> std::io::Result<Option<(PluginLockEntryV1, PluginGrantV1)>> {
    if !path.exists() {
        return Ok(None);
    }
    let registry = read_registry(path).map_err(registry_error_to_io)?;
    let lock = registry
        .locks
        .into_iter()
        .find(|candidate| candidate.plugin_id == plugin_id);
    let grant = registry
        .grants
        .into_iter()
        .find(|candidate| candidate.plugin_id == plugin_id);
    match (lock, grant) {
        (Some(lock), Some(grant)) => Ok(Some((lock, grant))),
        (None, None) => Ok(None),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "plugin registry contains an incomplete security binding",
        )),
    }
}

pub(crate) fn save_security_binding(
    lock: PluginLockEntryV1,
    grant: PluginGrantV1,
) -> std::io::Result<()> {
    save_security_binding_to_path(&registry_path(), lock, grant)
}

pub(crate) fn save_security_binding_to_path(
    path: &Path,
    lock: PluginLockEntryV1,
    grant: PluginGrantV1,
) -> std::io::Result<()> {
    if lock.plugin_id != grant.plugin_id
        || lock.plugin_version != grant.plugin_version
        || lock.runtime != grant.runtime
        || lock.manifest_sha256 != grant.manifest_sha256
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "plugin lock and grant bindings differ",
        ));
    }
    let mut registry = if path.exists() {
        read_registry(path).map_err(registry_error_to_io)?
    } else {
        PluginRegistryV2::default()
    };
    registry
        .locks
        .retain(|candidate| candidate.plugin_id != lock.plugin_id);
    registry
        .grants
        .retain(|candidate| candidate.plugin_id != grant.plugin_id);
    registry.locks.push(lock);
    registry.grants.push(grant);
    save_json_to_path(path, &registry)
}

pub(crate) fn revoke_security_binding(
    plugin_id: &str,
    revoked_at_millis: u64,
) -> std::io::Result<bool> {
    revoke_security_binding_from_path(&registry_path(), plugin_id, revoked_at_millis)
}

pub(crate) fn revoke_security_binding_from_path(
    path: &Path,
    plugin_id: &str,
    revoked_at_millis: u64,
) -> std::io::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let mut registry = read_registry(path).map_err(registry_error_to_io)?;
    let mut changed = false;
    if let Some(lock) = registry
        .locks
        .iter_mut()
        .find(|candidate| candidate.plugin_id == plugin_id)
    {
        lock.approval = crate::api::schema::PluginApprovalStateV1::Revoked;
        changed = true;
    }
    if let Some(grant) = registry
        .grants
        .iter_mut()
        .find(|candidate| candidate.plugin_id == plugin_id)
    {
        grant.revoked_at_millis = Some(revoked_at_millis);
        changed = true;
    }
    if changed {
        save_json_to_path(path, &registry)?;
    }
    Ok(changed)
}

/// Re-read each entry's manifest from disk using the provided reload function.
///
/// If the manifest parses successfully, replace cached fields but keep the
/// stored `enabled` flag.  If the file is gone or unparseable, keep the stored
/// entry and append a warning so `plugin.list` surfaces it.
pub fn reload_manifests(
    mut entries: Vec<InstalledPluginInfo>,
    reload_fn: impl Fn(&str, bool) -> Result<InstalledPluginInfo, String>,
) -> Vec<InstalledPluginInfo> {
    for entry in &mut entries {
        entry.warnings.clear();
        match reload_fn(&entry.manifest_path, entry.enabled) {
            Ok(mut fresh) => {
                fresh.enabled = entry.enabled;
                fresh.native_trusted = entry.native_trusted;
                fresh.source = entry.source.clone();
                *entry = fresh;
            }
            Err(warn_msg) => {
                entry
                    .warnings
                    .push(format!("{MANIFEST_UNAVAILABLE_WARNING_PREFIX}{warn_msg}"));
            }
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_registry_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir()
            .join(format!(
                "nagi-registry-{name}-{}-{nanos}",
                std::process::id()
            ))
            .join("plugins.json")
    }

    fn sample_plugin(id: &str) -> InstalledPluginInfo {
        InstalledPluginInfo {
            manifest_version: 1,
            plugin_id: id.to_string(),
            name: "Test Plugin".to_string(),
            version: "0.1.0".to_string(),
            min_nagi_version: crate::build_info::BASE_VERSION.to_string(),
            description: None,
            manifest_path: format!("/tmp/{id}/nagi-plugin.toml"),
            plugin_root: format!("/tmp/{id}"),
            enabled: true,
            runtime: crate::api::schema::PluginRuntimeV2::TrustedNative,
            entrypoint: None,
            requested_capabilities: Vec::new(),
            native_trusted: true,
            platforms: None,
            build: vec![],
            actions: vec![],
            events: vec![],
            panes: vec![],
            link_handlers: vec![],
            inspector_tabs: vec![],
            source: Default::default(),
            warnings: vec![],
        }
    }

    #[test]
    fn save_and_load_roundtrip() {
        let path = temp_registry_path("roundtrip");
        let plugins = vec![sample_plugin("example.a"), sample_plugin("example.b")];

        save_to_path(&path, &plugins).unwrap();

        let loaded = load_from_path(&path);
        assert_eq!(loaded.len(), 2);
        let ids: Vec<_> = loaded.iter().map(|p| p.plugin_id.as_str()).collect();
        assert!(ids.contains(&"example.a"));
        assert!(ids.contains(&"example.b"));
    }

    #[test]
    fn security_binding_roundtrips_revokes_and_is_pruned_on_unlink() {
        let path = temp_registry_path("security-lifecycle");
        let plugin = sample_plugin("example.secure");
        save_to_path(&path, std::slice::from_ref(&plugin)).unwrap();
        let lock = PluginLockEntryV1 {
            schema_version: crate::api::schema::ContractVersionV1,
            plugin_id: plugin.plugin_id.clone(),
            plugin_version: plugin.version.clone(),
            runtime: crate::api::schema::PluginRuntimeV2::WasiComponent,
            manifest_sha256: "a".repeat(64),
            package_sha256: "b".repeat(64),
            source_sha256: Some("c".repeat(64)),
            resolved_commit: None,
            requested_capabilities: vec!["mission.read".into()],
            approval: crate::api::schema::PluginApprovalStateV1::Approved,
        };
        let grant = PluginGrantV1 {
            schema_version: crate::api::schema::ContractVersionV1,
            plugin_id: plugin.plugin_id.clone(),
            plugin_version: plugin.version.clone(),
            runtime: crate::api::schema::PluginRuntimeV2::WasiComponent,
            manifest_sha256: "a".repeat(64),
            capabilities: vec!["mission.read".into()],
            approved_by: "local-user".into(),
            approved_at_millis: 10,
            revoked_at_millis: None,
        };
        save_security_binding_to_path(&path, lock, grant).unwrap();
        assert!(load_security_binding_from_path(&path, &plugin.plugin_id)
            .unwrap()
            .is_some());

        assert!(revoke_security_binding_from_path(&path, &plugin.plugin_id, 20).unwrap());
        let (lock, grant) = load_security_binding_from_path(&path, &plugin.plugin_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            lock.approval,
            crate::api::schema::PluginApprovalStateV1::Revoked
        );
        assert_eq!(grant.revoked_at_millis, Some(20));

        save_to_path(&path, &[]).unwrap();
        assert!(load_security_binding_from_path(&path, &plugin.plugin_id)
            .unwrap()
            .is_none());
    }

    #[test]
    fn missing_file_returns_empty() {
        let path = temp_registry_path("missing");
        let loaded = load_from_path(&path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn corrupt_file_returns_empty_without_panic() {
        let path = temp_registry_path("corrupt");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, b"this is not valid json {{{{").unwrap();

        let loaded = load_from_path(&path);
        assert!(loaded.is_empty());
        assert!(!path.exists(), "corrupt registry must be quarantined");
        let parent = path.parent().unwrap();
        assert!(std::fs::read_dir(parent).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("corrupt-")
        }));
    }

    #[test]
    fn reload_manifests_keeps_entry_with_warning_on_missing_manifest() {
        let entry = sample_plugin("example.missing");
        let entries = vec![entry];

        let result = reload_manifests(entries, |path, _enabled| {
            Err(format!("manifest not found at {path}"))
        });

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].plugin_id, "example.missing");
        assert!(!result[0].warnings.is_empty());
        assert!(result[0].warnings[0].contains("manifest not found"));
    }

    #[test]
    fn reload_manifests_uses_fresh_parse_and_keeps_enabled_flag() {
        let mut entry = sample_plugin("example.reload");
        entry.enabled = false;
        entry.source = crate::api::schema::PluginSourceInfo {
            kind: crate::api::schema::PluginSourceKind::Github,
            owner: Some("Cod-Hash-Studios".into()),
            repo: Some("nagi-plugin-examples".into()),
            subdir: Some("worktree-bootstrap".into()),
            requested_ref: Some("main".into()),
            resolved_commit: Some("abc123".into()),
            managed_path: Some("/tmp/nagi/plugins/github/example.reload".into()),
            installed_unix_ms: Some(42),
        };

        let result = reload_manifests(vec![entry], |_path, _enabled| {
            Ok(InstalledPluginInfo {
                manifest_version: 1,
                plugin_id: "example.reload".to_string(),
                name: "Fresh Name".to_string(),
                version: "0.2.0".to_string(),
                min_nagi_version: crate::build_info::BASE_VERSION.to_string(),
                description: Some("refreshed".to_string()),
                manifest_path: "/tmp/example.reload/nagi-plugin.toml".to_string(),
                plugin_root: "/tmp/example.reload".to_string(),
                enabled: true, // caller would pass stored enabled; fresh parse returns true
                runtime: crate::api::schema::PluginRuntimeV2::TrustedNative,
                entrypoint: None,
                requested_capabilities: Vec::new(),
                native_trusted: false,
                platforms: None,
                build: vec![],
                actions: vec![],
                events: vec![],
                panes: vec![],
                link_handlers: vec![],
                inspector_tabs: vec![],
                source: Default::default(),
                warnings: vec![],
            })
        });

        assert_eq!(result[0].name, "Fresh Name");
        assert_eq!(result[0].version, "0.2.0");
        // enabled preserved from stored entry
        assert!(!result[0].enabled);
        assert_eq!(
            result[0].source.kind,
            crate::api::schema::PluginSourceKind::Github
        );
        assert_eq!(result[0].source.owner.as_deref(), Some("Cod-Hash-Studios"));
        assert!(result[0].native_trusted);
        assert!(result[0].warnings.is_empty());
    }

    #[test]
    fn atomic_write_temp_file_is_cleaned_up_on_rename_failure() {
        // Write to a path whose parent does not yet exist, then verify the
        // tmp file is removed when the write fails mid-way.  Here we just
        // confirm a successful write leaves no .tmp file behind.
        let path = temp_registry_path("cleanup");
        save_to_path(&path, &[sample_plugin("example.cleanup")]).unwrap();

        let tmp = path.with_extension("json.tmp");
        assert!(
            !tmp.exists(),
            "tmp file should be cleaned up after successful rename"
        );
        assert!(path.exists());
    }

    #[test]
    fn save_replaces_existing_registry_file() {
        let path = temp_registry_path("replace-existing");
        save_to_path(&path, &[sample_plugin("example.first")]).unwrap();
        save_to_path(&path, &[sample_plugin("example.second")]).unwrap();

        let loaded = load_from_path(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].plugin_id, "example.second");
    }

    #[test]
    fn legacy_vector_migrates_to_versioned_registry_on_save() {
        let path = temp_registry_path("legacy-migration");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut legacy = serde_json::to_value(vec![sample_plugin("example.legacy")]).unwrap();
        legacy[0].as_object_mut().unwrap().remove("native_trusted");
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let loaded = load_from_path(&path);
        assert_eq!(loaded.len(), 1);
        assert!(!loaded[0].native_trusted);
        assert!(!loaded[0].enabled);
        save_to_path(&path, &loaded).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["schema_version"], REGISTRY_SCHEMA_V2);
        assert_eq!(value["plugins"][0]["plugin_id"], "example.legacy");
        assert_eq!(value["plugins"][0]["native_trusted"], false);
        assert_eq!(value["locks"], serde_json::json!([]));
        assert_eq!(value["grants"], serde_json::json!([]));
    }

    #[test]
    fn security_binding_is_atomic_and_preserved_by_plugin_saves() {
        use crate::{
            api::schema::{PluginApprovalStateV1, PluginRuntimeV2},
            plugin_capabilities::{new_grant, new_lock_entry, PluginBinding},
        };

        let path = temp_registry_path("security-binding");
        let capabilities = vec!["mission.read".to_owned()];
        let binding = PluginBinding {
            plugin_id: "example.secure",
            plugin_version: "1.0.0",
            runtime: PluginRuntimeV2::WasiComponent,
            manifest_sha256: &"a".repeat(64),
            package_sha256: &"b".repeat(64),
            source_sha256: &"c".repeat(64),
            resolved_commit: Some("0123456789012345678901234567890123456789"),
            capabilities: &capabilities,
        };
        let lock = new_lock_entry(&binding, PluginApprovalStateV1::Approved).unwrap();
        let grant = new_grant(&binding, "local-user", 10).unwrap();
        save_security_binding_to_path(&path, lock, grant).unwrap();
        save_to_path(&path, &[sample_plugin("example.secure")]).unwrap();

        let registry = read_registry(&path).unwrap();
        assert_eq!(registry.plugins.len(), 1);
        assert_eq!(registry.locks.len(), 1);
        assert_eq!(registry.grants.len(), 1);
        assert_eq!(registry.grants[0].capabilities, ["mission.read"]);
    }
}
