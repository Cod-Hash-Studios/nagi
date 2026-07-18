use std::{
    collections::BTreeMap,
    fmt,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::mission::{
    attention::AttentionDecision,
    model::{ProviderKind, ProviderMode},
};

pub(super) const CONSENT_TTL: Duration = Duration::from_secs(30);

const DIGEST_PREFIX: &[u8] = b"server-consent-subject-canonical-v1";

pub(super) struct RespondConsentContext {
    mission_id: String,
    run_id: String,
    attention_id: String,
    provider_request_id: String,
    request_generation: u64,
}

impl RespondConsentContext {
    pub(super) fn new(
        mission_id: impl Into<String>,
        run_id: impl Into<String>,
        attention_id: impl Into<String>,
        provider_request_id: impl Into<String>,
        request_generation: u64,
    ) -> Self {
        Self {
            mission_id: mission_id.into(),
            run_id: run_id.into(),
            attention_id: attention_id.into(),
            provider_request_id: provider_request_id.into(),
            request_generation,
        }
    }
}

pub(super) struct ConsentSubject {
    client_id: u64,
    kind: ConsentSubjectKind,
}

enum ConsentSubjectKind {
    StartWorkspaceWrite {
        mission_id: String,
        run_id: String,
        provider: ProviderKind,
        mode: ProviderMode,
        canonical_worktree: String,
    },
    Respond {
        context: RespondConsentContext,
        decision: AttentionDecision,
        answers_digest: [u8; 32],
    },
}

impl ConsentSubject {
    pub(super) fn start_workspace_write(
        client_id: u64,
        mission_id: impl Into<String>,
        run_id: impl Into<String>,
        provider: ProviderKind,
        mode: ProviderMode,
        worktree: &Path,
    ) -> Result<Self, ConsentSubjectError> {
        let canonical_worktree = worktree
            .canonicalize()
            .map_err(|_| ConsentSubjectError::WorktreeResolutionFailed)?;
        let canonical_worktree = canonical_worktree
            .to_str()
            .ok_or(ConsentSubjectError::NonUnicodeWorktree)?
            .to_owned();
        Ok(Self {
            client_id,
            kind: ConsentSubjectKind::StartWorkspaceWrite {
                mission_id: mission_id.into(),
                run_id: run_id.into(),
                provider,
                mode,
                canonical_worktree,
            },
        })
    }

    pub(super) fn respond(
        client_id: u64,
        context: RespondConsentContext,
        decision: AttentionDecision,
        answers: &BTreeMap<String, Vec<String>>,
    ) -> Self {
        Self {
            client_id,
            kind: ConsentSubjectKind::Respond {
                context,
                decision,
                answers_digest: canonical_answers_digest(answers),
            },
        }
    }

    fn digest(&self) -> [u8; 32] {
        match &self.kind {
            ConsentSubjectKind::StartWorkspaceWrite {
                mission_id,
                run_id,
                provider,
                mode,
                canonical_worktree,
            } => {
                let mut digest =
                    CanonicalSubjectDigest::new(b"server-consent-start-workspace-write-v2");
                digest.u64(self.client_id);
                digest.string(mission_id);
                digest.string(run_id);
                digest.u8(provider_tag(*provider));
                digest.u8(mode_tag(*mode));
                digest.string(canonical_worktree);
                digest.finish()
            }
            ConsentSubjectKind::Respond {
                context,
                decision,
                answers_digest,
            } => {
                let mut digest = CanonicalSubjectDigest::new(b"server-consent-respond-v2");
                digest.u64(self.client_id);
                digest.string(&context.mission_id);
                digest.string(&context.run_id);
                digest.string(&context.attention_id);
                digest.string(&context.provider_request_id);
                digest.u64(context.request_generation);
                digest.u8(decision_tag(*decision));
                digest.bytes(answers_digest);
                digest.finish()
            }
        }
    }
}

impl fmt::Debug for ConsentSubject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ConsentSubjectKind::StartWorkspaceWrite {
                mission_id,
                run_id,
                provider,
                mode,
                canonical_worktree,
            } => formatter
                .debug_struct("StartWorkspaceWrite")
                .field("client_id", &self.client_id)
                .field("mission_id", mission_id)
                .field("run_id", run_id)
                .field("provider", provider)
                .field("mode", mode)
                .field("canonical_worktree", canonical_worktree)
                .finish(),
            ConsentSubjectKind::Respond {
                context,
                decision,
                answers_digest,
            } => formatter
                .debug_struct("Respond")
                .field("client_id", &self.client_id)
                .field("mission_id", &context.mission_id)
                .field("run_id", &context.run_id)
                .field("attention_id", &context.attention_id)
                .field("provider_request_id", &context.provider_request_id)
                .field("request_generation", &context.request_generation)
                .field("decision", decision)
                .field("answers_digest", &Redacted(answers_digest))
                .finish(),
        }
    }
}

