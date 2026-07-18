#![allow(
    dead_code,
    reason = "structured run observations are tested ahead of the public mission cockpit"
)]

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Starting,
    Ready,
    Working,
    NeedsApproval,
    NeedsAnswer,
    Blocked,
    Failed,
    Disconnected,
    Unknown,
}

impl SessionStatus {
    fn requires_attention(self) -> bool {
        matches!(
            self,
            Self::NeedsApproval | Self::NeedsAnswer | Self::Blocked
        )
    }

    fn is_safety_worsening(self) -> bool {
        self.requires_attention() || matches!(self, Self::Failed | Self::Disconnected)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationSource {
    StructuredHook,
    ProviderApi,
    Process,
    TerminalHeuristic,
}

impl ObservationSource {
    const fn confidence(self) -> Confidence {
        match self {
            Self::StructuredHook | Self::ProviderApi => Confidence::Exact,
            Self::Process => Confidence::Strong,
            Self::TerminalHeuristic => Confidence::Inferred,
        }
    }

    const fn priority(self) -> u8 {
        match self {
            Self::StructuredHook | Self::ProviderApi => 3,
            Self::Process => 2,
            Self::TerminalHeuristic => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Exact,
    Strong,
    Inferred,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SessionObservation {
    event_id: String,
    status: SessionStatus,
    source: ObservationSource,
    provider_sequence: Option<u64>,
    received_sequence: u64,
    turn_id: Option<String>,
    observed_at_millis: u64,
    expires_at_millis: Option<u64>,
    turn_completed: bool,
}

impl SessionObservation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        event_id: impl Into<String>,
        status: SessionStatus,
        source: ObservationSource,
        provider_sequence: Option<u64>,
        received_sequence: u64,
        turn_id: Option<&str>,
        observed_at_millis: u64,
    ) -> Self {
        Self {
            event_id: event_id.into(),
            status,
            source,
            provider_sequence,
            received_sequence,
            turn_id: turn_id.map(str::to_owned),
            observed_at_millis,
            expires_at_millis: None,
            turn_completed: false,
        }
    }

    #[must_use]
    pub const fn expires_at(mut self, expires_at_millis: u64) -> Self {
        self.expires_at_millis = Some(expires_at_millis);
        self
    }

    #[must_use]
    pub const fn with_turn_completed(mut self) -> Self {
        self.turn_completed = true;
        self
    }

    fn normalize_heuristic(mut self) -> Self {
        match self.source {
            ObservationSource::StructuredHook | ObservationSource::ProviderApi => self,
            ObservationSource::Process => {
                self.provider_sequence = None;
                self.turn_id = None;
                self.turn_completed = false;
                self
            }
            ObservationSource::TerminalHeuristic => {
                let claimed_completion = self.turn_completed;
                self.provider_sequence = None;
                self.turn_id = None;
                self.turn_completed = false;
                if claimed_completion
                    || !matches!(self.status, SessionStatus::Ready | SessionStatus::Unknown)
                {
                    self.status = SessionStatus::Unknown;
                }
                self
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SessionSnapshot {
    status: SessionStatus,
    source: ObservationSource,
    confidence: Confidence,
    provider_sequence: Option<u64>,
    received_sequence: u64,
    current_turn_id: Option<String>,
    current_turn_completed: bool,
    observed_at_millis: u64,
    expires_at_millis: Option<u64>,
    applied_event_ids: BTreeSet<String>,
}

impl SessionSnapshot {
    #[must_use]
    pub fn starting(observed_at_millis: u64) -> Self {
        Self {
            status: SessionStatus::Starting,
            source: ObservationSource::Process,
            confidence: Confidence::Strong,
            provider_sequence: None,
            received_sequence: 0,
            current_turn_id: None,
            current_turn_completed: false,
            observed_at_millis,
            expires_at_millis: None,
            applied_event_ids: BTreeSet::new(),
        }
    }

    #[must_use]
    pub fn apply(mut self, observation: SessionObservation) -> Self {
        let observation = observation.normalize_heuristic();
        if self.applied_event_ids.contains(&observation.event_id)
            || !self.should_accept(&observation)
        {
            return self;
        }

        let is_new_turn = matches!(
            (&observation.turn_id, &self.current_turn_id),
            (Some(incoming), Some(current)) if incoming != current
        ) || matches!(
            (&observation.turn_id, &self.current_turn_id),
            (Some(_), None)
        );
        self.status = observation.status;
        self.source = observation.source;
        self.confidence = observation.source.confidence();
        if observation.provider_sequence.is_some() {
            self.provider_sequence = observation.provider_sequence;
        }
        self.received_sequence = observation.received_sequence;
        if observation.turn_id.is_some() {
            self.current_turn_id = observation.turn_id;
            self.current_turn_completed = if is_new_turn {
                observation.turn_completed
            } else {
                self.current_turn_completed || observation.turn_completed
            };
        }
        self.observed_at_millis = observation.observed_at_millis;
        self.expires_at_millis = observation.expires_at_millis;
        self.applied_event_ids.insert(observation.event_id);
        self
    }

    fn should_accept(&self, observation: &SessionObservation) -> bool {
        let same_turn = observation.turn_id == self.current_turn_id;
        let unscoped = observation.turn_id.is_none();

        if observation.received_sequence <= self.received_sequence {
            return false;
        }

        if same_turn
            && matches!(
                (observation.provider_sequence, self.provider_sequence),
                (Some(incoming), Some(current)) if incoming <= current
            )
        {
            return false;
        }

        if (same_turn || unscoped)
            && self.current_turn_completed
            && !observation.status.is_safety_worsening()
        {
            return false;
        }

        if observation.source.priority() < self.source.priority()
            && !matches!(
                observation.status,
                SessionStatus::Failed | SessionStatus::Disconnected
            )
            && !(self.status == SessionStatus::Starting && self.applied_event_ids.is_empty())
        {
            return false;
        }

        true
    }

    #[must_use]
    pub const fn status(&self) -> SessionStatus {
        self.status
    }

    #[must_use]
    pub const fn source(&self) -> ObservationSource {
        self.source
    }

    #[must_use]
    pub const fn confidence(&self) -> Confidence {
        self.confidence
    }

    #[must_use]
    pub const fn current_turn_completed(&self) -> bool {
        self.current_turn_completed
    }

    #[must_use]
    pub fn current_turn_id(&self) -> Option<&str> {
        self.current_turn_id.as_deref()
    }

    #[must_use]
    pub fn applied_event_count(&self) -> usize {
        self.applied_event_ids.len()
    }

    #[must_use]
    pub fn visible_at(&self, now_millis: u64) -> VisibleSessionState {
        let expired = self
            .expires_at_millis
            .is_some_and(|expires_at| now_millis >= expires_at);

        VisibleSessionState {
            status: if expired {
                SessionStatus::Unknown
            } else {
                self.status
            },
            source: self.source,
            confidence: self.confidence,
            age_millis: now_millis.saturating_sub(self.observed_at_millis),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct VisibleSessionState {
    pub status: SessionStatus,
    pub source: ObservationSource,
    pub confidence: Confidence,
    pub age_millis: u64,
}
