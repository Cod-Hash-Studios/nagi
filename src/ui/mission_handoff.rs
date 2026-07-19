use ratatui::{
    layout::{Constraint, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

use crate::{
    api::schema::MissionProvider,
    app::state::AppState,
    ui::{
        components::{action_bar, section},
        design::{icons::IconSet, tokens::UiTokens},
        text::{middle_elide, truncate_end},
        widgets::render_panel_shell_with_border_set,
    },
};

pub(super) fn render_mission_handoff_overlay(app: &AppState, frame: &mut Frame) {
    super::dim_background(frame, frame.area());
    let popup = app.navigator_popup_rect();
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
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    let Some(draft) = app.mission_handoff.as_ref() else {
        return;
    };

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "HANDOFF  ",
                    Style::default()
                        .fg(tokens.text_muted)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "{}  →  {}",
                        provider_label(draft.source_provider),
                        provider_label(draft.target_provider)
                    ),
                    Style::default()
                        .fg(tokens.text)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("same mission  ", Style::default().fg(tokens.focus)),
                Span::styled(
                    middle_elide(&draft.mission_id, header.width.saturating_sub(16) as usize),
                    Style::default().fg(tokens.text_muted),
                ),
            ]),
            Line::from(Span::styled(
                match icons {
                    IconSet::Unicode => "─".repeat(header.width as usize),
                    IconSet::Ascii => "-".repeat(header.width as usize),
                },
                Style::default().fg(tokens.border),
            )),
        ]),
        header,
    );

    let mut lines = vec![
        section::heading("Choose the next runtime", tokens, icons),
        Line::from(vec![
            Span::styled("  ‹  ", Style::default().fg(tokens.focus)),
            Span::styled(
                provider_label(draft.target_provider),
                Style::default()
                    .fg(tokens.text)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ›", Style::default().fg(tokens.focus)),
        ]),
        Line::default(),
    ];
    if draft.loading {
        lines.push(Line::from(vec![
            Span::styled("◌  ", Style::default().fg(tokens.focus)),
            Span::styled(
                "Binding a fresh workspace snapshot…",
                Style::default().fg(tokens.text_muted),
            ),
        ]));
    } else if let Some(artifact) = &draft.artifact {
        lines.push(section::heading("Bound continuation", tokens, icons));
        lines.push(Line::from(vec![
            Span::styled("Run       ", Style::default().fg(tokens.text_muted)),
            Span::styled(
                middle_elide(
                    &artifact.suggested_run_id,
                    body.width.saturating_sub(10) as usize,
                ),
                Style::default().fg(tokens.text),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Snapshot  ", Style::default().fg(tokens.text_muted)),
            Span::styled(
                format!(
                    "{}…{}",
                    &artifact.artifact_sha256[..8],
                    &artifact.artifact_sha256[artifact.artifact_sha256.len() - 8..]
                ),
                Style::default().fg(tokens.focus),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Workspace ", Style::default().fg(tokens.text_muted)),
            Span::styled(
                if artifact.diff.dirty {
                    format!("{} changed path(s)", artifact.diff.changed_paths.len())
                } else {
                    "clean".to_owned()
                },
                Style::default().fg(if artifact.diff.dirty {
                    tokens.attention
                } else {
                    tokens.proof_fresh
                }),
            ),
        ]));
        for path in artifact.diff.changed_paths.iter().take(4) {
            lines.push(Line::from(vec![
                Span::styled("  · ", Style::default().fg(tokens.text_muted)),
                Span::styled(
                    middle_elide(path, body.width.saturating_sub(4) as usize),
                    Style::default().fg(tokens.text),
                ),
            ]));
        }
        if artifact.diff.changed_paths.len() > 4 {
            lines.push(Line::from(Span::styled(
                format!("    +{} more", artifact.diff.changed_paths.len() - 4),
                Style::default().fg(tokens.text_muted),
            )));
        }
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled(
                if draft.workspace_write_confirmed {
                    "[x] "
                } else {
                    "[ ] "
                },
                Style::default().fg(if draft.workspace_write_confirmed {
                    tokens.proof_fresh
                } else {
                    tokens.attention
                }),
            ),
            Span::styled(
                "Allow the target provider to write in this worktree",
                Style::default().fg(tokens.text),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            "Proof is invalidated and rerun. Hidden provider reasoning is never transferred.",
            Style::default().fg(tokens.text_muted),
        )));
    }
    if let Some(error) = &draft.error {
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled("!  ", Style::default().fg(tokens.attention)),
            Span::styled(
                truncate_end(error, body.width.saturating_sub(3) as usize),
                Style::default().fg(tokens.attention),
            ),
        ]));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
    action_bar::render(
        frame,
        footer,
        &[
            ("←/→", "provider"),
            ("space", "write scope"),
            ("enter", "continue"),
            ("esc", "back"),
        ],
        tokens,
    );
}

fn provider_label(provider: MissionProvider) -> &'static str {
    match provider {
        MissionProvider::Codex => "Codex",
        MissionProvider::ClaudeCode => "Claude Code",
        MissionProvider::OpenCode => "OpenCode",
        MissionProvider::Acp => "ACP agent",
    }
}
