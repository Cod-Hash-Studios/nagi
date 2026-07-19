use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
    Frame,
};

use crate::{
    api::schema::{
        AttentionDeliveryStateV1, AttentionItemV1, AttentionKindV1, AttentionResponseCapabilityV1,
        AttentionRiskV1, AttentionStateV1,
    },
    app::state::AppState,
    ui::{
        components::{action_bar, empty_state, focus_rail, section},
        design::{icons::IconSet, tokens::UiTokens},
        text::{middle_elide, truncate_end},
        widgets::render_panel_shell_with_border_set,
    },
};

pub(super) fn render_attention_inbox_overlay(app: &AppState, frame: &mut Frame) {
    super::dim_background(frame, frame.area());
    let popup = app.navigator_popup_rect();
    let tokens = UiTokens::from(&app.palette);
    let icons = IconSet::from(app.icon_style);
    let Some(inner) = render_panel_shell_with_border_set(
        frame,
        popup,
        tokens.attention,
        tokens.panel,
        icons.border_set(app.theme_components.border),
    ) else {
        return;
    };
    let [header, content, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    render_header(app, frame, header, tokens, icons);
    if app.attention_items.is_empty() {
        empty_state::render(
            frame,
            content,
            "Nothing needs you",
            Some(("esc", "return to mission")),
            tokens,
        );
    } else if content.width >= 92 {
        let [list, detail] =
            Layout::horizontal([Constraint::Percentage(43), Constraint::Percentage(57)])
                .areas(content);
        render_list(app, frame, list, tokens, icons);
        render_detail(app, frame, detail, tokens, icons);
    } else if content.height >= 14 {
        let [list, detail] =
            Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)])
                .areas(content);
        render_list(app, frame, list, tokens, icons);
        render_detail(app, frame, detail, tokens, icons);
    } else {
        render_list(app, frame, content, tokens, icons);
    }
    render_footer(app, frame, footer, tokens);
}

fn render_header(app: &AppState, frame: &mut Frame, area: Rect, tokens: UiTokens, icons: IconSet) {
    let open = app
        .attention_items
        .iter()
        .filter(|item| matches!(item.state, AttentionStateV1::Open))
        .count();
    let uncertain = app
        .attention_items
        .iter()
        .filter(|item| matches!(item.state, AttentionStateV1::ReconciliationRequired { .. }))
        .count();
    let rule = if icons == IconSet::Unicode {
        "─"
    } else {
        "-"
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "ATTENTION INBOX",
                    Style::default()
                        .fg(tokens.text)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {open} open · {uncertain} delivery uncertain"),
                    Style::default().fg(tokens.text_muted),
                ),
            ]),
            Line::from(Span::styled(
                "One place for provider questions and scoped consent",
                Style::default().fg(tokens.text_muted),
            )),
            Line::from(Span::styled(
                rule.repeat(area.width as usize),
                Style::default().fg(tokens.border),
            )),
        ]),
        area,
    );
}

fn render_list(app: &AppState, frame: &mut Frame, area: Rect, tokens: UiTokens, icons: IconSet) {
    if area.is_empty() {
        return;
    }
    let selected = app
        .attention_selected
        .min(app.attention_items.len().saturating_sub(1));
    let row_height = if area.height >= 10 { 2 } else { 1 };
    let capacity = (area.height / row_height).max(1) as usize;
    let start = app
        .attention_scroll
        .min(app.attention_items.len().saturating_sub(capacity));
    for (visible, item) in app
        .attention_items
        .iter()
        .skip(start)
        .take(capacity)
        .enumerate()
    {
        let index = start + visible;
        let rect = Rect::new(
            area.x,
            area.y + visible as u16 * row_height,
            area.width,
            row_height,
        );
        render_row(
            frame,
            rect,
            item,
            index == selected,
            tokens,
            icons,
            app.theme_components.selection,
        );
    }
}

