use ratatui::{style::Style, text::Line};

use crate::ui::design::{icons::IconSet, tokens::UiTokens};

pub(crate) fn line(width: u16, tick: u32, tokens: UiTokens, icons: IconSet) -> Line<'static> {
    let available = usize::from(width.min(24));
    if available == 0 {
        return Line::default();
    }
    let phase = usize::try_from(tick / 8).unwrap_or_default() % available;
    let (idle, active) = match icons {
        IconSet::Unicode => ('·', '━'),
        IconSet::Ascii => ('.', '='),
    };
    let mut cells = vec![idle; available];
    cells[phase] = active;
    Line::styled(
        cells.into_iter().collect::<String>(),
        Style::default().fg(tokens.text_muted),
    )
}
