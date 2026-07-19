use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use crate::ui::design::{
    icons::{IconSet, SemanticIcon},
    tokens::UiTokens,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StateBadgeKind {
    Attention,
    Working,
    ProofFresh,
    ProofStale,
}

impl StateBadgeKind {
    fn icon(self) -> SemanticIcon {
        match self {
            Self::Attention => SemanticIcon::Attention,
            Self::Working => SemanticIcon::Working,
            Self::ProofFresh => SemanticIcon::ProofFresh,
            Self::ProofStale => SemanticIcon::ProofStale,
        }
    }

    fn color(self, tokens: UiTokens) -> ratatui::style::Color {
        match self {
            Self::Attention => tokens.attention,
            Self::Working => tokens.working,
            Self::ProofFresh => tokens.proof_fresh,
            Self::ProofStale => tokens.proof_stale,
        }
    }
}

pub(crate) fn line(kind: StateBadgeKind, tokens: UiTokens, icons: IconSet) -> Line<'static> {
    let icon = kind.icon();
    let style = Style::default()
        .fg(kind.color(tokens))
        .add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled(icon.glyph(icons), style),
        Span::raw(" "),
        Span::styled(icon.label().to_uppercase(), style),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badges_keep_text_equivalents_in_both_icon_sets() {
        let tokens = UiTokens::from(&crate::app::state::Palette::nagi_dawn());
        for kind in [
            StateBadgeKind::Attention,
            StateBadgeKind::Working,
            StateBadgeKind::ProofFresh,
            StateBadgeKind::ProofStale,
        ] {
            for icons in [IconSet::Unicode, IconSet::Ascii] {
                let rendered = line(kind, tokens, icons)
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>();
                assert!(rendered.split_whitespace().count() >= 2);
            }
        }
    }
}
