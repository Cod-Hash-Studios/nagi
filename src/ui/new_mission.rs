use ratatui::{
    layout::{Constraint, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

use crate::{
    app::state::{AppState, NewMissionStep},
    project_recipe::RecipeConfidence,
    ui::{
        components::{action_bar, section},
        design::{icons::IconSet, tokens::UiTokens},
        text::{middle_elide, truncate_end},
        widgets::render_panel_shell_with_border_set,
    },
};

pub(super) fn render_new_mission_overlay(app: &AppState, frame: &mut Frame) {
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
    let Some(draft) = app.new_mission.as_ref() else {
        return;
    };
    let step = step_index(draft.step);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "NEW MISSION  ",
                    Style::default()
                        .fg(tokens.text_muted)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    step_title(draft.step),
                    Style::default()
                        .fg(tokens.text)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled(
                    format!("{}  ", step_marks(step, icons)),
                    Style::default().fg(tokens.focus),
                ),
                Span::styled(
                    format!("step {step}/5  "),
                    Style::default().fg(tokens.text_muted),
                ),
                Span::styled(
                    middle_elide(
                        &draft.repository_path.to_string_lossy(),
                        header.width.saturating_sub(22) as usize,
                    ),
                    Style::default().fg(tokens.text_muted),
                ),
            ]),
            Line::from(Span::styled(
                "─".repeat(header.width as usize),
                Style::default().fg(tokens.border),
            )),
        ]),
        header,
    );

    let mut lines = body_lines(app, body.width, tokens, icons);
    if let Some(error) = &draft.error {
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled("! ", Style::default().fg(tokens.attention)),
            Span::styled(
                truncate_end(error, body.width.saturating_sub(2) as usize),
                Style::default().fg(tokens.attention),
            ),
        ]));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
    let actions = match draft.step {
        NewMissionStep::Provider => vec![
            ("←/→", "provider"),
            ("enter", "continue"),
            ("esc", "cancel"),
        ],
        NewMissionStep::Confirm => vec![
            ("space", "write scope"),
            ("enter", "launch"),
            ("⇧tab", "back"),
            ("esc", "cancel"),
        ],
        _ => vec![("enter", "continue"), ("⇧tab", "back"), ("esc", "cancel")],
    };
    action_bar::render(frame, footer, &actions, tokens);
}