pub(super) struct ConsentAuthority {
    server_epoch: [u8; 32],
}

impl ConsentAuthority {
    pub(super) fn new() -> Result<Self, ConsentError> {
        let mut server_epoch = [0_u8; 32];
        getrandom::fill(&mut server_epoch).map_err(|_| ConsentError::RandomUnavailable)?;
        Ok(Self { server_epoch })
    }

    pub(super) fn issue(&self, subject: &ConsentSubject) -> Result<ConsentGrant, ConsentError> {
        let mut nonce = [0_u8; 32];
        getrandom::fill(&mut nonce).map_err(|_| ConsentError::RandomUnavailable)?;
        let expires_at = Instant::now()
            .checked_add(CONSENT_TTL)
            .ok_or(ConsentError::ClockOverflow)?;
        Ok(ConsentGrant {
            nonce,
            server_epoch: self.server_epoch,
            subject_digest: subject.digest(),
            expires_at,
            used: AtomicBool::new(false),
        })
    }

    #[cfg(test)]
    fn issue_at(
        &self,
        subject: &ConsentSubject,
        now: Instant,
    ) -> Result<ConsentGrant, ConsentError> {
        let mut nonce = [0_u8; 32];
        getrandom::fill(&mut nonce).map_err(|_| ConsentError::RandomUnavailable)?;
        let expires_at = now
            .checked_add(CONSENT_TTL)
            .ok_or(ConsentError::ClockOverflow)?;
        Ok(ConsentGrant {
            nonce,
            server_epoch: self.server_epoch,
            subject_digest: subject.digest(),
            expires_at,
            used: AtomicBool::new(false),
        })
    }

    #[cfg(test)]
    fn issue_with_nonce_at(
        &self,
        subject: &ConsentSubject,
        now: Instant,
        nonce: [u8; 32],
    ) -> Result<ConsentGrant, ConsentError> {
        let expires_at = now
            .checked_add(CONSENT_TTL)
            .ok_or(ConsentError::ClockOverflow)?;
        Ok(ConsentGrant {
            nonce,
            server_epoch: self.server_epoch,
            subject_digest: subject.digest(),
            expires_at,
            used: AtomicBool::new(false),
        })
    }

    pub(super) fn consume(
        &self,
        grant: &ConsentGrant,
        expected_subject: &ConsentSubject,
    ) -> Result<(), ConsentError> {
        self.consume_with_expiry(grant, expected_subject, Instant::now() >= grant.expires_at)
    }

    #[cfg(test)]
    fn consume_at(
        &self,
        grant: &ConsentGrant,
        expected_subject: &ConsentSubject,
        now: Instant,
    ) -> Result<(), ConsentError> {
        self.consume_with_expiry(grant, expected_subject, now >= grant.expires_at)
    }

    fn consume_with_expiry(
        &self,
        grant: &ConsentGrant,
        expected_subject: &ConsentSubject,
        expired: bool,
    ) -> Result<(), ConsentError> {
        grant
            .used
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| ConsentError::AlreadyUsed)?;

        if grant.server_epoch != self.server_epoch {
            return Err(ConsentError::ServerEpochMismatch);
        }
        if expired {
            return Err(ConsentError::Expired);
        }
        if grant.subject_digest != expected_subject.digest() {
            return Err(ConsentError::SubjectMismatch);
        }
        Ok(())
    }
}

pub(super) struct ConsentGrant {
    nonce: [u8; 32],
    server_epoch: [u8; 32],
    subject_digest: [u8; 32],
    expires_at: Instant,
    used: AtomicBool,
}

impl fmt::Debug for ConsentGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConsentGrant")
            .field("nonce", &Redacted(&self.nonce))
            .field("server_epoch", &Redacted(&self.server_epoch))
            .field("subject_digest", &Redacted(&self.subject_digest))
            .field("expires_at", &self.expires_at)
            .field("used", &self.used.load(Ordering::Acquire))
            .finish()
    }
}

