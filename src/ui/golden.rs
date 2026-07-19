use std::{env, fmt::Write as _, fs, path::PathBuf};

use ratatui::{backend::TestBackend, layout::Rect, style::Color, Terminal};
use sha2::{Digest, Sha256};

use super::{compute_view, render};
use crate::{
    api::schema::{
        AttentionDecisionV1, AttentionDeliveryStateV1, AttentionFailureCodeV1, AttentionItemV1,
        AttentionKindV1, AttentionPaneTargetV1, AttentionResponseCapabilityV1, AttentionRiskV1,
        AttentionSourceV1, AttentionStateV1, ContractVersionV1, MissionCheckStatusV1,
        MissionProvider, MissionStatus, MissionViewV1,
    },
    app::{state::Palette, AppState, Mode},
    config::{CustomThemeColors, UiIconStyleConfig},
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
        "settings" => app.mode = Mode::Settings,
        "mission-inspector" => app.mode = Mode::MissionInspector,
        "proof-review" => app.mode = Mode::ProofReview,
        "attention-inbox" => app.mode = Mode::AttentionInbox,
        _ => panic!("unknown golden surface: {surface}"),
    }
    app
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
