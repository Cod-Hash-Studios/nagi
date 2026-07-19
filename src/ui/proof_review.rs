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
        text::truncate_end,
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
    let detail_height = area.height.min(6);
    let visible_height = area.height.saturating_sub(detail_height) as usize;
    let start = app
        .proof_review_scroll
        .min(mission.checks.len().saturating_sub(visible_height.max(1)));
    let [checks_area, detail_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(detail_height)]).areas(area);
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
                focus_rail::span(selected_row, tokens, icons, app.theme_components.selection),
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
            ]))
            .style(focus_rail::row_style(
                selected_row,
                tokens,
                app.theme_components.selection,
            )),
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
            Span::styled("Command log  ", Style::default().fg(tokens.text_muted)),
            Span::styled(
                command_log_summary(evidence.exit_code, evidence.duration_millis),
                Style::default().fg(if evidence.exit_code == Some(0) {
                    tokens.proof_fresh
                } else {
                    tokens.text
                }),
            ),
        ]));
        details.push(Line::from(vec![
            Span::styled("Artifacts    ", Style::default().fg(tokens.text_muted)),
            Span::styled(
                format!("{} captured", evidence.artifact_count),
                Style::default().fg(tokens.text),
            ),
        ]));
        details.push(Line::from(vec![
            Span::styled("Recorded     ", Style::default().fg(tokens.text_muted)),
            Span::styled(
                format_recorded_at(evidence.recorded_at_millis),
                Style::default().fg(tokens.text),
            ),
        ]));
        details.push(Line::from(vec![
            Span::styled("Workspace    ", Style::default().fg(tokens.text_muted)),
            Span::styled(
                short_digest(&evidence.workspace_digest),
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
            Span::styled("Proof pack   ", Style::default().fg(tokens.text_muted)),
            Span::styled(
                short_digest(digest),
                Style::default().fg(tokens.proof_fresh),
            ),
        ]));
    }
    frame.render_widget(Paragraph::new(details), detail_area);
}

fn command_log_summary(exit_code: Option<i32>, duration_millis: Option<u64>) -> String {
    let outcome =
        exit_code.map_or_else(|| "no exit code".to_owned(), |code| format!("exit {code}"));
    duration_millis.map_or(outcome.clone(), |duration| {
        format!("{outcome} · {}", format_duration(duration))
    })
}

fn format_duration(duration_millis: u64) -> String {
    if duration_millis < 1_000 {
        return format!("{duration_millis} ms");
    }
    let seconds = duration_millis as f64 / 1_000.0;
    let rendered = format!("{seconds:.2}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned();
    format!("{rendered} s")
}

fn short_digest(digest: &str) -> String {
    if digest.len() <= 17 {
        return digest.to_owned();
    }
    format!("{}…{}", &digest[..8], &digest[digest.len() - 8..])
}

fn format_recorded_at(recorded_at_millis: u64) -> String {
    let total_seconds = recorded_at_millis / 1_000;
    let days = (total_seconds / 86_400).min(i64::MAX as u64) as i64;
    let seconds_of_day = total_seconds % 86_400;
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let (year, month, day) = civil_date_from_unix_days(days);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC")
}

fn civil_date_from_unix_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
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

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;

    #[test]
    fn proof_review_renders_compact_human_evidence_details() {
        let mut mission: MissionViewV1 = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/api/mission-view-v1.json"
        )))
        .unwrap();
        mission.evidence_pack_digest = Some("d".repeat(64));
        mission.evidence[0].workspace_digest =
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into();
        mission.evidence[0].recorded_at_millis = 1_704_067_200_000;
        mission.evidence[0].duration_millis = Some(1_240);
        mission.evidence[0].artifact_count = 2;

        let mut app = AppState::test_new();
        app.selected_mission_id = Some(mission.mission_id.clone());
        app.mission_views.push(mission);
        crate::ui::compute_view(&mut app, Rect::new(0, 0, 120, 32));
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal
            .draw(|frame| render_proof_review_overlay(&app, frame))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let output = (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(output.contains("Command log  exit 0 · 1.24 s"), "{output}");
        assert!(output.contains("Artifacts    2 captured"), "{output}");
        assert!(output.contains("2024-01-01 00:00 UTC"), "{output}");
        assert!(output.contains("01234567…89abcdef"), "{output}");
        assert!(output.contains("dddddddd…dddddddd"), "{output}");
        assert!(!output.contains("1704067200000"), "{output}");
        assert!(!output.contains(&"d".repeat(32)), "{output}");
    }
}
