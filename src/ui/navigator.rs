use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Frame,
};

use super::{
    components::{
        action_bar,
        card::{self, Card},
        empty_state, focus_rail,
        inspector::{self, Inspector},
        metric, progress_steps, skeleton,
        state_badge::{self, StateBadgeKind},
        surface::{self, SurfaceKind},
        timeline,
    },
    design::{
        icons::{IconSet, SemanticIcon},
        tokens::UiTokens,
    },
    scrollbar::{render_scrollbar, should_show_scrollbar},
    status::{agent_icon_with_set, state_label_color},
    text::{display_width_u16, truncate_end},
    widgets::render_panel_shell_with_border_set,
};
use crate::app::state::{
    AppState, CockpitScope, NavigatorRow, NavigatorStateFilter, NavigatorTarget,
};
use crate::terminal::TerminalRuntimeRegistry;

pub(super) fn render_navigator_overlay(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    frame: &mut Frame,
) {
    let popup = app.navigator_popup_rect();
    let tokens = UiTokens::from(&app.palette);
    let icons = IconSet::from(app.icon_style);
    let Some(inner) = render_panel_shell_with_border_set(
        frame,
        popup,
        tokens.focus,
        tokens.panel,
        icons.border_set(),
    ) else {
        return;
    };

    let search = app.navigator_search_rect();
    let header = app.navigator_header_rect();
    let body = app.navigator_body_rect();
    let detail = app.navigator_detail_rect();
    let footer = app.navigator_footer_rect();
    render_header(app, frame, header);
    render_separator_between(frame, header, search, inner.width, app);
    render_search(app, frame, search);

    if body.height > 0 {
        render_separator_between(frame, search, body, inner.width, app);
        render_rows(app, terminal_runtimes, frame, body);
        render_navigator_scrollbar(app, terminal_runtimes, frame, body);
    }
    render_detail(app, terminal_runtimes, frame, detail);
    render_footer(app, frame, footer);
}

