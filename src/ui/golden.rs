use std::{env, fmt::Write as _, fs, path::PathBuf};

use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier},
    Terminal,
};
use sha2::{Digest, Sha256};

use super::{compute_view, render};
use crate::{
    api::schema::{
        AttentionDecisionV1, AttentionDeliveryStateV1, AttentionFailureCodeV1, AttentionItemV1,
        AttentionKindV1, AttentionPaneTargetV1, AttentionResponseCapabilityV1, AttentionRiskV1,
        AttentionSourceV1, AttentionStateV1, ContractVersionV1, MissionCheckStatusV1,
        MissionHandoffArtifactV1, MissionHandoffDiffV1, MissionProvider, MissionStatus,
        MissionViewV1,
    },
    app::{
        state::{MissionHandoffDraft, NewMissionDraft, NewMissionStep, Palette},
        AppState, Mode,
    },
    config::{CustomThemeColors, UiIconStyleConfig},
    project_recipe::{ProjectRecipe, RecipeConfidence},
    workspace::Workspace,
};

const SIZES: &[(u16, u16)] = &[(60, 20), (80, 24), (120, 32), (200, 48)];
const THEMES: &[&str] = &["nagi-night", "nagi-dawn", "terminal-16", "custom-ume"];
const SURFACES: &[&str] = &[
    "terminal",
    "sessions-1",
    "sessions-8",
    "sessions-50",
    "sessions-500",
    "mission-cockpit",
    "command-palette",
    "new-mission",
    "mission-handoff",
    "settings",
    "mission-inspector",
    "proof-review",
    "attention-inbox",
];

#[test]
fn primary_ui_goldens_match_all_supported_sizes_and_themes() {
    let update = env::var_os("NAGI_UPDATE_GOLDENS").is_some_and(|value| value == "1");
    let mut updated = 0usize;
    let mut mismatches = Vec::new();

    for surface in SURFACES {
        for theme in THEMES {
            for &(width, height) in SIZES {
                let actual = render_golden(surface, theme, width, height);
                let path = golden_path(surface, theme, width, height);
                if update {
                    fs::create_dir_all(path.parent().expect("golden parent")).unwrap();
                    fs::write(&path, actual).unwrap();
                    updated += 1;
                    continue;
                }
                match fs::read_to_string(&path) {
                    Ok(expected) if expected == actual => {}
                    Ok(_) => mismatches.push(format!("changed: {}", path.display())),
                    Err(error) => mismatches.push(format!("missing: {} ({error})", path.display())),
                }
            }
        }
    }

    if update {
        eprintln!("updated {updated} UI golden snapshots");
    } else {
        assert!(
            mismatches.is_empty(),
            "UI golden mismatch(es):\n{}\nRun `python3 scripts/render_ui_goldens.py --update` after reviewing intentional UI changes.",
            mismatches.join("\n")
        );
    }
}

fn render_golden(surface: &str, theme: &str, width: u16, height: u16) -> String {
    let mut app = fixture(surface);
    apply_theme(&mut app, theme);
    compute_view(&mut app, Rect::new(0, 0, width, height));

    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render(&app, frame)).unwrap();
    let buffer = terminal.backend().buffer();

    if let Some(directory) = env::var_os("NAGI_EXPORT_GOLDEN_MEDIA_DIR") {
        let directory = PathBuf::from(directory);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("{surface}__{theme}__{width}x{height}.svg"));
        fs::write(
            path,
            render_svg(buffer, surface, theme, width, height, &app.palette),
        )
        .unwrap();
    }

    let mut style_hasher = Sha256::new();
    for cell in buffer.content() {
        style_hasher.update(cell.symbol().as_bytes());
        style_hasher.update(format!("{:?}", cell.style()).as_bytes());
        style_hasher.update([0]);
    }

    let mut snapshot = String::new();
    writeln!(snapshot, "# nagi-ui-golden v1").unwrap();
    writeln!(snapshot, "# surface: {surface}").unwrap();
    writeln!(snapshot, "# theme: {theme}").unwrap();
    writeln!(snapshot, "# size: {width}x{height}").unwrap();
    writeln!(snapshot, "# palette: {:?}", app.palette).unwrap();
    writeln!(snapshot, "# style-sha256: {:x}", style_hasher.finalize()).unwrap();
    writeln!(snapshot, "# ---").unwrap();
    for y in 0..height {
        let mut row = String::new();
        for x in 0..width {
            row.push_str(buffer[(x, y)].symbol());
        }
        snapshot.push_str(row.trim_end());
        snapshot.push('\n');
    }
    snapshot
}

