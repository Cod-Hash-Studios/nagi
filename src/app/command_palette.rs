#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommandPaletteAction {
    OpenCockpit,
    NewMission {
        provider: crate::api::schema::MissionProvider,
    },
    NewWorkspace,
    NewTab,
    Settings,
    Keybinds,
    ReloadConfig,
    Detach,
    Plugin {
        qualified_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandPaletteCommand {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) provenance: String,
    pub(crate) enabled: bool,
    pub(crate) disabled_reason: Option<String>,
    pub(crate) action: CommandPaletteAction,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CommandPaletteState {
    pub(crate) query: String,
    pub(crate) selected: usize,
    pub(crate) selected_id: Option<String>,
    pub(crate) scroll: usize,
}

pub(crate) fn reconcile_selection(
    state: &mut CommandPaletteState,
    commands: &[&CommandPaletteCommand],
) {
    if commands.is_empty() {
        state.selected = 0;
        state.selected_id = None;
        state.scroll = 0;
        return;
    }
    if let Some(selected_id) = state.selected_id.as_deref() {
        if let Some(index) = commands
            .iter()
            .position(|command| command.id == selected_id)
        {
            state.selected = index;
            return;
        }
    }
    state.selected = state.selected.min(commands.len() - 1);
    state.selected_id = Some(commands[state.selected].id.clone());
}

pub(crate) fn filtered_commands<'a>(
    commands: &'a [CommandPaletteCommand],
    query: &str,
) -> Vec<&'a CommandPaletteCommand> {
    let tokens = query
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return commands.iter().collect();
    }
    let mut ranked = commands
        .iter()
        .filter_map(|command| command_match_score(command, &tokens).map(|score| (score, command)))
        .collect::<Vec<_>>();
    ranked.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| left.title.cmp(&right.title))
            .then_with(|| left.id.cmp(&right.id))
    });
    ranked.into_iter().map(|(_, command)| command).collect()
}

pub(crate) fn available_commands(
    plugins: &crate::app::state::InstalledPluginRegistry,
    has_workspace: bool,
) -> Vec<CommandPaletteCommand> {
    let mut commands = vec![
        core_command(
            "core.cockpit",
            "Open cockpit",
            "Inspect every workspace and agent",
            true,
            None,
            CommandPaletteAction::OpenCockpit,
        ),
        core_command(
            "core.workspace.new",
            "New workspace",
            "Start another durable workspace",
            true,
            None,
            CommandPaletteAction::NewWorkspace,
        ),
        core_command(
            "core.tab.new",
            "New tab",
            "Add a tab to the current workspace",
            has_workspace,
            (!has_workspace).then(|| "No active workspace".to_string()),
            CommandPaletteAction::NewTab,
        ),
        core_command(
            "core.settings",
            "Open settings",
            "Customize appearance and behavior",
            true,
            None,
            CommandPaletteAction::Settings,
        ),
        core_command(
            "core.keybindings",
            "Show keybindings",
            "Review keyboard controls",
            true,
            None,
            CommandPaletteAction::Keybinds,
        ),
        core_command(
            "core.reload",
            "Reload configuration",
            "Apply the latest config and theme files",
            true,
            None,
            CommandPaletteAction::ReloadConfig,
        ),
        core_command(
            "core.detach",
            "Detach session",
            "Leave panes running in the background",
            true,
            None,
            CommandPaletteAction::Detach,
        ),
        provider_command(
            "provider.codex.mission.new",
            "New mission with Codex",
            "Start a durable, proof-bound mission",
            "Codex",
            crate::api::schema::MissionProvider::Codex,
        ),
        provider_command(
            "provider.claude-code.mission.new",
            "New mission with Claude Code",
            "Start a durable, proof-bound mission",
            "Claude Code",
            crate::api::schema::MissionProvider::ClaudeCode,
        ),
    ];
    for plugin in plugins.values() {
        for action in &plugin.actions {
            let disabled_reason = plugin_action_disabled_reason(plugin, action, has_workspace);
            let qualified_id = format!("{}.{}", plugin.plugin_id, action.id);
            commands.push(CommandPaletteCommand {
                id: format!("plugin.{qualified_id}"),
                title: action.title.clone(),
                description: action.description.clone().unwrap_or_default(),
                provenance: format!("plugin · {}", plugin.name),
                enabled: disabled_reason.is_none(),
                disabled_reason,
                action: CommandPaletteAction::Plugin { qualified_id },
            });
        }
    }
    commands.sort_by(|left, right| left.id.cmp(&right.id));
    commands
}