fn render_separator_between(
    frame: &mut Frame,
    upper: Rect,
    lower: Rect,
    width: u16,
    app: &AppState,
) {
    let separator_y = upper.y.saturating_add(upper.height);
    if upper.height > 0 && lower.height > 0 && separator_y < lower.y {
        render_separator(frame, Rect::new(upper.x, separator_y, width, 1), app);
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CockpitCounts {
    blocked: usize,
    working: usize,
    done: usize,
}

fn cockpit_counts(app: &AppState) -> CockpitCounts {
    let mut counts = CockpitCounts::default();
    if app.navigator.scope == CockpitScope::Missions {
        for mission in &app.mission_views {
            use crate::api::schema::MissionStatus;
            match mission.status {
                MissionStatus::ReviewRequired | MissionStatus::Blocked | MissionStatus::Failed => {
                    counts.blocked += 1
                }
                MissionStatus::Preparing | MissionStatus::Active => counts.working += 1,
                MissionStatus::ReadyToClose | MissionStatus::Archived => counts.done += 1,
                MissionStatus::Draft => {}
            }
        }
        return counts;
    }
    for workspace in &app.workspaces {
        for tab in &workspace.tabs {
            for pane_id in tab.layout.pane_ids() {
                let Some(pane) = tab.panes.get(&pane_id) else {
                    continue;
                };
                let status = app
                    .terminals
                    .get(&pane.attached_terminal_id)
                    .map(|terminal| terminal.state)
                    .unwrap_or(crate::detect::AgentState::Unknown);
                match (status, pane.seen) {
                    (crate::detect::AgentState::Blocked, _) => counts.blocked += 1,
                    (crate::detect::AgentState::Working, _) => counts.working += 1,
                    (crate::detect::AgentState::Idle, false) => counts.done += 1,
                    _ => {}
                }
            }
        }
    }
    counts
}

fn render_header(app: &AppState, frame: &mut Frame, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let p = &app.palette;
    let tokens = UiTokens::from(p);
    let surface = match app.navigator.scope {
        CockpitScope::Missions => "MISSION COCKPIT",
        CockpitScope::Sessions => "SESSION COCKPIT",
    };
    let title = if matches!(app.icon_style, crate::config::UiIconStyleConfig::Ascii) {
        format!("  NAGI | {surface}")
    } else {
        format!("  NAGI · {surface}")
    };
    let counts = cockpit_counts(app);
    let metrics = format!(
        "{} need you   {} working   {} done  ",
        counts.blocked, counts.working, counts.done
    );
    let gap = area
        .width
        .saturating_sub(display_width_u16(&title))
        .saturating_sub(display_width_u16(&metrics));
    let mut spans = vec![Span::styled(
        title,
        Style::default()
            .fg(tokens.text)
            .add_modifier(Modifier::BOLD),
    )];
    if gap > 0 {
        spans.push(Span::raw(" ".repeat(gap as usize)));
        spans.extend(metric::spans(
            counts.blocked,
            "need you",
            tokens.attention,
            tokens,
            false,
        ));
        spans.push(Span::styled("   ", Style::default()));
        spans.extend(metric::spans(
            counts.working,
            "working",
            tokens.working,
            tokens,
            false,
        ));
        spans.push(Span::styled("   ", Style::default()));
        spans.extend(metric::spans(counts.done, "done", p.green, tokens, false));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_search(app: &AppState, frame: &mut Frame, area: Rect) {
    let p = &app.palette;
    let tokens = UiTokens::from(p);
    let focus_style = if app.navigator.search_focused {
        Style::default()
            .fg(tokens.focus)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(tokens.text_muted)
    };
    let (count, noun, placeholder) = match app.navigator.scope {
        CockpitScope::Missions => (app.mission_views.len(), "missions", "search missions"),
        CockpitScope::Sessions => (
            app.workspaces
                .iter()
                .flat_map(|workspace| workspace.tabs.iter())
                .map(|tab| tab.panes.len())
                .sum::<usize>(),
            "panes",
            "search panes",
        ),
    };
    let mut spans = vec![Span::styled(" / ", focus_style)];
    let query = app.navigator.query.trim();
    match app.navigator.state_filter {
        Some(NavigatorStateFilter::Blocked) => push_state_chip(
            &mut spans,
            crate::detect::AgentState::Blocked,
            true,
            app.spinner_tick,
            "blocked",
            app,
        ),
        Some(NavigatorStateFilter::Working) => push_state_chip(
            &mut spans,
            crate::detect::AgentState::Working,
            true,
            app.spinner_tick,
            "working",
            app,
        ),
        Some(NavigatorStateFilter::Idle) => push_state_chip(
            &mut spans,
            crate::detect::AgentState::Idle,
            true,
            app.spinner_tick,
            "idle",
            app,
        ),
        Some(NavigatorStateFilter::Done) => push_state_chip(
            &mut spans,
            crate::detect::AgentState::Idle,
            false,
            app.spinner_tick,
            "done",
            app,
        ),
        None if query.is_empty()
            && app.navigator.scope == CockpitScope::Missions
            && app.mission_action_error.is_some() =>
        {
            spans.push(Span::styled(
                format!(
                    "! {}",
                    truncate_end(
                        app.mission_action_error.as_deref().unwrap_or_default(),
                        area.width.saturating_sub(24) as usize,
                    )
                ),
                Style::default().fg(tokens.attention),
            ));
        }
        None if query.is_empty() => spans.push(Span::styled(
            placeholder,
            Style::default().fg(tokens.text_muted),
        )),
        None => spans.push(Span::styled(
            query.to_string(),
            Style::default().fg(tokens.text),
        )),
    }
    spans.push(Span::styled(
        format!(
            "{count:>width$} {noun}",
            width = area.width.saturating_sub(16) as usize
        ),
        Style::default().fg(tokens.text_muted),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn push_state_chip(
    spans: &mut Vec<Span<'static>>,
    state: crate::detect::AgentState,
    seen: bool,
    tick: u32,
    label: &'static str,
    app: &AppState,
) {
    let (icon, icon_style) = agent_icon_with_set(
        state,
        seen,
        tick,
        &app.palette,
        IconSet::from(app.icon_style),
    );
    spans.push(Span::styled(icon, icon_style.add_modifier(Modifier::BOLD)));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        label,
        Style::default()
            .fg(state_label_color(state, seen, &app.palette))
            .add_modifier(Modifier::BOLD),
    ));
}

fn render_separator(frame: &mut Frame, area: Rect, app: &AppState) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let glyph = if matches!(app.icon_style, crate::config::UiIconStyleConfig::Ascii) {
        "-"
    } else {
        "─"
    };
    let line = glyph.repeat(area.width as usize);
    let tokens = UiTokens::from(&app.palette);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().fg(tokens.border)),
        area,
    );
}

fn render_rows(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    frame: &mut Frame,
    body: Rect,
) {
    let rows = app.navigator_rows_from(terminal_runtimes);
    if rows.is_empty() {
        let tokens = UiTokens::from(&app.palette);
        empty_state::render(
            frame,
            body,
            match app.navigator.scope {
                CockpitScope::Missions => "No missions match",
                CockpitScope::Sessions => "No panes match",
            },
            Some(("ctrl+u", "clear filter")),
            tokens,
        );
        return;
    }
    let start = app.navigator.scroll.min(rows.len());
    let end = rows.len().min(start.saturating_add(body.height as usize));
    for (visible_idx, row) in rows[start..end].iter().enumerate() {
        let idx = start + visible_idx;
        let y = body.y + visible_idx as u16;
        let rect = Rect::new(body.x, y, body.width, 1);
        let selected = idx == app.navigator.selected;
        render_row(app, frame, rect, row, selected);
    }
}

fn render_row(app: &AppState, frame: &mut Frame, rect: Rect, row: &NavigatorRow, selected: bool) {
    let p = &app.palette;
    let tokens = UiTokens::from(p);
    let icons = IconSet::from(app.icon_style);
    frame.render_widget(Clear, rect);
    let surface_kind = if selected {
        SurfaceKind::Elevated
    } else {
        SurfaceKind::Panel
    };
    let base_style = surface::style(tokens, surface_kind);
    let row_background = base_style.bg.unwrap_or(tokens.panel);
    let dim_style = Style::default().fg(tokens.text_muted).bg(row_background);
    let text_style = if selected {
        base_style.add_modifier(Modifier::BOLD)
    } else if row.is_current {
        Style::default()
            .fg(tokens.text)
            .bg(row_background)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(tokens.text_muted).bg(row_background)
    };
    let (status_icon, status_style) =
        agent_icon_with_set(row.status, row.seen, app.spinner_tick, p, icons);
    let status_style = status_style.bg(row_background);

    let prefix = if row.is_workspace {
        if row.expanded {
            SemanticIcon::Expanded.glyph(icons)
        } else {
            SemanticIcon::Collapsed.glyph(icons)
        }
    } else if row.depth > 0 {
        match icons {
            IconSet::Unicode => "├─",
            IconSet::Ascii => "|-",
        }
    } else {
        "  "
    };
    let current = if row.is_current {
        SemanticIcon::Current.glyph(icons)
    } else {
        " "
    };
    let focus_rail_width = display_width_u16(SemanticIcon::FocusRail.glyph(icons));
    let indent = "  ".repeat(row.depth as usize);
    let navigation_prefix = format!("{indent}{prefix}   {current} ");
    let left_fixed_width = focus_rail_width.saturating_add(display_width_u16(&navigation_prefix));
    let meta_width = metadata_width(rect.width);
    let left_budget = rect
        .width
        .saturating_sub(meta_width)
        .saturating_sub(left_fixed_width)
        .saturating_sub(3) as usize;
    let title = truncate_end(&row.label, left_budget);

    let spans = vec![
        focus_rail::span(selected, tokens, icons),
        Span::styled(navigation_prefix, dim_style),
        Span::styled(status_icon, status_style),
        Span::raw(" "),
        Span::styled(title, text_style),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)).style(base_style), rect);

    if meta_width > 0 {
        let meta_rect = Rect::new(
            rect.x + rect.width.saturating_sub(meta_width),
            rect.y,
            meta_width,
            1,
        );
        let portable_meta = portable_chrome_text(app, &row.meta);
        let meta = truncate_end(&portable_meta, meta_width.saturating_sub(2) as usize);
        let meta_style = if row.is_workspace || row.is_tab {
            Style::default().fg(tokens.text_muted).bg(row_background)
        } else {
            Style::default()
                .fg(state_label_color(row.status, row.seen, p))
                .bg(row_background)
        };
        frame.render_widget(
            Paragraph::new(format!(" {meta}")).style(meta_style),
            meta_rect,
        );
    }
}

