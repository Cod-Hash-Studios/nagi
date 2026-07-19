use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

use crate::{
    api::schema::{MissionCheckStatusV1, MissionStatus, MissionViewV1},
    app::state::AppState,
    ui::{
        components::{action_bar, progress_steps, section, state_badge},
        design::{icons::IconSet, tokens::UiTokens},
        text::{middle_elide, truncate_end},
        widgets::render_panel_shell_with_border_set,
    },
};

pub(super) fn render_mission_inspector_overlay(app: &AppState, frame: &mut Frame) {
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
        Constraint::Length(4),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    let Some(mission) = selected_mission(app) else {
        frame.render_widget(
            Paragraph::new("Mission no longer exists").style(Style::default().fg(tokens.attention)),
            body,
        );
        action_bar::render(frame, footer, &[("esc", "back")], tokens);
        return;
    };

    render_header(app, frame, header, mission, tokens, icons);
    let lines = inspector_lines(app, mission, body.width, tokens, icons);
    let max_scroll = lines.len().saturating_sub(body.height as usize);
    let scroll = app.mission_inspector_scroll.min(max_scroll);
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0))
            .wrap(Wrap { trim: false }),
        body,
    );
    if app.mission_inspector_tab == 0 {
        action_bar::render(
            frame,
            footer,
            &[
                ("tab", "extensions"),
                ("p", "proof"),
                ("a", "attention"),
                ("h", "handoff"),
                ("j/k", "scroll"),
                ("esc", "cockpit"),
            ],
            tokens,
        );
    } else {
        action_bar::render(
            frame,
            footer,
            &[
                ("tab", "next"),
                ("r", "refresh"),
                ("p", "proof"),
                ("j/k", "scroll"),
                ("esc", "cockpit"),
            ],
            tokens,
        );
    }
}

fn render_header(
    app: &AppState,
    frame: &mut Frame,
    area: Rect,
    mission: &MissionViewV1,
    tokens: UiTokens,
    icons: IconSet,
) {
    if area.is_empty() {
        return;
    }
    let state = mission_badge(mission.status, tokens, icons);
    let title = truncate_end(&mission.title, area.width.saturating_sub(2) as usize);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "MISSION  ",
                    Style::default()
                        .fg(tokens.text_muted)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    title,
                    Style::default()
                        .fg(tokens.text)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            state,
            mission_tabs_line(app, tokens),
            Line::from(Span::styled(
                match icons {
                    IconSet::Unicode => "─".repeat(area.width as usize),
                    IconSet::Ascii => "-".repeat(area.width as usize),
                },
                Style::default().fg(tokens.border),
            )),
        ]),
        area,
    );
}

fn mission_tabs_line(app: &AppState, tokens: UiTokens) -> Line<'static> {
    let tabs = app.mission_plugin_inspector_tabs();
    let selected = app.mission_inspector_tab.min(tabs.len());
    let mut spans = Vec::with_capacity((tabs.len() + 1) * 2);
    for (index, title) in std::iter::once("Overview".to_owned())
        .chain(tabs.into_iter().map(|tab| tab.title))
        .enumerate()
    {
        if index > 0 {
            spans.push(Span::styled("  ", Style::default().fg(tokens.border)));
        }
        let label = format!(" {} ", truncate_end(&title, 20));
        spans.push(Span::styled(
            label,
            if index == selected {
                Style::default()
                    .fg(tokens.panel)
                    .bg(tokens.focus)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(tokens.text_muted)
            },
        ));
    }
    Line::from(spans)
}

fn inspector_lines(
    app: &AppState,
    mission: &MissionViewV1,
    width: u16,
    tokens: UiTokens,
    icons: IconSet,
) -> Vec<Line<'static>> {
    if app.mission_inspector_tab == 0 {
        mission_lines(mission, width, tokens, icons)
    } else {
        plugin_inspector_lines(app, tokens, icons)
    }
}

