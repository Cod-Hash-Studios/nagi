use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::{
    api::schema::{MissionCheckStatusV1, MissionStatus, MissionViewV1},
    app::state::AppState,
    ui::{
        components::{action_bar, empty_state, focus_rail, section},
        design::{icons::IconSet, tokens::UiTokens},
        text::{middle_elide, truncate_end},
        widgets::render_panel_shell_with_border_set,
    },
};

pub(super) fn render_proof_review_overlay(app: &AppState, frame: &mut Frame) {
    super::dim_background(frame, frame.area());
    let popup = app.navigator_popup_rect();
    let tokens = UiTokens::from(&app.palette);
    let icons = IconSet::from(app.icon_style);
    let Some(inner) = render_panel_shell_with_border_set(
        frame,
        popup,
        tokens.focus,
        tokens.panel,
        icons.border_set(),
    ) else {
        return;
    };
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    let Some(mission) = selected_mission(app) else {
        empty_state::render(
            frame,
            body,
            "Mission no longer exists",
            Some(("esc", "back")),
            tokens,
        );
        return;
    };
    render_header(frame, header, app, mission, tokens, icons);
    render_checks(frame, body, app, mission, tokens, icons);
    let actions = if mission.status == MissionStatus::ReadyToClose {
        vec![
            ("j/k", "select"),
            ("c", "recheck + close"),
            ("esc", "mission"),
        ]
    } else {
        vec![("j/k", "select"), ("esc", "mission")]
    };
    action_bar::render(frame, footer, &actions, tokens);
}

fn render_header(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    mission: &MissionViewV1,
    tokens: UiTokens,
    icons: IconSet,
) {
    let ready = mission.status == MissionStatus::ReadyToClose
        && mission
            .checks
            .iter()
            .filter(|check| check.required)
            .all(|check| check.status == MissionCheckStatusV1::Passed)
        && mission.unresolved_attention_count == 0;
    let state_label = if ready {
        "READY TO CLOSE"
    } else {
        "CLOSE BLOCKED"
    };
    let state_color = if ready {
        tokens.proof_fresh
    } else {
        tokens.attention
    };
    let rule = if icons == IconSet::Unicode {
        "─"
    } else {
        "-"
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "PROOF REVIEW  ",
                    Style::default()
                        .fg(tokens.text_muted)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    truncate_end(&mission.title, area.width.saturating_sub(16) as usize),
                    Style::default()
                        .fg(tokens.text)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled(
                    state_label,
                    Style::default()
                        .fg(state_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "  {} required check(s) · {} unresolved attention",
                        mission.checks.iter().filter(|check| check.required).count(),
                        mission.unresolved_attention_count
                    ),
                    Style::default().fg(tokens.text_muted),
                ),
            ]),
            app.mission_action_error.as_ref().map_or_else(
                || {
                    Line::from(Span::styled(
                        rule.repeat(area.width as usize),
                        Style::default().fg(tokens.border),
                    ))
                },
                |error| {
                    Line::from(Span::styled(
                        truncate_end(error, area.width as usize),
                        Style::default().fg(tokens.attention),
                    ))
                },
            ),
        ]),
        area,
    );
}

fn render_checks(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    mission: &MissionViewV1,
    tokens: UiTokens,
    icons: IconSet,
) {
    if area.is_empty() {
        return;
    }
    if mission.checks.is_empty() {
        empty_state::render(
            frame,
            area,
            "No closure checks configured",
            Some(("esc", "return to mission")),
            tokens,
        );
        return;
    }
    let selected = app.proof_review_selected.min(mission.checks.len() - 1);
    let visible_height = area.height.saturating_sub(5) as usize;
    let start = app
        .proof_review_scroll
        .min(mission.checks.len().saturating_sub(visible_height.max(1)));
    let [checks_area, detail_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(area.height.min(4))]).areas(area);
    for (visible, check) in mission
        .checks
        .iter()
        .skip(start)
        .take(checks_area.height as usize)
        .enumerate()
    {
        let index = start + visible;
        let row = Rect::new(
            checks_area.x,
            checks_area.y + visible as u16,
            checks_area.width,
            1,
        );
        let selected_row = index == selected;
        let (label, color) = check_status(check.status, tokens);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                focus_rail::span(selected_row, tokens, icons),
                Span::raw(" "),
                Span::styled(format!("{label:<10}"), Style::default().fg(color)),
                Span::styled(
                    truncate_end(&check.check_id, row.width.saturating_sub(24) as usize),
                    Style::default()
                        .fg(tokens.text)
                        .add_modifier(if selected_row {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::styled(
                    if check.required {
                        "  required"
                    } else {
                        "  optional"
                    },
                    Style::default().fg(tokens.text_muted),
                ),
            ])),
            row,
        );
    }

    let check = &mission.checks[selected];
    let evidence = mission
        .evidence
        .iter()
        .find(|evidence| evidence.check_id == check.check_id);
    let mut details = vec![section::heading("Selected evidence", tokens, icons)];
    if let Some(evidence) = evidence {
        details.push(Line::from(vec![
            Span::styled("Workspace  ", Style::default().fg(tokens.text_muted)),
            Span::styled(
                middle_elide(
                    &evidence.workspace_digest,
                    detail_area.width.saturating_sub(12) as usize,
                ),
                Style::default().fg(tokens.text),
            ),
        ]));
        details.push(Line::from(vec![
            Span::styled("Artifacts  ", Style::default().fg(tokens.text_muted)),
            Span::styled(
                evidence.artifact_count.to_string(),
                Style::default().fg(tokens.text),
            ),
            Span::styled("   Recorded  ", Style::default().fg(tokens.text_muted)),
            Span::styled(
                evidence.recorded_at_millis.to_string(),
                Style::default().fg(tokens.text),
            ),
        ]));
    } else {
        details.push(Line::from(Span::styled(
            "No evidence record. This check cannot satisfy closure.",
            Style::default().fg(tokens.proof_stale),
        )));
    }
    if let Some(digest) = &mission.evidence_pack_digest {
        details.push(Line::from(vec![
            Span::styled("Pack       ", Style::default().fg(tokens.text_muted)),
            Span::styled(
                middle_elide(digest, detail_area.width.saturating_sub(12) as usize),
                Style::default().fg(tokens.proof_fresh),
            ),
        ]));
    }
    frame.render_widget(Paragraph::new(details), detail_area);
}

fn selected_mission(app: &AppState) -> Option<&MissionViewV1> {
    let selected = app.selected_mission_id.as_deref()?;
    app.mission_views
        .iter()
        .find(|mission| mission.mission_id == selected)
}

pub(crate) fn proof_check_count(app: &AppState) -> usize {
    selected_mission(app).map_or(0, |mission| mission.checks.len())
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
