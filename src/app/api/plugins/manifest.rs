use crate::api::schema::{
    InstalledPluginInfo, PluginManifestAction, PluginManifestBuild, PluginManifestEventHook,
    PluginManifestLinkHandler, PluginManifestPane, PluginPanePlacement, PluginPlatform,
    PluginSourceInfo, PluginSourceKind,
};
use crate::popup_size::PopupSize;

const PLUGIN_ID_MAX_CHARS: usize = 120;
const PLUGIN_ACTION_ID_MAX_CHARS: usize = 120;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPluginManifest {
    id: String,
    name: String,
    version: String,
    #[serde(default)]
    min_nagi_version: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    platforms: Option<Vec<RawPlatform>>,
    #[serde(default)]
    build: Vec<RawPluginManifestBuild>,
    #[serde(default)]
    actions: Vec<RawPluginManifestAction>,
    #[serde(default)]
    events: Vec<RawPluginManifestEventHook>,
    #[serde(default)]
    panes: Vec<RawPluginManifestPane>,
    #[serde(default)]
    link_handlers: Vec<RawPluginManifestLinkHandler>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPluginManifestBuild {
    #[serde(default)]
    platforms: Option<Vec<RawPlatform>>,
    command: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPluginManifestAction {
    id: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    contexts: Vec<crate::api::schema::PluginActionContext>,
    #[serde(default)]
    platforms: Option<Vec<RawPlatform>>,
    command: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPluginManifestEventHook {
    on: String,
    #[serde(default)]
    platforms: Option<Vec<RawPlatform>>,
    command: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPluginManifestPane {
    id: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    platforms: Option<Vec<RawPlatform>>,
    #[serde(default)]
    placement: PluginPanePlacement,
    #[serde(default)]
    width: Option<PopupSize>,
    #[serde(default)]
    height: Option<PopupSize>,
    command: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPluginManifestLinkHandler {
    id: String,
    title: String,
    pattern: String,
    action: String,
    #[serde(default)]
    platforms: Option<Vec<RawPlatform>>,
}

/// Raw string platform value from the manifest, validated before conversion.
#[derive(Debug, serde::Deserialize)]
#[serde(try_from = "String")]
struct RawPlatform(PluginPlatform);

impl TryFrom<String> for RawPlatform {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "linux" => Ok(RawPlatform(PluginPlatform::Linux)),
            "macos" => Ok(RawPlatform(PluginPlatform::Macos)),
            "windows" => Ok(RawPlatform(PluginPlatform::Windows)),
            other => Err(format!(
                "invalid_plugin_platform: unknown platform '{other}'"
            )),
        }
    }
}

pub(crate) fn load_plugin_manifest(
    path: &str,
    enabled: bool,
) -> Result<InstalledPluginInfo, (&'static str, String)> {
    let path = std::path::PathBuf::from(path);
    let manifest_path = if path.is_dir() {
        path.join("nagi-plugin.toml")
    } else {
        path
    };
    let manifest_path = manifest_path
        .canonicalize()
        .map_err(|err| ("plugin_manifest_not_found", err.to_string()))?;
    let plugin_root = manifest_path
        .parent()
        .ok_or_else(|| {
            (
                "invalid_plugin_manifest_path",
                "manifest path has no parent directory".to_string(),
            )
        })?
        .to_path_buf();
    let content = std::fs::read_to_string(&manifest_path)
        .map_err(|err| ("plugin_manifest_read_failed", err.to_string()))?;
    let raw = match parse_plugin_manifest(&content, &plugin_root)? {
        ParsedPluginManifest::Legacy(raw) => raw,
        ParsedPluginManifest::V2(manifest) => {
            return installed_plugin_from_v2(manifest, &manifest_path, &plugin_root, enabled)
        }
    };
    let plugin_id = normalize_plugin_id(&raw.id)
        .ok_or_else(|| ("invalid_plugin_id", "invalid plugin id".to_string()))?;
    let name = non_empty_trimmed(&raw.name, "invalid_plugin_name", "plugin name is required")?;
    let version = non_empty_trimmed(
        &raw.version,
        "invalid_plugin_version",
        "plugin version is required",
    )?;
    let min_nagi_version = validate_min_nagi_version(raw.min_nagi_version.as_deref())?;
    let description = raw
        .description
        .map(|description| description.trim().to_string())
        .filter(|description| !description.is_empty());
    let platforms = normalize_platforms(raw.platforms)?;
    let build = raw
        .build
        .into_iter()
        .map(normalize_manifest_build)
        .collect::<Result<Vec<_>, _>>()?;
    let mut actions = raw
        .actions
        .into_iter()
        .map(normalize_manifest_action)
        .collect::<Result<Vec<_>, _>>()?;
    reject_duplicate_action_ids(&actions)?;
    actions.sort_by(|a, b| a.id.cmp(&b.id));
    let mut events = raw
        .events
        .into_iter()
        .map(normalize_manifest_event)
        .collect::<Result<Vec<_>, _>>()?;
    events.sort_by(|a, b| a.on.cmp(&b.on).then_with(|| a.command.cmp(&b.command)));
    let mut panes = raw
        .panes
        .into_iter()
        .map(normalize_manifest_pane)
        .collect::<Result<Vec<_>, _>>()?;
    reject_duplicate_pane_ids(&panes)?;
    panes.sort_by(|a, b| a.id.cmp(&b.id));
    let link_handlers = raw
        .link_handlers
        .into_iter()
        .map(normalize_manifest_link_handler)
        .collect::<Result<Vec<_>, _>>()?;
    reject_duplicate_link_handler_ids(&link_handlers)?;
    validate_link_handler_actions(&link_handlers, &actions)?;

    let mut warnings = validate_event_names(&events);
    if platforms.is_none() {
        warnings.push("manifest does not declare platforms; platform support unknown".to_string());
    }

    Ok(InstalledPluginInfo {
        manifest_version: 1,
        plugin_id,
        name,
        version,
        min_nagi_version,
        description,
        manifest_path: manifest_path.display().to_string(),
        plugin_root: plugin_root.display().to_string(),
        enabled,
        runtime: crate::api::schema::PluginRuntimeV2::TrustedNative,
        entrypoint: None,
        requested_capabilities: Vec::new(),
        native_trusted: false,
        platforms,
        build,
        actions,
        events,
        panes,
        link_handlers,
        inspector_tabs: Vec::new(),
        source: Default::default(),
        warnings,
    })
}

#[derive(Debug)]
enum ParsedPluginManifest {
    Legacy(RawPluginManifest),
    V2(crate::api::schema::PluginManifestV2),
}

fn parse_plugin_manifest(
    content: &str,
    plugin_root: &std::path::Path,
) -> Result<ParsedPluginManifest, (&'static str, String)> {
    let mut value = toml::from_str::<toml::Value>(content)
        .map_err(|error| ("plugin_manifest_parse_failed", error.to_string()))?;
    let declared_version = value.get("manifest_version").map(|version| {
        version.as_integer().ok_or_else(|| {
            (
                "invalid_plugin_manifest_version",
                "manifest_version must be the integer 1 or 2".to_owned(),
            )
        })
    });
    match declared_version.transpose()? {
        None => toml::from_str(content)
            .map(ParsedPluginManifest::Legacy)
            .map_err(|error| ("plugin_manifest_parse_failed", error.to_string())),
        Some(1) => {
            value
                .as_table_mut()
                .expect("a TOML document root is always a table")
                .remove("manifest_version");
            value
                .try_into::<RawPluginManifest>()
                .map(ParsedPluginManifest::Legacy)
                .map_err(|error| ("plugin_manifest_parse_failed", error.to_string()))
        }
        Some(2) => {
            let manifest = toml::from_str::<crate::api::schema::PluginManifestV2>(content)
                .map_err(|error| ("plugin_manifest_v2_parse_failed", error.to_string()))?;
            validate_manifest_v2(manifest, plugin_root).map(ParsedPluginManifest::V2)
        }
        Some(version) => Err((
            "unsupported_plugin_manifest_version",
            format!("unsupported plugin manifest version {version}; supported versions are legacy v1 and 2"),
        )),
    }
}

fn installed_plugin_from_v2(
    manifest: crate::api::schema::PluginManifestV2,
    manifest_path: &std::path::Path,
    plugin_root: &std::path::Path,
    enabled: bool,
) -> Result<InstalledPluginInfo, (&'static str, String)> {
    use crate::api::schema::{PluginActionContext, PluginManifestAction, PluginRuntimeV2};

    if manifest.runtime != PluginRuntimeV2::WasiComponent {
        return Err((
            "plugin_v2_native_runtime_unavailable",
            "manifest v2 trusted-native plugins are not accepted; use a legacy native manifest with explicit unrestricted trust"
                .to_owned(),
        ));
    }
    let actions = manifest
        .contributions
        .commands
        .iter()
        .map(|command| PluginManifestAction {
            id: command.id.clone(),
            title: command.title.trim().to_owned(),
            description: None,
            contexts: command
                .contexts
                .iter()
                .map(|context| match context.as_str() {
                    "global" => PluginActionContext::Global,
                    "mission" => PluginActionContext::Mission,
                    "pane" => PluginActionContext::Pane,
                    "workspace" => PluginActionContext::Workspace,
                    _ => unreachable!("v2 contribution contexts were validated"),
                })
                .collect(),
            platforms: None,
            command: Vec::new(),
        })
        .collect();
    let inspector_tabs = manifest.contributions.inspector_tabs;
    Ok(InstalledPluginInfo {
        manifest_version: 2,
        plugin_id: manifest.id,
        name: manifest.name,
        version: manifest.version,
        min_nagi_version: manifest.min_nagi_version,
        description: manifest.description,
        manifest_path: manifest_path.display().to_string(),
        plugin_root: plugin_root.display().to_string(),
        enabled,
        runtime: manifest.runtime,
        entrypoint: Some(manifest.entrypoint),
        requested_capabilities: manifest.capabilities,
        native_trusted: false,
        platforms: None,
        build: Vec::new(),
        actions,
        events: Vec::new(),
        panes: Vec::new(),
        link_handlers: Vec::new(),
        inspector_tabs,
        source: Default::default(),
        warnings: Vec::new(),
    })
}

fn validate_manifest_v2(
    mut manifest: crate::api::schema::PluginManifestV2,
    plugin_root: &std::path::Path,
) -> Result<crate::api::schema::PluginManifestV2, (&'static str, String)> {
    use std::path::Component;

    let plugin_root = plugin_root.canonicalize().map_err(|error| {
        (
            "invalid_plugin_manifest_path",
            format!("plugin root is unavailable: {error}"),
        )
    })?;

    manifest.id = normalize_plugin_id(&manifest.id)
        .ok_or_else(|| ("invalid_plugin_id", "invalid plugin id".to_owned()))?;
    manifest.name = non_empty_trimmed(
        &manifest.name,
        "invalid_plugin_name",
        "plugin name is required",
    )?;
    manifest.version = non_empty_trimmed(
        &manifest.version,
        "invalid_plugin_version",
        "plugin version is required",
    )?;
    if crate::update::Version::parse(&manifest.version).is_none() {
        return Err((
            "invalid_plugin_version",
            "plugin v2 version must be semantic".to_owned(),
        ));
    }
    manifest.min_nagi_version = validate_min_nagi_version(Some(&manifest.min_nagi_version))?;
    manifest.description = manifest
        .description
        .map(|description| description.trim().to_owned())
        .filter(|description| !description.is_empty());
    let entrypoint = std::path::Path::new(&manifest.entrypoint);
    if manifest.entrypoint.is_empty()
        || manifest.entrypoint.len() > 4_096
        || entrypoint.is_absolute()
        || entrypoint
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err((
            "invalid_plugin_v2_entrypoint",
            "plugin v2 entrypoint must be one exact relative path".to_owned(),
        ));
    }
    let entrypoint = plugin_root.join(entrypoint);
    let entrypoint = entrypoint.canonicalize().map_err(|error| {
        (
            "plugin_v2_entrypoint_not_found",
            format!("plugin v2 entrypoint is unavailable: {error}"),
        )
    })?;
    if !entrypoint.is_file() || !entrypoint.starts_with(&plugin_root) {
        return Err((
            "invalid_plugin_v2_entrypoint",
            "plugin v2 entrypoint must be a file inside the plugin root".to_owned(),
        ));
    }
    manifest.entrypoint = entrypoint.to_string_lossy().into_owned();
    manifest.capabilities =
        crate::plugin_capabilities::normalize_capabilities(&manifest.capabilities)
            .map_err(|error| ("invalid_plugin_capability", error.to_string()))?;
    validate_v2_contributions(&manifest.contributions, &manifest.capabilities)?;
    Ok(manifest)
}

fn validate_v2_contributions(
    contributions: &crate::api::schema::PluginContributionsV2,
    capabilities: &[String],
) -> Result<(), (&'static str, String)> {
    let mut command_ids = std::collections::BTreeSet::new();
    for command in &contributions.commands {
        if normalize_action_id(&command.id).as_deref() != Some(command.id.trim())
            || command.title.trim().is_empty()
            || command.title.len() > 160
            || !command_ids.insert(command.id.as_str())
            || command.contexts.iter().any(|context| {
                !matches!(
                    context.as_str(),
                    "global" | "mission" | "pane" | "workspace"
                )
            })
        {
            return Err((
                "invalid_plugin_v2_command",
                format!(
                    "invalid or duplicate v2 command contribution '{}'",
                    command.id
                ),
            ));
        }
    }
    let mut inspector_ids = std::collections::BTreeSet::new();
    for tab in &contributions.inspector_tabs {
        if normalize_action_id(&tab.id).as_deref() != Some(tab.id.trim())
            || normalize_action_id(&tab.source).as_deref() != Some(tab.source.trim())
            || tab.title.trim().is_empty()
            || tab.title.len() > 160
            || !inspector_ids.insert(tab.id.as_str())
        {
            return Err((
                "invalid_plugin_v2_inspector_tab",
                format!("invalid or duplicate inspector tab '{}'", tab.id),
            ));
        }
        if !command_ids.contains(tab.source.as_str()) {
            return Err((
                "invalid_plugin_v2_inspector_tab",
                format!(
                    "inspector tab '{}' references missing command source '{}'",
                    tab.id, tab.source
                ),
            ));
        }
    }
    if !contributions.inspector_tabs.is_empty()
        && !capabilities
            .iter()
            .any(|capability| capability == "mission.read")
    {
        return Err((
            "plugin_inspector_capability_required",
            "inspector tab contributions require the mission.read capability".to_owned(),
        ));
    }
    Ok(())
}

fn validate_min_nagi_version(value: Option<&str>) -> Result<String, (&'static str, String)> {
    let Some(value) = value else {
        return Err((
            "invalid_plugin_min_nagi_version",
            "plugin min_nagi_version is required".to_string(),
        ));
    };
    let value = non_empty_trimmed(
        value,
        "invalid_plugin_min_nagi_version",
        "plugin min_nagi_version is required",
    )?;
    let required = crate::update::Version::parse(&value).ok_or_else(|| {
        (
            "invalid_plugin_min_nagi_version",
            format!(
                "plugin min_nagi_version must be a semantic version like {}",
                crate::build_info::BASE_VERSION
            ),
        )
    })?;
    let current = crate::update::Version::current();
    if required > current {
        return Err((
            "plugin_requires_newer_nagi",
            format!("plugin requires Nagi {required} or newer; current Nagi is {current}"),
        ));
    }
    Ok(required.to_string())
}

fn normalize_manifest_build(
    build: RawPluginManifestBuild,
) -> Result<PluginManifestBuild, (&'static str, String)> {
    let platforms = normalize_platforms(build.platforms)?;
    let command = normalize_command(build.command)?;
    Ok(PluginManifestBuild { platforms, command })
}

pub(super) fn normalize_plugin_source(
    plugin: &InstalledPluginInfo,
    source: PluginSourceInfo,
) -> Result<PluginSourceInfo, (&'static str, String)> {
    if source.kind == PluginSourceKind::Local {
        return Ok(source);
    }
    let Some(managed_path) = source.managed_path.as_deref() else {
        return Err((
            "invalid_plugin_source",
            "GitHub plugin source requires managed_path".to_string(),
        ));
    };
    let managed_path = std::path::PathBuf::from(managed_path)
        .canonicalize()
        .map_err(|err| ("invalid_plugin_source", err.to_string()))?;
    let plugin_root = std::path::PathBuf::from(&plugin.plugin_root)
        .canonicalize()
        .map_err(|err| ("invalid_plugin_source", err.to_string()))?;
    let expected = crate::session::data_dir()
        .join("plugins")
        .join("github")
        .join(crate::api::schema::plugin_managed_path_component(
            &plugin.plugin_id,
        ))
        .canonicalize()
        .map_err(|err| ("invalid_plugin_source", err.to_string()))?;
    if managed_path != expected {
        return Err((
            "invalid_plugin_source",
            "GitHub plugin managed_path does not match the plugin id".to_string(),
        ));
    }
    if !plugin_root.starts_with(&managed_path) {
        return Err((
            "invalid_plugin_source",
            "plugin manifest is not inside the managed checkout".to_string(),
        ));
    }
    Ok(source)
}

fn reject_duplicate_action_ids(
    actions: &[PluginManifestAction],
) -> Result<(), (&'static str, String)> {
    let mut seen = std::collections::HashSet::new();
    for action in actions {
        if !seen.insert(action.id.as_str()) {
            return Err((
                "duplicate_plugin_action_id",
                format!("duplicate action id '{}'", action.id),
            ));
        }
    }
    Ok(())
}

fn validate_event_names(events: &[crate::api::schema::PluginManifestEventHook]) -> Vec<String> {
    let known = crate::api::schema::plugin_hook_event_names();
    events
        .iter()
        .filter(|hook| !known.contains(&hook.on.as_str()))
        .map(|hook| format!("unknown event '{}'", hook.on))
        .collect()
}

fn reject_duplicate_pane_ids(panes: &[PluginManifestPane]) -> Result<(), (&'static str, String)> {
    let mut seen = std::collections::HashSet::new();
    for pane in panes {
        if !seen.insert(pane.id.as_str()) {
            return Err((
                "duplicate_plugin_pane_id",
                format!("duplicate pane id '{}'", pane.id),
            ));
        }
    }
    Ok(())
}

fn reject_duplicate_link_handler_ids(
    handlers: &[PluginManifestLinkHandler],
) -> Result<(), (&'static str, String)> {
    let mut seen = std::collections::HashSet::new();
    for handler in handlers {
        if !seen.insert(handler.id.as_str()) {
            return Err((
                "duplicate_plugin_link_handler_id",
                format!("duplicate link handler id '{}'", handler.id),
            ));
        }
    }
    Ok(())
}

fn validate_link_handler_actions(
    handlers: &[PluginManifestLinkHandler],
    actions: &[PluginManifestAction],
) -> Result<(), (&'static str, String)> {
    for handler in handlers {
        if !actions.iter().any(|action| action.id == handler.action) {
            return Err((
                "invalid_plugin_link_handler_action",
                format!(
                    "link handler '{}' references unknown action '{}'",
                    handler.id, handler.action
                ),
            ));
        }
    }
    Ok(())
}

fn normalize_manifest_action(
    action: RawPluginManifestAction,
) -> Result<PluginManifestAction, (&'static str, String)> {
    let id = normalize_action_id(&action.id)
        .ok_or_else(|| ("invalid_plugin_action_id", "invalid action id".to_string()))?;
    let title = non_empty_trimmed(
        &action.title,
        "invalid_plugin_action_title",
        "action title is required",
    )?;
    let description = action
        .description
        .map(|description| description.trim().to_string())
        .filter(|description| !description.is_empty());
    let platforms = normalize_platforms(action.platforms)?;
    let command = normalize_command(action.command)?;
    Ok(PluginManifestAction {
        id,
        title,
        description,
        contexts: action.contexts,
        platforms,
        command,
    })
}

fn normalize_manifest_pane(
    pane: RawPluginManifestPane,
) -> Result<PluginManifestPane, (&'static str, String)> {
    let id = normalize_action_id(&pane.id)
        .ok_or_else(|| ("invalid_plugin_pane_id", "invalid pane id".to_string()))?;
    let title = non_empty_trimmed(
        &pane.title,
        "invalid_plugin_pane_title",
        "pane title is required",
    )?;
    let description = pane
        .description
        .map(|description| description.trim().to_string())
        .filter(|description| !description.is_empty());
    let platforms = normalize_platforms(pane.platforms)?;
    let command = normalize_command(pane.command)?;
    if pane.placement != PluginPanePlacement::Popup
        && (pane.width.is_some() || pane.height.is_some())
    {
        return Err((
            "invalid_plugin_pane_size",
            "pane width and height are only supported when placement is popup".to_string(),
        ));
    }
    Ok(PluginManifestPane {
        id,
        title,
        description,
        platforms,
        placement: pane.placement,
        width: pane.width,
        height: pane.height,
        command,
    })
}

fn normalize_manifest_event(
    event: RawPluginManifestEventHook,
) -> Result<PluginManifestEventHook, (&'static str, String)> {
    let on = non_empty_trimmed(&event.on, "invalid_plugin_event", "event name is required")?;
    let platforms = normalize_platforms(event.platforms)?;
    let command = normalize_command(event.command)?;
    Ok(PluginManifestEventHook {
        on,
        platforms,
        command,
    })
}

fn normalize_manifest_link_handler(
    handler: RawPluginManifestLinkHandler,
) -> Result<PluginManifestLinkHandler, (&'static str, String)> {
    let id = normalize_action_id(&handler.id).ok_or_else(|| {
        (
            "invalid_plugin_link_handler_id",
            "invalid link handler id".to_string(),
        )
    })?;
    let title = non_empty_trimmed(
        &handler.title,
        "invalid_plugin_link_handler_title",
        "link handler title is required",
    )?;
    let pattern = non_empty_trimmed(
        &handler.pattern,
        "invalid_plugin_link_handler_pattern",
        "link handler pattern is required",
    )?;
    regex::Regex::new(&pattern)
        .map_err(|err| ("invalid_plugin_link_handler_pattern", err.to_string()))?;
    let action = normalize_action_id(&handler.action).ok_or_else(|| {
        (
            "invalid_plugin_link_handler_action",
            "invalid link handler action".to_string(),
        )
    })?;
    let platforms = normalize_platforms(handler.platforms)?;
    Ok(PluginManifestLinkHandler {
        id,
        title,
        pattern,
        action,
        platforms,
    })
}

fn normalize_platforms(
    raw: Option<Vec<RawPlatform>>,
) -> Result<Option<Vec<PluginPlatform>>, (&'static str, String)> {
    match raw {
        None => Ok(None),
        Some(list) if list.is_empty() => Err((
            "invalid_plugin_platform",
            "platforms must not be an empty array; omit the field to leave platforms undeclared"
                .to_string(),
        )),
        Some(list) => Ok(Some(list.into_iter().map(|p| p.0).collect())),
    }
}

/// Returns the platform the current binary was compiled for.
fn current_platform() -> PluginPlatform {
    if cfg!(target_os = "linux") {
        PluginPlatform::Linux
    } else if cfg!(target_os = "macos") {
        PluginPlatform::Macos
    } else {
        PluginPlatform::Windows
    }
}

/// Resolve the effective platforms for an action or event: use the item's own
/// platforms if declared, otherwise inherit from the plugin-level platforms.
/// Returns a reference to whichever `Option<Vec<PluginPlatform>>` applies.
pub(super) fn effective_platforms<'a>(
    item_platforms: &'a Option<Vec<PluginPlatform>>,
    plugin_platforms: &'a Option<Vec<PluginPlatform>>,
) -> &'a Option<Vec<PluginPlatform>> {
    if item_platforms.is_some() {
        item_platforms
    } else {
        plugin_platforms
    }
}

