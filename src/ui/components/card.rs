use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::ui::design::{icons::IconSet, tokens::UiTokens};

pub(crate) struct Card<'a> {
    pub(crate) title: &'a str,
    pub(crate) body: Vec<Line<'static>>,
    pub(crate) selected: bool,
}

pub(crate) fn render(
    frame: &mut Frame,
    area: Rect,
    card: Card<'_>,
    tokens: UiTokens,
    icons: IconSet,
    border_style: crate::theme::manifest::ThemeBorderStyle,
) {
    if area.width < 3 || area.height < 3 {
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(icons.border_set(border_style))
        .border_style(Style::default().fg(if card.selected {
            tokens.focus
        } else {
            tokens.border
        }))
        .title(Line::from(vec![Span::styled(
            format!(" {} ", card.title),
            Style::default()
                .fg(tokens.text)
                .add_modifier(Modifier::BOLD),
        )]));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(card.body), inner);
}