fn render_svg(
    buffer: &Buffer,
    surface: &str,
    theme: &str,
    width: u16,
    height: u16,
    palette: &Palette,
) -> String {
    const CELL_WIDTH: f32 = 9.6;
    const CELL_HEIGHT: f32 = 20.0;
    const FONT_SIZE: f32 = 16.0;
    let pixel_width = f32::from(width) * CELL_WIDTH;
    let pixel_height = f32::from(height) * CELL_HEIGHT;
    let canvas = color_hex(Some(palette.panel_bg), "#171b24");
    let default_text = color_hex(Some(palette.text), "#f4f0e8");
    let mut svg = String::new();

    writeln!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" role="img" aria-labelledby="title desc" viewBox="0 0 {pixel_width:.1} {pixel_height:.1}" width="{pixel_width:.1}" height="{pixel_height:.1}">"#
    )
    .unwrap();
    writeln!(
        svg,
        "<title id=\"title\">Nagi {}</title>",
        escape_xml(surface)
    )
    .unwrap();
    writeln!(
        svg,
        "<desc id=\"desc\">Deterministic Nagi TUI render in the {} theme at {} by {} cells.</desc>",
        escape_xml(theme),
        width,
        height
    )
    .unwrap();
    writeln!(
        svg,
        r#"<rect width="100%" height="100%" rx="10" fill="{canvas}"/>"#
    )
    .unwrap();

    for y in 0..height {
        for x in 0..width {
            let cell = &buffer[(x, y)];
            let style = cell.style();
            let reversed = style.add_modifier.contains(Modifier::REVERSED);
            let foreground = if reversed { style.bg } else { style.fg };
            let background = if reversed { style.fg } else { style.bg };
            if background.is_some_and(|color| color != Color::Reset) {
                let fill = color_hex(background, &canvas);
                writeln!(
                    svg,
                    r#"<rect x="{:.1}" y="{:.1}" width="{CELL_WIDTH:.1}" height="{CELL_HEIGHT:.1}" fill="{fill}"/>"#,
                    f32::from(x) * CELL_WIDTH,
                    f32::from(y) * CELL_HEIGHT,
                )
                .unwrap();
            }

            let symbol = cell.symbol();
            if symbol.trim().is_empty() {
                continue;
            }
            let fill = color_hex(foreground, &default_text);
            let weight = if style.add_modifier.contains(Modifier::BOLD) {
                "700"
            } else {
                "450"
            };
            let font_style = if style.add_modifier.contains(Modifier::ITALIC) {
                "italic"
            } else {
                "normal"
            };
            let decoration = if style.add_modifier.contains(Modifier::UNDERLINED) {
                "underline"
            } else {
                "none"
            };
            let opacity = if style.add_modifier.contains(Modifier::DIM) {
                "0.66"
            } else {
                "1"
            };
            writeln!(
                svg,
                r#"<text x="{:.1}" y="{:.1}" fill="{fill}" fill-opacity="{opacity}" font-family="'JetBrains Mono','SFMono-Regular','Cascadia Mono',monospace" font-size="{FONT_SIZE:.1}" font-style="{font_style}" font-weight="{weight}" text-decoration="{decoration}" xml:space="preserve">{}</text>"#,
                f32::from(x) * CELL_WIDTH,
                f32::from(y) * CELL_HEIGHT + FONT_SIZE,
                escape_xml(symbol),
            )
            .unwrap();
        }
    }
    svg.push_str("</svg>\n");
    svg
}

fn color_hex(color: Option<Color>, fallback: &str) -> String {
    let (red, green, blue) = match color {
        None | Some(Color::Reset) => return fallback.to_owned(),
        Some(Color::Black) => (0, 0, 0),
        Some(Color::Red) => (205, 49, 49),
        Some(Color::Green) => (13, 188, 121),
        Some(Color::Yellow) => (229, 229, 16),
        Some(Color::Blue) => (36, 114, 200),
        Some(Color::Magenta) => (188, 63, 188),
        Some(Color::Cyan) => (17, 168, 205),
        Some(Color::Gray) => (204, 204, 204),
        Some(Color::DarkGray) => (102, 102, 102),
        Some(Color::LightRed) => (241, 76, 76),
        Some(Color::LightGreen) => (35, 209, 139),
        Some(Color::LightYellow) => (245, 245, 67),
        Some(Color::LightBlue) => (59, 142, 234),
        Some(Color::LightMagenta) => (214, 112, 214),
        Some(Color::LightCyan) => (41, 184, 219),
        Some(Color::White) => (242, 242, 242),
        Some(Color::Indexed(index)) => indexed_rgb(index),
        Some(Color::Rgb(red, green, blue)) => (red, green, blue),
    };
    format!("#{red:02x}{green:02x}{blue:02x}")
}