pub(super) fn ensure_platform_supported(
    platforms: &Option<Vec<PluginPlatform>>,
    subject: &str,
) -> Result<(), (&'static str, String)> {
    if let Some(platforms) = platforms {
        let host = current_platform();
        if !platforms.contains(&host) {
            return Err((
                "platform_unsupported",
                format!(
                    "{subject} does not support the current platform ({})",
                    platform_name(host)
                ),
            ));
        }
    }
    Ok(())
}

fn platform_name(p: PluginPlatform) -> &'static str {
    match p {
        PluginPlatform::Linux => "linux",
        PluginPlatform::Macos => "macos",
        PluginPlatform::Windows => "windows",
    }
}

fn normalize_command(command: Vec<String>) -> Result<Vec<String>, (&'static str, String)> {
    let command = command
        .into_iter()
        .map(|arg| arg.trim().to_string())
        .collect::<Vec<_>>();
    if command.is_empty() || command.iter().any(|arg| arg.is_empty()) {
        return Err((
            "invalid_plugin_command",
            "command must contain non-empty argv strings".to_string(),
        ));
    }
    Ok(command)
}

fn non_empty_trimmed(
    value: &str,
    code: &'static str,
    message: &'static str,
) -> Result<String, (&'static str, String)> {
    let value = value.trim().to_string();
    if value.is_empty() {
        Err((code, message.to_string()))
    } else {
        Ok(value)
    }
}