fn render_navigator_scrollbar(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    frame: &mut Frame,
    body: Rect,
) {
    if body.width <= 1 || body.height == 0 {
        return;
    }
    let rows = app.navigator_rows_from(terminal_runtimes).len();
    let viewport = body.height as usize;
    if rows <= viewport {
        return;
    }
    let metrics = crate::pane::ScrollMetrics {
        viewport_rows: viewport,
        offset_from_bottom: rows
            .saturating_sub(viewport)
            .saturating_sub(app.navigator.scroll),
        max_offset_from_bottom: rows.saturating_sub(viewport),
    };
    if !should_show_scrollbar(metrics) {
        return;
    }
    let track = Rect::new(body.x + body.width - 1, body.y, 1, body.height);
    render_scrollbar(
        frame,
        metrics,
        track,
        UiTokens::from(&app.palette).panel,
        UiTokens::from(&app.palette).text_muted,
        match IconSet::from(app.icon_style) {
            IconSet::Unicode => "▕",
            IconSet::Ascii => "|",
        },
    );
}

fn metadata_width(width: u16) -> u16 {
    if width >= 90 {
        28
    } else if width >= 68 {
        20
    } else if width >= 52 {
        14
    } else {
        0
    }
}

fn render_detail(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    frame: &mut Frame,
    area: Rect,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let detail = selected_detail(app, terminal_runtimes);
    if detail.is_empty() {
        return;
    }
    if area.height >= 6 {
        render_inspector(app, terminal_runtimes, frame, area, &detail);
        return;
    }
    if area.height >= 3 {
        let row = selected_navigator_row(app, terminal_runtimes);
        let title = row
            .as_ref()
            .map(|row| row.label.as_str())
            .unwrap_or("Selection");
        let tokens = UiTokens::from(&app.palette);
        let body = row
            .as_ref()
            .map(|row| compact_detail_line(app, row, area.width.saturating_sub(4)))
            .unwrap_or_default();
        card::render(
            frame,
            area,
            Card {
                title,
                body: vec![body],
                selected: true,
            },
            tokens,
            IconSet::from(app.icon_style),
        );
        return;
    }
    let tokens = UiTokens::from(&app.palette);
    let row = selected_navigator_row(app, terminal_runtimes);
    let line = row
        .as_ref()
        .map(|row| compact_detail_line(app, row, area.width.saturating_sub(2)))
        .unwrap_or_default();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(line).style(surface::style(tokens, SurfaceKind::Elevated)),
        area,
    );
}

fn compact_detail_line(app: &AppState, row: &NavigatorRow, width: u16) -> Line<'static> {
    let tokens = UiTokens::from(&app.palette);
    let icons = IconSet::from(app.icon_style);
    let (mut spans, summary) = match &row.target {
        NavigatorTarget::Mission { mission_id } => {
            let Some(mission) = app
                .mission_views
                .iter()
                .find(|mission| mission.mission_id == *mission_id)
            else {
                return Line::from(Span::styled(
                    "Mission no longer exists",
                    Style::default().fg(tokens.attention),
                ));
            };
            (
                mission_status_badge(mission.status, tokens, icons).spans,
                mission.objective.clone(),
            )
        }
        NavigatorTarget::MissionProject { repository_path } => {
            let missions = app
                .mission_views
                .iter()
                .filter(|mission| mission.repository_path == *repository_path)
                .count();
            let needs_you = app
                .mission_views
                .iter()
                .filter(|mission| {
                    mission.repository_path == *repository_path
                        && mission.unresolved_attention_count > 0
                })
                .count();
            (
                vec![Span::styled(
                    "PROJECT",
                    Style::default()
                        .fg(tokens.text_muted)
                        .add_modifier(Modifier::BOLD),
                )],
                format!("{missions} missions · {needs_you} need you"),
            )
        }
        _ => {
            let (icon, style) =
                agent_icon_with_set(row.status, row.seen, app.spinner_tick, &app.palette, icons);
            (
                vec![
                    Span::styled(icon, style.add_modifier(Modifier::BOLD)),
                    Span::raw(" "),
                    Span::styled(
                        display_state(row.status, row.seen).to_uppercase(),
                        Style::default()
                            .fg(state_label_color(row.status, row.seen, &app.palette))
                            .add_modifier(Modifier::BOLD),
                    ),
                ],
                row.meta.clone(),
            )
        }
    };
    let used = spans
        .iter()
        .map(|span| display_width_u16(span.content.as_ref()))
        .sum::<u16>();
    if used < width {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            truncate_end(
                &summary,
                width.saturating_sub(used).saturating_sub(2) as usize,
            ),
            Style::default().fg(tokens.text_muted),
        ));
    }
    Line::from(spans)
}