struct Redacted<'a, T>(&'a T);

impl<T> fmt::Debug for Redacted<'_, T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = self.0;
        formatter.write_str("[REDACTED]")
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(super) enum ConsentSubjectError {
    #[error("the worktree could not be resolved to a canonical path")]
    WorktreeResolutionFailed,
    #[error("the canonical worktree path is not valid Unicode")]
    NonUnicodeWorktree,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(super) enum ConsentError {
    #[error("secure random data is unavailable")]
    RandomUnavailable,
    #[error("the consent expiry could not be represented")]
    ClockOverflow,
    #[error("the consent grant was already used")]
    AlreadyUsed,
    #[error("the consent grant belongs to another server epoch")]
    ServerEpochMismatch,
    #[error("the consent grant expired")]
    Expired,
    #[error("the consent grant does not match the requested action")]
    SubjectMismatch,
}

struct CanonicalSubjectDigest {
    hasher: Sha256,
}

impl CanonicalSubjectDigest {
    fn new(domain: &'static [u8]) -> Self {
        let mut digest = Self {
            hasher: Sha256::new(),
        };
        digest.bytes(DIGEST_PREFIX);
        digest.bytes(domain);
        digest
    }

    fn bytes(&mut self, value: &[u8]) {
        self.hasher.update((value.len() as u64).to_be_bytes());
        self.hasher.update(value);
    }

    fn string(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    fn u8(&mut self, value: u8) {
        self.bytes(&[value]);
    }

    fn u64(&mut self, value: u64) {
        self.bytes(&value.to_be_bytes());
    }

    fn finish(self) -> [u8; 32] {
        self.hasher.finalize().into()
    }
}

fn canonical_answers_digest(answers: &BTreeMap<String, Vec<String>>) -> [u8; 32] {
    let mut digest = CanonicalSubjectDigest::new(b"server-consent-answers-v1");
    digest.u64(answers.len() as u64);
    for (key, values) in answers {
        digest.string(key);
        digest.u64(values.len() as u64);
        for value in values {
            digest.string(value);
        }
    }
    digest.finish()
}

const fn provider_tag(provider: ProviderKind) -> u8 {
    match provider {
        ProviderKind::Codex => 0,
        ProviderKind::ClaudeCode => 1,
        ProviderKind::OpenCode => 2,
    }
}

const fn mode_tag(mode: ProviderMode) -> u8 {
    match mode {
        ProviderMode::Managed => 0,
        ProviderMode::Passthrough => 1,
    }
}

const fn decision_tag(decision: AttentionDecision) -> u8 {
    match decision {
        AttentionDecision::ApproveOnce => 0,
        AttentionDecision::ApproveForSession => 1,
        AttentionDecision::AllowForMission => 2,
        AttentionDecision::Deny => 3,
        AttentionDecision::Answer => 4,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        path::PathBuf,
        sync::{Arc, Barrier},
        thread,
        time::{Duration, Instant},
    };

    use super::*;
    use crate::mission::{
        attention::AttentionDecision,
        model::{ProviderKind, ProviderMode},
    };

    fn start_subject_for_client(client_id: u64, worktree: &std::path::Path) -> ConsentSubject {
        ConsentSubject::start_workspace_write(
            client_id,
            "mission-1",
            "run-1",
            ProviderKind::Codex,
            ProviderMode::Managed,
            worktree,
        )
        .expect("test worktree should resolve")
    }

    fn start_subject(worktree: &std::path::Path) -> ConsentSubject {
        start_subject_for_client(7, worktree)
    }

    fn answers(entries: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        entries
            .iter()
            .map(|(key, values)| {
                (
                    (*key).to_owned(),
                    values.iter().map(|value| (*value).to_owned()).collect(),
                )
            })
            .collect()
    }

    fn base_answers() -> BTreeMap<String, Vec<String>> {
        answers(&[
            ("question", &["first", "second"]),
            ("scope", &["workspace"]),
        ])
    }

    fn respond_context() -> RespondConsentContext {
        RespondConsentContext::new("mission-1", "run-1", "attention-1", "request-1", 7)
    }

    fn respond_subject() -> ConsentSubject {
        let answers = base_answers();
        ConsentSubject::respond(7, respond_context(), AttentionDecision::Answer, &answers)
    }

    fn assert_respond_mismatch_burns(mismatch: ConsentSubject) {
        let authority = ConsentAuthority::new().expect("authority");
        let expected = respond_subject();
        let now = Instant::now();
        let grant = authority.issue_at(&expected, now).expect("grant");

        assert_eq!(
            authority.consume_at(&grant, &mismatch, now),
            Err(ConsentError::SubjectMismatch)
        );
        assert_eq!(
            authority.consume_at(&grant, &expected, now),
            Err(ConsentError::AlreadyUsed)
        );
    }

    fn hex(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        encoded
    }

    #[test]
    fn canonical_subject_digests_have_stable_golden_vectors() {
        let start = ConsentSubject {
            kind: ConsentSubjectKind::StartWorkspaceWrite {
                mission_id: "mission-1".to_owned(),
                run_id: "run-1".to_owned(),
                provider: ProviderKind::Codex,
                mode: ProviderMode::Managed,
                canonical_worktree: "/canonical/worktree".to_owned(),
            },
            client_id: 7,
        };
        assert_eq!(
            hex(&start.digest()),
            "cde5ad8b2af943ce090b23aa250a66e5b0edf7f78c402ba643683f2fda5c60f9"
        );
        assert_eq!(
            hex(&respond_subject().digest()),
            "204788bb54ee82c9fa7bc905902d662a52d2337d5593c8a661320fb267b455bf"
        );
    }

    #[test]
    fn grant_is_exactly_one_shot_even_under_concurrent_consumers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let authority = ConsentAuthority::new().expect("CSPRNG available");
        let subject = start_subject(temp.path());
        let now = Instant::now();
        let grant = authority.issue_at(&subject, now).expect("grant");
        let barrier = Arc::new(Barrier::new(16));

        let outcomes = thread::scope(|scope| {
            let mut threads = Vec::new();
            for _ in 0..16 {
                let barrier = Arc::clone(&barrier);
                let authority = &authority;
                let grant = &grant;
                let subject = &subject;
                threads.push(scope.spawn(move || {
                    barrier.wait();
                    authority.consume_at(&grant, &subject, now)
                }));
            }
            threads
                .into_iter()
                .map(|thread| thread.join().expect("consumer should not panic"))
                .collect::<Vec<_>>()
        });

        assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|result| **result == Err(ConsentError::AlreadyUsed))
                .count(),
            15
        );
    }

    #[test]
    fn every_start_subject_mismatch_burns_the_grant() {
        let first = tempfile::tempdir().expect("first tempdir");
        let second = tempfile::tempdir().expect("second tempdir");
        let now = Instant::now();
        let mismatches = [
            ConsentSubject::start_workspace_write(
                7,
                "mission-2",
                "run-1",
                ProviderKind::Codex,
                ProviderMode::Managed,
                first.path(),
            )
            .expect("subject"),
            ConsentSubject::start_workspace_write(
                7,
                "mission-1",
                "run-2",
                ProviderKind::Codex,
                ProviderMode::Managed,
                first.path(),
            )
            .expect("subject"),
            ConsentSubject::start_workspace_write(
                7,
                "mission-1",
                "run-1",
                ProviderKind::ClaudeCode,
                ProviderMode::Managed,
                first.path(),
            )
            .expect("subject"),
            ConsentSubject::start_workspace_write(
                7,
                "mission-1",
                "run-1",
                ProviderKind::Codex,
                ProviderMode::Passthrough,
                first.path(),
            )
            .expect("subject"),
            start_subject(second.path()),
            respond_subject(),
        ];

        for mismatch in mismatches {
            let authority = ConsentAuthority::new().expect("authority");
            let expected = start_subject(first.path());
            let grant = authority.issue_at(&expected, now).expect("grant");
            assert_eq!(
                authority.consume_at(&grant, &mismatch, now),
                Err(ConsentError::SubjectMismatch)
            );
            assert_eq!(
                authority.consume_at(&grant, &expected, now),
                Err(ConsentError::AlreadyUsed)
            );
        }
    }

    #[test]
    fn respond_has_one_subject_client_binding_and_mismatch_burns_the_grant() {
        assert_respond_mismatch_burns(ConsentSubject::respond(
            8,
            respond_context(),
            AttentionDecision::Answer,
            &base_answers(),
        ));
    }

    #[test]
    fn respond_mission_id_mismatch_burns_the_grant() {
        let context =
            RespondConsentContext::new("mission-2", "run-1", "attention-1", "request-1", 7);
        assert_respond_mismatch_burns(ConsentSubject::respond(
            7,
            context,
            AttentionDecision::Answer,
            &base_answers(),
        ));
    }

    #[test]
    fn respond_run_id_mismatch_burns_the_grant() {
        let context =
            RespondConsentContext::new("mission-1", "run-2", "attention-1", "request-1", 7);
        assert_respond_mismatch_burns(ConsentSubject::respond(
            7,
            context,
            AttentionDecision::Answer,
            &base_answers(),
        ));
    }

    #[test]
    fn respond_attention_id_mismatch_burns_the_grant() {
        let context =
            RespondConsentContext::new("mission-1", "run-1", "attention-2", "request-1", 7);
        assert_respond_mismatch_burns(ConsentSubject::respond(
            7,
            context,
            AttentionDecision::Answer,
            &base_answers(),
        ));
    }

    #[test]
    fn respond_provider_request_id_mismatch_burns_the_grant() {
        let context =
            RespondConsentContext::new("mission-1", "run-1", "attention-1", "request-2", 7);
        assert_respond_mismatch_burns(ConsentSubject::respond(
            7,
            context,
            AttentionDecision::Answer,
            &base_answers(),
        ));
    }

    #[test]
    fn respond_generation_mismatch_burns_the_grant() {
        let context =
            RespondConsentContext::new("mission-1", "run-1", "attention-1", "request-1", 8);
        assert_respond_mismatch_burns(ConsentSubject::respond(
            7,
            context,
            AttentionDecision::Answer,
            &base_answers(),
        ));
    }

    #[test]
    fn respond_decision_mismatch_burns_the_grant() {
        assert_respond_mismatch_burns(ConsentSubject::respond(
            7,
            respond_context(),
            AttentionDecision::Deny,
            &base_answers(),
        ));
    }

    #[test]
    fn answer_value_modification_burns_the_grant() {
        let modified = answers(&[
            ("question", &["changed", "second"]),
            ("scope", &["workspace"]),
        ]);
        assert_respond_mismatch_burns(ConsentSubject::respond(
            7,
            respond_context(),
            AttentionDecision::Answer,
            &modified,
        ));
    }

    #[test]
    fn answer_key_addition_burns_the_grant() {
        let added = answers(&[
            ("extra", &["value"]),
            ("question", &["first", "second"]),
            ("scope", &["workspace"]),
        ]);
        assert_respond_mismatch_burns(ConsentSubject::respond(
            7,
            respond_context(),
            AttentionDecision::Answer,
            &added,
        ));
    }

    #[test]
    fn answer_key_removal_burns_the_grant() {
        let removed = answers(&[("question", &["first", "second"])]);
        assert_respond_mismatch_burns(ConsentSubject::respond(
            7,
            respond_context(),
            AttentionDecision::Answer,
            &removed,
        ));
    }

    #[test]
    fn answer_key_order_does_not_change_the_subject() {
        let mut first = BTreeMap::new();
        first.insert(
            "question".to_owned(),
            vec!["first".to_owned(), "second".to_owned()],
        );
        first.insert("scope".to_owned(), vec!["workspace".to_owned()]);
        let mut reversed = BTreeMap::new();
        reversed.insert("scope".to_owned(), vec!["workspace".to_owned()]);
        reversed.insert(
            "question".to_owned(),
            vec!["first".to_owned(), "second".to_owned()],
        );
        let expected =
            ConsentSubject::respond(7, respond_context(), AttentionDecision::Answer, &first);
        let reordered =
            ConsentSubject::respond(7, respond_context(), AttentionDecision::Answer, &reversed);

        assert_eq!(expected.digest(), reordered.digest());
    }

    #[test]
    fn answer_value_order_change_burns_the_grant() {
        let reordered = answers(&[
            ("question", &["second", "first"]),
            ("scope", &["workspace"]),
        ]);
        assert_respond_mismatch_burns(ConsentSubject::respond(
            7,
            respond_context(),
            AttentionDecision::Answer,
            &reordered,
        ));
    }

    #[test]
    fn start_wrong_client_burns_then_correct_client_is_replay() {
        let temp = tempfile::tempdir().expect("tempdir");
        let authority = ConsentAuthority::new().expect("authority");
        let subject = start_subject_for_client(41, temp.path());
        let wrong_client = start_subject_for_client(42, temp.path());
        let now = Instant::now();
        let grant = authority.issue_at(&subject, now).expect("grant");

        assert_eq!(
            authority.consume_at(&grant, &wrong_client, now),
            Err(ConsentError::SubjectMismatch)
        );
        assert_eq!(
            authority.consume_at(&grant, &subject, now),
            Err(ConsentError::AlreadyUsed)
        );
    }

    #[test]
    fn production_consume_owns_the_clock_and_burns_an_expired_grant() {
        let temp = tempfile::tempdir().expect("tempdir");
        let authority = ConsentAuthority::new().expect("authority");
        let subject = start_subject(temp.path());
        let stale_issued_at = Instant::now() - CONSENT_TTL - Duration::from_nanos(1);
        let grant = authority
            .issue_at(&subject, stale_issued_at)
            .expect("stale grant");

        assert_eq!(
            authority.consume(&grant, &subject),
            Err(ConsentError::Expired)
        );
        assert_eq!(
            authority.consume(&grant, &subject),
            Err(ConsentError::AlreadyUsed)
        );
    }

    #[test]
    fn ttl_accepts_just_before_boundary_and_expires_at_boundary() {
        let temp = tempfile::tempdir().expect("tempdir");
        let authority = ConsentAuthority::new().expect("authority");
        let subject = start_subject(temp.path());
        let issued_at = Instant::now();
        let before = authority.issue_at(&subject, issued_at).expect("grant");
        let boundary = authority.issue_at(&subject, issued_at).expect("grant");

        assert_eq!(
            authority.consume_at(
                &before,
                &subject,
                issued_at + CONSENT_TTL - Duration::from_nanos(1),
            ),
            Ok(())
        );
        assert_eq!(
            authority.consume_at(&boundary, &subject, issued_at + CONSENT_TTL),
            Err(ConsentError::Expired)
        );
        assert_eq!(
            authority.consume_at(&boundary, &subject, issued_at),
            Err(ConsentError::AlreadyUsed)
        );
    }

    #[test]
    fn prior_server_epoch_is_rejected_and_burned() {
        let temp = tempfile::tempdir().expect("tempdir");
        let prior = ConsentAuthority::new().expect("prior authority");
        let current = ConsentAuthority::new().expect("current authority");
        let subject = start_subject(temp.path());
        let now = Instant::now();
        let grant = prior.issue_at(&subject, now).expect("grant");

        assert_eq!(
            current.consume_at(&grant, &subject, now),
            Err(ConsentError::ServerEpochMismatch)
        );
        assert_eq!(
            prior.consume_at(&grant, &subject, now),
            Err(ConsentError::AlreadyUsed)
        );
    }

    #[test]
    fn debug_output_redacts_nonce_epoch_and_digests() {
        let authority = ConsentAuthority {
            server_epoch: [0xE1; 32],
        };
        let subject = respond_subject();
        let grant = authority
            .issue_with_nonce_at(&subject, Instant::now(), [0xA5; 32])
            .expect("grant");
        let grant_debug = format!("{grant:?}");
        let subject_debug = format!("{subject:?}");

        assert!(grant_debug.contains("[REDACTED]"));
        assert!(!grant_debug.contains(&hex(&[0xA5; 32])));
        assert!(!grant_debug.contains(&hex(&[0xE1; 32])));
        assert!(!grant_debug.contains(&hex(&grant.subject_digest)));
        assert!(subject_debug.contains("answers_digest: [REDACTED]"));
        assert!(!subject_debug.contains("first"));
        assert!(!subject_debug.contains("workspace"));
    }

    #[test]
    fn start_constructor_resolves_the_canonical_worktree() {
        let root = tempfile::tempdir().expect("tempdir");
        let nested = root.path().join("nested");
        std::fs::create_dir(&nested).expect("nested directory");
        let non_canonical = nested.join("..").join("nested");

        let subject = start_subject(&non_canonical);
        let ConsentSubjectKind::StartWorkspaceWrite {
            canonical_worktree, ..
        } = subject.kind
        else {
            panic!("expected start subject");
        };
        assert_eq!(
            PathBuf::from(canonical_worktree),
            nested.canonicalize().expect("canonical nested path")
        );
    }
}
