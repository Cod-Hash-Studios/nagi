#![allow(
    dead_code,
    reason = "V1 attention projections are bundled before the public inbox endpoint is wired"
)]

use serde::{Deserialize, Serialize};

use super::{ContractVersionV1, MissionProvider};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AttentionListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub mission_id: Option<String>,
    #[serde(default)]
    pub include_closed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AttentionTarget {
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub attention_id: String,
}

/// Provider-neutral attention projection. It deliberately separates the item
/// lifecycle from response delivery so an uncertain write cannot look
/// resolved merely because the provider connection disappeared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AttentionItemV1 {
    pub schema_version: ContractVersionV1,
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub attention_id: String,
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub mission_id: String,
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_.:-]+$"))]
    pub run_id: String,
    #[schemars(length(min = 1, max = 256))]
    pub session_id: String,
    pub pane: AttentionPaneTargetV1,
    pub kind: AttentionKindV1,
    #[schemars(length(min = 1, max = 4_096))]
    pub requested_action: String,
    #[schemars(length(min = 1, max = 4_096))]
    pub scope: String,
    pub risk: AttentionRiskV1,
    pub provider: MissionProvider,
    pub source: AttentionSourceV1,
    pub response_capability: AttentionResponseCapabilityV1,
    /// Provider-native question contract. Empty for permission and recovered
    /// items. IDs are exact response keys, not UI-generated labels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 4))]
    pub questions: Vec<AttentionQuestionV1>,
    pub created_at_millis: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_millis: Option<u64>,
    #[schemars(range(min = 1))]
    pub occurrence_count: u32,
    pub unread: bool,
    pub state: AttentionStateV1,
    pub delivery: AttentionDeliveryStateV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AttentionQuestionV1 {
    #[schemars(length(min = 1, max = 1_024))]
    pub id: String,
    #[schemars(length(min = 1, max = 128))]
    pub header: String,
    #[schemars(length(min = 1, max = 4_096))]
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 8))]
    pub options: Vec<AttentionQuestionOptionV1>,
    pub multiple: bool,
    pub custom_allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AttentionQuestionOptionV1 {
    #[schemars(length(min = 1, max = 1_024))]
    pub label: String,
    #[schemars(length(max = 4_096))]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AttentionPaneTargetV1 {
    #[schemars(length(min = 1, max = 256))]
    pub workspace_id: String,
    #[schemars(length(min = 1, max = 256))]
    pub pane_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKindV1 {
    PermissionRequest,
    ProviderQuestion,
    CommandFailed,
    WorktreeConflict,
    TurnComplete,
    Disconnected,
    SecurityWarning,
    ManualVerification,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttentionRiskV1 {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttentionSourceV1 {
    StructuredHook,
    ProviderApi,
    Process,
    TerminalHeuristic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttentionResponseCapabilityV1 {
    Reliable,
    OpenPaneOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AttentionStateV1 {
    Open,
    PendingResponse {
        decision: AttentionDecisionV1,
        #[schemars(length(min = 1, max = 128))]
        actor: String,
        requested_at_millis: u64,
    },
    Resolved {
        decision: AttentionDecisionV1,
        #[schemars(length(min = 1, max = 128))]
        actor: String,
        at_millis: u64,
    },
    ReconciliationRequired {
        decision: AttentionDecisionV1,
        #[schemars(length(min = 1, max = 128))]
        actor: String,
        code: AttentionFailureCodeV1,
        at_millis: u64,
    },
    Dismissed {
        #[schemars(length(min = 1, max = 128))]
        actor: String,
        #[schemars(length(min = 1, max = 1_024))]
        reason: String,
        at_millis: u64,
    },
    Expired {
        at_millis: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttentionDecisionV1 {
    ApproveOnce,
    ApproveForSession,
    AllowForMission,
    Deny,
    Answer,
}

/// State of the latest provider response attempt. `DeliveryUnknown` is kept
/// distinct from a definite rejection because retrying those cases has
/// different safety consequences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AttentionDeliveryStateV1 {
    NotRequested,
    Pending {
        #[schemars(range(min = 1))]
        attempt: u32,
        requested_at_millis: u64,
    },
    Acknowledged {
        #[schemars(range(min = 1))]
        attempt: u32,
        at_millis: u64,
    },
    DefinitelyNotApplied {
        #[schemars(range(min = 1))]
        attempt: u32,
        code: AttentionFailureCodeV1,
        at_millis: u64,
    },
    DeliveryUnknown {
        #[schemars(range(min = 1))]
        attempt: u32,
        code: AttentionFailureCodeV1,
        at_millis: u64,
    },
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttentionFailureCodeV1 {
    Rejected,
    DisconnectedBeforeWrite,
    Timeout,
    TransportClosed,
}
