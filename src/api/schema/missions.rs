use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MissionProviderMode {
    Managed,
    Passthrough,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MissionRunInfo {
    pub run_id: String,
    pub provider: MissionProvider,
    pub mode: MissionProviderMode,
    pub worktree_path: String,
    pub base_revision: String,
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
