use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};

use crate::ui::design::tokens::UiTokens;

pub(crate) fn spans(
    value: usize,
    label: &str,
    color: Color,
    tokens: UiTokens,
    compact: bool,
) -> [Span<'static>; 2] {
    let label = if compact {
        label.chars().next().map(String::from).unwrap_or_default()
    } else {
        label.to_string()
    };
    [
        Span::styled(
            value.to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {label}"), Style::default().fg(tokens.text_muted)),
    ]
}
