use ratatui::{
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::ui::design::tokens::UiTokens;

pub(crate) fn render(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    action: Option<(&str, &str)>,
    tokens: UiTokens,
) {
    if area.is_empty() {
        return;
    }
    let mut lines = vec![Line::from(Span::styled(
        title.to_string(),
        Style::default()
            .fg(tokens.text_muted)
            .add_modifier(Modifier::BOLD),
    ))];
    if let Some((key, label)) = action {
        lines.push(Line::from(vec![
            Span::styled(
                key.to_string(),
                Style::default()
                    .fg(tokens.focus)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {label}"), Style::default().fg(tokens.text_muted)),
        ]));
    }
    let height = lines.len().min(area.height as usize) as u16;
    let target = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(height) / 2,
        area.width,
        height,
    );
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), target);
}