fn indexed_rgb(index: u8) -> (u8, u8, u8) {
    const ANSI: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    if index < 16 {
        return ANSI[usize::from(index)];
    }
    if index < 232 {
        let value = index - 16;
        let red = value / 36;
        let green = (value % 36) / 6;
        let blue = value % 6;
        let component = |level: u8| if level == 0 { 0 } else { 55 + level * 40 };
        return (component(red), component(green), component(blue));
    }
    let gray = 8 + (index - 232) * 10;
    (gray, gray, gray)
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn fixture(surface: &str) -> AppState {
    let workspace_count = match surface {
        "sessions-1" => 1,
        "sessions-50" => 50,
        "sessions-500" => 500,
        _ => 8,
    };
    let mut app = AppState::test_new();
    app.workspaces = (0..workspace_count)
        .map(|index| {
            let name = if index == 0 {
                "nagi-release-with-a-deliberately-long-workspace-label".to_string()
            } else {
                format!("mission-{index:03}")
            };
            Workspace::test_new(&name)
        })
        .collect();
    app.active = (!app.workspaces.is_empty()).then_some(0);
    app.selected = 0;
    app.ensure_test_terminals();

    let missions = mission_fixtures();
    app.set_mission_views(missions.clone());
    app.selected_mission_id = Some(missions[0].mission_id.clone());
    app.set_attention_items(attention_fixtures());

    match surface {
        "terminal" => app.mode = Mode::Terminal,
        "sessions-1" | "sessions-8" | "sessions-50" | "sessions-500" => {
            app.mission_views.clear();
            app.open_navigator();
        }
        "mission-cockpit" => app.open_navigator(),
        "command-palette" => app.open_command_palette(),
        "new-mission" => {
            app.new_mission = Some(new_mission_fixture());
            app.mode = Mode::NewMission;
        }
        "mission-handoff" => {
            app.mission_handoff = Some(mission_handoff_fixture(&missions[0]));
            app.mode = Mode::MissionHandoff;
        }
        "settings" => app.mode = Mode::Settings,
        "mission-inspector" => app.mode = Mode::MissionInspector,
        "proof-review" => {
            if let Some(mission) = app.mission_views.first_mut() {
                mission.evidence_pack_digest = Some("e".repeat(64));
                if let Some(evidence) = mission.evidence.first_mut() {
                    evidence.recorded_at_millis = 1_784_540_540_000;
                    evidence.duration_millis = Some(1_240);
                    evidence.artifact_count = 2;
                }
            }
            app.mode = Mode::ProofReview;
        }
        "attention-inbox" => app.mode = Mode::AttentionInbox,
        _ => panic!("unknown golden surface: {surface}"),
    }
    app
}

fn new_mission_fixture() -> NewMissionDraft {
    NewMissionDraft {
        step: NewMissionStep::Provider,
        repository_path: PathBuf::from("/Users/nagi/workspaces/release-candidate"),
        recipe: ProjectRecipe {
            id: "rust",
            label: "Rust",
            command_line: "cargo test --locked".into(),
            confidence: RecipeConfidence::ProjectTest,
        },
        project_recipe_summary: Some("setup · 2 checks · 1 service".into()),
        objective: "Ship the release cockpit without losing provider context".into(),
        criteria: "Codex and Claude resume safely; declared proof passes".into(),
        proof_command: "cargo test --locked".into(),
        provider_index: 1,
        workspace_write_confirmed: false,
        error: None,
    }
}

fn mission_handoff_fixture(mission: &MissionViewV1) -> MissionHandoffDraft {
    MissionHandoffDraft {
        mission_id: mission.mission_id.clone(),
        source_provider: MissionProvider::Codex,
        target_provider: MissionProvider::ClaudeCode,
        artifact: Some(MissionHandoffArtifactV1 {
            schema_version: ContractVersionV1,
            artifact_sha256: "d".repeat(64),
            generated_at_millis: 1_753_027_200_000,
            mission_id: mission.mission_id.clone(),
            source_run_id: "run-codex-17".into(),
            suggested_run_id: "run-claude-18".into(),
            source_provider: MissionProvider::Codex,
            target_provider: MissionProvider::ClaudeCode,
            repository_path: "/Users/nagi/workspaces/release-candidate".into(),
            worktree_path: "/Users/nagi/workspaces/release-candidate/.worktrees/mission-1".into(),
            base_revision: "a".repeat(40),
            head_revision: "b".repeat(40),
            objective: mission.objective.clone(),
            acceptance_criteria: mission
                .criteria
                .iter()
                .map(|criterion| criterion.description.clone())
                .collect(),
            diff: MissionHandoffDiffV1 {
                workspace_digest: "c".repeat(64),
                dirty: true,
                changed_paths: vec![
                    "src/app/command_palette.rs".into(),
                    "src/ui/proof_review.rs".into(),
                ],
                stat: "2 files changed, 48 insertions(+), 7 deletions(-)".into(),
            },
            decisions: Vec::new(),
            checks: mission.checks.clone(),
            selected_logs: vec!["cargo test --locked · passed".into()],
            warnings: Vec::new(),
        }),
        workspace_write_confirmed: true,
        loading: false,
        error: None,
    }
}

fn mission_fixtures() -> Vec<MissionViewV1> {
    let ready: MissionViewV1 = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/api/mission-view-v1.json"
    )))
    .unwrap();
    let mut active = ready.clone();
    active.mission_id = "mission-2".into();
    active.title = "Index 500 repository sessions without selection jumps".into();
    active.status = MissionStatus::Active;
    active.checks[0].status = MissionCheckStatusV1::Missing;
    active.evidence.clear();
    let mut stale = ready.clone();
    stale.mission_id = "mission-3".into();
    stale.title = "Reconcile proof after a disconnected provider".into();
    stale.status = MissionStatus::Blocked;
    stale.checks[0].status = MissionCheckStatusV1::Stale;
    stale.unresolved_attention_count = 1;
    let mut failed = ready.clone();
    failed.mission_id = "mission-4".into();
    failed.title = "Recover a failed setup recipe safely".into();
    failed.status = MissionStatus::Failed;
    failed.checks[0].status = MissionCheckStatusV1::Failed;
    vec![ready, active, stale, failed]
}

