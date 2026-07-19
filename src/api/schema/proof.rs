use serde::{Deserialize, Serialize};

use super::ContractVersionV1;

/// Portable receipt minted only after the core verifier has bound fresh
/// evidence to one mission run, worktree and authority snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProofReceiptV1 {
    pub schema_version: ContractVersionV1,
    pub identity: ProofIdentityV1,
    #[schemars(regex(pattern = r"^(?:[0-9A-Fa-f]{40}|[0-9A-Fa-f]{64})$"))]
    pub head_revision: String,
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub workspace_digest: String,
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub criteria_digest: String,
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub checkset_digest: String,
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub attention_digest: String,
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub evidence_digest: String,
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub subject_digest: String,
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub seal_digest: String,
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub authority_head_digest: String,
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub lease_digest: String,
    #[schemars(range(min = 1))]
    pub authority_sequence: u64,
    pub verified_at_millis: u64,
    pub decision: ProofClosureDecisionV1,
    #[schemars(length(min = 1, max = 32))]
    pub fresh_evidence: Vec<ProofEvidenceV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProofIdentityV1 {
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub mission_id: String,
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub run_id: String,
    #[schemars(length(min = 1, max = 4_096))]
    pub repository_identity: String,
    #[schemars(length(min = 1, max = 4_096))]
    pub worktree_identity: String,
    #[schemars(regex(pattern = r"^(?:[0-9A-Fa-f]{40}|[0-9A-Fa-f]{64})$"))]
    pub base_revision: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProofClosureDecisionV1 {
    ReadyToClose,
    Archived,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProofEvidenceV1 {
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub check_id: String,
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub declaration_digest: String,
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub evidence_digest: String,
    pub required: bool,
    #[schemars(
        length(max = 16),
        inner(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))
    )]
    pub covered_criterion_ids: Vec<String>,
    pub source: ProofEvidenceSourceV1,
    #[schemars(length(max = 32))]
    pub artifacts: Vec<ProofArtifactV1>,
}

/// Only command and authorized manual evidence can appear in a verified
/// receipt. Provider claims are intentionally absent because the proof
/// evaluator never accepts them as closure evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProofEvidenceSourceV1 {
    Command {
        #[schemars(length(min = 1, max = 1_024))]
        program: String,
        #[schemars(length(max = 128), inner(length(max = 4_096)))]
        args: Vec<String>,
        #[schemars(length(min = 1, max = 4_096))]
        cwd: String,
        exit_code: i32,
        started_at_millis: u64,
        finished_at_millis: u64,
    },
    Manual {
        #[schemars(length(min = 1, max = 128))]
        reviewer: String,
        #[schemars(length(min = 1, max = 4_096))]
        reason: String,
        is_override: bool,
        recorded_at_millis: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProofArtifactV1 {
    #[schemars(length(min = 1, max = 4_096))]
    pub path: String,
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub content_digest: String,
    #[schemars(length(min = 1, max = 256))]
    pub media_type: String,
}
