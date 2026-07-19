use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::ui::design::tokens::UiTokens;
use crate::ui::text::display_width_u16;

use super::key_hint;

pub(crate) fn render(frame: &mut Frame, area: Rect, actions: &[(&str, &str)], tokens: UiTokens) {
    if area.is_empty() {
        return;
    }
    let mut spans = Vec::<Span<'static>>::new();
    for (key, action) in actions {
        let hint = key_hint::spans(key, action, tokens);
        let hint_width = hint
            .iter()
            .map(|span| display_width_u16(span.content.as_ref()))
            .sum::<u16>();
        let used = spans
            .iter()
            .map(|span| display_width_u16(span.content.as_ref()))
            .sum::<u16>();
        if used.saturating_add(hint_width) > area.width {
            break;
        }
        spans.extend(hint);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;

    fn rendered(width: u16) -> String {
        let backend = TestBackend::new(width, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    &[("enter", "open"), ("/", "search"), ("esc", "close")],
                    UiTokens::from(&crate::app::state::Palette::nagi_night()),
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<String>()
    }

    #[test]
    fn narrow_action_bars_only_render_complete_hints() {
        assert_eq!(rendered(20).trim_end(), "enter open");
        assert!(!rendered(20).contains('/'));
        assert!(!rendered(20).contains("esc"));
    }

    #[test]
    fn wide_action_bars_keep_every_hint() {
        assert_eq!(rendered(40).trim_end(), "enter open  / search  esc close");
    }
}
