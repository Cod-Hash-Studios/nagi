use std::sync::OnceLock;

use regex::Regex;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::api::schema::{
    AttentionDecisionV1, ContractVersionV1, MissionCheckSummaryV1, MissionHandoffArtifactV1,
    MissionHandoffDecisionStateV1, MissionHandoffDecisionV1, MissionHandoffDiffV1, MissionProvider,
};

use super::{
    attention::AttentionDecision,
    model::{MissionStatus, ProviderKind},
    store::{DurableAttentionView, MissionView, PersistedResponseState},
    verifier::{TrustedGit, VerificationError},
};

#[derive(Debug, Error)]
pub(crate) enum MissionHandoffError {
    #[error("mission has no source run to hand off")]
    SourceRunMissing,
    #[error("mission can hand off only from blocked, failed, or review state")]
    InvalidSourceState,
    #[error("handoff target must differ from the source provider")]
    SameProvider,
    #[error("handoff requires every source-run attention request to be resolved first")]
    UnresolvedAttention,
    #[error("mission handoff Git snapshot failed: {0}")]
    Git(#[from] VerificationError),
    #[error("mission handoff artifact serialization failed: {0}")]
    Serialize(#[from] serde_json::Error),
}

pub(crate) fn build_preview(
    mission: &MissionView,
    attention: &[DurableAttentionView],
    checks: Vec<MissionCheckSummaryV1>,
    target_provider: MissionProvider,
    generated_at_millis: u64,
) -> Result<MissionHandoffArtifactV1, MissionHandoffError> {
    if !matches!(
        mission.status,
        MissionStatus::Blocked
            | MissionStatus::Failed
            | MissionStatus::ReviewRequired
            | MissionStatus::ReadyToClose
    ) {
        return Err(MissionHandoffError::InvalidSourceState);
    }
    let run = mission
        .run
        .as_ref()
        .ok_or(MissionHandoffError::SourceRunMissing)?;
    let source_provider = wire_provider(run.provider);
    if source_provider == target_provider {
        return Err(MissionHandoffError::SameProvider);
    }
    let git = TrustedGit::discover()?;
    let summary =
        git.handoff_summary(std::path::Path::new(&run.worktree_path), &run.base_revision)?;
    let suggested_run_id = suggested_run_id(mission, target_provider, &summary.workspace_digest);
    let decisions = attention
        .iter()
        .filter(|item| item.mission_id == mission.mission_id)
        .filter_map(|item| {
            let response = item.response.as_ref()?;
            Some(MissionHandoffDecisionV1 {
                attention_id: item.attention_id.clone(),
                decision: wire_decision(response.decision),
                actor_id: redact_text(&response.actor_id),
                state: match response.state {
                    PersistedResponseState::Requested => MissionHandoffDecisionStateV1::Requested,
                    PersistedResponseState::Acknowledged { .. } => {
                        MissionHandoffDecisionStateV1::Acknowledged
                    }
                    PersistedResponseState::Failed { .. } => MissionHandoffDecisionStateV1::Failed,
                    PersistedResponseState::ReconciliationRequired { .. } => {
                        MissionHandoffDecisionStateV1::ReconciliationRequired
                    }
                },
                updated_at_millis: response.updated_at_millis,
            })
        })
        .collect();
    let mut artifact = MissionHandoffArtifactV1 {
        schema_version: ContractVersionV1,
        artifact_sha256: String::new(),
        generated_at_millis,
        mission_id: mission.mission_id.clone(),
        source_run_id: run.run_id.clone(),
        suggested_run_id,
        source_provider,
        target_provider,
        repository_path: mission.repository_path.clone(),
        worktree_path: run.worktree_path.clone(),
        base_revision: run.base_revision.clone(),
        head_revision: summary.head_revision,
        objective: redact_text(&mission.objective),
        acceptance_criteria: mission
            .acceptance_criteria
            .iter()
            .map(|criterion| redact_text(criterion))
            .collect(),
        diff: MissionHandoffDiffV1 {
            workspace_digest: summary.workspace_digest,
            dirty: !summary.changed_paths.is_empty(),
            changed_paths: summary
                .changed_paths
                .iter()
                .map(|path| redact_text(path))
                .collect(),
            stat: redact_text(&summary.diff_stat),
        },
        decisions,
        checks,
        selected_logs: Vec::new(),
        warnings: vec![
            "Hidden provider reasoning and proprietary session state are not transferred."
                .to_owned(),
            "No provider log excerpt is included until the user selects one for redacted export."
                .to_owned(),
            "Fresh proof remains bound to its source run and must be rerun before closure."
                .to_owned(),
        ],
    };
    let payload = serde_json::to_vec(&artifact)?;
    artifact.artifact_sha256 = format!("{:x}", Sha256::digest(payload));
    Ok(artifact)
}

fn suggested_run_id(
    mission: &MissionView,
    target_provider: MissionProvider,
    workspace_digest: &str,
) -> String {
    let provider = match target_provider {
        MissionProvider::Codex => "codex",
        MissionProvider::ClaudeCode => "claude",
        MissionProvider::OpenCode => "opencode",
        MissionProvider::Acp => "acp",
    };
    let mut digest = Sha256::new();
    digest.update(b"nagi-mission-handoff-run-v1");
    digest.update(mission.mission_id.as_bytes());
    digest.update(mission.updated_at_millis.to_be_bytes());
    digest.update(provider.as_bytes());
    digest.update(workspace_digest.as_bytes());
    let suffix = format!("{:x}", digest.finalize());
    format!("handoff-{provider}-{}", &suffix[..12])
}

const fn wire_provider(provider: ProviderKind) -> MissionProvider {
    match provider {
        ProviderKind::Codex => MissionProvider::Codex,
        ProviderKind::ClaudeCode => MissionProvider::ClaudeCode,
        ProviderKind::OpenCode => MissionProvider::OpenCode,
        ProviderKind::Acp => MissionProvider::Acp,
    }
}

const fn wire_decision(decision: AttentionDecision) -> AttentionDecisionV1 {
    match decision {
        AttentionDecision::ApproveOnce => AttentionDecisionV1::ApproveOnce,
        AttentionDecision::ApproveForSession => AttentionDecisionV1::ApproveForSession,
        AttentionDecision::AllowForMission => AttentionDecisionV1::AllowForMission,
        AttentionDecision::Deny => AttentionDecisionV1::Deny,
        AttentionDecision::Answer => AttentionDecisionV1::Answer,
    }
}

fn redact_text(input: &str) -> String {
    static RULES: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let rules = RULES.get_or_init(|| {
        vec![
            (
                Regex::new(r"(?i)(bearer\s+)[A-Za-z0-9._~+/=-]{12,}").unwrap(),
                "$1[REDACTED]",
            ),
            (
                Regex::new(
                    r"(?i)\b(?:sk-[A-Za-z0-9_-]{12,}|gh[pousr]_[A-Za-z0-9_]{12,}|AKIA[0-9A-Z]{16})\b",
                )
                .unwrap(),
                "[REDACTED]",
            ),
            (
                Regex::new(r"(?im)^(\s*(?:api[_-]?key|token|secret|password|authorization)\s*[:=]\s*)(\S.*)$")
                    .unwrap(),
                "$1[REDACTED]",
            ),
            (
                Regex::new(r"(?i)(https?://)[^/\s:@]+:[^@\s/]+@").unwrap(),
                "$1[REDACTED]@",
            ),
        ]
    });
    rules
        .iter()
        .fold(input.to_owned(), |text, (rule, replacement)| {
            rule.replace_all(&text, *replacement).into_owned()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::Path, process::Command};

    use crate::mission::{
        model::ProviderMode,
        store::{MissionRunView, MissionView},
    };

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("run git fixture command");
        assert!(
            output.status.success(),
            "git fixture command failed: {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn handoff_mission(repo: &Path, base_revision: String) -> MissionView {
        let path = repo.canonicalize().unwrap().to_string_lossy().into_owned();
        MissionView {
            mission_id: "mission-handoff".into(),
            title: "Continue safely".into(),
            repository_path: path.clone(),
            repository_hash: "repository-hash".into(),
            objective: "Fix login; token=ghp_abcdefghijklmnopqrstuvwxyz".into(),
            acceptance_criteria: vec!["Bearer abcdefghijklmnopqrstuvwxyz must not leak".into()],
            check_declarations: Vec::new(),
            status: MissionStatus::Blocked,
            run: Some(MissionRunView {
                run_id: "run-codex".into(),
                provider: ProviderKind::Codex,
                mode: ProviderMode::Managed,
                worktree_path: path,
                base_revision,
                provider_session_id: Some("opaque-provider-session".into()),
                execute_declared_checks: false,
                execute_project_recipe: false,
                handoff_from_run_id: None,
                handoff_artifact_sha256: None,
            }),
            run_history: Vec::new(),
            unresolved_attention_count: 0,
            latest_evidence_pack_digest: None,
            ready_receipt: None,
            archive_receipt: None,
            evidence: Vec::new(),
            updated_at_millis: 42,
        }
    }

    #[test]
    fn redaction_removes_common_secrets_without_erasing_normal_text() {
        let input = "Token: ghp_abcdefghijklmnopqrstuvwxyz\nUse https://user:pass@example.com and sk-abcdefghijklmnop\nkeep this";
        let redacted = redact_text(input);
        assert!(!redacted.contains("abcdefghijklmnopqrstuvwxyz"));
        assert!(!redacted.contains("user:pass"));
        assert!(!redacted.contains("sk-abcdefghijklmnop"));
        assert!(redacted.contains("keep this"));
    }

    #[test]
    fn preview_is_redacted_digest_bound_and_captures_dirty_paths() {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init", "-q"]);
        git(directory.path(), &["config", "user.name", "Handoff Test"]);
        git(
            directory.path(),
            &["config", "user.email", "handoff@example.invalid"],
        );
        std::fs::write(directory.path().join("tracked.txt"), "before\n").unwrap();
        git(directory.path(), &["add", "tracked.txt"]);
        git(directory.path(), &["commit", "-qm", "fixture"]);
        let base = git(directory.path(), &["rev-parse", "HEAD"]);
        std::fs::write(directory.path().join("tracked.txt"), "after\n").unwrap();
        std::fs::write(
            directory
                .path()
                .join("token=ghp_abcdefghijklmnopqrstuvwxyz"),
            "x",
        )
        .unwrap();

        let mission = handoff_mission(directory.path(), base);
        let artifact =
            build_preview(&mission, &[], Vec::new(), MissionProvider::ClaudeCode, 100).unwrap();

        assert_eq!(artifact.artifact_sha256.len(), 64);
        assert!(artifact.diff.dirty);
        assert!(artifact
            .diff
            .changed_paths
            .iter()
            .any(|path| path == "tracked.txt"));
        let serialized = serde_json::to_string(&artifact).unwrap();
        assert!(!serialized.contains("ghp_abcdefghijklmnopqrstuvwxyz"));
        assert!(!serialized.contains("opaque-provider-session"));
        assert!(artifact
            .warnings
            .iter()
            .any(|warning| warning.contains("must be rerun")));

        let mut unsigned = artifact.clone();
        unsigned.artifact_sha256.clear();
        let expected = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&unsigned).unwrap())
        );
        assert_eq!(artifact.artifact_sha256, expected);
    }

    #[test]
    fn preview_rejects_same_provider_before_reading_git() {
        let mut mission = handoff_mission(Path::new("."), "0".repeat(40));
        mission.run.as_mut().unwrap().worktree_path = "/missing".into();
        assert!(matches!(
            build_preview(&mission, &[], Vec::new(), MissionProvider::Codex, 100),
            Err(MissionHandoffError::SameProvider)
        ));
    }
}