fn selected_navigator_row(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> Option<NavigatorRow> {
    app.navigator_rows_from(terminal_runtimes)
        .get(app.navigator.selected)
        .cloned()
}

fn render_inspector(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    frame: &mut Frame,
    area: Rect,
    summary: &str,
) {
    let Some(row) = selected_navigator_row(app, terminal_runtimes) else {
        return;
    };
    if let NavigatorTarget::Mission { mission_id } = &row.target {
        render_mission_inspector(app, frame, area, mission_id);
        return;
    }
    if let NavigatorTarget::MissionProject { repository_path } = &row.target {
        render_mission_project_inspector(app, frame, area, repository_path);
        return;
    }
    let tokens = UiTokens::from(&app.palette);
    let icons = IconSet::from(app.icon_style);
    let state = match (row.status, row.seen) {
        (crate::detect::AgentState::Blocked, _) => {
            state_badge::line(StateBadgeKind::Attention, tokens, icons)
        }
        (crate::detect::AgentState::Working, _) => {
            state_badge::line(StateBadgeKind::Working, tokens, icons)
        }
        (crate::detect::AgentState::Idle, false) => {
            state_badge::line(StateBadgeKind::ProofFresh, tokens, icons)
        }
        (crate::detect::AgentState::Idle, true) => {
            Line::from(Span::styled("IDLE", Style::default().fg(tokens.text_muted)))
        }
        (crate::detect::AgentState::Unknown, _) => Line::from(Span::styled(
            "SHELL",
            Style::default().fg(tokens.text_muted),
        )),
    };
    let kind = if row.is_workspace {
        "Workspace"
    } else if row.is_tab {
        "Tab"
    } else {
        "Pane"
    };
    let mut tail = match (row.status, row.seen) {
        (crate::detect::AgentState::Working, _) => vec![
            skeleton::line(
                area.width.saturating_sub(4),
                app.spinner_tick,
                tokens,
                icons,
            ),
            timeline::item("agent", "working now", "live", tokens, icons),
        ],
        (crate::detect::AgentState::Blocked, _) => {
            vec![timeline::item(
                "agent",
                "waiting for input",
                "now",
                tokens,
                icons,
            )]
        }
        (crate::detect::AgentState::Idle, false) => {
            vec![timeline::item("agent", "finished", "new", tokens, icons)]
        }
        (crate::detect::AgentState::Idle, true) => {
            vec![timeline::item("agent", "idle", "live", tokens, icons)]
        }
        (crate::detect::AgentState::Unknown, _) => {
            vec![timeline::item("shell", "attached", "live", tokens, icons)]
        }
    };
    tail.push(Line::from(vec![
        Span::styled("enter", Style::default().fg(tokens.focus)),
        Span::styled(
            " switch to selection",
            Style::default().fg(tokens.text_muted),
        ),
    ]));
    inspector::render(
        frame,
        area,
        Inspector {
            eyebrow: kind,
            title: &row.label,
            state,
            summary,
            facts: vec![
                ("Type", kind.to_string()),
                ("State", display_state(row.status, row.seen).to_string()),
                ("Signal", portable_chrome_text(app, &row.meta)),
            ],
            tail,
        },
        tokens,
        icons,
    );
}

fn selected_detail(app: &AppState, terminal_runtimes: &TerminalRuntimeRegistry) -> String {
    let rows = app.navigator_rows_from(terminal_runtimes);
    let Some(row) = rows.get(app.navigator.selected) else {
        return String::new();
    };
    let detail = match &row.target {
        NavigatorTarget::MissionProject { repository_path } => {
            mission_project_detail(app, repository_path)
        }
        NavigatorTarget::Mission { mission_id } => mission_detail(app, mission_id),
        NavigatorTarget::Workspace { ws_idx } => workspace_detail(app, terminal_runtimes, *ws_idx),
        NavigatorTarget::Tab { ws_idx, tab_idx } => {
            tab_detail(app, terminal_runtimes, *ws_idx, *tab_idx)
        }
        NavigatorTarget::Pane {
            ws_idx,
            tab_idx,
            pane_id,
        } => pane_detail(app, terminal_runtimes, *ws_idx, *tab_idx, *pane_id),
    };
    portable_chrome_text(app, &detail)
}

fn mission_project_detail(app: &AppState, repository_path: &str) -> String {
    let count = app
        .mission_views
        .iter()
        .filter(|mission| mission.repository_path == repository_path)
        .count();
    format!(
        "{repository_path}{}{} missions",
        detail_separator(app),
        count
    )
}

fn mission_detail(app: &AppState, mission_id: &str) -> String {
    let Some(mission) = app
        .mission_views
        .iter()
        .find(|mission| mission.mission_id == mission_id)
    else {
        return String::new();
    };
    let mut parts = vec![
        mission_status_text(mission.status).to_string(),
        format!(
            "{} / {} criteria fresh",
            mission_fresh_criteria(mission),
            mission.criteria.len()
        ),
    ];
    if let Some(run) = &mission.run {
        parts.push(mission_provider_text(run.provider).to_string());
    }
    parts.push(mission.objective.clone());
    parts.join(detail_separator(app))
}

fn mission_fresh_criteria(mission: &crate::api::schema::MissionViewV1) -> usize {
    mission
        .criteria
        .iter()
        .filter(|criterion| {
            !criterion.required_check_ids.is_empty()
                && criterion.required_check_ids.iter().all(|required_id| {
                    mission.checks.iter().any(|check| {
                        check.check_id == *required_id
                            && check.status == crate::api::schema::MissionCheckStatusV1::Passed
                    })
                })
        })
        .count()
}

fn mission_status_text(status: crate::api::schema::MissionStatus) -> &'static str {
    use crate::api::schema::MissionStatus;
    match status {
        MissionStatus::Draft => "draft",
        MissionStatus::Preparing => "preparing",
        MissionStatus::Active => "working",
        MissionStatus::ReviewRequired => "review required",
        MissionStatus::ReadyToClose => "proven",
        MissionStatus::Blocked => "blocked",
        MissionStatus::Failed => "failed",
        MissionStatus::Archived => "archived",
    }
}