fn mission_lines(
    mission: &MissionViewV1,
    width: u16,
    tokens: UiTokens,
    icons: IconSet,
) -> Vec<Line<'static>> {
    let fresh = fresh_criteria(mission);
    let provider = mission
        .run
        .as_ref()
        .map(|run| provider_label(run.provider))
        .unwrap_or("not started");
    let worktree = mission
        .run
        .as_ref()
        .map(|run| run.worktree_path.as_str())
        .unwrap_or(&mission.repository_path);
    let mut lines = vec![
        section::heading("Objective", tokens, icons),
        Line::from(Span::styled(
            mission.objective.clone(),
            Style::default().fg(tokens.text),
        )),
        Line::default(),
        Line::from(vec![
            Span::styled("Provider   ", Style::default().fg(tokens.text_muted)),
            Span::styled(provider.to_string(), Style::default().fg(tokens.text)),
        ]),
        Line::from(vec![
            Span::styled("Worktree   ", Style::default().fg(tokens.text_muted)),
            Span::styled(
                middle_elide(worktree, width.saturating_sub(11) as usize),
                Style::default().fg(tokens.text),
            ),
        ]),
        Line::from(vec![
            Span::styled("Attention  ", Style::default().fg(tokens.text_muted)),
            Span::styled(
                mission.unresolved_attention_count.to_string(),
                Style::default().fg(if mission.unresolved_attention_count > 0 {
                    tokens.attention
                } else {
                    tokens.text
                }),
            ),
        ]),
        Line::default(),
        section::heading("Acceptance criteria", tokens, icons),
        progress_steps::line(
            "Fresh proof",
            fresh,
            mission.criteria.len(),
            tokens,
            icons,
            width,
        ),
    ];
    for criterion in &mission.criteria {
        let criterion_fresh = !criterion.required_check_ids.is_empty()
            && criterion.required_check_ids.iter().all(|required_id| {
                mission.checks.iter().any(|check| {
                    check.check_id == *required_id && check.status == MissionCheckStatusV1::Passed
                })
            });
        let (mark, color) = if criterion_fresh {
            (
                if icons == IconSet::Unicode {
                    "✓"
                } else {
                    "OK"
                },
                tokens.proof_fresh,
            )
        } else {
            (
                if icons == IconSet::Unicode {
                    "○"
                } else {
                    "--"
                },
                tokens.proof_stale,
            )
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {mark} "), Style::default().fg(color)),
            Span::styled(
                criterion.description.clone(),
                Style::default().fg(tokens.text),
            ),
        ]));
    }
    lines.push(Line::default());
    lines.push(section::heading("Checks and evidence", tokens, icons));
    if mission.checks.is_empty() {
        lines.push(Line::from(Span::styled(
            "No closure checks configured",
            Style::default().fg(tokens.proof_stale),
        )));
    } else {
        for check in &mission.checks {
            let (label, color) = check_status(check.status, tokens);
            lines.push(Line::from(vec![
                Span::styled(format!(" {label:<9}"), Style::default().fg(color)),
                Span::styled(check.check_id.clone(), Style::default().fg(tokens.text)),
                Span::styled(
                    if check.required {
                        "  required"
                    } else {
                        "  optional"
                    },
                    Style::default().fg(tokens.text_muted),
                ),
            ]));
        }
    }
    if let Some(digest) = &mission.evidence_pack_digest {
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled("Evidence   ", Style::default().fg(tokens.text_muted)),
            Span::styled(
                format!("{}…{}", &digest[..8], &digest[digest.len() - 8..]),
                Style::default().fg(tokens.proof_fresh),
            ),
        ]));
    }
    lines
}