pub(crate) fn normalize_plugin_id(value: &str) -> Option<String> {
    normalize_identifier(value, PLUGIN_ID_MAX_CHARS)
}

pub(super) fn normalize_action_id(value: &str) -> Option<String> {
    normalize_local_identifier(value, PLUGIN_ACTION_ID_MAX_CHARS)
}

fn normalize_identifier(value: &str, max_chars: usize) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.chars().count() <= max_chars
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'.' | b'_' | b'-')))
    .then(|| value.to_string())
}

fn normalize_local_identifier(value: &str, max_chars: usize) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.chars().count() <= max_chars
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-')))
    .then(|| value.to_string())
}

#[cfg(test)]
mod versioned_manifest_tests {
    use super::*;

    fn write_v2(root: &std::path::Path, extra: &str) -> String {
        std::fs::write(root.join("plugin.wasm"), b"component").unwrap();
        format!(
            r#"
manifest_version = 2
id = "example.review"
name = "Review"
version = "1.2.0"
min_nagi_version = "{}"
runtime = "wasi-component"
entrypoint = "plugin.wasm"
capabilities = ["mission.read", "workspace.files.read:changed"]
{extra}

[[contributions.commands]]
id = "review-current"
title = "Review current mission"
contexts = ["mission"]
"#,
            crate::build_info::BASE_VERSION
        )
    }