fn mission_provider_text(provider: crate::api::schema::MissionProvider) -> &'static str {
    match provider {
        crate::api::schema::MissionProvider::Codex => "Codex",
        crate::api::schema::MissionProvider::ClaudeCode => "Claude Code",
        crate::api::schema::MissionProvider::OpenCode => "OpenCode",
        crate::api::schema::MissionProvider::Acp => "ACP agent",
    }
}

fn mission_status_badge(
    status: crate::api::schema::MissionStatus,
    tokens: UiTokens,
    icons: IconSet,
) -> Line<'static> {
    use crate::api::schema::MissionStatus;
    match status {
        MissionStatus::Preparing | MissionStatus::Active => {
            state_badge::line(StateBadgeKind::Working, tokens, icons)
        }
        MissionStatus::ReadyToClose | MissionStatus::Archived => {
            state_badge::line(StateBadgeKind::ProofFresh, tokens, icons)
        }
        MissionStatus::Draft => state_badge::line(StateBadgeKind::ProofStale, tokens, icons),
        MissionStatus::ReviewRequired | MissionStatus::Blocked | MissionStatus::Failed => {
            state_badge::line(StateBadgeKind::Attention, tokens, icons)
        }
    }
}

fn render_mission_inspector(app: &AppState, frame: &mut Frame, area: Rect, mission_id: &str) {
    let Some(mission) = app
        .mission_views
        .iter()
        .find(|mission| mission.mission_id == mission_id)
    else {
        return;
    };
    let tokens = UiTokens::from(&app.palette);
    let icons = IconSet::from(app.icon_style);
    let fresh = mission_fresh_criteria(mission);
    let provider = mission
        .run
        .as_ref()
        .map(|run| mission_provider_text(run.provider).to_string())
        .unwrap_or_else(|| "not started".to_string());
    let worktree = mission
        .run
        .as_ref()
        .map(|run| run.worktree_path.clone())
        .unwrap_or_else(|| mission.repository_path.clone());
    let mut tail = vec![progress_steps::line(
        "Acceptance criteria",
        fresh,
        mission.criteria.len(),
        tokens,
        icons,
        area.width.saturating_sub(4),
    )];
    if mission.unresolved_attention_count > 0 {
        tail.push(timeline::item(
            "attention",
            &format!(
                "{} unresolved request(s)",
                mission.unresolved_attention_count
            ),
            "now",
            tokens,
            icons,
        ));
    } else if mission.evidence_pack_digest.is_some() {
        tail.push(timeline::item(
            "proof",
            "evidence pack recorded",
            "fresh",
            tokens,
            icons,
        ));
    } else {
        tail.push(timeline::item(
            "mission",
            mission_status_text(mission.status),
            "current",
            tokens,
            icons,
        ));
    }
    tail.push(Line::from(vec![
        Span::styled("enter", Style::default().fg(tokens.focus)),
        Span::styled(" open mission", Style::default().fg(tokens.text_muted)),
        Span::styled("   tab", Style::default().fg(tokens.focus)),
        Span::styled(" sessions", Style::default().fg(tokens.text_muted)),
    ]));
    inspector::render(
        frame,
        area,
        Inspector {
            eyebrow: "Mission",
            title: &mission.title,
            state: mission_status_badge(mission.status, tokens, icons),
            summary: &mission.objective,
            facts: vec![
                ("Provider", provider),
                (
                    "Criteria",
                    format!("{fresh} / {} fresh", mission.criteria.len()),
                ),
                ("Attention", mission.unresolved_attention_count.to_string()),
                ("Worktree", worktree),
            ],
            tail,
        },
        tokens,
        icons,
    );
}