impl crate::app::state::AppState {
    pub(crate) fn open_command_palette(&mut self) {
        self.command_palette = CommandPaletteState::default();
        let commands = available_commands(&self.installed_plugins, self.active.is_some());
        let filtered = filtered_commands(&commands, "");
        reconcile_selection(&mut self.command_palette, &filtered);
        self.mode = crate::app::state::Mode::CommandPalette;
    }
}

fn core_command(
    id: &str,
    title: &str,
    description: &str,
    enabled: bool,
    disabled_reason: Option<String>,
    action: CommandPaletteAction,
) -> CommandPaletteCommand {
    CommandPaletteCommand {
        id: id.to_string(),
        title: title.to_string(),
        description: description.to_string(),
        provenance: "core".to_string(),
        enabled,
        disabled_reason,
        action,
    }
}

fn provider_command(
    id: &str,
    title: &str,
    description: &str,
    label: &str,
    provider: crate::api::schema::MissionProvider,
) -> CommandPaletteCommand {
    CommandPaletteCommand {
        id: id.to_string(),
        title: title.to_string(),
        description: description.to_string(),
        provenance: format!("provider · {label}"),
        enabled: true,
        disabled_reason: None,
        action: CommandPaletteAction::NewMission { provider },
    }
}

fn plugin_action_disabled_reason(
    plugin: &crate::api::schema::InstalledPluginInfo,
    action: &crate::api::schema::PluginManifestAction,
    has_workspace: bool,
) -> Option<String> {
    if !plugin.enabled {
        return Some("Plugin is disabled".to_string());
    }
    if plugin.warnings.iter().any(|warning| {
        warning.starts_with(crate::persist::plugin_registry::MANIFEST_UNAVAILABLE_WARNING_PREFIX)
    }) {
        return Some("Plugin manifest is unavailable".to_string());
    }
    let platforms = action.platforms.as_ref().or(plugin.platforms.as_ref());
    if platforms.is_some_and(|platforms| !platforms.contains(&current_plugin_platform())) {
        return Some("Unavailable on this platform".to_string());
    }
    let needs_workspace = !action.contexts.is_empty()
        && action
            .contexts
            .iter()
            .all(|context| *context != crate::api::schema::PluginActionContext::Global);
    if needs_workspace && !has_workspace {
        return Some("No active workspace".to_string());
    }
    None
}

const fn current_plugin_platform() -> crate::api::schema::PluginPlatform {
    #[cfg(target_os = "macos")]
    {
        crate::api::schema::PluginPlatform::Macos
    }
    #[cfg(target_os = "linux")]
    {
        crate::api::schema::PluginPlatform::Linux
    }
    #[cfg(target_os = "windows")]
    {
        crate::api::schema::PluginPlatform::Windows
    }
}

fn command_match_score(command: &CommandPaletteCommand, tokens: &[String]) -> Option<u32> {
    if tokens.is_empty() {
        return Some(0);
    }
    let title = command.title.to_ascii_lowercase();
    let description = command.description.to_ascii_lowercase();
    let id = command.id.to_ascii_lowercase();
    let provenance = command.provenance.to_ascii_lowercase();
    let haystack = format!("{title} {description} {id} {provenance}");
    let mut score = 0;
    for token in tokens {
        if !is_subsequence(token, &haystack) {
            return None;
        }
        score += field_score(token, &title, 100, 60);
        score += field_score(token, &id, 45, 30);
        score += field_score(token, &provenance, 25, 15);
        score += field_score(token, &description, 12, 8);
        if haystack.contains(token) {
            score += 10;
        }
    }
    Some(score)
}