fn plugin_inspector_lines(app: &AppState, tokens: UiTokens, icons: IconSet) -> Vec<Line<'static>> {
    let tabs = app.mission_plugin_inspector_tabs();
    let Some(tab) = app
        .mission_inspector_tab
        .checked_sub(1)
        .and_then(|index| tabs.get(index))
    else {
        return vec![Line::from(Span::styled(
            "Plugin inspector tab is no longer available",
            Style::default().fg(tokens.attention),
        ))];
    };
    let mut lines = vec![
        section::heading(&tab.title, tokens, icons),
        Line::from(vec![
            Span::styled("Plugin   ", Style::default().fg(tokens.text_muted)),
            Span::styled(tab.plugin_id.clone(), Style::default().fg(tokens.text)),
        ]),
        Line::default(),
    ];
    if app.plugin_inspector_active_key.as_deref() != Some(tab.key().as_str()) {
        lines.push(Line::from(Span::styled(
            "Press r to load this contribution",
            Style::default().fg(tokens.text_muted),
        )));
        return lines;
    }
    if app.plugin_inspector_log_id.is_some() {
        lines.push(Line::from(vec![
            Span::styled(
                if icons == IconSet::Unicode {
                    "◌ "
                } else {
                    ".. "
                },
                Style::default().fg(tokens.working),
            ),
            Span::styled(
                "Refreshing structured plugin data…",
                Style::default().fg(tokens.text),
            ),
        ]));
        return lines;
    }
    if let Some(error) = &app.plugin_inspector_error {
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(tokens.attention),
        )));
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "Press r to retry",
            Style::default().fg(tokens.text_muted),
        )));
        return lines;
    }
    let Some(document) = &app.plugin_inspector_document else {
        lines.push(Line::from(Span::styled(
            "Press r to load this contribution",
            Style::default().fg(tokens.text_muted),
        )));
        return lines;
    };
    if let Some(summary) = &document.summary {
        lines.push(Line::from(Span::styled(
            summary.clone(),
            Style::default()
                .fg(tokens.text)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());
    }
    for block in &document.blocks {
        render_plugin_block(&mut lines, block, tokens, icons);
        lines.push(Line::default());
    }
    lines
}

fn render_plugin_block(
    lines: &mut Vec<Line<'static>>,
    block: &crate::api::schema::PluginUiBlockV1,
    tokens: UiTokens,
    icons: IconSet,
) {
    use crate::api::schema::PluginUiBlockV1;
    match block {
        PluginUiBlockV1::Section { title, rows } => {
            lines.push(section::heading(title, tokens, icons));
            for row in rows {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{:<16}", row.label),
                        Style::default().fg(tokens.text_muted),
                    ),
                    Span::styled(row.value.clone(), plugin_tone_style(row.tone, tokens)),
                ]));
            }
        }
        PluginUiBlockV1::Metrics { items } => {
            lines.push(section::heading("Metrics", tokens, icons));
            for item in items {
                let mut spans = vec![
                    Span::styled(
                        format!("{:<16}", item.label),
                        Style::default().fg(tokens.text_muted),
                    ),
                    Span::styled(
                        item.value.clone(),
                        plugin_tone_style(item.tone, tokens).add_modifier(Modifier::BOLD),
                    ),
                ];
                if let Some(detail) = &item.detail {
                    spans.push(Span::styled(
                        format!("  {detail}"),
                        Style::default().fg(tokens.text_muted),
                    ));
                }
                lines.push(Line::from(spans));
            }
        }
        PluginUiBlockV1::List { title, items } => {
            lines.push(section::heading(
                title.as_deref().unwrap_or("Details"),
                tokens,
                icons,
            ));
            for item in items {
                lines.push(Line::from(vec![
                    Span::styled("• ", plugin_tone_style(item.tone, tokens)),
                    Span::styled(item.title.clone(), Style::default().fg(tokens.text)),
                    Span::styled(
                        item.detail
                            .as_deref()
                            .map(|detail| format!("  {detail}"))
                            .unwrap_or_default(),
                        Style::default().fg(tokens.text_muted),
                    ),
                ]));
            }
        }
        PluginUiBlockV1::Notice {
            tone, title, body, ..
        } => {
            lines.push(section::heading(
                title.as_deref().unwrap_or("Notice"),
                tokens,
                icons,
            ));
            lines.push(Line::from(Span::styled(
                body.clone(),
                plugin_tone_style(*tone, tokens),
            )));
        }
    }
}

fn plugin_tone_style(tone: crate::api::schema::PluginUiToneV1, tokens: UiTokens) -> Style {
    use crate::api::schema::PluginUiToneV1;
    Style::default().fg(match tone {
        PluginUiToneV1::Neutral => tokens.text,
        PluginUiToneV1::Success => tokens.proof_fresh,
        PluginUiToneV1::Warning => tokens.proof_stale,
        PluginUiToneV1::Danger => tokens.attention,
    })
}

fn selected_mission(app: &AppState) -> Option<&MissionViewV1> {
    let selected = app.selected_mission_id.as_deref()?;
    app.mission_views
        .iter()
        .find(|mission| mission.mission_id == selected)
}

pub(crate) fn mission_inspector_max_scroll(app: &AppState, body_height: u16) -> usize {
    selected_mission(app)
        .map(|mission| {
            inspector_lines(
                app,
                mission,
                app.navigator_inner_rect().width,
                UiTokens::from(&app.palette),
                IconSet::from(app.icon_style),
            )
            .len()
            .saturating_sub(body_height as usize)
        })
        .unwrap_or(0)
}

fn fresh_criteria(mission: &MissionViewV1) -> usize {
    mission
        .criteria
        .iter()
        .filter(|criterion| {
            !criterion.required_check_ids.is_empty()
                && criterion.required_check_ids.iter().all(|required_id| {
                    mission.checks.iter().any(|check| {
                        check.check_id == *required_id
                            && check.status == MissionCheckStatusV1::Passed
                    })
                })
        })
        .count()
}

fn provider_label(provider: crate::api::schema::MissionProvider) -> &'static str {
    match provider {
        crate::api::schema::MissionProvider::Codex => "Codex",
        crate::api::schema::MissionProvider::ClaudeCode => "Claude Code",
        crate::api::schema::MissionProvider::OpenCode => "OpenCode",
        crate::api::schema::MissionProvider::Acp => "ACP agent",
    }
}

