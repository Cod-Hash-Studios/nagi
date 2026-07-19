use nagi_plugin_sdk::{
    stdin_json, stdout_document, InspectorInput, Invocation, Tone, UiBlock, UiDocument, UiRow,
};
use serde_json::Value;

fn main() -> Result<(), String> {
    let invocation = Invocation::from_env()?;
    let input: InspectorInput<Value> = stdin_json()?;
    let mission = &input.mission;
    let evidence_count = mission["evidence"].as_array().map_or(0, Vec::len);
    let check_count = mission["checks"].as_array().map_or(0, Vec::len);
    let mut exported = false;
    if invocation.action_id.as_deref() == Some("export") {
        let json = serde_json::to_vec_pretty(mission).map_err(|error| error.to_string())?;
        std::fs::write("/workspace/nagi-evidence.json", json)
            .map_err(|error| format!("JSON export failed: {error}"))?;
        let markdown = format!(
            "# {}\n\nStatus: `{}`\n\nChecks: {}\n\nEvidence records: {}\n",
            mission["title"].as_str().unwrap_or("Mission evidence"),
            mission["status"].as_str().unwrap_or("unknown"),
            check_count,
            evidence_count
        );
        std::fs::write("/workspace/nagi-evidence.md", markdown)
            .map_err(|error| format!("Markdown export failed: {error}"))?;
        exported = true;
    }
    let mut document = UiDocument::new(vec![
        UiBlock::Section {
            title: "Evidence package".into(),
            rows: vec![
                UiRow {
                    label: "Checks".into(),
                    value: check_count.to_string(),
                    tone: Tone::Neutral,
                },
                UiRow {
                    label: "Evidence".into(),
                    value: evidence_count.to_string(),
                    tone: if evidence_count == 0 {
                        Tone::Warning
                    } else {
                        Tone::Success
                    },
                },
                UiRow {
                    label: "Digest".into(),
                    value: mission["evidence_pack_digest"]
                        .as_str()
                        .unwrap_or("Not sealed")
                        .to_owned(),
                    tone: Tone::Neutral,
                },
            ],
        },
        UiBlock::Notice {
            tone: if exported {
                Tone::Success
            } else {
                Tone::Neutral
            },
            title: Some(
                if exported {
                    "Export complete"
                } else {
                    "Ready to export"
                }
                .into(),
            ),
            body: if exported {
                "Wrote nagi-evidence.md and nagi-evidence.json to the worktree."
            } else {
                "Run Export evidence to worktree. No files are written while previewing."
            }
            .into(),
        },
    ]);
    document.summary = Some("Portable mission proof".into());
    stdout_document(&document)
}