fn render_mission_project_inspector(
    app: &AppState,
    frame: &mut Frame,
    area: Rect,
    repository_path: &str,
) {
    let missions = app
        .mission_views
        .iter()
        .filter(|mission| mission.repository_path == repository_path)
        .collect::<Vec<_>>();
    let tokens = UiTokens::from(&app.palette);
    let icons = IconSet::from(app.icon_style);
    let title = std::path::Path::new(repository_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(repository_path);
    let attention = missions
        .iter()
        .filter(|mission| mission.unresolved_attention_count > 0)
        .count();
    inspector::render(
        frame,
        area,
        Inspector {
            eyebrow: "Project",
            title,
            state: if attention > 0 {
                state_badge::line(StateBadgeKind::Attention, tokens, icons)
            } else {
                state_badge::line(StateBadgeKind::ProofStale, tokens, icons)
            },
            summary: repository_path,
            facts: vec![
                ("Missions", missions.len().to_string()),
                ("Need you", attention.to_string()),
            ],
            tail: vec![Line::from(vec![
                Span::styled("space", Style::default().fg(tokens.focus)),
                Span::styled(" collapse project", Style::default().fg(tokens.text_muted)),
            ])],
        },
        tokens,
        icons,
    );
}

fn portable_chrome_text(app: &AppState, text: &str) -> String {
    if matches!(app.icon_style, crate::config::UiIconStyleConfig::Ascii) {
        text.replace(" · ", " | ").replace('·', "|")
    } else {
        text.to_string()
    }
}

fn workspace_detail(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    ws_idx: usize,
) -> String {
    let Some(ws) = app.workspaces.get(ws_idx) else {
        return String::new();
    };
    let label = ws.display_name_from(&app.terminals, terminal_runtimes);
    let pane_count = ws.tabs.iter().map(|tab| tab.panes.len()).sum::<usize>();
    let mut parts = vec![label, format!("{pane_count} panes")];
    if !rowless_workspace_activity(app, terminal_runtimes, ws_idx).is_empty() {
        parts.push(rowless_workspace_activity(app, terminal_runtimes, ws_idx));
    }
    parts.join(detail_separator(app))
}

fn tab_detail(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    ws_idx: usize,
    tab_idx: usize,
) -> String {
    let Some(ws) = app.workspaces.get(ws_idx) else {
        return String::new();
    };
    let Some(tab) = ws.tabs.get(tab_idx) else {
        return String::new();
    };
    let mut parts = vec![
        ws.display_name_from(&app.terminals, terminal_runtimes),
        format!(
            "tab: {}",
            ws.tab_display_name(tab_idx)
                .unwrap_or_else(|| (tab_idx + 1).to_string())
        ),
        format!("{} panes", tab.panes.len()),
    ];
    let rows = app.navigator_rows_from(terminal_runtimes);
    if let Some(meta) = rows
        .into_iter()
        .find(|row| matches!(row.target, NavigatorTarget::Tab { ws_idx: row_ws_idx, tab_idx: row_tab_idx } if row_ws_idx == ws_idx && row_tab_idx == tab_idx))
        .map(|row| row.meta)
        .filter(|meta| !meta.is_empty())
    {
        parts.push(meta);
    }
    parts.join(detail_separator(app))
}

fn pane_detail(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    ws_idx: usize,
    tab_idx: usize,
    pane_id: crate::layout::PaneId,
) -> String {
    let Some(ws) = app.workspaces.get(ws_idx) else {
        return String::new();
    };
    let Some(tab) = ws.tabs.get(tab_idx) else {
        return String::new();
    };
    let mut parts = vec![ws.display_name_from(&app.terminals, terminal_runtimes)];
    if ws.tabs.len() > 1 {
        parts.push(format!(
            "tab: {}",
            ws.tab_display_name(tab_idx)
                .unwrap_or_else(|| (tab_idx + 1).to_string())
        ));
    }
    if let Some(pane_number) = ws.public_pane_number(pane_id) {
        parts.push(format!("pane {pane_number}"));
    }
    if let Some(terminal_id) = tab.terminal_id(pane_id) {
        if let Some(terminal) = app.terminals.get(terminal_id) {
            let presentation = terminal.effective_presentation();
            if let Some(title) = presentation.title {
                parts.push(title);
            }
            let display_agent = terminal.effective_display_agent();
            if let Some(agent) = display_agent.as_deref().or_else(|| {
                terminal
                    .agent_name
                    .as_deref()
                    .or_else(|| terminal.effective_agent_label())
            }) {
                parts.push(agent.to_string());
                let seen = tab
                    .panes
                    .get(&pane_id)
                    .map(|pane| pane.seen)
                    .unwrap_or(true);
                let state = row_state(app, ws_idx, tab_idx, pane_id);
                let status = presentation
                    .state_labels
                    .get(display_state(state, seen))
                    .cloned()
                    .unwrap_or_else(|| display_state(state, seen).to_string());
                parts.push(status);
            } else {
                parts.push("shell".to_string());
            }
        }
    }
    parts.join(detail_separator(app))
}

fn detail_separator(app: &AppState) -> &'static str {
    if matches!(app.icon_style, crate::config::UiIconStyleConfig::Ascii) {
        " | "
    } else {
        " · "
    }
}

fn rowless_workspace_activity(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    ws_idx: usize,
) -> String {
    app.navigator_rows_from(terminal_runtimes)
        .into_iter()
        .find(|row| matches!(row.target, NavigatorTarget::Workspace { ws_idx: row_ws_idx } if row_ws_idx == ws_idx))
        .map(|row| row.meta)
        .unwrap_or_default()
}

fn row_state(
    app: &AppState,
    ws_idx: usize,
    tab_idx: usize,
    pane_id: crate::layout::PaneId,
) -> crate::detect::AgentState {
    app.workspaces
        .get(ws_idx)
        .and_then(|ws| ws.tabs.get(tab_idx))
        .and_then(|tab| tab.terminal_id(pane_id))
        .and_then(|terminal_id| app.terminals.get(terminal_id))
        .map(|terminal| terminal.state)
        .unwrap_or(crate::detect::AgentState::Unknown)
}

