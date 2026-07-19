use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use crate::ui::design::{icons::IconSet, tokens::UiTokens};

pub(crate) fn heading(label: &str, tokens: UiTokens, icons: IconSet) -> Line<'static> {
    let rule = match icons {
        IconSet::Unicode => "  ─",
        IconSet::Ascii => "  -",
    };
    Line::from(vec![
        Span::styled(
            label.to_uppercase(),
            Style::default()
                .fg(tokens.text_muted)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(rule, Style::default().fg(tokens.border)),
    ])
}
