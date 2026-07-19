pub(crate) mod action_bar;
pub(crate) mod card;
pub(crate) mod empty_state;
pub(crate) mod focus_rail;
pub(crate) mod inspector;
pub(crate) mod key_hint;
pub(crate) mod metric;
pub(crate) mod progress_steps;
pub(crate) mod section;
pub(crate) mod skeleton;
pub(crate) mod state_badge;
pub(crate) mod surface;
pub(crate) mod timeline;

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, layout::Rect, text::Line, Terminal};

    use super::{card, inspector, progress_steps, section, skeleton, timeline};
    use crate::ui::design::{icons::IconSet, tokens::UiTokens};

    fn rendered_ascii(draw: impl FnOnce(&mut ratatui::Frame, UiTokens)) -> String {
        let mut terminal = Terminal::new(TestBackend::new(48, 16)).expect("test terminal");
        let tokens = UiTokens::from(&crate::app::state::Palette::nagi_night());
        terminal
            .draw(|frame| draw(frame, tokens))
            .expect("component render");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    #[test]
    fn composite_primitives_have_a_complete_ascii_rendering() {
        let output = rendered_ascii(|frame, tokens| {
            card::render(
                frame,
                Rect::new(0, 0, 24, 4),
                card::Card {
                    title: "Mission",
                    body: vec![progress_steps::line(
                        "criteria",
                        1,
                        2,
                        tokens,
                        IconSet::Ascii,
                        20,
                    )],
                    selected: true,
                },
                tokens,
                IconSet::Ascii,
            );
            inspector::render(
                frame,
                Rect::new(24, 0, 24, 16),
                inspector::Inspector {
                    eyebrow: "pane",
                    title: "worker",
                    state: Line::from("WORKING"),
                    summary: "bounded context",
                    facts: vec![("State", "working".to_string())],
                    tail: vec![
                        skeleton::line(18, 8, tokens, IconSet::Ascii),
                        timeline::item("agent", "working", "now", tokens, IconSet::Ascii),
                    ],
                },
                tokens,
                IconSet::Ascii,
            );
            frame.render_widget(
                ratatui::widgets::Paragraph::new(section::heading("Proof", tokens, IconSet::Ascii)),
                Rect::new(0, 5, 20, 1),
            );
        });

        assert!(output.is_ascii());
        assert!(output.contains("Mission"));
        assert!(output.contains("ACTIVITY"));
        assert!(output.contains("| agent working"));
    }

    #[test]
    fn components_are_noop_safe_in_zero_and_tiny_rectangles() {
        let _ = rendered_ascii(|frame, tokens| {
            card::render(
                frame,
                Rect::new(0, 0, 2, 2),
                card::Card {
                    title: "tiny",
                    body: vec![],
                    selected: false,
                },
                tokens,
                IconSet::Unicode,
            );
            inspector::render(
                frame,
                Rect::new(0, 0, 1, 1),
                inspector::Inspector {
                    eyebrow: "tiny",
                    title: "tiny",
                    state: Line::default(),
                    summary: "tiny",
                    facts: vec![],
                    tail: vec![],
                },
                tokens,
                IconSet::Unicode,
            );
        });
    }
}