fn attention_fixtures() -> Vec<AttentionItemV1> {
    let open = AttentionItemV1 {
        schema_version: ContractVersionV1,
        attention_id: "attention-1".into(),
        mission_id: "mission-3".into(),
        run_id: "run-3".into(),
        session_id: "session-3".into(),
        pane: AttentionPaneTargetV1 {
            workspace_id: "mission-003".into(),
            pane_id: "pane-1".into(),
        },
        kind: AttentionKindV1::PermissionRequest,
        requested_action: "Allow the release agent to publish signed artifacts".into(),
        scope: "One GitHub release for the current immutable commit".into(),
        risk: AttentionRiskV1::Critical,
        provider: MissionProvider::OpenCode,
        source: AttentionSourceV1::ProviderApi,
        response_capability: AttentionResponseCapabilityV1::Reliable,
        questions: Vec::new(),
        created_at_millis: 1,
        expires_at_millis: None,
        occurrence_count: 1,
        unread: true,
        state: AttentionStateV1::Open,
        delivery: AttentionDeliveryStateV1::NotRequested,
    };
    let mut uncertain = open.clone();
    uncertain.attention_id = "attention-2".into();
    uncertain.requested_action = "Confirm whether the interrupted deployment was applied".into();
    uncertain.risk = AttentionRiskV1::High;
    uncertain.state = AttentionStateV1::ReconciliationRequired {
        decision: AttentionDecisionV1::ApproveOnce,
        actor: "operator".into(),
        code: AttentionFailureCodeV1::DisconnectedBeforeWrite,
        at_millis: 2,
    };
    uncertain.delivery = AttentionDeliveryStateV1::DeliveryUnknown {
        attempt: 1,
        code: AttentionFailureCodeV1::DisconnectedBeforeWrite,
        at_millis: 2,
    };
    vec![open, uncertain]
}

fn apply_theme(app: &mut AppState, theme: &str) {
    let (palette, icons) = match theme {
        "nagi-night" => (Palette::nagi_night(), UiIconStyleConfig::Unicode),
        "nagi-dawn" => (Palette::nagi_dawn(), UiIconStyleConfig::Unicode),
        "terminal-16" => (Palette::terminal(), UiIconStyleConfig::Ascii),
        "custom-ume" => {
            let custom = CustomThemeColors {
                accent: Some("#d66b8f".into()),
                panel_bg: Some("#17131b".into()),
                surface0: Some("#302432".into()),
                surface1: Some("#473148".into()),
                text: Some("#fff4ed".into()),
                green: Some("#8fcf9b".into()),
                yellow: Some("#efc46b".into()),
                red: Some("#f07078".into()),
                ..CustomThemeColors::default()
            };
            (
                Palette::nagi_night().with_overrides(&custom),
                UiIconStyleConfig::Unicode,
            )
        }
        _ => panic!("unknown golden theme: {theme}"),
    };
    app.palette = palette;
    app.theme_name = theme.to_string();
    app.icon_style = icons;
    app.accent = match app.palette.accent {
        Color::Reset => Color::Blue,
        color => color,
    };
}

fn golden_path(surface: &str, theme: &str, width: u16, height: u16) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{surface}__{theme}__{width}x{height}.txt"))
}

#[test]
fn svg_export_escapes_metadata_and_maps_terminal_colors() {
    assert_eq!(escape_xml("proof & <done>"), "proof &amp; &lt;done&gt;");
    assert_eq!(color_hex(Some(Color::Indexed(16)), "#ffffff"), "#000000");
    assert_eq!(color_hex(Some(Color::Indexed(231)), "#000000"), "#ffffff");
    assert_eq!(color_hex(None, "#123456"), "#123456");
}
