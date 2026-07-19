#![cfg(unix)]

mod support;

use std::{fs, os::unix::fs::PermissionsExt as _, path::Path, time::Duration};

use support::headless::{initialize_git_repository, wait_until, HeadlessHarness};

#[test]
fn provider_disconnect_never_becomes_false_completion_and_survives_restart() {
    let provider_root = tempfile::tempdir().unwrap();
    let fixture = provider_root.path().join("codex-disconnect");
    fs::write(
        &fixture,
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/providers/codex.sh"
        )),
    )
    .unwrap();
    fs::set_permissions(&fixture, fs::Permissions::from_mode(0o700)).unwrap();
    let shim = provider_root.path().join("codex");
    fs::write(
        &shim,
        format!("#!/bin/sh\nexec '{}' \"$@\"\n", fixture.display()),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o700)).unwrap();

    let mut server = HeadlessHarness::start(Some(provider_root.path()));
    let repository = server.root().join("repository");
    initialize_git_repository(&repository);
    create_and_configure(&server, &repository);
    let started = server.request(serde_json::json!({
        "id": "start",
        "method": "mission.start",
        "params": {
            "mission_id": "mission-provider-disconnect",
            "run_id": "run-provider-disconnect",
            "provider": "codex",
            "mode": "managed",
            "execute_declared_checks": false,
            "execute_project_recipe": false
        }
    }));
    assert_eq!(
        started["result"]["type"], "mission_run_started",
        "{started}"
    );

    let mut terminal_status = None;
    wait_until(Duration::from_secs(10), || {
        let response = server.request(serde_json::json!({
            "id": "poll",
            "method": "mission.get",
            "params": {"mission_id": "mission-provider-disconnect"}
        }));
        terminal_status = response["result"]["mission"]["status"]
            .as_str()
            .map(str::to_owned);
        matches!(terminal_status.as_deref(), Some("failed" | "blocked"))
    });
    assert_eq!(terminal_status.as_deref(), Some("failed"));

    server.restart();
    let restored = server.request(serde_json::json!({
        "id": "restored",
        "method": "mission.get",
        "params": {"mission_id": "mission-provider-disconnect"}
    }));
    assert_eq!(restored["result"]["mission"]["status"], "failed");
    assert_ne!(restored["result"]["mission"]["status"], "ready_to_close");
    assert_eq!(
        restored["result"]["mission"]["unresolved_attention_count"],
        0
    );
}

fn create_and_configure(server: &HeadlessHarness, repository: &Path) {
    let created = server.request(serde_json::json!({
        "id": "create",
        "method": "mission.create",
        "params": {
            "mission_id": "mission-provider-disconnect",
            "title": "Do not lie after disconnect",
            "repository_path": repository,
            "objective": "Preserve provider uncertainty",
            "acceptance_criteria": ["A disconnect cannot report completion"]
        }
    }));
    assert_eq!(created["result"]["created"], true, "{created}");
    let configured = server.request(serde_json::json!({
        "id": "configure",
        "method": "mission.configure",
        "params": {
            "mission_id": "mission-provider-disconnect",
            "checks": [{
                "kind": "command",
                "id": "never-false-completion",
                "program": "true",
                "args": [],
                "cwd": ".",
                "relevant_paths": [{"type": "all"}],
                "required_artifacts": [],
                "required": true,
                "covers": [0]
            }]
        }
    }));
    assert_eq!(configured["result"]["configured"], true, "{configured}");
}
