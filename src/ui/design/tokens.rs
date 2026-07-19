use ratatui::style::Color;

use crate::app::state::Palette;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UiTokens {
    pub canvas: Color,
    pub panel: Color,
    pub panel_elevated: Color,
    pub border: Color,
    pub text: Color,
    pub text_muted: Color,
    pub focus: Color,
    pub attention: Color,
    pub working: Color,
    pub proof_fresh: Color,
    pub proof_stale: Color,
    pub danger: Color,
}

impl From<&Palette> for UiTokens {
    fn from(palette: &Palette) -> Self {
        Self {
            canvas: palette.panel_bg,
            panel: palette.panel_bg,
            panel_elevated: palette.surface0,
            border: palette.surface1,
            text: palette.text,
            text_muted: palette.subtext0,
            focus: palette.accent,
            attention: palette.red,
            working: palette.blue,
            proof_fresh: palette.teal,
            proof_stale: palette.peach,
            danger: palette.red,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_tokens_map_palette_fields_semantically() {
        let palette = Palette::catppuccin();
        let tokens = UiTokens::from(&palette);

        assert_eq!(tokens.canvas, palette.panel_bg);
        assert_eq!(tokens.panel, palette.panel_bg);
        assert_eq!(tokens.panel_elevated, palette.surface0);
        assert_eq!(tokens.border, palette.surface1);
        assert_eq!(tokens.text, palette.text);
        assert_eq!(tokens.text_muted, palette.subtext0);
        assert_eq!(tokens.focus, palette.accent);
        assert_eq!(tokens.attention, palette.red);
        assert_eq!(tokens.working, palette.blue);
        assert_eq!(tokens.proof_fresh, palette.teal);
        assert_eq!(tokens.proof_stale, palette.peach);
        assert_eq!(tokens.danger, palette.red);
    }

    #[test]
    fn nagi_builtins_meet_primary_text_contrast() {
        for palette in [Palette::nagi_dawn(), Palette::nagi_night()] {
            assert!(contrast_ratio(palette.text, palette.panel_bg) >= 7.0);
            assert!(contrast_ratio(palette.subtext0, palette.panel_bg) >= 4.5);
        }
    }

    fn contrast_ratio(foreground: Color, background: Color) -> f64 {
        let Color::Rgb(fr, fg, fb) = foreground else {
            return f64::INFINITY;
        };
        let Color::Rgb(br, bg, bb) = background else {
            return f64::INFINITY;
        };
        let foreground = luminance(fr, fg, fb);
        let background = luminance(br, bg, bb);
        let lighter = foreground.max(background);
        let darker = foreground.min(background);
        (lighter + 0.05) / (darker + 0.05)
    }

    fn luminance(red: u8, green: u8, blue: u8) -> f64 {
        let linear = |channel: u8| {
            let value = f64::from(channel) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue)
    }
}