fn mission_badge(status: MissionStatus, tokens: UiTokens, icons: IconSet) -> Line<'static> {
    use state_badge::StateBadgeKind;
    match status {
        MissionStatus::Preparing | MissionStatus::Active => {
            state_badge::line(StateBadgeKind::Working, tokens, icons)
        }
        MissionStatus::ReadyToClose | MissionStatus::Archived => {
            state_badge::line(StateBadgeKind::ProofFresh, tokens, icons)
        }
        MissionStatus::Draft => state_badge::line(StateBadgeKind::ProofStale, tokens, icons),
        MissionStatus::ReviewRequired | MissionStatus::Blocked | MissionStatus::Failed => {
            state_badge::line(StateBadgeKind::Attention, tokens, icons)
        }
    }
}

fn check_status(
    status: MissionCheckStatusV1,
    tokens: UiTokens,
) -> (&'static str, ratatui::style::Color) {
    match status {
        MissionCheckStatusV1::Passed => ("PASS", tokens.proof_fresh),
        MissionCheckStatusV1::Failed => ("FAIL", tokens.attention),
        MissionCheckStatusV1::Stale => ("STALE", tokens.proof_stale),
        MissionCheckStatusV1::Missing => ("MISSING", tokens.proof_stale),
        MissionCheckStatusV1::ManualMissing => ("REVIEW", tokens.proof_stale),
        MissionCheckStatusV1::ProviderClaimOnly => ("CLAIM", tokens.proof_stale),
        MissionCheckStatusV1::DeclarationMismatch => ("CONTRACT", tokens.attention),
        MissionCheckStatusV1::IdentityMismatch => ("IDENTITY", tokens.attention),
        MissionCheckStatusV1::ArtifactMissingOrChanged => ("ARTIFACT", tokens.attention),
        MissionCheckStatusV1::ManualNotAuthorized => ("REVIEWER", tokens.attention),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;

    #[test]
    fn missing_mission_is_a_safe_recoverable_screen() {
        let mut app = AppState::test_new();
        app.selected_mission_id = Some("gone".into());
        app.view.sidebar_rect = Rect::new(0, 0, 20, 24);
        app.view.terminal_area = Rect::new(20, 0, 60, 24);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

        terminal
            .draw(|frame| render_mission_inspector_overlay(&app, frame))
            .unwrap();

        let output = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(output.contains("Mission no longer exists"));
        assert!(output.contains("back"));
    }

    #[test]
    fn renders_theme_safe_plugin_inspector_documents() {
        let mut app = AppState::test_new();
        app.mission_views = vec![serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/api/mission-view-v1.json"
        )))
        .unwrap()];
        app.selected_mission_id = Some(app.mission_views[0].mission_id.clone());
        app.installed_plugins.clear();
        app.installed_plugins.insert(
            "example.review".into(),
            crate::api::schema::InstalledPluginInfo {
                manifest_version: 2,
                plugin_id: "example.review".into(),
                name: "Review".into(),
                version: "1.0.0".into(),
                min_nagi_version: crate::build_info::BASE_VERSION.into(),
                description: None,
                manifest_path: "/tmp/review/nagi-plugin.toml".into(),
                plugin_root: "/tmp/review".into(),
                enabled: true,
                runtime: crate::api::schema::PluginRuntimeV2::WasiComponent,
                entrypoint: Some("/tmp/review/plugin.wasm".into()),
                requested_capabilities: vec!["mission.read".into()],
                native_trusted: false,
                platforms: None,
                build: Vec::new(),
                actions: vec![crate::api::schema::PluginManifestAction {
                    id: "review-current".into(),
                    title: "Review current mission".into(),
                    description: None,
                    contexts: vec![crate::api::schema::PluginActionContext::Mission],
                    platforms: None,
                    command: Vec::new(),
                }],
                events: Vec::new(),
                panes: Vec::new(),
                link_handlers: Vec::new(),
                inspector_tabs: vec![crate::api::schema::PluginInspectorTabContributionV2 {
                    id: "risk".into(),
                    title: "Risk".into(),
                    source: "review-current".into(),
                }],
                source: Default::default(),
                warnings: Vec::new(),
            },
        );
        app.mission_inspector_tab = 1;
        app.plugin_inspector_active_key = Some("example.review:risk".into());
        app.plugin_inspector_document = Some(
            serde_json::from_str(
                r#"{"schema_version":1,"summary":"Review ready","blocks":[{"type":"section","title":"Checks","rows":[{"label":"CI","value":"Passing","tone":"success"}]}]}"#,
            )
            .unwrap(),
        );
        app.view.sidebar_rect = Rect::new(0, 0, 24, 32);
        app.view.terminal_area = Rect::new(24, 0, 96, 32);
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();

        terminal
            .draw(|frame| render_mission_inspector_overlay(&app, frame))
            .unwrap();

        let output = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for expected in [
            "Overview",
            "Risk",
            "example.review",
            "Review ready",
            "Passing",
        ] {
            assert!(output.contains(expected), "missing {expected}: {output}");
        }
    }
}
