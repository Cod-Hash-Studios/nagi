use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use crate::ui::{
    design::{icons::IconSet, tokens::UiTokens},
    text::{display_width_u16, truncate_end},
};

pub(crate) fn line(
    label: &str,
    completed: usize,
    total: usize,
    tokens: UiTokens,
    icons: IconSet,
    width: u16,
) -> Line<'static> {
    let marker = match icons {
        IconSet::Unicode => "◉",
        IconSet::Ascii => "*",
    };
    let counter = if total == 0 {
        "not configured".to_string()
    } else {
        format!("{completed}/{total} fresh")
    };
    let fixed = usize::from(display_width_u16(marker))
        .saturating_add(usize::from(display_width_u16(&counter)))
        .saturating_add(3);
    let mut title = truncate_end(label, usize::from(width).saturating_sub(fixed));
    if icons == IconSet::Ascii {
        title = title.replace('…', ".");
    }
    Line::from(vec![
        Span::styled(
            marker,
            Style::default()
                .fg(if completed == total && total > 0 {
                    tokens.proof_fresh
                } else {
                    tokens.proof_stale
                })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {title}"), Style::default().fg(tokens.text)),
        Span::styled(
            format!("  {counter}"),
            Style::default().fg(tokens.text_muted),
        ),
    ])
}