fn field_score(token: &str, field: &str, prefix: u32, word_prefix: u32) -> u32 {
    if field.starts_with(token) {
        prefix
    } else if field
        .split(|ch: char| !ch.is_alphanumeric())
        .any(|word| word.starts_with(token))
    {
        word_prefix
    } else {
        0
    }
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle
        .chars()
        .all(|needle_char| chars.any(|haystack_char| haystack_char == needle_char))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(enabled: bool) -> crate::api::schema::InstalledPluginInfo {
        crate::api::schema::InstalledPluginInfo {
            manifest_version: 1,
            plugin_id: "example.review".into(),
            name: "Example Review".into(),
            version: "1.0.0".into(),
            min_nagi_version: String::new(),
            description: None,
            manifest_path: "/tmp/plugin.toml".into(),
            plugin_root: "/tmp".into(),
            enabled,
            runtime: crate::api::schema::PluginRuntimeV2::TrustedNative,
            entrypoint: None,
            requested_capabilities: Vec::new(),
            native_trusted: true,
            platforms: None,
            build: Vec::new(),
            actions: vec![crate::api::schema::PluginManifestAction {
                id: "review-current".into(),
                title: "Review current mission".into(),
                description: Some("Inspect the active diff".into()),
                contexts: vec![crate::api::schema::PluginActionContext::Workspace],
                platforms: None,
                command: vec!["review".into()],
            }],
            events: Vec::new(),
            panes: Vec::new(),
            link_handlers: Vec::new(),
            inspector_tabs: Vec::new(),
            source: crate::api::schema::PluginSourceInfo::default(),
            warnings: Vec::new(),
        }
    }

    fn command(id: &str, title: &str, provenance: &str) -> CommandPaletteCommand {
        CommandPaletteCommand {
            id: id.into(),
            title: title.into(),
            description: String::new(),
            provenance: provenance.into(),
            enabled: true,
            disabled_reason: None,
            action: CommandPaletteAction::Settings,
        }
    }

    #[test]
    fn fuzzy_search_matches_ordered_characters_and_ranks_title_prefix_first() {
        let commands = vec![
            command(
                "plugin.review",
                "Review current mission",
                "plugin · example",
            ),
            command("core.reload", "Reload configuration", "core"),
            command("core.review", "Review proof", "core"),
        ];

        let matches = filtered_commands(&commands, "rev pr");

        assert_eq!(
            matches
                .iter()
                .map(|command| command.id.as_str())
                .collect::<Vec<_>>(),
            vec!["core.review", "plugin.review"]
        );
    }

    #[test]
    fn selection_follows_command_identity_when_live_actions_reorder() {
        let commands = [
            command("core.settings", "Settings", "core"),
            command("plugin.review", "Review", "plugin · example"),
            command("core.goto", "Open cockpit", "core"),
        ];
        let refreshed = vec![&commands[2], &commands[0], &commands[1]];
        let mut state = CommandPaletteState {
            selected: 1,
            selected_id: Some("plugin.review".into()),
            ..CommandPaletteState::default()
        };

        reconcile_selection(&mut state, &refreshed);

        assert_eq!(state.selected, 2);
        assert_eq!(state.selected_id.as_deref(), Some("plugin.review"));
    }

    #[test]
    fn plugin_actions_expose_provenance_and_disabled_reason() {
        let plugins = [("example.review".to_string(), plugin(false))]
            .into_iter()
            .collect();

        let commands = available_commands(&plugins, true);
        let action = commands
            .iter()
            .find(|command| command.id == "plugin.example.review.review-current")
            .expect("plugin action must be discoverable");

        assert_eq!(action.provenance, "plugin · Example Review");
        assert!(!action.enabled);
        assert_eq!(
            action.disabled_reason.as_deref(),
            Some("Plugin is disabled")
        );
        assert_eq!(
            action.action,
            CommandPaletteAction::Plugin {
                qualified_id: "example.review.review-current".into()
            }
        );
    }

    #[test]
    fn managed_provider_quick_actions_expose_codex_and_claude() {
        let commands = available_commands(&Default::default(), false);

        let codex = commands
            .iter()
            .find(|command| command.id == "provider.codex.mission.new")
            .expect("Codex mission action must be discoverable");
        assert_eq!(codex.title, "New mission with Codex");
        assert_eq!(codex.provenance, "provider · Codex");
        assert!(codex.enabled);

        let claude = commands
            .iter()
            .find(|command| command.id == "provider.claude-code.mission.new")
            .expect("Claude Code mission action must be discoverable");
        assert_eq!(claude.title, "New mission with Claude Code");
        assert_eq!(claude.provenance, "provider · Claude Code");
        assert!(claude.enabled);
    }

    #[test]
    fn opening_palette_resets_search_and_selects_the_first_available_action() {
        let mut state = crate::app::state::AppState::test_new();
        state.command_palette.query = "stale".into();
        state.command_palette.selected = 4;
        state.command_palette.selected_id = Some("missing".into());

        state.open_command_palette();

        assert_eq!(state.mode, crate::app::state::Mode::CommandPalette);
        assert!(state.command_palette.query.is_empty());
        assert_eq!(state.command_palette.selected, 0);
        assert_eq!(
            state.command_palette.selected_id.as_deref(),
            Some("core.cockpit")
        );
    }
}
