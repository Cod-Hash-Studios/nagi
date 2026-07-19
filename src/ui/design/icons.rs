#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IconSet {
    Unicode,
    Ascii,
}

impl From<crate::config::UiIconStyleConfig> for IconSet {
    fn from(style: crate::config::UiIconStyleConfig) -> Self {
        match style {
            crate::config::UiIconStyleConfig::Unicode => Self::Unicode,
            crate::config::UiIconStyleConfig::Ascii => Self::Ascii,
        }
    }
}

impl IconSet {
    pub(crate) fn border_set(
        self,
        preference: crate::theme::manifest::ThemeBorderStyle,
    ) -> ratatui::symbols::border::Set<'static> {
        match (self, preference) {
            (Self::Unicode, crate::theme::manifest::ThemeBorderStyle::Soft)
            | (Self::Unicode, crate::theme::manifest::ThemeBorderStyle::Rounded) => {
                ratatui::symbols::border::ROUNDED
            }
            (Self::Unicode, crate::theme::manifest::ThemeBorderStyle::Plain) => {
                ratatui::symbols::border::PLAIN
            }
            (Self::Unicode, crate::theme::manifest::ThemeBorderStyle::Ascii) | (Self::Ascii, _) => {
                ratatui::symbols::border::Set {
                    top_left: "+",
                    top_right: "+",
                    bottom_left: "+",
                    bottom_right: "+",
                    vertical_left: "|",
                    vertical_right: "|",
                    horizontal_top: "-",
                    horizontal_bottom: "-",
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SemanticIcon {
    FocusRail,
    Expanded,
    Collapsed,
    Current,
    Attention,
    Working,
    ProofFresh,
    ProofStale,
}

impl SemanticIcon {
    pub(crate) fn glyph(self, set: IconSet) -> &'static str {
        match (self, set) {
            (Self::FocusRail, IconSet::Unicode) => "▏",
            (Self::FocusRail, IconSet::Ascii) => ">",
            (Self::Expanded, IconSet::Unicode) => "▾",
            (Self::Expanded, IconSet::Ascii) => "v",
            (Self::Collapsed, IconSet::Unicode) => "▸",
            (Self::Collapsed, IconSet::Ascii) => ">",
            (Self::Current, IconSet::Unicode) => "◆",
            (Self::Current, IconSet::Ascii) => "*",
            (Self::Attention, IconSet::Unicode) => "●",
            (Self::Attention, IconSet::Ascii) => "!",
            (Self::Working, IconSet::Unicode) => "●",
            (Self::Working, IconSet::Ascii) => "~",
            (Self::ProofFresh, IconSet::Unicode) => "✓",
            (Self::ProofFresh, IconSet::Ascii) => "+",
            (Self::ProofStale, IconSet::Unicode) => "!",
            (Self::ProofStale, IconSet::Ascii) => "!",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::FocusRail => "selected",
            Self::Expanded => "expanded",
            Self::Collapsed => "collapsed",
            Self::Current => "current",
            Self::Attention => "needs you",
            Self::Working => "working",
            Self::ProofFresh => "proof fresh",
            Self::ProofStale => "proof stale",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_semantic_icon_has_a_portable_ascii_fallback_and_text_label() {
        for icon in [
            SemanticIcon::FocusRail,
            SemanticIcon::Expanded,
            SemanticIcon::Collapsed,
            SemanticIcon::Current,
            SemanticIcon::Attention,
            SemanticIcon::Working,
            SemanticIcon::ProofFresh,
            SemanticIcon::ProofStale,
        ] {
            assert!(icon.glyph(IconSet::Ascii).is_ascii());
            assert!(!icon.glyph(IconSet::Unicode).is_empty());
            assert!(!icon.label().is_empty());
        }
        for symbol in [
            IconSet::Ascii
                .border_set(crate::theme::manifest::ThemeBorderStyle::Soft)
                .top_left,
            IconSet::Ascii
                .border_set(crate::theme::manifest::ThemeBorderStyle::Soft)
                .vertical_left,
            IconSet::Ascii
                .border_set(crate::theme::manifest::ThemeBorderStyle::Soft)
                .horizontal_top,
        ] {
            assert!(symbol.is_ascii());
        }
    }

    #[test]
    fn theme_border_preferences_select_requested_glyphs() {
        use crate::theme::manifest::ThemeBorderStyle;

        assert_eq!(
            IconSet::Unicode.border_set(ThemeBorderStyle::Soft).top_left,
            "╭"
        );
        assert_eq!(
            IconSet::Unicode
                .border_set(ThemeBorderStyle::Plain)
                .top_left,
            "┌"
        );
        assert_eq!(
            IconSet::Unicode
                .border_set(ThemeBorderStyle::Ascii)
                .top_left,
            "+"
        );
        assert_eq!(
            IconSet::Ascii
                .border_set(ThemeBorderStyle::Rounded)
                .top_left,
            "+"
        );
    }
}
