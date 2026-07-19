#![allow(
    dead_code,
    reason = "closure proofs are tested but not public until check execution is wired"
)]

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    digest::CanonicalDigest,
    evidence::{
        CheckDeclaration, EvidenceAssessment, EvidenceRecord, MissionReadiness, WorkspaceSnapshot,
    },
    runtime::AuthoritySnapshot,
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProofIdentity {
    mission_id: String,
    run_id: String,
    repository_identity: String,
    worktree_identity: String,
    base_revision: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ClosurePlan {
    criteria: BTreeSet<String>,
    criteria_digest: String,
    checkset_digest: String,
}

impl ClosurePlan {
    pub(crate) fn new(
        criterion_ids: &[String],
        declarations: &[CheckDeclaration],
    ) -> Result<Self, ProofError> {
        let criteria = criterion_ids.iter().cloned().collect::<BTreeSet<_>>();
        if criteria.is_empty() {
            return Err(ProofError::InvalidClosurePlan);
        }

        let mut seen = BTreeSet::new();
        let mut covered = BTreeSet::new();
        let mut required_count = 0;
        for declaration in declarations {
            if !seen.insert(declaration.id()) {
                return Err(ProofError::InvalidClosurePlan);
            }
            if !declaration
                .covered_criteria()
                .iter()
                .all(|criterion| criteria.contains(criterion))
            {
                return Err(ProofError::InvalidClosurePlan);
            }
            if declaration.is_required() {
                required_count += 1;
                covered.extend(declaration.covered_criteria().iter().cloned());
            }
        }
        if required_count == 0 || covered != criteria {
            return Err(ProofError::InvalidClosurePlan);
        }

        Ok(Self {
            criteria: criteria.clone(),
            criteria_digest: digest_criteria(&criteria),
            checkset_digest: digest_checkset(declarations),
        })
    }

    fn matches(&self, proof: &VerifiedProof) -> bool {
        self.criteria_digest == proof.criteria_digest
            && self.checkset_digest == proof.checkset_digest
    }

    fn matches_declarations(&self, declarations: &[CheckDeclaration]) -> bool {
        self.checkset_digest == digest_checkset(declarations)
    }

    pub(crate) fn criteria_digest(&self) -> &str {
        &self.criteria_digest
    }

    pub(crate) fn checkset_digest(&self) -> &str {
        &self.checkset_digest
    }
}

impl ProofIdentity {
    pub fn new(
        mission_id: impl Into<String>,
        run_id: impl Into<String>,
        repository_identity: impl Into<String>,
        worktree_identity: impl Into<String>,
        base_revision: impl Into<String>,
    ) -> Result<Self, ProofError> {
        let identity = Self {
            mission_id: mission_id.into(),
            run_id: run_id.into(),
            repository_identity: repository_identity.into(),
            worktree_identity: worktree_identity.into(),
            base_revision: base_revision.into(),
        };
        if [
            identity.mission_id.as_str(),
            identity.run_id.as_str(),
            identity.repository_identity.as_str(),
            identity.worktree_identity.as_str(),
        ]
        .iter()
        .any(|value| value.trim().is_empty())
        {
            return Err(ProofError::EmptyIdentity);
        }
        if !matches!(identity.base_revision.len(), 40 | 64)
            || !identity
                .base_revision
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ProofError::InvalidBaseRevision);
        }
        Ok(identity)
    }

    pub(crate) fn digest(&self) -> String {
        let mut digest = CanonicalDigest::new(b"mission-proof-identity-v1");
        digest.string(&self.mission_id);
        digest.string(&self.run_id);
        digest.string(&self.repository_identity);
        digest.string(&self.worktree_identity);
        digest.string(&self.base_revision);
        digest.finish()
    }

    pub(crate) fn mission_id(&self) -> &str {
        &self.mission_id
    }

    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }

    pub(crate) fn worktree_identity(&self) -> &str {
        &self.worktree_identity
    }

    pub(crate) fn repository_identity(&self) -> &str {
        &self.repository_identity
    }

    pub(crate) fn base_revision(&self) -> &str {
        &self.base_revision
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckProofStatus {
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

impl From<EvidenceAssessment> for CheckProofStatus {
    fn from(value: EvidenceAssessment) -> Self {
        match value {
            EvidenceAssessment::Passed => Self::Passed,
            EvidenceAssessment::Failed => Self::Failed,
            EvidenceAssessment::Stale => Self::Stale,
            EvidenceAssessment::DeclarationMismatch => Self::DeclarationMismatch,
            EvidenceAssessment::IdentityMismatch => Self::IdentityMismatch,
            EvidenceAssessment::ArtifactMissingOrChanged => Self::ArtifactMissingOrChanged,
            EvidenceAssessment::ManualNotAuthorized => Self::ManualNotAuthorized,
            EvidenceAssessment::ProviderClaimOnly => Self::ProviderClaimOnly,
        }
    }
}

#[derive(Debug)]
pub struct ProofReport {
    readiness: MissionReadiness,
    check_statuses: BTreeMap<String, CheckProofStatus>,
    uncovered_criteria: BTreeSet<String>,
    duplicate_check_ids: BTreeSet<String>,
    verified: Option<VerifiedProof>,
}

impl ProofReport {
    #[must_use]
    pub const fn readiness(&self) -> MissionReadiness {
        self.readiness
    }

    #[must_use]
    pub fn check_status(&self, check_id: &str) -> Option<CheckProofStatus> {
        self.check_statuses.get(check_id).copied()
    }

    #[must_use]
    pub fn uncovered_criteria(&self) -> Vec<&str> {
        self.uncovered_criteria.iter().map(String::as_str).collect()
    }

    #[must_use]
    pub fn duplicate_check_ids(&self) -> Vec<&str> {
        self.duplicate_check_ids
            .iter()
            .map(String::as_str)
            .collect()
    }

    pub fn into_verified(self) -> Result<VerifiedProof, ProofError> {
        self.verified.ok_or(ProofError::ReportNotVerified)
    }
}

#[derive(Debug)]
pub struct VerifiedProof {
    identity_digest: String,
    workspace_digest: String,
    checkset_digest: String,
    criteria_digest: String,
    attention_digest: String,
    evidence_digest: String,
    authority_head_digest: String,
    lease_digest: String,
    authority_sequence: u64,
    verified_at_millis: u64,
}

impl VerifiedProof {
    pub(crate) fn matches_identity(&self, identity: &ProofIdentity) -> bool {
        self.identity_digest == identity.digest()
    }

    pub(crate) fn matches_closure_plan(&self, plan: &ClosurePlan) -> bool {
        plan.matches(self)
    }

    pub(crate) const fn verified_at_millis(&self) -> u64 {
        self.verified_at_millis
    }

    pub(crate) const fn authority_sequence(&self) -> u64 {
        self.authority_sequence
    }

    pub(crate) fn authority_head_digest(&self) -> &str {
        &self.authority_head_digest
    }

    pub(crate) fn lease_digest(&self) -> &str {
        &self.lease_digest
    }

    pub(crate) fn seal_digest(&self) -> String {
        seal_digest_from_parts(
            &self.identity_digest,
            &self.workspace_digest,
            &self.checkset_digest,
            &self.criteria_digest,
            &self.attention_digest,
            &self.evidence_digest,
            &self.authority_head_digest,
            &self.lease_digest,
            self.authority_sequence,
            self.verified_at_millis,
        )
    }

    pub(crate) fn subject_digest(&self) -> String {
        subject_digest_from_parts(
            &self.identity_digest,
            &self.workspace_digest,
            &self.checkset_digest,
            &self.criteria_digest,
        )
    }
}

pub(crate) fn proof_subject_digest(
    identity: &ProofIdentity,
    workspace: &WorkspaceSnapshot,
    closure_plan: &ClosurePlan,
) -> String {
    subject_digest_from_parts(
        &identity.digest(),
        &workspace.digest(),
        closure_plan.checkset_digest(),
        closure_plan.criteria_digest(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn proof_seal_digest(
    identity: &ProofIdentity,
    workspace: &WorkspaceSnapshot,
    closure_plan: &ClosurePlan,
    attention_digest: &str,
    evidence_digest: &str,
    authority_head_digest: &str,
    lease_digest: &str,
    authority_sequence: u64,
    verified_at_millis: u64,
) -> String {
    seal_digest_from_parts(
        &identity.digest(),
        &workspace.digest(),
        closure_plan.checkset_digest(),
        closure_plan.criteria_digest(),
        attention_digest,
        evidence_digest,
        authority_head_digest,
        lease_digest,
        authority_sequence,
        verified_at_millis,
    )
}

#[allow(clippy::too_many_arguments)]
fn seal_digest_from_parts(
    identity_digest: &str,
    workspace_digest: &str,
    checkset_digest: &str,
    criteria_digest: &str,
    attention_digest: &str,
    evidence_digest: &str,
    authority_head_digest: &str,
    lease_digest: &str,
    authority_sequence: u64,
    verified_at_millis: u64,
) -> String {
    let mut digest = CanonicalDigest::new(b"mission-verified-proof-seal-v1");
    digest.string(identity_digest);
    digest.string(workspace_digest);
    digest.string(checkset_digest);
    digest.string(criteria_digest);
    digest.string(attention_digest);
    digest.string(evidence_digest);
    digest.string(authority_head_digest);
    digest.string(lease_digest);
    digest.u64(authority_sequence);
    digest.u64(verified_at_millis);
    digest.finish()
}

fn subject_digest_from_parts(
    identity_digest: &str,
    workspace_digest: &str,
    checkset_digest: &str,
    criteria_digest: &str,
) -> String {
    let mut digest = CanonicalDigest::new(b"mission-verified-proof-subject-v1");
    digest.string(identity_digest);
    digest.string(workspace_digest);
    digest.string(checkset_digest);
    digest.string(criteria_digest);
    digest.finish()
}

pub struct ProofEvaluator;

impl ProofEvaluator {
    #[allow(clippy::too_many_arguments)]
    pub fn evaluate(
        identity: &ProofIdentity,
        closure_plan: &ClosurePlan,
        declarations: &[CheckDeclaration],
        records: &BTreeMap<String, EvidenceRecord>,
        current: &WorkspaceSnapshot,
        unresolved_attention_ids: &BTreeSet<String>,
        authority: &AuthoritySnapshot,
    ) -> Result<ProofReport, ProofError> {
        if authority.sequence() == 0 {
            return Err(ProofError::InvalidAuthoritySequence);
        }
        let evidence_digest = digest_evidence(records);
        if authority.identity_digest() != identity.digest()
            || authority.workspace_digest() != current.digest()
            || authority.attention_digest() != digest_attention(unresolved_attention_ids)
            || authority.evidence_digest() != evidence_digest
        {
            return Err(ProofError::AuthorityBindingMismatch);
        }
        let criteria = closure_plan.criteria.clone();
        let mut covered_criteria = BTreeSet::new();
        let mut check_statuses = BTreeMap::new();
        let mut seen_check_ids = BTreeSet::new();
        let mut duplicate_check_ids = BTreeSet::new();
        let mut required_count = 0;

        for declaration in declarations {
            if !seen_check_ids.insert(declaration.id().to_owned()) {
                duplicate_check_ids.insert(declaration.id().to_owned());
                continue;
            }
            if declaration.is_required() {
                required_count += 1;
                covered_criteria.extend(declaration.covered_criteria().iter().cloned());
            }

            let status = match records.get(declaration.id()) {
                Some(record) => record.assess(declaration, identity, current).into(),
                None if declaration.is_manual() => CheckProofStatus::ManualMissing,
                None => CheckProofStatus::Missing,
            };
            check_statuses.insert(declaration.id().to_owned(), status);
        }

        let uncovered_criteria = criteria
            .difference(&covered_criteria)
            .cloned()
            .collect::<BTreeSet<_>>();
        let all_required_pass = declarations
            .iter()
            .filter(|declaration| declaration.is_required())
            .all(|declaration| {
                check_statuses.get(declaration.id()) == Some(&CheckProofStatus::Passed)
            });
        let configuration_valid = required_count > 0
            && !criteria.is_empty()
            && uncovered_criteria.is_empty()
            && duplicate_check_ids.is_empty()
            && closure_plan.matches_declarations(declarations);
        let is_verified =
            configuration_valid && all_required_pass && unresolved_attention_ids.is_empty();

        let verified = is_verified.then(|| VerifiedProof {
            identity_digest: identity.digest(),
            workspace_digest: current.digest(),
            checkset_digest: closure_plan.checkset_digest.clone(),
            criteria_digest: closure_plan.criteria_digest.clone(),
            attention_digest: digest_attention(unresolved_attention_ids),
            evidence_digest,
            authority_head_digest: authority.head_digest().to_owned(),
            lease_digest: authority.lease_digest().to_owned(),
            authority_sequence: authority.sequence(),
            verified_at_millis: authority.captured_at_millis(),
        });

        Ok(ProofReport {
            readiness: if is_verified {
                MissionReadiness::Verified
            } else {
                MissionReadiness::ReviewRequired
            },
            check_statuses,
            uncovered_criteria,
            duplicate_check_ids,
            verified,
        })
    }
}

fn digest_checkset(declarations: &[CheckDeclaration]) -> String {
    let mut declarations = declarations
        .iter()
        .map(CheckDeclaration::digest)
        .collect::<Vec<_>>();
    declarations.sort_unstable();
    let mut digest = CanonicalDigest::new(b"mission-checkset-v1");
    digest.u64(declarations.len() as u64);
    for declaration in declarations {
        digest.string(&declaration);
    }
    digest.finish()
}

fn digest_criteria(criteria: &BTreeSet<String>) -> String {
    let mut digest = CanonicalDigest::new(b"mission-criteria-v1");
    digest.u64(criteria.len() as u64);
    for criterion in criteria {
        digest.string(criterion);
    }
    digest.finish()
}

pub(crate) fn digest_attention(attention_ids: &BTreeSet<String>) -> String {
    let mut digest = CanonicalDigest::new(b"mission-unresolved-attention-v1");
    digest.u64(attention_ids.len() as u64);
    for item_id in attention_ids {
        digest.string(item_id);
    }
    digest.finish()
}

pub(crate) fn digest_evidence(records: &BTreeMap<String, EvidenceRecord>) -> String {
    let mut digest = CanonicalDigest::new(b"mission-evidence-set-v1");
    digest.u64(records.len() as u64);
    for (check_id, record) in records {
        digest.string(check_id);
        digest.string(&record.digest());
    }
    digest.finish()
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ProofError {
    #[error("proof identity fields cannot be empty")]
    EmptyIdentity,
    #[error("proof base revision must be a full hexadecimal object id")]
    InvalidBaseRevision,
    #[error("proof report is not verified")]
    ReportNotVerified,
    #[error("closure plan must cover every mission criterion with unique required checks")]
    InvalidClosurePlan,
    #[error("proof authority sequence must be greater than zero")]
    InvalidAuthoritySequence,
    #[error("proof inputs do not match the runtime authority snapshot")]
    AuthorityBindingMismatch,
}
