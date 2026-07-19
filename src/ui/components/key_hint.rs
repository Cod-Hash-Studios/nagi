use ratatui::{
    style::{Modifier, Style},
    text::Span,
};

use crate::ui::design::tokens::UiTokens;

pub(crate) fn spans(key: &str, action: &str, tokens: UiTokens) -> [Span<'static>; 2] {
    [
        Span::styled(
            key.to_string(),
            Style::default()
                .fg(tokens.focus)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {action}  "),
            Style::default().fg(tokens.text_muted),
        ),
    ]
}
