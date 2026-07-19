use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{
    command_palette::{
        available_commands, filtered_commands, reconcile_selection, CommandPaletteAction,
    },
    state::{AppState, ToastKind, ToastNotification},
    App,
};

impl App {
    pub(crate) fn dispatch_command_palette_key(&mut self, key: KeyEvent) {
        let Some(action) = handle_command_palette_key(&mut self.state, key) else {
            return;
        };
        self.execute_command_palette_action(action);
    }

    pub(crate) fn dispatch_command_palette_index(&mut self, index: usize) {
        select_command_palette_index(&mut self.state, index);
        if let Some(action) = selected_action(&mut self.state) {
            self.execute_command_palette_action(action);
        }
    }

    fn execute_command_palette_action(&mut self, action: CommandPaletteAction) {
        match action {
            CommandPaletteAction::OpenCockpit => {
                self.state.open_navigator_from(&self.terminal_runtimes)
            }
            CommandPaletteAction::NewWorkspace => self.execute_tui_navigate_action(
                super::navigate::NavigateAction::NewWorkspace,
                super::navigate::ActionContext::Direct,
            ),
            CommandPaletteAction::NewTab => self.execute_tui_navigate_action(
                super::navigate::NavigateAction::NewTab,
                super::navigate::ActionContext::Direct,
            ),
            CommandPaletteAction::Settings => super::settings::open_settings(&mut self.state),
            CommandPaletteAction::Keybinds => super::modal::open_keybind_help(&mut self.state),
            CommandPaletteAction::ReloadConfig => {
                self.runtime_server_reload_config("tui.command_palette.reload_config");
                super::modal::leave_modal(&mut self.state);
            }
            CommandPaletteAction::Detach => {
                super::modal::leave_modal(&mut self.state);
                super::modal::request_detach(&mut self.state);
            }
            CommandPaletteAction::Plugin { qualified_id } => {
                super::modal::leave_modal(&mut self.state);
                let previous_toast = self.state.toast.clone();
                if let Err(error) = self.invoke_plugin_action_from_keybind(qualified_id) {
                    self.state.toast = Some(ToastNotification {
                        kind: ToastKind::NeedsAttention,
                        title: "Plugin action failed".to_string(),
                        context: error,
                        position: None,
                        target: None,
                    });
                }
                self.sync_toast_deadline(previous_toast);
            }
        }
    }
}

pub(crate) fn handle_command_palette_key(
    state: &mut AppState,
    key: KeyEvent,
) -> Option<CommandPaletteAction> {
    match key.code {
        KeyCode::Esc => {
            super::modal::leave_modal(state);
            return None;
        }
        KeyCode::Enter => return selected_action(state),
        KeyCode::Backspace => {
            state.command_palette.query.pop();
            state.command_palette.selected_id = None;
            state.command_palette.selected = 0;
            state.command_palette.scroll = 0;
        }
        KeyCode::Up | KeyCode::Char('p')
            if key.code == KeyCode::Up || key.modifiers == KeyModifiers::CONTROL =>
        {
            move_selection(state, -1);
            return None;
        }
        KeyCode::Down | KeyCode::Char('n')
            if key.code == KeyCode::Down || key.modifiers == KeyModifiers::CONTROL =>
        {
            move_selection(state, 1);
            return None;
        }
        KeyCode::Home => {
            state.command_palette.selected = 0;
            state.command_palette.selected_id = None;
        }
        KeyCode::End => {
            let commands = available_commands(&state.installed_plugins, state.active.is_some());
            let matches = filtered_commands(&commands, &state.command_palette.query);
            state.command_palette.selected = matches.len().saturating_sub(1);
            state.command_palette.selected_id = None;
        }
        KeyCode::Char(character)
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
        {
            state.command_palette.query.push(character);
            state.command_palette.selected_id = None;
            state.command_palette.selected = 0;
            state.command_palette.scroll = 0;
        }
        _ => return None,
    }
    refresh_selection(state);
    None
}

fn refresh_selection(state: &mut AppState) {
    let commands = available_commands(&state.installed_plugins, state.active.is_some());
    let matches = filtered_commands(&commands, &state.command_palette.query);
    reconcile_selection(&mut state.command_palette, &matches);
}

pub(crate) fn insert_command_palette_text(state: &mut AppState, text: &str) {
    state
        .command_palette
        .query
        .extend(text.chars().filter(|character| !character.is_control()));
    state.command_palette.selected = 0;
    state.command_palette.selected_id = None;
    state.command_palette.scroll = 0;
    refresh_selection(state);
}

fn move_selection(state: &mut AppState, delta: isize) {
    let commands = available_commands(&state.installed_plugins, state.active.is_some());
    let matches = filtered_commands(&commands, &state.command_palette.query);
    reconcile_selection(&mut state.command_palette, &matches);
    if matches.is_empty() {
        return;
    }
    state.command_palette.selected = state
        .command_palette
        .selected
        .saturating_add_signed(delta)
        .min(matches.len() - 1);
    state.command_palette.selected_id = Some(matches[state.command_palette.selected].id.clone());
}

fn selected_action(state: &mut AppState) -> Option<CommandPaletteAction> {
    let commands = available_commands(&state.installed_plugins, state.active.is_some());
    let matches = filtered_commands(&commands, &state.command_palette.query);
    reconcile_selection(&mut state.command_palette, &matches);
    let command = matches.get(state.command_palette.selected)?;
    command.enabled.then(|| command.action.clone())
}

pub(crate) fn select_command_palette_index(state: &mut AppState, index: usize) {
    let commands = available_commands(&state.installed_plugins, state.active.is_some());
    let matches = filtered_commands(&commands, &state.command_palette.query);
    let Some(command) = matches.get(index) else {
        return;
    };
    state.command_palette.selected = index;
    state.command_palette.selected_id = Some(command.id.clone());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_filters_palette_and_keeps_selection_on_the_best_match() {
        let mut state = AppState::test_new();
        state.open_command_palette();

        for character in "settings".chars() {
            handle_command_palette_key(
                &mut state,
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
            );
        }

        assert_eq!(state.command_palette.query, "settings");
        assert_eq!(
            state.command_palette.selected_id.as_deref(),
            Some("core.settings")
        );
    }

    #[test]
    fn enter_returns_the_selected_enabled_action() {
        let mut state = AppState::test_new();
        state.open_command_palette();
        for character in "settings".chars() {
            handle_command_palette_key(
                &mut state,
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
            );
        }

        let action = handle_command_palette_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );

        assert_eq!(action, Some(CommandPaletteAction::Settings));
    }

    #[test]
    fn disabled_action_cannot_be_executed() {
        let mut state = AppState::test_new();
        state.open_command_palette();
        for character in "new tab".chars() {
            handle_command_palette_key(
                &mut state,
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
            );
        }

        let action = handle_command_palette_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );

        assert_eq!(action, None);
    }
}
