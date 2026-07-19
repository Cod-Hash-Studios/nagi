use ratatui::{style::Style, text::Span};

use crate::ui::design::{
    icons::{IconSet, SemanticIcon},
    tokens::UiTokens,
};

pub(crate) fn span(
    selected: bool,
    tokens: UiTokens,
    icons: IconSet,
    selection: crate::theme::manifest::ThemeSelectionStyle,
) -> Span<'static> {
    let glyph = if selected && selection == crate::theme::manifest::ThemeSelectionStyle::Rail {
        SemanticIcon::FocusRail.glyph(icons)
    } else {
        " "
    };
    Span::styled(glyph, Style::default().fg(tokens.focus))
}

pub(crate) fn row_style(
    selected: bool,
    tokens: UiTokens,
    selection: crate::theme::manifest::ThemeSelectionStyle,
) -> Style {
    if selected && selection == crate::theme::manifest::ThemeSelectionStyle::Fill {
        Style::default().fg(tokens.panel).bg(tokens.focus)
    } else {
        Style::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::manifest::ThemeSelectionStyle;

    #[test]
    fn fill_selection_uses_background_instead_of_a_focus_rail() {
        let tokens = UiTokens::from(&crate::app::state::Palette::nagi_night());

        assert_eq!(
            span(true, tokens, IconSet::Unicode, ThemeSelectionStyle::Rail)
                .content
                .as_ref(),
            "▏"
        );
        assert_eq!(
            span(true, tokens, IconSet::Unicode, ThemeSelectionStyle::Fill)
                .content
                .as_ref(),
            " "
        );
        assert_eq!(
            row_style(true, tokens, ThemeSelectionStyle::Fill).bg,
            Some(tokens.focus)
        );
        assert_eq!(row_style(true, tokens, ThemeSelectionStyle::Rail).bg, None);
    }
}