    #[test]
    fn valid_v2_is_parsed_as_a_sandboxed_component() {
        let root = tempfile::tempdir().unwrap();
        let source = write_v2(root.path(), "");
        let ParsedPluginManifest::V2(manifest) =
            parse_plugin_manifest(&source, root.path()).unwrap()
        else {
            panic!("valid v2 manifest was downgraded to legacy")
        };
        assert_eq!(
            manifest.runtime,
            crate::api::schema::PluginRuntimeV2::WasiComponent
        );
        assert!(std::path::Path::new(&manifest.entrypoint).is_absolute());
    }

    #[test]
    fn unknown_versions_and_v2_fields_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let unknown_version =
            write_v2(root.path(), "").replace("manifest_version = 2", "manifest_version = 99");
        assert_eq!(
            parse_plugin_manifest(&unknown_version, root.path())
                .unwrap_err()
                .0,
            "unsupported_plugin_manifest_version"
        );
        let unknown_field = write_v2(root.path(), "pretend_sandboxed = true");
        assert_eq!(
            parse_plugin_manifest(&unknown_field, root.path())
                .unwrap_err()
                .0,
            "plugin_manifest_v2_parse_failed"
        );
    }

    #[test]
    fn legacy_v1_can_declare_version_one_but_cannot_smuggle_security_fields() {
        let legacy = format!(
            r#"
manifest_version = 1
id = "example.legacy"
name = "Legacy"
version = "1.0.0"
min_nagi_version = "{}"
"#,
            crate::build_info::BASE_VERSION
        );
        assert!(matches!(
            parse_plugin_manifest(&legacy, std::path::Path::new(".")),
            Ok(ParsedPluginManifest::Legacy(_))
        ));
        let smuggled = format!("{legacy}\nruntime = \"wasi-component\"\n");
        assert_eq!(
            parse_plugin_manifest(&smuggled, std::path::Path::new("."))
                .unwrap_err()
                .0,
            "plugin_manifest_parse_failed"
        );
    }

    #[test]
    fn v2_rejects_escaping_entrypoints_and_invalid_capabilities() {
        let root = tempfile::tempdir().unwrap();
        let escaping = write_v2(root.path(), "").replace("plugin.wasm", "../plugin.wasm");
        assert_eq!(
            parse_plugin_manifest(&escaping, root.path()).unwrap_err().0,
            "invalid_plugin_v2_entrypoint"
        );
        let invalid = write_v2(root.path(), "").replace(
            "workspace.files.read:changed",
            "workspace.files.read:../../home",
        );
        assert_eq!(
            parse_plugin_manifest(&invalid, root.path()).unwrap_err().0,
            "invalid_plugin_capability"
        );
    }

    #[test]
    fn inspector_tabs_require_a_real_command_source_and_mission_read() {
        let root = tempfile::tempdir().unwrap();
        let valid = write_v2(
            root.path(),
            r#"
[[contributions.inspector_tabs]]
id = "risk"
title = "Risk"
source = "review-current"
"#,
        );
        let ParsedPluginManifest::V2(manifest) =
            parse_plugin_manifest(&valid, root.path()).unwrap()
        else {
            panic!("expected v2 manifest")
        };
        assert_eq!(manifest.contributions.inspector_tabs.len(), 1);

        let missing_source = valid.replace("source = \"review-current\"", "source = \"missing\"");
        assert_eq!(
            parse_plugin_manifest(&missing_source, root.path())
                .unwrap_err()
                .0,
            "invalid_plugin_v2_inspector_tab"
        );

        let missing_capability = valid.replace(
            "capabilities = [\"mission.read\", \"workspace.files.read:changed\"]",
            "capabilities = [\"workspace.files.read:changed\"]",
        );
        assert_eq!(
            parse_plugin_manifest(&missing_capability, root.path())
                .unwrap_err()
                .0,
            "plugin_inspector_capability_required"
        );
    }
}
