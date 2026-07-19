use ratatui::{style::Style, text::Span};

use crate::ui::design::{
    icons::{IconSet, SemanticIcon},
    tokens::UiTokens,
};

pub(crate) fn span(selected: bool, tokens: UiTokens, icons: IconSet) -> Span<'static> {
    let glyph = if selected {
        SemanticIcon::FocusRail.glyph(icons)
    } else {
        " "
    };
    Span::styled(glyph, Style::default().fg(tokens.focus))
}
