use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::ui::design::tokens::UiTokens;

use super::key_hint;

pub(crate) fn render(frame: &mut Frame, area: Rect, actions: &[(&str, &str)], tokens: UiTokens) {
    if area.is_empty() {
        return;
    }
    let mut spans = Vec::<Span<'static>>::new();
    for (key, action) in actions {
        spans.extend(key_hint::spans(key, action, tokens));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}
