#![allow(
    dead_code,
    reason = "closure evidence is tested but not public until check execution is wired"
)]

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{digest::CanonicalDigest, proof::ProofIdentity};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommandSpec {
    program: String,
    args: Vec<String>,
    cwd: String,
}

impl CommandSpec {
    #[must_use]
    pub fn new<I, S>(program: impl Into<String>, args: I, cwd: impl Into<String>) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let cwd = cwd.into();
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: normalize_contract_path(&cwd, true).unwrap_or(cwd),
        }
    }

    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }

    #[must_use]
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    fn update_digest(&self, digest: &mut CanonicalDigest) {
        digest.string(&self.program);
        digest.u64(self.args.len() as u64);
        for arg in &self.args {
            digest.string(arg);
        }
        digest.string(&self.cwd);
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PathRule {
    All,
    Exact { path: String },
    Prefix { prefix: String },
}

impl PathRule {
    #[must_use]
    pub fn exact(path: impl Into<String>) -> Self {
        let path = path.into();
        Self::Exact {
            path: normalize_contract_path(&path, false).unwrap_or(path),
        }
    }

    #[must_use]
    pub fn prefix(prefix: impl Into<String>) -> Self {
        let prefix = prefix.into();
        match normalize_contract_path(&prefix, true) {
            Ok(prefix) if prefix == "." => Self::All,
            Ok(prefix) => Self::Prefix { prefix },
            Err(()) => Self::Prefix { prefix },
        }
    }

    fn matches(&self, path: &str) -> bool {
        match self {
            Self::All => true,
            Self::Exact { path: expected } => path == expected,
            Self::Prefix { prefix } => {
                let prefix = prefix.trim_end_matches('/');
                !prefix.is_empty()
                    && (path == prefix
                        || path
                            .strip_prefix(prefix)
                            .is_some_and(|suffix| suffix.starts_with('/')))
            }
        }
    }

    fn digest(&self) -> String {
        let mut digest = CanonicalDigest::new(b"mission-path-rule-v1");
        match self {
            Self::All => digest.u8(0),
            Self::Exact { path } => {
                digest.u8(1);
                digest.string(path);
            }
            Self::Prefix { prefix } => {
                digest.u8(2);
                digest.string(prefix);
            }
        }
        digest.finish()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArtifactRequirement {
    path: String,
}

impl ArtifactRequirement {
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        let path = path.into();
        Self {
            path: normalize_contract_path(&path, false).unwrap_or(path),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CheckKind {
    Command { command: CommandSpec },
    Manual,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckDeclaration {
    id: String,
    kind: CheckKind,
    relevant_paths: Vec<PathRule>,
    required_artifacts: Vec<ArtifactRequirement>,
    include_ignored: bool,
    required: bool,
    covered_criteria: BTreeSet<String>,
    allowed_reviewers: BTreeSet<String>,
    allow_manual_override: bool,
}

impl CheckDeclaration {
    #[must_use]
    pub fn command(
        id: impl Into<String>,
        command: CommandSpec,
        relevant_paths: Vec<PathRule>,
        required_artifacts: Vec<ArtifactRequirement>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: CheckKind::Command { command },
            relevant_paths,
            required_artifacts,
            include_ignored: false,
            required: true,
            covered_criteria: BTreeSet::new(),
            allowed_reviewers: BTreeSet::new(),
            allow_manual_override: false,
        }
    }

    #[must_use]
    pub fn manual(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: CheckKind::Manual,
            relevant_paths: Vec::new(),
            required_artifacts: Vec::new(),
            include_ignored: false,
            required: true,
            covered_criteria: BTreeSet::new(),
            allowed_reviewers: BTreeSet::new(),
            allow_manual_override: false,
        }
    }

    #[must_use]
    pub const fn include_ignored(mut self, include_ignored: bool) -> Self {
        self.include_ignored = include_ignored;
        self
    }

    #[must_use]
    pub const fn optional(mut self) -> Self {
        self.required = false;
        self
    }

    #[must_use]
    pub fn covers<I, S>(mut self, criterion_ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.covered_criteria
            .extend(criterion_ids.into_iter().map(Into::into));
        self
    }

    #[must_use]
    pub fn reviewed_by<I, S>(mut self, reviewers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_reviewers
            .extend(reviewers.into_iter().map(Into::into));
        self
    }

    #[must_use]
    pub const fn allow_manual_override(mut self) -> Self {
        self.allow_manual_override = true;
        self
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    fn command_spec(&self) -> Option<&CommandSpec> {
        match &self.kind {
            CheckKind::Command { command } => Some(command),
            CheckKind::Manual => None,
        }
    }

    pub(crate) fn is_manual(&self) -> bool {
        self.command_spec().is_none()
    }

    pub(crate) const fn is_required(&self) -> bool {
        self.required
    }

    pub(crate) fn covered_criteria(&self) -> &BTreeSet<String> {
        &self.covered_criteria
    }

    pub(crate) fn digest(&self) -> String {
        let mut digest = CanonicalDigest::new(b"mission-check-declaration-v1");
        digest.string(&self.id);
        match &self.kind {
            CheckKind::Command { command } => {
                digest.u8(0);
                command.update_digest(&mut digest);
            }
            CheckKind::Manual => digest.u8(1),
        }

        let mut relevant_paths = self
            .relevant_paths
            .iter()
            .map(PathRule::digest)
            .collect::<Vec<_>>();
        relevant_paths.sort_unstable();
        digest.u64(relevant_paths.len() as u64);
        for path in relevant_paths {
            digest.string(&path);
        }

        let mut required_artifacts = self
            .required_artifacts
            .iter()
            .map(|artifact| artifact.path.as_str())
            .collect::<Vec<_>>();
        required_artifacts.sort_unstable();
        digest.u64(required_artifacts.len() as u64);
        for path in required_artifacts {
            digest.string(path);
        }

        digest.bool(self.include_ignored);
        digest.bool(self.required);
        digest.u64(self.covered_criteria.len() as u64);
        for criterion in &self.covered_criteria {
            digest.string(criterion);
        }
        digest.u64(self.allowed_reviewers.len() as u64);
        for reviewer in &self.allowed_reviewers {
            digest.string(reviewer);
        }
        digest.bool(self.allow_manual_override);
        digest.finish()
    }

    pub(crate) fn validate_persisted(&self) -> Result<(), EvidenceError> {
        if !valid_contract_id(&self.id) || self.covered_criteria.len() > 32 {
            return Err(EvidenceError::InvalidCheckDeclaration);
        }
        if self.relevant_paths.len() > 64
            || self.required_artifacts.len() > 32
            || self.allowed_reviewers.len() > 32
        {
            return Err(EvidenceError::InvalidCheckDeclaration);
        }
        if !self.has_canonical_path_contracts() {
            return Err(EvidenceError::InvalidCheckDeclaration);
        }
        if self
            .covered_criteria
            .iter()
            .any(|criterion| !valid_sha256(criterion))
            || self
                .allowed_reviewers
                .iter()
                .any(|reviewer| !valid_contract_id(reviewer))
        {
            return Err(EvidenceError::InvalidCheckDeclaration);
        }
        match &self.kind {
            CheckKind::Command { command } => {
                if command.program.trim().is_empty()
                    || command.program.len() > 1024
                    || command.program.contains('\0')
                    || command.args.len() > 128
                    || command
                        .args
                        .iter()
                        .any(|arg| arg.len() > 4 * 1024 || arg.contains('\0'))
                    || !valid_relative_contract_path(&command.cwd, true)
                    || !self.allowed_reviewers.is_empty()
                    || self.allow_manual_override
                {
                    return Err(EvidenceError::InvalidCheckDeclaration);
                }
            }
            CheckKind::Manual => {
                if !self.relevant_paths.is_empty()
                    || !self.required_artifacts.is_empty()
                    || self.include_ignored
                    || self.allowed_reviewers.is_empty()
                {
                    return Err(EvidenceError::InvalidCheckDeclaration);
                }
            }
        }
        Ok(())
    }

    fn has_canonical_path_contracts(&self) -> bool {
        self.relevant_paths.iter().all(|rule| match rule {
            PathRule::All => true,
            PathRule::Exact { path } => valid_relative_contract_path(path, false),
            PathRule::Prefix { prefix } => {
                prefix != "." && valid_relative_contract_path(prefix, false)
            }
        }) && self
            .required_artifacts
            .iter()
            .all(|artifact| valid_relative_contract_path(&artifact.path, false))
            && self
                .command_spec()
                .is_none_or(|command| valid_relative_contract_path(&command.cwd, true))
    }

    fn reviewer_is_allowed(&self, reviewer: &str, is_override: bool) -> bool {
        self.is_manual()
            && self.allowed_reviewers.contains(reviewer)
            && (!is_override || self.allow_manual_override)
    }
}

fn valid_contract_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_relative_contract_path(value: &str, allow_root: bool) -> bool {
    normalize_contract_path(value, allow_root).is_ok_and(|normalized| normalized == value)
}

/// Produces one platform-independent lexical spelling for a workspace-relative
/// path. Backslashes and colons are rejected so a contract has the same meaning
/// on Unix and Windows. The root is represented only by `.` when it is allowed.
fn normalize_contract_path(value: &str, allow_root: bool) -> Result<String, ()> {
    if value.is_empty()
        || value.len() > 4 * 1024
        || value.contains(['\0', '\\', ':'])
        || value.starts_with('/')
    {
        return Err(());
    }

    let mut components = Vec::new();
    for component in value.split('/') {
        match component {
            "" | "." => {}
            ".." => return Err(()),
            component => components.push(component),
        }
    }

    if components.is_empty() {
        return allow_root.then(|| ".".to_owned()).ok_or(());
    }
    Ok(components.join("/"))
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileDisposition {
    Tracked,
    Staged,
    Unstaged,
    Untracked,
    Ignored,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FileFingerprint {
    path: String,
    content_hash: String,
    disposition: FileDisposition,
}

impl FileFingerprint {
    #[must_use]
    pub fn new(
        path: impl Into<String>,
        content_hash: impl Into<String>,
        disposition: FileDisposition,
    ) -> Self {
        Self {
            path: path.into(),
            content_hash: content_hash.into(),
            disposition,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkspaceSnapshot {
    tree_hash: String,
    diff_hash: String,
    files: BTreeMap<String, FileFingerprint>,
    artifacts: BTreeMap<String, String>,
}

impl WorkspaceSnapshot {
    #[must_use]
    pub fn new(
        tree_hash: impl Into<String>,
        diff_hash: impl Into<String>,
        files: Vec<FileFingerprint>,
    ) -> Self {
        Self {
            tree_hash: tree_hash.into(),
            diff_hash: diff_hash.into(),
            files: files
                .into_iter()
                .map(|file| (file.path.clone(), file))
                .collect(),
            artifacts: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn with_artifacts<I, P, H>(mut self, artifacts: I) -> Self
    where
        I: IntoIterator<Item = (P, H)>,
        P: Into<String>,
        H: Into<String>,
    {
        self.artifacts = artifacts
            .into_iter()
            .map(|(path, hash)| (path.into(), hash.into()))
            .collect();
        self
    }

    fn relevant_files(
        &self,
        rules: &[PathRule],
        include_ignored: bool,
    ) -> BTreeMap<&str, (&str, FileDisposition)> {
        self.files
            .values()
            .filter(|file| include_ignored || file.disposition != FileDisposition::Ignored)
            .filter(|file| rules.is_empty() || rules.iter().any(|rule| rule.matches(&file.path)))
            .map(|file| {
                (
                    file.path.as_str(),
                    (file.content_hash.as_str(), file.disposition),
                )
            })
            .collect()
    }

    pub(crate) fn artifact_hash(&self, path: &str) -> Option<&str> {
        self.artifacts.get(path).map(String::as_str)
    }

    pub(crate) fn digest(&self) -> String {
        let mut digest = CanonicalDigest::new(b"mission-workspace-snapshot-v1");
        digest.string(&self.tree_hash);
        digest.string(&self.diff_hash);
        digest.u64(self.files.len() as u64);
        for (path, file) in &self.files {
            digest.string(path);
            digest.string(&file.content_hash);
            digest.u8(match file.disposition {
                FileDisposition::Tracked => 0,
                FileDisposition::Staged => 1,
                FileDisposition::Unstaged => 2,
                FileDisposition::Untracked => 3,
                FileDisposition::Ignored => 4,
            });
        }
        digest.u64(self.artifacts.len() as u64);
        for (path, hash) in &self.artifacts {
            digest.string(path);
            digest.string(hash);
        }
        digest.finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ArtifactEvidence {
    path: String,
    content_hash: String,
    media_type: String,
}

impl ArtifactEvidence {
    #[must_use]
    pub fn new(
        path: impl Into<String>,
        content_hash: impl Into<String>,
        media_type: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            content_hash: content_hash.into(),
            media_type: media_type.into(),
        }
    }

    #[must_use]
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    Passed,
    Failed,
    Stale,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CommandEvidence {
    check_id: String,
    declaration_digest: String,
    proof_identity_digest: String,
    command: CommandSpec,
    base_tree_hash: String,
    result_tree_hash: String,
    diff_hash: String,
    exit_code: i32,
    started_at_millis: u64,
    finished_at_millis: u64,
    artifacts: Vec<ArtifactEvidence>,
    verified_workspace: WorkspaceSnapshot,
    relevant_paths: Vec<PathRule>,
    include_ignored: bool,
    required_artifacts: Vec<ArtifactRequirement>,
}

impl CommandEvidence {
    /// Records a completed command against the exact post-command workspace.
    ///
    /// # Errors
    ///
    /// Returns an error when the declaration is a manual check or the finish
    /// timestamp precedes the start timestamp.
    pub fn new(
        declaration: &CheckDeclaration,
        identity: &ProofIdentity,
        before: &WorkspaceSnapshot,
        after: &WorkspaceSnapshot,
        exit_code: i32,
        started_at_millis: u64,
        finished_at_millis: u64,
        artifacts: Vec<ArtifactEvidence>,
    ) -> Result<Self, EvidenceError> {
        if !declaration.has_canonical_path_contracts() {
            return Err(EvidenceError::InvalidCheckDeclaration);
        }
        let command = declaration
            .command_spec()
            .ok_or(EvidenceError::ManualCheckCannotHaveCommandEvidence)?;
        if finished_at_millis < started_at_millis {
            return Err(EvidenceError::InvalidDuration);
        }
        for required in &declaration.required_artifacts {
            let recorded = artifacts
                .iter()
                .find(|artifact| artifact.path == required.path)
                .ok_or_else(|| EvidenceError::ArtifactNotInWorkspace(required.path.clone()))?;
            if after.artifact_hash(&required.path) != Some(recorded.content_hash.as_str()) {
                return Err(EvidenceError::ArtifactNotInWorkspace(required.path.clone()));
            }
        }

        Ok(Self {
            check_id: declaration.id.clone(),
            declaration_digest: declaration.digest(),
            proof_identity_digest: identity.digest(),
            command: command.clone(),
            base_tree_hash: before.tree_hash.clone(),
            result_tree_hash: after.tree_hash.clone(),
            diff_hash: after.diff_hash.clone(),
            exit_code,
            started_at_millis,
            finished_at_millis,
            artifacts,
            verified_workspace: after.clone(),
            relevant_paths: declaration.relevant_paths.clone(),
            include_ignored: declaration.include_ignored,
            required_artifacts: declaration.required_artifacts.clone(),
        })
    }

    #[must_use]
    pub fn status_against(&self, current: &WorkspaceSnapshot) -> EvidenceStatus {
        let verified_files = self
            .verified_workspace
            .relevant_files(&self.relevant_paths, self.include_ignored);
        let current_files = current.relevant_files(&self.relevant_paths, self.include_ignored);
        if verified_files != current_files {
            return EvidenceStatus::Stale;
        }

        let artifacts_present = self.required_artifacts.iter().all(|required| {
            self.artifacts.iter().any(|artifact| {
                artifact.path == required.path
                    && current.artifact_hash(&required.path) == Some(artifact.content_hash.as_str())
            })
        });
        if self.exit_code == 0 && artifacts_present {
            EvidenceStatus::Passed
        } else {
            EvidenceStatus::Failed
        }
    }

    #[must_use]
    pub fn check_id(&self) -> &str {
        &self.check_id
    }

    #[must_use]
    pub fn command(&self) -> &CommandSpec {
        &self.command
    }

    #[must_use]
    pub fn base_tree_hash(&self) -> &str {
        &self.base_tree_hash
    }

    #[must_use]
    pub fn result_tree_hash(&self) -> &str {
        &self.result_tree_hash
    }

    #[must_use]
    pub fn diff_hash(&self) -> &str {
        &self.diff_hash
    }

    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        self.exit_code
    }

    #[must_use]
    pub const fn duration_millis(&self) -> u64 {
        self.finished_at_millis - self.started_at_millis
    }

    #[must_use]
    pub fn artifacts(&self) -> &[ArtifactEvidence] {
        &self.artifacts
    }

    fn assess(
        &self,
        declaration: &CheckDeclaration,
        identity: &ProofIdentity,
        current: &WorkspaceSnapshot,
    ) -> EvidenceAssessment {
        if self.check_id != declaration.id || self.declaration_digest != declaration.digest() {
            return EvidenceAssessment::DeclarationMismatch;
        }
        if self.proof_identity_digest != identity.digest() {
            return EvidenceAssessment::IdentityMismatch;
        }
        match self.status_against(current) {
            EvidenceStatus::Passed => EvidenceAssessment::Passed,
            EvidenceStatus::Failed => {
                let artifact_changed = self.required_artifacts.iter().any(|required| {
                    self.artifacts.iter().all(|artifact| {
                        artifact.path != required.path
                            || current.artifact_hash(&required.path)
                                != Some(artifact.content_hash.as_str())
                    })
                });
                if artifact_changed {
                    EvidenceAssessment::ArtifactMissingOrChanged
                } else {
                    EvidenceAssessment::Failed
                }
            }
            EvidenceStatus::Stale => EvidenceAssessment::Stale,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ManualEvidence {
    check_id: String,
    declaration_digest: String,
    proof_identity_digest: String,
    workspace_digest: String,
    author: String,
    recorded_at_millis: u64,
    reason: String,
    is_override: bool,
}

impl ManualEvidence {
    pub fn new(
        declaration: &CheckDeclaration,
        identity: &ProofIdentity,
        workspace: &WorkspaceSnapshot,
        author: impl Into<String>,
        recorded_at_millis: u64,
        reason: impl Into<String>,
        is_override: bool,
    ) -> Result<Self, EvidenceError> {
        let author = author.into();
        let reason = reason.into();
        if author.trim().is_empty() {
            return Err(EvidenceError::EmptyReviewer);
        }
        if reason.trim().is_empty() {
            return Err(EvidenceError::EmptyManualReason);
        }
        if !declaration.reviewer_is_allowed(&author, is_override) {
            return Err(EvidenceError::ReviewerNotAllowed);
        }

        Ok(Self {
            check_id: declaration.id.clone(),
            declaration_digest: declaration.digest(),
            proof_identity_digest: identity.digest(),
            workspace_digest: workspace.digest(),
            author,
            recorded_at_millis,
            reason,
            is_override,
        })
    }

    #[must_use]
    pub fn check_id(&self) -> &str {
        &self.check_id
    }

    #[must_use]
    pub fn author(&self) -> &str {
        &self.author
    }

    #[must_use]
    pub const fn recorded_at_millis(&self) -> u64 {
        self.recorded_at_millis
    }

    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }

    #[must_use]
    pub const fn is_override(&self) -> bool {
        self.is_override
    }

    fn assess(
        &self,
        declaration: &CheckDeclaration,
        identity: &ProofIdentity,
        current: &WorkspaceSnapshot,
    ) -> EvidenceAssessment {
        if self.check_id != declaration.id || self.declaration_digest != declaration.digest() {
            return EvidenceAssessment::DeclarationMismatch;
        }
        if self.proof_identity_digest != identity.digest() {
            return EvidenceAssessment::IdentityMismatch;
        }
        if self.workspace_digest != current.digest() {
            return EvidenceAssessment::Stale;
        }
        if !declaration.reviewer_is_allowed(&self.author, self.is_override) {
            return EvidenceAssessment::ManualNotAuthorized;
        }
        EvidenceAssessment::Passed
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProviderClaim {
    check_id: String,
    claim: String,
    source: String,
    recorded_at_millis: u64,
}

impl ProviderClaim {
    #[must_use]
    pub fn new(
        check_id: impl Into<String>,
        claim: impl Into<String>,
        source: impl Into<String>,
        recorded_at_millis: u64,
    ) -> Self {
        Self {
            check_id: check_id.into(),
            claim: claim.into(),
            source: source.into(),
            recorded_at_millis,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EvidenceRecord {
    Command(Box<CommandEvidence>),
    Manual(ManualEvidence),
    ProviderClaim(ProviderClaim),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EvidenceAssessment {
    Passed,
    Failed,
    Stale,
    DeclarationMismatch,
    IdentityMismatch,
    ArtifactMissingOrChanged,
    ManualNotAuthorized,
    ProviderClaimOnly,
}

impl EvidenceRecord {
    pub(crate) fn assess(
        &self,
        declaration: &CheckDeclaration,
        identity: &ProofIdentity,
        current: &WorkspaceSnapshot,
    ) -> EvidenceAssessment {
        match self {
            Self::Command(evidence) if !declaration.is_manual() => {
                evidence.assess(declaration, identity, current)
            }
            Self::Manual(evidence) if declaration.is_manual() => {
                evidence.assess(declaration, identity, current)
            }
            Self::ProviderClaim(_) => EvidenceAssessment::ProviderClaimOnly,
            Self::Command(_) | Self::Manual(_) => EvidenceAssessment::DeclarationMismatch,
        }
    }

    pub(crate) fn digest(&self) -> String {
        let mut digest = CanonicalDigest::new(b"mission-evidence-record-v1");
        match self {
            Self::Command(evidence) => {
                digest.u8(0);
                digest.string(&evidence.check_id);
                digest.string(&evidence.declaration_digest);
                digest.string(&evidence.proof_identity_digest);
                evidence.command.update_digest(&mut digest);
                digest.string(&evidence.base_tree_hash);
                digest.string(&evidence.result_tree_hash);
                digest.string(&evidence.diff_hash);
                digest.i32(evidence.exit_code);
                digest.u64(evidence.started_at_millis);
                digest.u64(evidence.finished_at_millis);

                let mut artifacts = evidence
                    .artifacts
                    .iter()
                    .map(|artifact| {
                        let mut artifact_digest =
                            CanonicalDigest::new(b"mission-artifact-evidence-v1");
                        artifact_digest.string(&artifact.path);
                        artifact_digest.string(&artifact.content_hash);
                        artifact_digest.string(&artifact.media_type);
                        artifact_digest.finish()
                    })
                    .collect::<Vec<_>>();
                artifacts.sort_unstable();
                digest.u64(artifacts.len() as u64);
                for artifact in artifacts {
                    digest.string(&artifact);
                }

                digest.string(&evidence.verified_workspace.digest());
                let mut relevant_paths = evidence
                    .relevant_paths
                    .iter()
                    .map(PathRule::digest)
                    .collect::<Vec<_>>();
                relevant_paths.sort_unstable();
                digest.u64(relevant_paths.len() as u64);
                for path in relevant_paths {
                    digest.string(&path);
                }
                digest.bool(evidence.include_ignored);

                let mut required_artifacts = evidence
                    .required_artifacts
                    .iter()
                    .map(|artifact| artifact.path.as_str())
                    .collect::<Vec<_>>();
                required_artifacts.sort_unstable();
                digest.u64(required_artifacts.len() as u64);
                for path in required_artifacts {
                    digest.string(path);
                }
            }
            Self::Manual(evidence) => {
                digest.u8(1);
                digest.string(&evidence.check_id);
                digest.string(&evidence.declaration_digest);
                digest.string(&evidence.proof_identity_digest);
                digest.string(&evidence.workspace_digest);
                digest.string(&evidence.author);
                digest.u64(evidence.recorded_at_millis);
                digest.string(&evidence.reason);
                digest.bool(evidence.is_override);
            }
            Self::ProviderClaim(claim) => {
                digest.u8(2);
                digest.string(&claim.check_id);
                digest.string(&claim.claim);
                digest.string(&claim.source);
                digest.u64(claim.recorded_at_millis);
            }
        }
        digest.finish()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionReadiness {
    ReviewRequired,
    Verified,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum EvidenceError {
    #[error("check declaration is invalid or unsafe to persist")]
    InvalidCheckDeclaration,
    #[error("manual check cannot have command evidence")]
    ManualCheckCannotHaveCommandEvidence,
    #[error("evidence finish timestamp precedes its start timestamp")]
    InvalidDuration,
    #[error("required artifact is not present in the verified workspace: {0}")]
    ArtifactNotInWorkspace(String),
    #[error("manual reviewer cannot be empty")]
    EmptyReviewer,
    #[error("manual evidence reason cannot be empty")]
    EmptyManualReason,
    #[error("manual reviewer or override is not allowed by this check")]
    ReviewerNotAllowed,
}
