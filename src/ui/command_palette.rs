use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::{
    app::{
        command_palette::{available_commands, filtered_commands, CommandPaletteCommand},
        state::AppState,
    },
    ui::{
        components::{action_bar, empty_state, focus_rail},
        design::{icons::IconSet, tokens::UiTokens},
        text::{display_width, truncate_end},
        widgets::{centered_popup_rect, render_panel_shell_with_border_set},
    },
};

pub(crate) fn command_palette_popup_rect(area: Rect) -> Option<Rect> {
    let width = area.width.saturating_sub(4).min(92);
    let height = area.height.saturating_sub(2).min(24);
    centered_popup_rect(area, width, height)
}

pub(crate) fn command_palette_command_index_at(
    app: &AppState,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<usize> {
    let popup = command_palette_popup_rect(area)?;
    let inner = Rect::new(
        popup.x.saturating_add(1),
        popup.y.saturating_add(1),
        popup.width.saturating_sub(2),
        popup.height.saturating_sub(2),
    );
    let [_, body, _] = command_palette_areas(inner);
    if column < body.x
        || column >= body.x.saturating_add(body.width)
        || row < body.y
        || row >= body.y.saturating_add(body.height)
    {
        return None;
    }
    let commands = available_commands(&app.installed_plugins, app.active.is_some());
    let matches = filtered_commands(&commands, &app.command_palette.query);
    let capacity = (body.height / 2) as usize;
    if matches.is_empty() || capacity == 0 {
        return None;
    }
    let selected = app.command_palette.selected.min(matches.len() - 1);
    let start = visible_start(selected, matches.len(), capacity);
    let index = start + ((row - body.y) / 2) as usize;
    (index < matches.len() && index < start + capacity).then_some(index)
}

fn command_palette_areas(inner: Rect) -> [Rect; 3] {
    Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner)
}

fn visible_start(selected: usize, command_count: usize, capacity: usize) -> usize {
    selected
        .saturating_sub(capacity.saturating_sub(1))
        .min(command_count.saturating_sub(capacity))
}

pub(crate) fn render_command_palette_overlay(app: &AppState, frame: &mut Frame) {
    super::dim_background(frame, frame.area());
    let Some(popup) = command_palette_popup_rect(frame.area()) else {
        return;
    };
    let tokens = UiTokens::from(&app.palette);
    let icons = IconSet::from(app.icon_style);
    let Some(inner) = render_panel_shell_with_border_set(
        frame,
        popup,
        tokens.focus,
        tokens.panel,
        icons.border_set(app.theme_components.border),
    ) else {
        return;
    };
    let [header, body, footer] = command_palette_areas(inner);
    let commands = available_commands(&app.installed_plugins, app.active.is_some());
    let matches = filtered_commands(&commands, &app.command_palette.query);
    render_header(app, frame, header, matches.len(), tokens, icons);
    render_commands(app, frame, body, &matches, tokens, icons);
    action_bar::render(
        frame,
        footer,
        &[("enter", "run"), ("↑↓", "select"), ("esc", "close")],
        tokens,
    );
}

fn render_header(
    app: &AppState,
    frame: &mut Frame,
    area: Rect,
    match_count: usize,
    tokens: UiTokens,
    icons: IconSet,
) {
    if area.is_empty() {
        return;
    }
    let title = format!("COMMANDS  {match_count}");
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                title,
                Style::default()
                    .fg(tokens.text)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  core + providers + plugins",
                Style::default().fg(tokens.text_muted),
            ),
        ])),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let prompt = if app.command_palette.query.is_empty() {
        "Type to search commands…".to_string()
    } else {
        app.command_palette.query.clone()
    };
    let cursor = match icons {
        IconSet::Unicode => "›",
        IconSet::Ascii => ">",
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("{cursor} "),
                Style::default()
                    .fg(tokens.focus)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate_end(&prompt, area.width.saturating_sub(2) as usize),
                Style::default().fg(if app.command_palette.query.is_empty() {
                    tokens.text_muted
                } else {
                    tokens.text
                }),
            ),
        ])),
        Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
    );
    let rule = match icons {
        IconSet::Unicode => "─",
        IconSet::Ascii => "-",
    };
    frame.render_widget(
        Paragraph::new(rule.repeat(area.width as usize)).style(Style::default().fg(tokens.border)),
        Rect::new(area.x, area.y.saturating_add(2), area.width, 1),
    );
}

