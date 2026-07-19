#![cfg(unix)]

mod support;

use std::{fs, io::Write, time::Duration};

use support::headless::{wait_until, HeadlessHarness};

#[test]
fn hard_crash_repairs_a_torn_final_journal_record_without_losing_authority() {
    let mut server = HeadlessHarness::start(None);
    let repository = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
    let created = server.request(serde_json::json!({
        "id": "create",
        "method": "mission.create",
        "params": {
            "mission_id": "mission-crash-recovery",
            "title": "Recover after abrupt shutdown",
            "repository_path": repository,
            "objective": "Keep the durable mission authority intact",
            "acceptance_criteria": ["The configured mission survives restart"]
        }
    }));
    assert_eq!(created["result"]["created"], true);
    let configured = server.request(serde_json::json!({
        "id": "configure",
        "method": "mission.configure",
        "params": {
            "mission_id": "mission-crash-recovery",
            "checks": [{
                "kind": "command",
                "id": "recovery-check",
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
    assert_eq!(configured["result"]["configured"], true);
    let before = server.request(serde_json::json!({
        "id": "before",
        "method": "mission.get",
        "params": {"mission_id": "mission-crash-recovery"}
    }));

    let mut journal = None;
    wait_until(Duration::from_secs(2), || {
        journal = fs::read_dir(server.config_home())
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("missions/missions.journal.bin"))
            .find(|candidate| candidate.is_file());
        journal.is_some()
    });
    let journal = journal.expect("mission journal path");
    server.kill_hard();
    let durable_len = fs::metadata(&journal).unwrap().len();
    fs::OpenOptions::new()
        .append(true)
        .open(&journal)
        .unwrap()
        .write_all(b"MSN")
        .unwrap();
    assert_eq!(fs::metadata(&journal).unwrap().len(), durable_len + 3);

    server.restart();
    let after = server.request(serde_json::json!({
        "id": "after",
        "method": "mission.get",
        "params": {"mission_id": "mission-crash-recovery"}
    }));
    assert_eq!(
        after["result"]["mission"],
        before["result"]["mission"],
        "restart response: {after}\n{}",
        server.diagnostics()
    );
    assert_eq!(
        fs::metadata(&journal).unwrap().len(),
        durable_len,
        "startup must truncate only the torn final record"
    );
    assert_eq!(
        server.request(serde_json::json!({"id":"ping","method":"ping","params":{}}))["result"]
            ["type"],
        "pong"
    );
}
