use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::ContractVersionV1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MissionCreateParams {
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub mission_id: String,
    #[schemars(length(min = 1, max = 256))]
    pub title: String,
    #[schemars(length(min = 1, max = 4096))]
    pub repository_path: String,
    #[schemars(length(min = 1, max = 8_192))]
    pub objective: String,
    #[schemars(length(min = 1, max = 16), inner(length(min = 1, max = 1_024)))]
    pub acceptance_criteria: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MissionTarget {
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub mission_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MissionHandoffPreviewParams {
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub mission_id: String,
    pub to: MissionProvider,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MissionHandoffStartParams {
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub mission_id: String,
    pub to: MissionProvider,
    /// Timestamp from the inspected preview. The server rebuilds that exact
    /// artifact before starting the continuation.
    pub generated_at_millis: u64,
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub artifact_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MissionConfigureParams {
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub mission_id: String,
    #[schemars(length(min = 1, max = 32))]
    pub checks: Vec<MissionCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MissionCheck {
    Command {
        #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
        id: String,
        #[schemars(length(min = 1, max = 1_024))]
        program: String,
        args: Vec<String>,
        #[schemars(length(min = 1, max = 4_096))]
        cwd: String,
        relevant_paths: Vec<MissionPathRule>,
        required_artifacts: Vec<String>,
        #[serde(default)]
        include_ignored: bool,
        required: bool,
        covers: Vec<usize>,
    },
    Manual {
        #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
        id: String,
        reviewers: Vec<String>,
        #[serde(default)]
        allow_override: bool,
        required: bool,
        covers: Vec<usize>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MissionPathRule {
    All,
    Exact { path: String },
    Prefix { prefix: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MissionStartParams {
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub mission_id: String,
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub run_id: String,
    pub provider: MissionProvider,
    pub mode: MissionProviderMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    /// Explicit consent to execute the mission's declared commands after the
    /// provider finishes. Commands run with the local user's OS permissions.
    #[serde(default)]
    pub execute_declared_checks: bool,
    /// Explicit local consent to run `.nagi/project.toml` setup and services.
    /// The public socket cannot grant this authority.
    #[serde(default)]
    pub execute_project_recipe: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MissionRespondParams {
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub mission_id: String,
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub run_id: String,
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub attention_id: String,
    pub decision: MissionResponseDecision,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub answers: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MissionResponseDecision {
    ApproveOnce,
    ApproveForSession,
    Deny,
    Answer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MissionStatus {
    Draft,
    Preparing,
    Active,
    ReviewRequired,
    ReadyToClose,
    Blocked,
    Failed,
    Archived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MissionProvider {
    Codex,
    ClaudeCode,
    OpenCode,
    Acp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MissionProviderMode {
    Managed,
    Passthrough,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MissionHandoffArtifactV1 {
    pub schema_version: ContractVersionV1,
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub artifact_sha256: String,
    pub generated_at_millis: u64,
    #[schemars(length(min = 1, max = 128))]
    pub mission_id: String,
    #[schemars(length(min = 1, max = 128))]
    pub source_run_id: String,
    #[schemars(length(min = 1, max = 128))]
    pub suggested_run_id: String,
    pub source_provider: MissionProvider,
    pub target_provider: MissionProvider,
    #[schemars(length(min = 1, max = 4_096))]
    pub repository_path: String,
    #[schemars(length(min = 1, max = 4_096))]
    pub worktree_path: String,
    #[schemars(regex(pattern = r"^(?:[0-9A-Fa-f]{40}|[0-9A-Fa-f]{64})$"))]
    pub base_revision: String,
    #[schemars(regex(pattern = r"^(?:[0-9A-Fa-f]{40}|[0-9A-Fa-f]{64})$"))]
    pub head_revision: String,
    #[schemars(length(min = 1, max = 8_192))]
    pub objective: String,
    #[schemars(length(min = 1, max = 16), inner(length(min = 1, max = 1_024)))]
    pub acceptance_criteria: Vec<String>,
    pub diff: MissionHandoffDiffV1,
    #[schemars(length(max = 128))]
    pub decisions: Vec<MissionHandoffDecisionV1>,
    #[schemars(length(max = 32))]
    pub checks: Vec<MissionCheckSummaryV1>,
    #[schemars(length(max = 16), inner(length(min = 1, max = 1_024)))]
    pub selected_logs: Vec<String>,
    #[schemars(length(max = 16), inner(length(min = 1, max = 1_024)))]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MissionHandoffDiffV1 {
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub workspace_digest: String,
    pub dirty: bool,
    #[schemars(length(max = 200_000), inner(length(min = 1, max = 4_096)))]
    pub changed_paths: Vec<String>,
    #[schemars(length(max = 32_768))]
    pub stat: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MissionHandoffDecisionV1 {
    #[schemars(length(min = 1, max = 128))]
    pub attention_id: String,
    pub decision: super::AttentionDecisionV1,
    #[schemars(length(min = 1, max = 128))]
    pub actor_id: String,
    pub state: MissionHandoffDecisionStateV1,
    pub updated_at_millis: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MissionHandoffDecisionStateV1 {
    Requested,
    Acknowledged,
    Failed,
    ReconciliationRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MissionRunInfo {
    pub run_id: String,
    pub provider: MissionProvider,
    pub mode: MissionProviderMode,
    pub worktree_path: String,
    pub base_revision: String,
    #[serde(default)]
    pub execute_declared_checks: bool,
    #[serde(default)]
    pub execute_project_recipe: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MissionInfo {
    pub mission_id: String,
    pub title: String,
    pub repository_path: String,
    pub objective: String,
    pub acceptance_criteria: Vec<String>,
    pub closure_configured: bool,
    pub check_count: usize,
    pub status: MissionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<MissionRunInfo>,
    pub unresolved_attention_count: usize,
    pub updated_at_millis: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MissionSummary {
    pub mission_id: String,
    pub title: String,
    pub repository_path: String,
    pub status: MissionStatus,
    pub unresolved_attention_count: usize,
    pub updated_at_millis: u64,
}

/// Complete first-generation mission projection for cockpit and automation
/// consumers. `MissionInfo` remains only as a strict legacy migration shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MissionViewV1 {
    pub schema_version: ContractVersionV1,
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub mission_id: String,
    #[schemars(length(min = 1, max = 256))]
    pub title: String,
    #[schemars(length(min = 1, max = 4_096))]
    pub repository_path: String,
    #[schemars(length(min = 1, max = 8_192))]
    pub objective: String,
    #[schemars(length(min = 1, max = 16))]
    pub criteria: Vec<MissionCriterionSummaryV1>,
    pub closure_configured: bool,
    #[schemars(range(max = 32))]
    pub declared_check_count: u32,
    #[schemars(length(max = 32))]
    pub checks: Vec<MissionCheckSummaryV1>,
    #[schemars(length(max = 32))]
    pub evidence: Vec<MissionEvidenceSummaryV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub evidence_pack_digest: Option<String>,
    /// False only for a projection migrated from the legacy summary, whose
    /// check and evidence details were never present on the wire.
    pub details_available: bool,
    pub status: MissionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<MissionRunViewV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 128))]
    pub run_history: Vec<MissionRunViewV1>,
    #[schemars(range(max = 100_000))]
    pub unresolved_attention_count: u32,
    pub updated_at_millis: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MissionRunViewV1 {
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub run_id: String,
    pub provider: MissionProvider,
    pub mode: MissionProviderMode,
    #[schemars(length(min = 1, max = 4_096))]
    pub worktree_path: String,
    #[schemars(regex(pattern = r"^(?:[0-9A-Fa-f]{40}|[0-9A-Fa-f]{64})$"))]
    pub base_revision: String,
    pub execute_declared_checks: bool,
    #[serde(default)]
    pub execute_project_recipe: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub handoff_from_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub handoff_artifact_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MissionCriterionSummaryV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub criterion_id: Option<String>,
    #[schemars(length(min = 1, max = 1_024))]
    pub description: String,
    pub coverage: MissionCriterionCoverageV1,
    #[schemars(
        length(max = 32),
        inner(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))
    )]
    pub required_check_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MissionCriterionCoverageV1 {
    Unknown,
    Uncovered,
    Covered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MissionCheckSummaryV1 {
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub check_id: String,
    pub kind: MissionCheckKindV1,
    pub required: bool,
    #[schemars(
        length(max = 16),
        inner(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))
    )]
    pub covered_criterion_ids: Vec<String>,
    pub status: MissionCheckStatusV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MissionCheckKindV1 {
    Command,
    Manual,
}

/// Mirrors every outcome currently produced by the core proof evaluator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MissionCheckStatusV1 {
    Passed,
    Failed,
    Stale,
    Missing,
    ManualMissing,
    ProviderClaimOnly,
    DeclarationMismatch,
    IdentityMismatch,
    ArtifactMissingOrChanged,
    ManualNotAuthorized,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MissionEvidenceSummaryV1 {
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub check_id: String,
    pub kind: MissionEvidenceKindV1,
    pub assessment: MissionEvidenceAssessmentV1,
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub workspace_digest: String,
    pub recorded_at_millis: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_millis: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[schemars(range(max = 32))]
    pub artifact_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128))]
    pub reviewer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_override: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128))]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MissionEvidenceKindV1 {
    Command,
    Manual,
    ProviderClaim,
}

/// Assessment values for evidence records that actually exist. Missing
/// evidence is represented on the corresponding check summary instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MissionEvidenceAssessmentV1 {
    Passed,
    Failed,
    Stale,
    ProviderClaimOnly,
    DeclarationMismatch,
    IdentityMismatch,
    ArtifactMissingOrChanged,
    ManualNotAuthorized,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MissionViewMigrationError {
    #[error("legacy mission field {field} is outside the V1 contract bounds")]
    InvalidLegacyField { field: &'static str },
    #[error("legacy mission check count exceeds the V1 contract bound")]
    CheckCountOutOfRange,
    #[error("legacy mission attention count exceeds the V1 contract bound")]
    AttentionCountOutOfRange,
}

impl TryFrom<MissionInfo> for MissionViewV1 {
    type Error = MissionViewMigrationError;

    fn try_from(legacy: MissionInfo) -> Result<Self, Self::Error> {
        if !valid_projection_id(&legacy.mission_id) {
            return Err(MissionViewMigrationError::InvalidLegacyField {
                field: "mission_id",
            });
        }
        if !valid_projection_text(&legacy.title, 256) {
            return Err(MissionViewMigrationError::InvalidLegacyField { field: "title" });
        }
        if !valid_projection_text(&legacy.repository_path, 4_096) {
            return Err(MissionViewMigrationError::InvalidLegacyField {
                field: "repository_path",
            });
        }
        if !valid_projection_text(&legacy.objective, 8_192) {
            return Err(MissionViewMigrationError::InvalidLegacyField { field: "objective" });
        }
        if legacy.acceptance_criteria.is_empty()
            || legacy.acceptance_criteria.len() > 16
            || legacy
                .acceptance_criteria
                .iter()
                .any(|criterion| !valid_projection_text(criterion, 1_024))
        {
            return Err(MissionViewMigrationError::InvalidLegacyField {
                field: "acceptance_criteria",
            });
        }
        if legacy.closure_configured != (legacy.check_count > 0) {
            return Err(MissionViewMigrationError::InvalidLegacyField {
                field: "closure_configured",
            });
        }
        if let Some(run) = &legacy.run {
            if !valid_projection_id(&run.run_id) {
                return Err(MissionViewMigrationError::InvalidLegacyField { field: "run_id" });
            }
            if !valid_projection_text(&run.worktree_path, 4_096) {
                return Err(MissionViewMigrationError::InvalidLegacyField {
                    field: "worktree_path",
                });
            }
            if !valid_revision(&run.base_revision) {
                return Err(MissionViewMigrationError::InvalidLegacyField {
                    field: "base_revision",
                });
            }
        }
        let declared_check_count = u32::try_from(legacy.check_count)
            .ok()
            .filter(|count| *count <= 32)
            .ok_or(MissionViewMigrationError::CheckCountOutOfRange)?;
        let unresolved_attention_count = u32::try_from(legacy.unresolved_attention_count)
            .ok()
            .filter(|count| *count <= 100_000)
            .ok_or(MissionViewMigrationError::AttentionCountOutOfRange)?;

        Ok(Self {
            schema_version: ContractVersionV1,
            mission_id: legacy.mission_id,
            title: legacy.title,
            repository_path: legacy.repository_path,
            objective: legacy.objective,
            criteria: legacy
                .acceptance_criteria
                .into_iter()
                .map(|description| MissionCriterionSummaryV1 {
                    criterion_id: None,
                    description,
                    coverage: MissionCriterionCoverageV1::Unknown,
                    required_check_ids: Vec::new(),
                })
                .collect(),
            closure_configured: legacy.closure_configured,
            declared_check_count,
            checks: Vec::new(),
            evidence: Vec::new(),
            evidence_pack_digest: None,
            details_available: false,
            status: legacy.status,
            run: legacy.run.map(|run| MissionRunViewV1 {
                run_id: run.run_id,
                provider: run.provider,
                mode: run.mode,
                worktree_path: run.worktree_path,
                base_revision: run.base_revision,
                execute_declared_checks: run.execute_declared_checks,
                execute_project_recipe: false,
                handoff_from_run_id: None,
                handoff_artifact_sha256: None,
            }),
            run_history: Vec::new(),
            unresolved_attention_count,
            updated_at_millis: legacy.updated_at_millis,
        })
    }
}

fn valid_projection_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
}

fn valid_projection_text(value: &str, max_len: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max_len
}

fn valid_revision(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