fn render_commands(
    app: &AppState,
    frame: &mut Frame,
    area: Rect,
    commands: &[&CommandPaletteCommand],
    tokens: UiTokens,
    icons: IconSet,
) {
    let capacity = (area.height / 2) as usize;
    if capacity == 0 {
        return;
    }
    if commands.is_empty() {
        empty_state::render(
            frame,
            area,
            "No matching command",
            Some(("backspace", "change search")),
            tokens,
        );
        return;
    }
    let selected = app.command_palette.selected.min(commands.len() - 1);
    let start = visible_start(selected, commands.len(), capacity);
    for (visible_index, command) in commands.iter().skip(start).take(capacity).enumerate() {
        let index = start + visible_index;
        let selected = index == selected;
        render_command(
            frame,
            Rect::new(
                area.x,
                area.y
                    .saturating_add((visible_index as u16).saturating_mul(2)),
                area.width,
                2,
            ),
            command,
            selected,
            tokens,
            icons,
            app.theme_components.selection,
        );
    }
}

fn render_command(
    frame: &mut Frame,
    area: Rect,
    command: &CommandPaletteCommand,
    selected: bool,
    tokens: UiTokens,
    icons: IconSet,
    selection: crate::theme::manifest::ThemeSelectionStyle,
) {
    if area.width < 4 || area.height < 2 {
        return;
    }
    let title_width = area.width.saturating_sub(4) as usize;
    let provenance_width = display_width(&command.provenance).min(title_width / 3);
    let available_title_width = title_width.saturating_sub(provenance_width + 1);
    let title = truncate_end(&command.title, available_title_width);
    let provenance = truncate_end(&command.provenance, provenance_width);
    let used = display_width(&title) + display_width(&provenance);
    let gap = title_width.saturating_sub(used).max(1);
    let main_style = Style::default()
        .fg(if command.enabled {
            tokens.text
        } else {
            tokens.text_muted
        })
        .add_modifier(if selected {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });
    let title_line = Line::from(vec![
        focus_rail::span(selected, tokens, icons, selection),
        Span::raw(" "),
        Span::styled(title, main_style),
        Span::raw(" ".repeat(gap)),
        Span::styled(provenance, Style::default().fg(tokens.text_muted)),
    ]);
    let detail = command
        .disabled_reason
        .as_ref()
        .map(|reason| format!("Unavailable · {reason}"))
        .unwrap_or_else(|| command.description.clone());
    let detail_style = Style::default().fg(if command.enabled {
        tokens.text_muted
    } else {
        tokens.proof_stale
    });
    frame.render_widget(
        Paragraph::new(vec![
            title_line,
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    truncate_end(&detail, area.width.saturating_sub(2) as usize),
                    detail_style,
                ),
            ]),
        ])
        .style(focus_rail::row_style(selected, tokens, selection)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;

    fn rendered_with_icons(
        width: u16,
        height: u16,
        icon_style: crate::config::UiIconStyleConfig,
    ) -> String {
        let mut app = AppState::test_new();
        app.icon_style = icon_style;
        app.open_command_palette();
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_command_palette_overlay(&app, frame))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rendered(width: u16, height: u16) -> String {
        rendered_with_icons(width, height, crate::config::UiIconStyleConfig::Unicode)
    }

    #[test]
    fn palette_renders_search_provenance_actions_and_disabled_reason() {
        let output = rendered(80, 24);

        assert!(output.contains("COMMANDS"));
        assert!(output.contains("Open cockpit"));
        assert!(output.contains("core"));
        assert!(output.contains("No active workspace"));
        assert!(output.contains("run"));
    }

    #[test]
    fn palette_hit_testing_maps_the_first_visible_row_to_its_command() {
        let mut app = AppState::test_new();
        app.open_command_palette();
        let area = Rect::new(0, 0, 80, 24);
        let popup = command_palette_popup_rect(area).unwrap();
        let first_row_column = popup.x + 3;
        let first_row = popup.y + 1 + 3;

        assert_eq!(
            command_palette_command_index_at(&app, area, first_row_column, first_row),
            Some(0)
        );
    }

    #[test]
    fn palette_stays_bounded_across_supported_terminal_sizes() {
        for (width, height) in [(60, 20), (80, 24), (120, 35), (200, 60)] {
            let output = rendered(width, height);
            let lines = output.lines().collect::<Vec<_>>();

            assert_eq!(lines.len(), height as usize, "height at {width}x{height}");
            assert!(
                lines
                    .iter()
                    .all(|line| display_width(line) == width as usize),
                "width at {width}x{height}"
            );
            assert!(output.contains("COMMANDS"), "title at {width}x{height}");
            assert!(
                output.contains("Open cockpit"),
                "commands at {width}x{height}"
            );
            assert!(output.contains("esc"), "actions at {width}x{height}");
        }
    }

    #[test]
    fn palette_has_a_complete_ascii_fallback() {
        let output = rendered_with_icons(60, 20, crate::config::UiIconStyleConfig::Ascii);

        assert!(output.contains("> Type to search commands"));
        assert!(!output.contains('›'));
        assert!(!output.contains('─'));
        assert!(!output.contains('╭'));
    }
}