fn render_row(
    frame: &mut Frame,
    area: Rect,
    item: &AttentionItemV1,
    selected: bool,
    tokens: UiTokens,
    icons: IconSet,
    selection: crate::theme::manifest::ThemeSelectionStyle,
) {
    frame.render_widget(Clear, area);
    let (risk, risk_color) = risk_label(item.risk, tokens);
    let state = state_label(&item.state);
    let first = Line::from(vec![
        focus_rail::span(selected, tokens, icons, selection),
        Span::raw(" "),
        Span::styled(
            format!("{risk:<8} "),
            Style::default().fg(risk_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            truncate_end(
                &item.requested_action,
                area.width.saturating_sub(19) as usize,
            ),
            Style::default().fg(tokens.text).add_modifier(if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        ),
    ]);
    let mut lines = vec![first];
    if area.height > 1 {
        lines.push(Line::from(vec![
            Span::raw("   "),
            Span::styled(
                format!(
                    "{} · {} · {state}",
                    item.mission_id,
                    provider_label(item.provider)
                ),
                Style::default().fg(tokens.text_muted),
            ),
        ]));
    }
    frame.render_widget(
        Paragraph::new(lines).style(focus_rail::row_style(selected, tokens, selection)),
        area,
    );
}

fn render_detail(app: &AppState, frame: &mut Frame, area: Rect, tokens: UiTokens, icons: IconSet) {
    let Some(item) = app.attention_items.get(app.attention_selected) else {
        return;
    };
    let divider = if icons == IconSet::Unicode {
        "│"
    } else {
        "|"
    };
    for row in area.y..area.y.saturating_add(area.height) {
        frame.render_widget(
            Paragraph::new(divider).style(Style::default().fg(tokens.border)),
            Rect::new(area.x, row, 1, 1),
        );
    }
    if area.width < 4 {
        return;
    }
    let body = Rect::new(area.x + 2, area.y, area.width - 2, area.height);
    let (risk, risk_color) = risk_label(item.risk, tokens);
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                kind_label(item.kind),
                Style::default()
                    .fg(tokens.text_muted)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {risk}"),
                Style::default().fg(risk_color).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            item.requested_action.clone(),
            Style::default()
                .fg(tokens.text)
                .add_modifier(Modifier::BOLD),
        )),
        Line::default(),
        section::heading("Why and scope", tokens, icons),
        Line::from(Span::styled(
            item.scope.clone(),
            Style::default().fg(tokens.text),
        )),
        Line::default(),
        Line::from(vec![
            Span::styled("Mission    ", Style::default().fg(tokens.text_muted)),
            Span::styled(item.mission_id.clone(), Style::default().fg(tokens.text)),
        ]),
        Line::from(vec![
            Span::styled("Provider   ", Style::default().fg(tokens.text_muted)),
            Span::styled(
                provider_label(item.provider),
                Style::default().fg(tokens.text),
            ),
        ]),
        Line::from(vec![
            Span::styled("State      ", Style::default().fg(tokens.text_muted)),
            Span::styled(
                state_label(&item.state),
                Style::default().fg(state_color(&item.state, tokens)),
            ),
        ]),
        Line::from(vec![
            Span::styled("Delivery   ", Style::default().fg(tokens.text_muted)),
            Span::styled(
                delivery_label(&item.delivery),
                Style::default().fg(tokens.text),
            ),
        ]),
        Line::from(vec![
            Span::styled("Route      ", Style::default().fg(tokens.text_muted)),
            Span::styled(
                if item.response_capability == AttentionResponseCapabilityV1::Reliable {
                    "managed response"
                } else {
                    "open originating pane"
                },
                Style::default().fg(tokens.text),
            ),
        ]),
    ];
    if let Some(error) = &app.attention_error {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(tokens.attention),
        )));
    }
    if let Some(answer) = &app.attention_answer_input {
        let question = item.questions.get(answer.question_index);
        lines.push(Line::default());
        lines.push(section::heading(
            &question
                .map(|question| {
                    format!(
                        "{} · {} / {}",
                        question.header,
                        answer.question_index + 1,
                        item.questions.len()
                    )
                })
                .unwrap_or_else(|| "Your answer".to_owned()),
            tokens,
            icons,
        ));
        if let Some(question) = question {
            lines.push(Line::from(Span::styled(
                question.prompt.clone(),
                Style::default().fg(tokens.text),
            )));
            for option in &question.options {
                lines.push(Line::from(vec![
                    Span::styled("  · ", Style::default().fg(tokens.focus)),
                    Span::styled(
                        option.label.clone(),
                        Style::default()
                            .fg(tokens.text)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if option.description.is_empty() {
                            String::new()
                        } else {
                            format!("  {}", option.description)
                        },
                        Style::default().fg(tokens.text_muted),
                    ),
                ]));
            }
            if question.multiple {
                lines.push(Line::from(Span::styled(
                    "Separate multiple choices with commas",
                    Style::default().fg(tokens.text_muted),
                )));
            }
        }
        lines.push(Line::from(vec![
            Span::styled("> ", Style::default().fg(tokens.focus)),
            Span::styled(
                middle_elide(&answer.input, body.width.saturating_sub(2) as usize),
                Style::default().fg(tokens.text),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            if answer.question_index + 1 < item.questions.len() {
                "Enter continues · Esc cancels"
            } else {
                "Enter sends · Esc cancels"
            },
            Style::default().fg(tokens.text_muted),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
}

fn render_footer(app: &AppState, frame: &mut Frame, area: Rect, tokens: UiTokens) {
    let hints = app
        .attention_items
        .get(app.attention_selected)
        .map(
            |item| match (&item.state, item.kind, item.response_capability, item.risk) {
                (_, _, _, _) if app.attention_answer_input.is_some() => {
                    vec![("enter", "send"), ("esc", "cancel")]
                }
                (
                    AttentionStateV1::Open,
                    AttentionKindV1::ProviderQuestion,
                    AttentionResponseCapabilityV1::Reliable,
                    _,
                ) => vec![("r", "answer"), ("j/k", "move"), ("esc", "mission")],
                (
                    AttentionStateV1::Open,
                    _,
                    AttentionResponseCapabilityV1::Reliable,
                    AttentionRiskV1::Critical,
                ) => vec![("y", "approve once"), ("n", "deny"), ("esc", "mission")],
                (AttentionStateV1::Open, _, AttentionResponseCapabilityV1::Reliable, _) => vec![
                    ("y", "approve once"),
                    ("s", "session"),
                    ("n", "deny"),
                    ("esc", "mission"),
                ],
                _ => vec![("j/k", "move"), ("esc", "mission")],
            },
        )
        .unwrap_or_else(|| vec![("esc", "mission")]);
    action_bar::render(frame, area, &hints, tokens);
}

fn risk_label(risk: AttentionRiskV1, tokens: UiTokens) -> (&'static str, ratatui::style::Color) {
    match risk {
        AttentionRiskV1::Low => ("LOW", tokens.text_muted),
        AttentionRiskV1::Medium => ("MEDIUM", tokens.proof_stale),
        AttentionRiskV1::High => ("HIGH", tokens.attention),
        AttentionRiskV1::Critical => ("CRITICAL", tokens.attention),
    }
}

fn kind_label(kind: AttentionKindV1) -> &'static str {
    match kind {
        AttentionKindV1::PermissionRequest => "PERMISSION REQUEST",
        AttentionKindV1::ProviderQuestion => "PROVIDER QUESTION",
        AttentionKindV1::CommandFailed => "COMMAND FAILED",
        AttentionKindV1::WorktreeConflict => "WORKTREE CONFLICT",
        AttentionKindV1::TurnComplete => "TURN COMPLETE",
        AttentionKindV1::Disconnected => "DISCONNECTED",
        AttentionKindV1::SecurityWarning => "SECURITY WARNING",
        AttentionKindV1::ManualVerification => "MANUAL VERIFICATION",
    }
}

fn provider_label(provider: crate::api::schema::MissionProvider) -> &'static str {
    match provider {
        crate::api::schema::MissionProvider::Codex => "Codex",
        crate::api::schema::MissionProvider::ClaudeCode => "Claude Code",
        crate::api::schema::MissionProvider::OpenCode => "OpenCode",
        crate::api::schema::MissionProvider::Acp => "ACP agent",
    }
}

fn state_label(state: &AttentionStateV1) -> &'static str {
    match state {
        AttentionStateV1::Open => "open",
        AttentionStateV1::PendingResponse { .. } => "response pending",
        AttentionStateV1::Resolved { .. } => "resolved",
        AttentionStateV1::ReconciliationRequired { .. } => "delivery uncertain",
        AttentionStateV1::Dismissed { .. } => "dismissed",
        AttentionStateV1::Expired { .. } => "expired",
    }
}

fn state_color(state: &AttentionStateV1, tokens: UiTokens) -> ratatui::style::Color {
    match state {
        AttentionStateV1::Open | AttentionStateV1::ReconciliationRequired { .. } => {
            tokens.attention
        }
        AttentionStateV1::PendingResponse { .. } => tokens.working,
        AttentionStateV1::Resolved { .. } => tokens.proof_fresh,
        AttentionStateV1::Dismissed { .. } | AttentionStateV1::Expired { .. } => tokens.text_muted,
    }
}

fn delivery_label(delivery: &AttentionDeliveryStateV1) -> &'static str {
    match delivery {
        AttentionDeliveryStateV1::NotRequested => "not requested",
        AttentionDeliveryStateV1::Pending { .. } => "pending",
        AttentionDeliveryStateV1::Acknowledged { .. } => "acknowledged",
        AttentionDeliveryStateV1::DefinitelyNotApplied { .. } => "not applied",
        AttentionDeliveryStateV1::DeliveryUnknown { .. } => "unknown, reconcile",
        AttentionDeliveryStateV1::NotApplicable => "not applicable",
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;

    fn critical_item() -> AttentionItemV1 {
        AttentionItemV1 {
            schema_version: crate::api::schema::ContractVersionV1,
            attention_id: "attention-1".into(),
            mission_id: "mission-1".into(),
            run_id: "run-1".into(),
            session_id: "session-1".into(),
            pane: crate::api::schema::AttentionPaneTargetV1 {
                workspace_id: "workspace-1".into(),
                pane_id: "pane-1".into(),
            },
            kind: AttentionKindV1::PermissionRequest,
            requested_action: "Allow unrestricted filesystem access".into(),
            scope: "All files in the current worktree".into(),
            risk: AttentionRiskV1::Critical,
            provider: crate::api::schema::MissionProvider::OpenCode,
            source: crate::api::schema::AttentionSourceV1::ProviderApi,
            response_capability: AttentionResponseCapabilityV1::Reliable,
            questions: Vec::new(),
            created_at_millis: 1,
            expires_at_millis: None,
            occurrence_count: 1,
            unread: true,
            state: AttentionStateV1::Open,
            delivery: AttentionDeliveryStateV1::NotRequested,
        }
    }

    #[test]
    fn critical_request_is_explicit_and_hides_session_approval() {
        let mut app = AppState::test_new();
        app.attention_items.push(critical_item());
        app.view.sidebar_rect = Rect::new(0, 0, 20, 24);
        app.view.terminal_area = Rect::new(20, 0, 60, 24);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

        terminal
            .draw(|frame| render_attention_inbox_overlay(&app, frame))
            .unwrap();

        let output = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(output.contains("CRITICAL"));
        assert!(output.contains("CRITICAL Allow"));
        assert!(output.contains("approve once"));
        assert!(output.contains("deny"));
        assert!(!output.contains("session"));
    }
}
