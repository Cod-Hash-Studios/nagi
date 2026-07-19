use nagi_plugin_sdk::{
    stdin_json, stdout_document, InspectorInput, Tone, UiBlock, UiDocument, UiListItem, UiMetric,
};
use serde_json::Value;

fn main() -> Result<(), String> {
    let input: InspectorInput<Value> = stdin_json()?;
    let checks = input.mission["checks"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let passed = checks
        .iter()
        .filter(|check| check["status"] == "passed")
        .count();
    let failing = checks
        .iter()
        .filter(|check| {
            matches!(
                check["status"].as_str(),
                Some("failed" | "stale" | "artifact_missing_or_changed")
            )
        })
        .count();
    let items = checks
        .iter()
        .take(24)
        .map(|check| {
            let status = check["status"].as_str().unwrap_or("unknown");
            UiListItem {
                title: check["check_id"]
                    .as_str()
                    .unwrap_or("unnamed-check")
                    .to_owned(),
                detail: Some(status.replace('_', " ")),
                tone: match status {
                    "passed" => Tone::Success,
                    "failed" | "stale" | "artifact_missing_or_changed" => Tone::Danger,
                    _ => Tone::Warning,
                },
            }
        })
        .collect();
    let mut document = UiDocument::new(vec![
        UiBlock::Metrics {
            items: vec![
                UiMetric {
                    label: "Passed".into(),
                    value: passed.to_string(),
                    detail: None,
                    tone: Tone::Success,
                },
                UiMetric {
                    label: "Needs work".into(),
                    value: failing.to_string(),
                    detail: None,
                    tone: if failing == 0 {
                        Tone::Neutral
                    } else {
                        Tone::Danger
                    },
                },
                UiMetric {
                    label: "Evidence".into(),
                    value: input.mission["evidence"]
                        .as_array()
                        .map_or(0, Vec::len)
                        .to_string(),
                    detail: None,
                    tone: Tone::Neutral,
                },
            ],
        },
        UiBlock::List {
            title: Some("Declared checks".into()),
            items,
        },
    ]);
    document.summary = Some(
        if failing == 0 {
            "PR proof is green"
        } else {
            "PR proof needs attention"
        }
        .into(),
    );
    stdout_document(&document)
}