fn body_lines(app: &AppState, width: u16, tokens: UiTokens, icons: IconSet) -> Vec<Line<'static>> {
    let draft = app.new_mission.as_ref().unwrap();
    match draft.step {
        NewMissionStep::Objective => vec![
            section::heading(
                "What should be true when this mission is done?",
                tokens,
                icons,
            ),
            Line::default(),
            input_line(&draft.objective, "Fix the login redirect…", width, tokens),
            Line::default(),
            hint(
                "Use an outcome, not a task list. Nagi turns this into the mission brief.",
                tokens,
            ),
        ],
        NewMissionStep::Criteria => vec![
            section::heading("Define acceptance before the agent starts", tokens, icons),
            Line::default(),
            input_line(
                &draft.criteria,
                "Redirect test passes; requested URL is preserved",
                width,
                tokens,
            ),
            Line::default(),
            hint(
                "Separate criteria with semicolons, or paste one per line. Maximum 16.",
                tokens,
            ),
        ],
        NewMissionStep::ProofCommand => {
            let confidence = match draft.recipe.confidence {
                RecipeConfidence::ProjectTest => "project test detected",
                RecipeConfidence::BaselineOnly => "baseline only, edit this command",
            };
            vec![
                section::heading("Freeze the proof command", tokens, icons),
                Line::from(vec![
                    Span::styled("Detected  ", Style::default().fg(tokens.text_muted)),
                    Span::styled(draft.recipe.label, Style::default().fg(tokens.focus)),
                    Span::styled(
                        format!("  · {confidence}"),
                        Style::default().fg(tokens.text_muted),
                    ),
                ]),
                Line::default(),
                input_line(&draft.proof_command, "cargo test", width, tokens),
                Line::default(),
                hint(
                    "Executed as literal argv, never through a shell. Nagi reruns it before close.",
                    tokens,
                ),
            ]
        }
        NewMissionStep::Provider => {
            let providers = ["Codex", "Claude Code", "OpenCode", "ACP agent"];
            let mut lines = vec![
                section::heading("Choose the runtime, keep the workflow", tokens, icons),
                Line::default(),
            ];
            for (index, provider) in providers.into_iter().enumerate() {
                let selected = index == draft.provider_index;
                lines.push(Line::from(vec![
                    Span::styled(
                        if selected { "  ▸ " } else { "    " },
                        Style::default().fg(tokens.focus),
                    ),
                    Span::styled(
                        provider,
                        Style::default()
                            .fg(if selected {
                                tokens.text
                            } else {
                                tokens.text_muted
                            })
                            .add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                ]));
            }
            lines.push(Line::default());
            lines.push(hint(
                "Attention, evidence and proof stay provider-neutral.",
                tokens,
            ));
            lines
        }
        NewMissionStep::Confirm => {
            let provider =
                ["Codex", "Claude Code", "OpenCode", "ACP agent"][draft.provider_index.min(3)];
            let check = if draft.workspace_write_confirmed {
                "[x]"
            } else {
                "[ ]"
            };
            let mut lines = vec![
                section::heading("Review the authority boundary", tokens, icons),
                Line::from(vec![
                    Span::styled("Provider  ", Style::default().fg(tokens.text_muted)),
                    Span::styled(provider, Style::default().fg(tokens.text)),
                ]),
                Line::from(vec![
                    Span::styled("Proof     ", Style::default().fg(tokens.text_muted)),
                    Span::styled(
                        truncate_end(&draft.proof_command, width.saturating_sub(10) as usize),
                        Style::default().fg(tokens.proof_fresh),
                    ),
                ]),
                Line::default(),
                Line::from(vec![
                    Span::styled(
                        format!("{check} "),
                        Style::default().fg(if draft.workspace_write_confirmed {
                            tokens.proof_fresh
                        } else {
                            tokens.attention
                        }),
                    ),
                    Span::styled(
                        "Allow provider writes inside a new isolated worktree",
                        Style::default().fg(tokens.text),
                    ),
                ]),
            ];
            if let Some(summary) = &draft.project_recipe_summary {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{check} "),
                        Style::default().fg(if draft.workspace_write_confirmed {
                            tokens.proof_fresh
                        } else {
                            tokens.attention
                        }),
                    ),
                    Span::styled(
                        truncate_end(
                            &format!("Run .nagi recipe: {summary}"),
                            width.saturating_sub(4) as usize,
                        ),
                        Style::default().fg(tokens.text),
                    ),
                ]));
            }
            lines.push(Line::default());
            lines.push(hint(
                "Checked scopes run only inside the worktree; risky actions ask again.",
                tokens,
            ));
            lines
        }
    }
}

fn input_line(value: &str, placeholder: &str, width: u16, tokens: UiTokens) -> Line<'static> {
    let empty = value.trim().is_empty();
    let shown = if empty { placeholder } else { value };
    Line::from(vec![
        Span::styled("  ", Style::default().bg(tokens.panel_elevated)),
        Span::styled(
            truncate_end(shown, width.saturating_sub(5) as usize),
            Style::default()
                .fg(if empty {
                    tokens.text_muted
                } else {
                    tokens.text
                })
                .bg(tokens.panel_elevated),
        ),
        Span::styled(
            "▌ ",
            Style::default().fg(tokens.focus).bg(tokens.panel_elevated),
        ),
    ])
}

fn hint(text: &str, tokens: UiTokens) -> Line<'static> {
    Line::from(Span::styled(
        text.to_owned(),
        Style::default().fg(tokens.text_muted),
    ))
}

const fn step_index(step: NewMissionStep) -> usize {
    match step {
        NewMissionStep::Objective => 1,
        NewMissionStep::Criteria => 2,
        NewMissionStep::ProofCommand => 3,
        NewMissionStep::Provider => 4,
        NewMissionStep::Confirm => 5,
    }
}

const fn step_title(step: NewMissionStep) -> &'static str {
    match step {
        NewMissionStep::Objective => "Outcome",
        NewMissionStep::Criteria => "Acceptance",
        NewMissionStep::ProofCommand => "Proof",
        NewMissionStep::Provider => "Runtime",
        NewMissionStep::Confirm => "Launch",
    }
}

fn step_marks(active: usize, icons: IconSet) -> String {
    let (done, open) = if icons == IconSet::Unicode {
        ("●", "○")
    } else {
        ("*", "o")
    };
    (1..=5)
        .map(|step| if step <= active { done } else { open })
        .collect::<Vec<_>>()
        .join(" ")
}
