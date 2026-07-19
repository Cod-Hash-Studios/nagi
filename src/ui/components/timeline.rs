use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use crate::ui::design::{icons::IconSet, tokens::UiTokens};

pub(crate) fn item(
    actor: &str,
    event: &str,
    time: &str,
    tokens: UiTokens,
    icons: IconSet,
) -> Line<'static> {
    let stem = match icons {
        IconSet::Unicode => "│ ",
        IconSet::Ascii => "| ",
    };
    Line::from(vec![
        Span::styled(stem, Style::default().fg(tokens.border)),
        Span::styled(
            actor.to_string(),
            Style::default()
                .fg(tokens.text)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {event}"), Style::default().fg(tokens.text_muted)),
        Span::styled(format!("  {time}"), Style::default().fg(tokens.proof_stale)),
    ])
}
