use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::ui::design::{icons::IconSet, tokens::UiTokens};

use super::section;

pub(crate) struct Inspector<'a> {
    pub(crate) eyebrow: &'a str,
    pub(crate) title: &'a str,
    pub(crate) state: Line<'static>,
    pub(crate) summary: &'a str,
    pub(crate) facts: Vec<(&'a str, String)>,
    pub(crate) tail: Vec<Line<'static>>,
}

pub(crate) fn render(
    frame: &mut Frame,
    area: Rect,
    inspector: Inspector<'_>,
    tokens: UiTokens,
    icons: IconSet,
) {
    if area.is_empty() {
        return;
    }
    let divider = match icons {
        IconSet::Unicode => "│",
        IconSet::Ascii => "|",
    };
    for y in area.y..area.y.saturating_add(area.height) {
        frame.render_widget(
            Paragraph::new(divider).style(Style::default().fg(tokens.border)),
            Rect::new(area.x, y, 1, 1),
        );
    }
    if area.width < 4 {
        return;
    }
    let content = Rect::new(
        area.x.saturating_add(2),
        area.y,
        area.width.saturating_sub(2),
        area.height,
    );
    let mut lines = vec![
        Line::from(Span::styled(
            inspector.eyebrow.to_uppercase(),
            Style::default()
                .fg(tokens.text_muted)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            inspector.title.to_string(),
            Style::default()
                .fg(tokens.text)
                .add_modifier(Modifier::BOLD),
        )),
        inspector.state,
        Line::default(),
        section::heading("Context", tokens, icons),
        Line::from(Span::styled(
            inspector.summary.to_string(),
            Style::default().fg(tokens.text_muted),
        )),
    ];
    for (label, value) in inspector.facts {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{label:<10}"),
                Style::default().fg(tokens.text_muted),
            ),
            Span::styled(value, Style::default().fg(tokens.text)),
        ]));
    }
    if !inspector.tail.is_empty() {
        lines.push(Line::default());
        lines.push(section::heading("Activity", tokens, icons));
        lines.extend(inspector.tail);
    }
    lines.truncate(content.height as usize);
    frame.render_widget(Paragraph::new(lines), content);
}
