#![allow(
    dead_code,
    reason = "the durable attention model is tested ahead of the public mission cockpit"
)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    digest::CanonicalDigest,
    model::ProviderKind,
    run_state::{ObservationSource, SessionStatus},
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PaneTarget {
    workspace: String,
    pane: String,
}

impl PaneTarget {
    #[must_use]
    pub fn new(workspace: impl Into<String>, pane: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            pane: pane.into(),
        }
    }

    #[must_use]
    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    #[must_use]
    pub fn pane(&self) -> &str {
        &self.pane
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    PermissionRequest,
    ProviderQuestion,
    CommandFailed,
    WorktreeConflict,
    TurnComplete,
    Disconnected,
    SecurityWarning,
    ManualVerification,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionRisk {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AttentionEvent {
    event_id: String,
    mission_id: String,
    mission_run_id: String,
    session_id: String,
    pane_target: PaneTarget,
    kind: AttentionKind,
    requested_action: String,
    scope: String,
    risk: AttentionRisk,
    provider: ProviderKind,
    source: ObservationSource,
    response_capability: ResponseCapability,
    request_generation: u64,
    created_at_millis: u64,
    expires_at_millis: Option<u64>,
    provider_request_id: Option<String>,
}

impl AttentionEvent {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        event_id: impl Into<String>,
        mission_id: impl Into<String>,
        mission_run_id: impl Into<String>,
        session_id: impl Into<String>,
        pane_target: PaneTarget,
        kind: AttentionKind,
        requested_action: impl Into<String>,
        scope: impl Into<String>,
        risk: AttentionRisk,
        provider: ProviderKind,
        source: ObservationSource,
        created_at_millis: u64,
    ) -> Self {
        Self {
            event_id: event_id.into(),
            mission_id: mission_id.into(),
            mission_run_id: mission_run_id.into(),
            session_id: session_id.into(),
            pane_target,
            kind,
            requested_action: requested_action.into(),
            scope: scope.into(),
            risk,
            provider,
            source,
            response_capability: ResponseCapability::OpenPaneOnly,
            request_generation: 1,
            created_at_millis,
            expires_at_millis: None,
            provider_request_id: None,
        }
    }

    #[must_use]
    pub fn with_provider_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.provider_request_id = Some(request_id.into());
        self
    }

    #[must_use]
    pub const fn with_response_capability(mut self, capability: ResponseCapability) -> Self {
        self.response_capability = capability;
        self
    }

    #[must_use]
    pub const fn with_request_generation(mut self, generation: u64) -> Self {
        self.request_generation = generation;
        self
    }

    #[must_use]
    pub const fn expires_at(mut self, expires_at_millis: u64) -> Self {
        self.expires_at_millis = Some(expires_at_millis);
        self
    }

    fn has_same_semantics_as(&self, item: &AttentionItem) -> bool {
        self.has_same_payload_as(item)
            && matches!(
                item.status,
                AttentionStatus::Open | AttentionStatus::PendingResponse { .. }
            )
    }

    fn has_same_payload_as(&self, item: &AttentionItem) -> bool {
        self.mission_id == item.mission_id
            && self.mission_run_id == item.mission_run_id
            && self.session_id == item.session_id
            && self.kind == item.kind
            && self.requested_action == item.requested_action
            && self.scope == item.scope
            && self.risk == item.risk
            && self.provider == item.provider
            && self.pane_target == item.pane_target
            && self.provider_request_id == item.provider_request_id
            && self.source == item.source
            && self.response_capability == item.response_capability
            && self.request_generation == item.request_generation
            && self.created_at_millis == item.created_at_millis
            && self.expires_at_millis == item.expires_at_millis
    }

    fn provider_request_key(&self) -> Option<ProviderRequestKey> {
        self.provider_request_id
            .as_ref()
            .map(|provider_request_id| ProviderRequestKey {
                provider: self.provider,
                mission_id: self.mission_id.clone(),
                mission_run_id: self.mission_run_id.clone(),
                session_id: self.session_id.clone(),
                provider_request_id: provider_request_id.clone(),
                request_generation: self.request_generation,
            })
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct ProviderRequestKey {
    provider: ProviderKind,
    mission_id: String,
    mission_run_id: String,
    session_id: String,
    provider_request_id: String,
    request_generation: u64,
}

impl ProviderRequestKey {
    fn digest(&self) -> String {
        let mut digest = CanonicalDigest::new(b"provider-request-identity-v1");
        digest.u8(match self.provider {
            ProviderKind::Codex => 0,
            ProviderKind::ClaudeCode => 1,
            ProviderKind::OpenCode => 2,
        });
        digest.string(&self.mission_id);
        digest.string(&self.mission_run_id);
        digest.string(&self.session_id);
        digest.string(&self.provider_request_id);
        digest.u64(self.request_generation);
        digest.finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ProviderRequestIndexEntry {
    key: ProviderRequestKey,
    item_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AttentionItem {
    id: String,
    mission_id: String,
    mission_run_id: String,
    session_id: String,
    pane_target: PaneTarget,
    kind: AttentionKind,
    requested_action: String,
    scope: String,
    risk: AttentionRisk,
    provider: ProviderKind,
    source: ObservationSource,
    response_capability: ResponseCapability,
    request_generation: u64,
    created_at_millis: u64,
    expires_at_millis: Option<u64>,
    provider_request_id: Option<String>,
    occurrence_count: u32,
    unread: bool,
    status: AttentionStatus,
    response_attempts: Vec<ProviderResponseAttempt>,
}

impl From<AttentionEvent> for AttentionItem {
    fn from(event: AttentionEvent) -> Self {
        Self {
            id: event.event_id,
            mission_id: event.mission_id,
            mission_run_id: event.mission_run_id,
            session_id: event.session_id,
            pane_target: event.pane_target,
            kind: event.kind,
            requested_action: event.requested_action,
            scope: event.scope,
            risk: event.risk,
            provider: event.provider,
            source: event.source,
            response_capability: event.response_capability,
            request_generation: event.request_generation,
            created_at_millis: event.created_at_millis,
            expires_at_millis: event.expires_at_millis,
            provider_request_id: event.provider_request_id,
            occurrence_count: 1,
            unread: true,
            status: AttentionStatus::Open,
            response_attempts: Vec::new(),
        }
    }
}

impl AttentionItem {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn mission_id(&self) -> &str {
        &self.mission_id
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub const fn pane_target(&self) -> &PaneTarget {
        &self.pane_target
    }

    #[must_use]
    pub fn requested_action(&self) -> &str {
        &self.requested_action
    }

    #[must_use]
    pub const fn source(&self) -> ObservationSource {
        self.source
    }

    #[must_use]
    pub const fn kind(&self) -> AttentionKind {
        self.kind
    }

    #[must_use]
    pub fn scope(&self) -> &str {
        &self.scope
    }

    #[must_use]
    pub const fn risk(&self) -> AttentionRisk {
        self.risk
    }

    #[must_use]
    pub const fn is_unread(&self) -> bool {
        self.unread
    }

    #[must_use]
    pub const fn created_at_millis(&self) -> u64 {
        self.created_at_millis
    }

    #[must_use]
    pub const fn expires_at_millis(&self) -> Option<u64> {
        self.expires_at_millis
    }

    #[must_use]
    pub const fn occurrence_count(&self) -> u32 {
        self.occurrence_count
    }

    #[must_use]
    pub const fn status(&self) -> &AttentionStatus {
        &self.status
    }

    #[must_use]
    pub fn response_attempts(&self) -> &[ProviderResponseAttempt] {
        &self.response_attempts
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AttentionStatus {
    Open,
    PendingResponse {
        decision: AttentionDecision,
        actor: String,
        requested_at_millis: u64,
    },
    Resolved {
        decision: AttentionDecision,
        actor: String,
        at_millis: u64,
    },
    ReconciliationRequired {
        decision: AttentionDecision,
        actor: String,
        code: ResponseFailureCode,
        at_millis: u64,
    },
    Dismissed {
        actor: String,
        reason: String,
        at_millis: u64,
    },
    Expired {
        at_millis: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProviderResponseAttempt {
    token: ProviderResponseToken,
    decision: AttentionDecision,
    actor: String,
    requested_at_millis: u64,
    status: ProviderResponseAttemptStatus,
}

impl ProviderResponseAttempt {
    #[must_use]
    pub const fn token(&self) -> &ProviderResponseToken {
        &self.token
    }

    #[must_use]
    pub const fn failure_disposition(&self) -> Option<ResponseFailureDisposition> {
        match self.status {
            ProviderResponseAttemptStatus::DefinitelyNotApplied { .. } => {
                Some(ResponseFailureDisposition::DefinitelyNotApplied)
            }
            ProviderResponseAttemptStatus::DeliveryUnknown { .. } => {
                Some(ResponseFailureDisposition::DeliveryUnknown)
            }
            ProviderResponseAttemptStatus::Pending
            | ProviderResponseAttemptStatus::Acknowledged { .. } => None,
        }
    }

    #[must_use]
    pub const fn failure_code(&self) -> Option<ResponseFailureCode> {
        match self.status {
            ProviderResponseAttemptStatus::DefinitelyNotApplied { code, .. }
            | ProviderResponseAttemptStatus::DeliveryUnknown { code, .. } => Some(code),
            ProviderResponseAttemptStatus::Pending
            | ProviderResponseAttemptStatus::Acknowledged { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum ProviderResponseAttemptStatus {
    Pending,
    Acknowledged {
        at_millis: u64,
    },
    DefinitelyNotApplied {
        at_millis: u64,
        code: ResponseFailureCode,
    },
    DeliveryUnknown {
        at_millis: u64,
        code: ResponseFailureCode,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionDecision {
    ApproveOnce,
    ApproveForSession,
    AllowForMission,
    Deny,
    Answer,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseCapability {
    Reliable,
    OpenPaneOnly,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFailureDisposition {
    DefinitelyNotApplied,
    DeliveryUnknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFailureCode {
    Rejected,
    DisconnectedBeforeWrite,
    Timeout,
    TransportClosed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProviderResponseToken {
    item_id: String,
    request_generation: u64,
    attempt: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProviderResponseRoute {
    provider: ProviderKind,
    mission_id: String,
    mission_run_id: String,
    session_id: String,
    pane_target: PaneTarget,
    provider_request_id: String,
    request_generation: u64,
    scope: String,
}

impl ProviderResponseRoute {
    #[must_use]
    pub const fn provider(&self) -> ProviderKind {
        self.provider
    }

    #[must_use]
    pub fn mission_id(&self) -> &str {
        &self.mission_id
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub fn mission_run_id(&self) -> &str {
        &self.mission_run_id
    }

    #[must_use]
    pub fn provider_request_id(&self) -> &str {
        &self.provider_request_id
    }

    #[must_use]
    pub fn scope(&self) -> &str {
        &self.scope
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderResponseIntent {
    Respond {
        route: Box<ProviderResponseRoute>,
        decision: AttentionDecision,
        answer: Option<EphemeralAnswer>,
        token: ProviderResponseToken,
    },
    OpenPane {
        target: PaneTarget,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub struct EphemeralAnswer(String);

impl EphemeralAnswer {
    fn new(value: impl Into<String>) -> Result<Self, AttentionError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(AttentionError::EmptyAnswer);
        }
        if value.len() > 16 * 1024
            || value
                .chars()
                .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
        {
            return Err(AttentionError::InvalidAnswer);
        }
        Ok(Self(value))
    }

    pub(crate) fn expose_to_provider(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for EphemeralAnswer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("EphemeralAnswer([REDACTED])")
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct AttentionInbox {
    items: BTreeMap<String, AttentionItem>,
    seen_event_ids: BTreeMap<String, String>,
    provider_requests: BTreeMap<String, ProviderRequestIndexEntry>,
}

impl AttentionInbox {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            items: BTreeMap::new(),
            seen_event_ids: BTreeMap::new(),
            provider_requests: BTreeMap::new(),
        }
    }

    pub fn ingest(mut self, event: AttentionEvent) -> Result<Self, AttentionError> {
        if let Some(item_id) = self.seen_event_ids.get(&event.event_id) {
            let item = self
                .items
                .get(item_id)
                .ok_or(AttentionError::CorruptIndex)?;
            return if event.has_same_payload_as(item) {
                Ok(self)
            } else {
                Err(AttentionError::EventIdConflict)
            };
        }

        let provider_request_key = event.provider_request_key();
        let indexed_request = provider_request_key.as_ref().and_then(|key| {
            self.provider_requests
                .get(&key.digest())
                .map(|entry| (key, entry))
        });
        if let Some((key, entry)) = indexed_request {
            if key != &entry.key {
                return Err(AttentionError::CorruptIndex);
            }
            let item_id = entry.item_id.clone();
            let item = self
                .items
                .get_mut(&item_id)
                .ok_or(AttentionError::CorruptIndex)?;
            if !event.has_same_payload_as(item) {
                return Err(AttentionError::ProviderRequestConflict);
            }
            item.occurrence_count = item.occurrence_count.saturating_add(1);
            if matches!(
                item.status,
                AttentionStatus::Open | AttentionStatus::PendingResponse { .. }
            ) {
                item.unread = true;
            }
            self.seen_event_ids.insert(event.event_id, item_id);
            return Ok(self);
        }

        if let Some(item) = self
            .items
            .values_mut()
            .find(|item| event.has_same_semantics_as(item))
        {
            item.occurrence_count = item.occurrence_count.saturating_add(1);
            item.unread = true;
            self.seen_event_ids.insert(event.event_id, item.id.clone());
            return Ok(self);
        }

        let event_id = event.event_id.clone();
        self.items.insert(event_id.clone(), event.into());
        self.seen_event_ids
            .insert(event_id.clone(), event_id.clone());
        if let Some(key) = provider_request_key {
            self.provider_requests.insert(
                key.digest(),
                ProviderRequestIndexEntry {
                    key,
                    item_id: event_id,
                },
            );
        }
        Ok(self)
    }

    #[must_use]
    pub fn refresh(mut self, now_millis: u64) -> Self {
        for item in self.items.values_mut() {
            if matches!(
                item.status,
                AttentionStatus::Open | AttentionStatus::PendingResponse { .. }
            ) && item
                .expires_at_millis
                .is_some_and(|expires_at| now_millis >= expires_at)
            {
                item.status = match item.status.clone() {
                    AttentionStatus::Open => AttentionStatus::Expired {
                        at_millis: now_millis,
                    },
                    AttentionStatus::PendingResponse {
                        decision, actor, ..
                    } => {
                        Self::fail_latest_attempt(
                            item,
                            ResponseFailureDisposition::DeliveryUnknown,
                            ResponseFailureCode::Timeout,
                            now_millis,
                        );
                        AttentionStatus::ReconciliationRequired {
                            decision,
                            actor,
                            code: ResponseFailureCode::Timeout,
                            at_millis: now_millis,
                        }
                    }
                    _ => continue,
                };
            }
        }
        self
    }

    /// Applies a user decision and returns the updated inbox plus an optional
    /// provider response intent.
    ///
    /// # Errors
    ///
    /// Returns an error when the item does not exist, is expired or closed, has
    /// no provider request identity, or cannot accept the requested decision.
    pub fn decide(
        &self,
        item_id: &str,
        decision: AttentionDecision,
        actor: &str,
        at_millis: u64,
    ) -> (Self, Result<Option<ProviderResponseIntent>, AttentionError>) {
        let mut updated = self.clone();
        let Some(item) = updated.items.get_mut(item_id) else {
            return (updated, Err(AttentionError::NotFound));
        };
        if at_millis < item.created_at_millis {
            return (updated, Err(AttentionError::DecisionTimeWentBackwards));
        }
        if matches!(item.status, AttentionStatus::Expired { .. }) {
            return (updated, Err(AttentionError::Expired));
        }
        if item
            .expires_at_millis
            .is_some_and(|expires_at| at_millis >= expires_at)
        {
            item.status = AttentionStatus::Expired { at_millis };
            return (updated, Err(AttentionError::Expired));
        }
        if item.status != AttentionStatus::Open {
            return (updated, Err(AttentionError::AlreadyClosed));
        }

        let source_is_reliable = matches!(
            item.source,
            ObservationSource::StructuredHook | ObservationSource::ProviderApi
        );
        let critical_requires_native_review =
            item.risk == AttentionRisk::Critical && decision != AttentionDecision::Deny;
        if item.response_capability == ResponseCapability::OpenPaneOnly
            || !source_is_reliable
            || critical_requires_native_review
        {
            let target = item.pane_target.clone();
            return (
                updated,
                Ok(Some(ProviderResponseIntent::OpenPane { target })),
            );
        }
        if actor.trim().is_empty() {
            return (updated, Err(AttentionError::EmptyActor));
        }
        let decision_allowed = match (item.kind, decision, item.risk) {
            (
                AttentionKind::PermissionRequest,
                AttentionDecision::ApproveOnce | AttentionDecision::Deny,
                _,
            ) => true,
            (
                AttentionKind::PermissionRequest,
                AttentionDecision::ApproveForSession | AttentionDecision::AllowForMission,
                risk,
            ) => {
                matches!(risk, AttentionRisk::Low | AttentionRisk::Medium)
            }
            _ => false,
        };
        if !decision_allowed {
            return (updated, Err(AttentionError::DecisionNotAllowed));
        }

        let Some(provider_request_id) = item.provider_request_id.clone() else {
            return (updated, Err(AttentionError::MissingProviderRequestId));
        };
        let Some(attempt) = u32::try_from(item.response_attempts.len())
            .ok()
            .and_then(|attempts| attempts.checked_add(1))
        else {
            return (updated, Err(AttentionError::TooManyResponseAttempts));
        };
        let token = ProviderResponseToken {
            item_id: item_id.to_owned(),
            request_generation: item.request_generation,
            attempt,
        };
        let response = ProviderResponseIntent::Respond {
            route: Box::new(ProviderResponseRoute {
                provider: item.provider,
                mission_id: item.mission_id.clone(),
                mission_run_id: item.mission_run_id.clone(),
                session_id: item.session_id.clone(),
                pane_target: item.pane_target.clone(),
                provider_request_id,
                request_generation: item.request_generation,
                scope: item.scope.clone(),
            }),
            decision,
            answer: None,
            token: token.clone(),
        };

        item.unread = false;
        item.status = AttentionStatus::PendingResponse {
            decision,
            actor: actor.to_owned(),
            requested_at_millis: at_millis,
        };
        item.response_attempts.push(ProviderResponseAttempt {
            token,
            decision,
            actor: actor.to_owned(),
            requested_at_millis: at_millis,
            status: ProviderResponseAttemptStatus::Pending,
        });
        (updated, Ok(Some(response)))
    }

    pub fn answer(
        &self,
        item_id: &str,
        answer: impl Into<String>,
        actor: &str,
        at_millis: u64,
    ) -> (Self, Result<Option<ProviderResponseIntent>, AttentionError>) {
        let mut updated = self.clone();
        let Some(item) = updated.items.get_mut(item_id) else {
            return (updated, Err(AttentionError::NotFound));
        };
        if at_millis < item.created_at_millis {
            return (updated, Err(AttentionError::DecisionTimeWentBackwards));
        }
        if matches!(item.status, AttentionStatus::Expired { .. }) {
            return (updated, Err(AttentionError::Expired));
        }
        if item
            .expires_at_millis
            .is_some_and(|expires_at| at_millis >= expires_at)
        {
            item.status = AttentionStatus::Expired { at_millis };
            return (updated, Err(AttentionError::Expired));
        }
        if item.status != AttentionStatus::Open {
            return (updated, Err(AttentionError::AlreadyClosed));
        }
        if item.kind != AttentionKind::ProviderQuestion {
            return (updated, Err(AttentionError::DecisionNotAllowed));
        }
        let source_is_reliable = matches!(
            item.source,
            ObservationSource::StructuredHook | ObservationSource::ProviderApi
        );
        if item.response_capability == ResponseCapability::OpenPaneOnly || !source_is_reliable {
            let target = item.pane_target.clone();
            return (
                updated,
                Ok(Some(ProviderResponseIntent::OpenPane { target })),
            );
        }
        if actor.trim().is_empty() {
            return (updated, Err(AttentionError::EmptyActor));
        }
        let answer = match EphemeralAnswer::new(answer) {
            Ok(answer) => answer,
            Err(error) => return (updated, Err(error)),
        };
        let Some(provider_request_id) = item.provider_request_id.clone() else {
            return (updated, Err(AttentionError::MissingProviderRequestId));
        };
        let Some(attempt) = u32::try_from(item.response_attempts.len())
            .ok()
            .and_then(|attempts| attempts.checked_add(1))
        else {
            return (updated, Err(AttentionError::TooManyResponseAttempts));
        };
        let token = ProviderResponseToken {
            item_id: item_id.to_owned(),
            request_generation: item.request_generation,
            attempt,
        };
        let decision = AttentionDecision::Answer;
        let response = ProviderResponseIntent::Respond {
            route: Box::new(ProviderResponseRoute {
                provider: item.provider,
                mission_id: item.mission_id.clone(),
                mission_run_id: item.mission_run_id.clone(),
                session_id: item.session_id.clone(),
                pane_target: item.pane_target.clone(),
                provider_request_id,
                request_generation: item.request_generation,
                scope: item.scope.clone(),
            }),
            decision,
            answer: Some(answer),
            token: token.clone(),
        };
        item.unread = false;
        item.status = AttentionStatus::PendingResponse {
            decision,
            actor: actor.to_owned(),
            requested_at_millis: at_millis,
        };
        item.response_attempts.push(ProviderResponseAttempt {
            token,
            decision,
            actor: actor.to_owned(),
            requested_at_millis: at_millis,
            status: ProviderResponseAttemptStatus::Pending,
        });
        (updated, Ok(Some(response)))
    }

    /// Marks the latest provider response attempt as acknowledged.
    ///
    /// # Errors
    ///
    /// Returns an error when the item is missing, has no response in flight, or
    /// the acknowledgement timestamp predates the response attempt.
    pub fn confirm_response(
        &self,
        token: &ProviderResponseToken,
        at_millis: u64,
    ) -> (Self, Result<(), AttentionError>) {
        let mut updated = self.clone();
        let Some(item) = updated.items.get_mut(&token.item_id) else {
            return (updated, Err(AttentionError::NotFound));
        };
        if at_millis < item.created_at_millis {
            return (updated, Err(AttentionError::ResponseTimeWentBackwards));
        }
        let Some(attempt) = item.response_attempts.last_mut() else {
            return (updated, Err(AttentionError::NoResponsePending));
        };
        if attempt.token != *token {
            return (updated, Err(AttentionError::StaleResponseToken));
        }
        if matches!(
            attempt.status,
            ProviderResponseAttemptStatus::Acknowledged { .. }
        ) {
            return (updated, Ok(()));
        }
        if !matches!(
            attempt.status,
            ProviderResponseAttemptStatus::Pending
                | ProviderResponseAttemptStatus::DeliveryUnknown { .. }
        ) {
            return (updated, Err(AttentionError::StaleResponseToken));
        }
        let (decision, actor, requested_at_millis) = match item.status.clone() {
            AttentionStatus::PendingResponse {
                decision,
                actor,
                requested_at_millis,
            } => (decision, actor, requested_at_millis),
            AttentionStatus::ReconciliationRequired {
                decision,
                actor,
                at_millis,
                ..
            } => (decision, actor, at_millis),
            _ => return (updated, Err(AttentionError::NoResponsePending)),
        };
        if at_millis < requested_at_millis {
            return (updated, Err(AttentionError::ResponseTimeWentBackwards));
        }
        let Some(attempt) = item.response_attempts.last_mut() else {
            return (updated, Err(AttentionError::NoResponsePending));
        };
        attempt.status = ProviderResponseAttemptStatus::Acknowledged { at_millis };
        item.status = AttentionStatus::Resolved {
            decision,
            actor,
            at_millis,
        };
        (updated, Ok(()))
    }

    /// Reopens an item after a provider response failed while preserving the
    /// failed attempt for audit and retry UI.
    ///
    /// # Errors
    ///
    /// Returns an error when the item is missing, has no response in flight, or
    /// the failure timestamp predates the response attempt.
    pub fn fail_response(
        &self,
        token: &ProviderResponseToken,
        disposition: ResponseFailureDisposition,
        code: ResponseFailureCode,
        at_millis: u64,
    ) -> (Self, Result<(), AttentionError>) {
        let mut updated = self.clone();
        let Some(item) = updated.items.get_mut(&token.item_id) else {
            return (updated, Err(AttentionError::NotFound));
        };
        if at_millis < item.created_at_millis {
            return (updated, Err(AttentionError::ResponseTimeWentBackwards));
        }
        let Some(attempt) = item.response_attempts.last() else {
            return (updated, Err(AttentionError::NoResponsePending));
        };
        if attempt.token != *token || attempt.status != ProviderResponseAttemptStatus::Pending {
            return (updated, Err(AttentionError::StaleResponseToken));
        }
        let AttentionStatus::PendingResponse {
            requested_at_millis,
            ..
        } = item.status
        else {
            return (updated, Err(AttentionError::NoResponsePending));
        };
        if at_millis < requested_at_millis {
            return (updated, Err(AttentionError::ResponseTimeWentBackwards));
        }
        let request_expired = item
            .expires_at_millis
            .is_some_and(|expires_at| at_millis >= expires_at);
        Self::fail_latest_attempt(item, disposition, code, at_millis);
        item.status = match disposition {
            ResponseFailureDisposition::DefinitelyNotApplied if request_expired => {
                AttentionStatus::Expired { at_millis }
            }
            ResponseFailureDisposition::DefinitelyNotApplied => AttentionStatus::Open,
            ResponseFailureDisposition::DeliveryUnknown => {
                let AttentionStatus::PendingResponse {
                    decision, actor, ..
                } = item.status.clone()
                else {
                    return (updated, Err(AttentionError::NoResponsePending));
                };
                AttentionStatus::ReconciliationRequired {
                    decision,
                    actor,
                    code,
                    at_millis,
                }
            }
        };
        item.unread = true;
        (updated, Ok(()))
    }

    fn fail_latest_attempt(
        item: &mut AttentionItem,
        disposition: ResponseFailureDisposition,
        code: ResponseFailureCode,
        at_millis: u64,
    ) {
        if let Some(attempt) = item
            .response_attempts
            .last_mut()
            .filter(|attempt| attempt.status == ProviderResponseAttemptStatus::Pending)
        {
            attempt.status = match disposition {
                ResponseFailureDisposition::DefinitelyNotApplied => {
                    ProviderResponseAttemptStatus::DefinitelyNotApplied { at_millis, code }
                }
                ResponseFailureDisposition::DeliveryUnknown => {
                    ProviderResponseAttemptStatus::DeliveryUnknown { at_millis, code }
                }
            };
        }
    }

    /// Dismisses an open item while preserving who dismissed it and why.
    ///
    /// # Errors
    ///
    /// Returns an error when the item does not exist or is already closed.
    pub fn dismiss(
        &self,
        item_id: &str,
        actor: &str,
        reason: &str,
        at_millis: u64,
    ) -> Result<Self, AttentionError> {
        let mut updated = self.clone();
        let item = updated
            .items
            .get_mut(item_id)
            .ok_or(AttentionError::NotFound)?;
        if item.status != AttentionStatus::Open {
            return Err(AttentionError::AlreadyClosed);
        }
        if item.kind != AttentionKind::TurnComplete {
            return Err(AttentionError::SafetyItemCannotBeDismissed);
        }
        if actor.trim().is_empty() {
            return Err(AttentionError::EmptyActor);
        }
        if reason.trim().is_empty() {
            return Err(AttentionError::EmptyDismissReason);
        }
        if at_millis < item.created_at_millis {
            return Err(AttentionError::DecisionTimeWentBackwards);
        }

        item.unread = false;
        item.status = AttentionStatus::Dismissed {
            actor: actor.to_owned(),
            reason: reason.to_owned(),
            at_millis,
        };
        Ok(updated)
    }

    /// Marks an item as read without resolving or dismissing it.
    ///
    /// # Errors
    ///
    /// Returns an error when the item does not exist.
    pub fn mark_read(&self, item_id: &str) -> Result<Self, AttentionError> {
        let mut updated = self.clone();
        let item = updated
            .items
            .get_mut(item_id)
            .ok_or(AttentionError::NotFound)?;
        item.unread = false;
        Ok(updated)
    }

    #[must_use]
    pub fn effective_run_status(
        &self,
        mission_id: &str,
        mission_run_id: &str,
        provider: ProviderKind,
        session_id: &str,
        fallback: SessionStatus,
    ) -> SessionStatus {
        let open_items = self.items.values().filter(|item| {
            item.mission_id == mission_id
                && item.mission_run_id == mission_run_id
                && item.provider == provider
                && item.session_id == session_id
                && matches!(
                    item.status,
                    AttentionStatus::Open
                        | AttentionStatus::PendingResponse { .. }
                        | AttentionStatus::ReconciliationRequired { .. }
                )
        });

        let mut effective = fallback;
        for item in open_items {
            effective = match item.kind {
                AttentionKind::PermissionRequest => SessionStatus::NeedsApproval,
                AttentionKind::ProviderQuestion => {
                    if effective == SessionStatus::NeedsApproval {
                        effective
                    } else {
                        SessionStatus::NeedsAnswer
                    }
                }
                _ => {
                    if matches!(
                        effective,
                        SessionStatus::NeedsApproval | SessionStatus::NeedsAnswer
                    ) {
                        effective
                    } else {
                        SessionStatus::Blocked
                    }
                }
            };
        }
        effective
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn items(&self) -> impl Iterator<Item = &AttentionItem> {
        self.items.values()
    }

    #[must_use]
    pub fn item(&self, item_id: &str) -> Option<&AttentionItem> {
        self.items.get(item_id)
    }

    #[must_use]
    pub fn unread_count(&self) -> usize {
        self.items
            .values()
            .filter(|item| item.unread && item.status == AttentionStatus::Open)
            .count()
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum AttentionError {
    #[error("attention item not found")]
    NotFound,
    #[error("attention item has expired")]
    Expired,
    #[error("attention item is already closed")]
    AlreadyClosed,
    #[error("provider request identity is missing")]
    MissingProviderRequestId,
    #[error("this decision is not allowed for the attention kind and risk")]
    DecisionNotAllowed,
    #[error("attention actor cannot be empty")]
    EmptyActor,
    #[error("provider answer cannot be empty")]
    EmptyAnswer,
    #[error("provider answer is too large or contains unsupported control characters")]
    InvalidAnswer,
    #[error("attention decision timestamp precedes the request")]
    DecisionTimeWentBackwards,
    #[error("provider response attempt limit reached")]
    TooManyResponseAttempts,
    #[error("attention item has no provider response pending")]
    NoResponsePending,
    #[error("provider response token is stale")]
    StaleResponseToken,
    #[error("provider response timestamp precedes the response attempt")]
    ResponseTimeWentBackwards,
    #[error("attention event id was reused with a different payload")]
    EventIdConflict,
    #[error("provider request identity was reused with different semantics")]
    ProviderRequestConflict,
    #[error("attention inbox index is inconsistent")]
    CorruptIndex,
    #[error("unresolved safety attention cannot be dismissed")]
    SafetyItemCannotBeDismissed,
    #[error("attention dismissal reason cannot be empty")]
    EmptyDismissReason,
}
