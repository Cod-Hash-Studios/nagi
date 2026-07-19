use ratatui::style::Style;

use crate::ui::design::tokens::UiTokens;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SurfaceKind {
    Panel,
    Elevated,
}

pub(crate) fn style(tokens: UiTokens, kind: SurfaceKind) -> Style {
    match kind {
        SurfaceKind::Panel => Style::default().bg(tokens.panel).fg(tokens.text),
        SurfaceKind::Elevated => Style::default().bg(tokens.panel_elevated).fg(tokens.text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_variants_use_semantic_tokens_only() {
        let palette = crate::app::state::Palette::nagi_night();
        let tokens = UiTokens::from(&palette);
        assert_eq!(style(tokens, SurfaceKind::Panel).bg, Some(tokens.panel));
        assert_eq!(
            style(tokens, SurfaceKind::Elevated).bg,
            Some(tokens.panel_elevated)
        );
    }
}