fn display_state(state: crate::detect::AgentState, seen: bool) -> &'static str {
    match (state, seen) {
        (crate::detect::AgentState::Blocked, _) => "blocked",
        (crate::detect::AgentState::Working, _) => "working",
        (crate::detect::AgentState::Idle, false) => "done",
        (crate::detect::AgentState::Idle, true) => "idle",
        (crate::detect::AgentState::Unknown, _) => "unknown",
    }
}

fn render_footer(app: &AppState, frame: &mut Frame, area: Rect) {
    if area.height == 0 {
        return;
    }
    let p = &app.palette;
    let tokens = UiTokens::from(p);
    let portable = matches!(app.icon_style, crate::config::UiIconStyleConfig::Ascii);
    let mut hints = if app.navigator.search_focused {
        vec![
            (
                "enter",
                if app.navigator.scope == CockpitScope::Missions {
                    "open"
                } else {
                    "switch"
                },
            ),
            (if portable { "up/down" } else { "↑↓" }, "move"),
            ("ctrl+u", "clear"),
            ("esc", "back"),
        ]
    } else {
        vec![
            (
                "enter",
                if app.navigator.scope == CockpitScope::Missions {
                    "open"
                } else {
                    "switch"
                },
            ),
            (
                "tab",
                if app.navigator.scope == CockpitScope::Missions {
                    "sessions"
                } else {
                    "missions"
                },
            ),
            ("/", "search"),
            ("b/w/i/d/a", "states"),
            (if portable { "j/k" } else { "j/k/↑↓" }, "move"),
            ("esc", "close"),
        ]
    };
    if !app.navigator.search_focused && app.navigator.scope == CockpitScope::Missions {
        hints.insert(1, ("n", "new mission"));
    }
    action_bar::render(frame, area, &hints, tokens);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{detect::Agent, workspace::Workspace};
    use ratatui::{backend::TestBackend, layout::Direction, Terminal};

    fn cockpit_app() -> AppState {
        let mut app = AppState::test_new();
        let mut workspace = Workspace::test_new("demo");
        let blocked = workspace.tabs[0].root_pane;
        let working = workspace.test_split(Direction::Horizontal);
        let done = workspace.test_split(Direction::Vertical);
        app.workspaces.push(workspace);
        app.ensure_test_terminals();

        for (pane_id, state, seen) in [
            (blocked, crate::detect::AgentState::Blocked, true),
            (working, crate::detect::AgentState::Working, true),
            (done, crate::detect::AgentState::Idle, false),
        ] {
            let terminal_id = app.workspaces[0].tabs[0].panes[&pane_id]
                .attached_terminal_id
                .clone();
            app.terminals
                .get_mut(&terminal_id)
                .unwrap()
                .set_detected_state(Some(Agent::Codex), state);
            app.workspaces[0].tabs[0]
                .panes
                .get_mut(&pane_id)
                .unwrap()
                .seen = seen;
        }

        app
    }

    #[test]
    fn cockpit_counts_only_actionable_panes() {
        let app = cockpit_app();

        assert_eq!(
            cockpit_counts(&app),
            CockpitCounts {
                blocked: 1,
                working: 1,
                done: 1,
            }
        );
    }

    #[test]
    fn cockpit_counts_stay_global_when_rows_are_filtered() {
        let mut app = cockpit_app();
        app.navigator.state_filter = Some(NavigatorStateFilter::Working);
        let visible_panes = app
            .navigator_rows()
            .into_iter()
            .filter(|row| !row.is_workspace && !row.is_tab)
            .count();

        assert_eq!(visible_panes, 1);
        assert_eq!(
            cockpit_counts(&app),
            CockpitCounts {
                blocked: 1,
                working: 1,
                done: 1,
            }
        );
    }

    #[test]
    fn cockpit_counts_stay_global_when_workspace_is_collapsed() {
        let mut app = cockpit_app();
        app.navigator.expanded_workspaces.clear();
        let visible_panes = app
            .navigator_rows()
            .into_iter()
            .filter(|row| !row.is_workspace && !row.is_tab)
            .count();

        assert_eq!(visible_panes, 0);
        assert_eq!(
            cockpit_counts(&app),
            CockpitCounts {
                blocked: 1,
                working: 1,
                done: 1,
            }
        );
    }

    #[test]
    fn selected_row_uses_a_focus_rail_without_an_accent_fill() {
        let mut app = AppState::test_new();
        app.palette = crate::app::state::Palette::nagi_night();
        let row = NavigatorRow {
            id: crate::app::state::NavigatorRowId::Workspace("workspace-1".to_string()),
            target: NavigatorTarget::Workspace { ws_idx: 0 },
            depth: 0,
            label: "calm workspace".to_string(),
            meta: "1 working".to_string(),
            status: crate::detect::AgentState::Working,
            seen: true,
            is_current: true,
            is_workspace: true,
            is_tab: false,
            expanded: true,
            search_text: String::new(),
        };
        let area = Rect::new(0, 0, 40, 1);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();

        terminal
            .draw(|frame| render_row(&app, frame, area, &row, true))
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "▏");
        assert_eq!(buffer[(0, 0)].style().fg, Some(app.palette.accent));
        assert!((0..area.width).all(|x| buffer[(x, 0)].style().bg == Some(app.palette.surface0)));
        assert!((0..area.width).all(|x| buffer[(x, 0)].style().bg != Some(app.palette.accent)));
    }

    #[test]
    fn selected_row_honors_the_portable_ascii_icon_style() {
        let mut app = AppState::test_new();
        app.icon_style = crate::config::UiIconStyleConfig::Ascii;
        let row = NavigatorRow {
            id: crate::app::state::NavigatorRowId::Workspace("workspace-1".to_string()),
            target: NavigatorTarget::Workspace { ws_idx: 0 },
            depth: 0,
            label: "portable workspace".to_string(),
            meta: String::new(),
            status: crate::detect::AgentState::Unknown,
            seen: true,
            is_current: true,
            is_workspace: true,
            is_tab: false,
            expanded: false,
            search_text: String::new(),
        };
        let area = Rect::new(0, 0, 40, 1);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();

        terminal
            .draw(|frame| render_row(&app, frame, area, &row, true))
            .unwrap();

        let rendered = (0..area.width)
            .map(|column| terminal.backend().buffer()[(column, 0)].symbol())
            .collect::<String>();
        assert!(rendered.is_ascii());
        assert!(rendered.starts_with(">>"));
        assert!(rendered.contains('*'));
    }

    fn render_cockpit_at(width: u16, height: u16, ascii: bool) -> String {
        let mut app = cockpit_app();
        app.icon_style = if ascii {
            crate::config::UiIconStyleConfig::Ascii
        } else {
            crate::config::UiIconStyleConfig::Unicode
        };
        let sidebar_width = width.min(20);
        app.view.sidebar_rect = Rect::new(0, 0, sidebar_width, height);
        app.view.terminal_area = Rect::new(
            sidebar_width,
            0,
            width.saturating_sub(sidebar_width),
            height,
        );
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render_navigator_overlay(&app, &TerminalRuntimeRegistry::new(), frame))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    fn mission_cockpit_app() -> AppState {
        let mut app = AppState::test_new();
        app.set_mission_views(vec![crate::api::schema::MissionViewV1 {
            schema_version: crate::api::schema::ContractVersionV1,
            mission_id: "polish".into(),
            title: "Polish the cockpit".into(),
            repository_path: "/repos/nagi".into(),
            objective: "Make parallel agent work calm and inspectable".into(),
            criteria: vec![crate::api::schema::MissionCriterionSummaryV1 {
                criterion_id: Some("a".repeat(64)),
                description: "Responsive mission cockpit".into(),
                coverage: crate::api::schema::MissionCriterionCoverageV1::Covered,
                required_check_ids: vec!["ui".into()],
            }],
            closure_configured: true,
            declared_check_count: 1,
            checks: vec![crate::api::schema::MissionCheckSummaryV1 {
                check_id: "ui".into(),
                kind: crate::api::schema::MissionCheckKindV1::Command,
                required: true,
                covered_criterion_ids: vec!["a".repeat(64)],
                status: crate::api::schema::MissionCheckStatusV1::Passed,
            }],
            evidence: Vec::new(),
            evidence_pack_digest: Some("b".repeat(64)),
            details_available: true,
            status: crate::api::schema::MissionStatus::ReadyToClose,
            run: Some(crate::api::schema::MissionRunViewV1 {
                run_id: "run-1".into(),
                provider: crate::api::schema::MissionProvider::OpenCode,
                mode: crate::api::schema::MissionProviderMode::Managed,
                worktree_path: "/repos/nagi-worktree".into(),
                base_revision: "c".repeat(40),
                execute_declared_checks: true,
                execute_project_recipe: false,
                handoff_from_run_id: None,
                handoff_artifact_sha256: None,
            }),
            run_history: Vec::new(),
            unresolved_attention_count: 0,
            updated_at_millis: 1,
        }]);
        app.view.sidebar_rect = Rect::new(0, 0, 20, 32);
        app.view.terminal_area = Rect::new(20, 0, 100, 32);
        app.open_navigator();
        app.select_navigator_index_from(&TerminalRuntimeRegistry::new(), 1);
        app
    }

    #[test]
    fn mission_cockpit_renders_objective_provider_and_fresh_proof() {
        let app = mission_cockpit_app();
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal
            .draw(|frame| render_navigator_overlay(&app, &TerminalRuntimeRegistry::new(), frame))
            .unwrap();
        let output = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(output.contains("MISSION COCKPIT"));
        assert!(output.contains("Polish the cockpit"));
        assert!(output.contains("Make parallel agent work"));
        assert!(output.contains("OpenCode"));
        assert!(output.contains("1 / 1 fresh"));
    }

    #[test]
    fn cockpit_renders_at_all_supported_breakpoints() {
        for (width, height) in [(60, 20), (80, 24), (120, 32), (200, 48)] {
            let output = render_cockpit_at(width, height, false);
            assert!(
                output.contains("NAGI"),
                "missing header at {width}x{height}"
            );
        }
        let wide = render_cockpit_at(120, 32, false);
        assert!(wide.contains("CONTEXT"));
        assert!(wide.contains("ACTIVITY"));
    }

    #[test]
    fn complete_cockpit_chrome_is_ascii_in_portable_mode() {
        let output = render_cockpit_at(120, 32, true);
        let non_ascii = output
            .chars()
            .enumerate()
            .filter(|(_, character)| !character.is_ascii())
            .map(|(index, character)| (index % 120, index / 120, character))
            .collect::<Vec<_>>();
        assert!(output.is_ascii(), "non-ASCII cockpit glyphs: {non_ascii:?}");
        assert!(output.contains("NAGI | SESSION COCKPIT"));
        assert!(output.contains("CONTEXT"));
    }
}
