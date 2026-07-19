#![allow(
    dead_code,
    reason = "V1 provider capabilities are bundled before capability negotiation is public"
)]

use serde::{Deserialize, Serialize};

use super::{ContractVersionV1, MissionProvider, MissionProviderMode};

/// Capability declaration for one provider integration and execution mode.
/// A four-state capability avoids turning an absent or partial protocol signal
/// into a misleading boolean claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderCapabilitiesV1 {
    pub schema_version: ContractVersionV1,
    pub provider: MissionProvider,
    pub mode: MissionProviderMode,
    pub source: ProviderCapabilitySourceV1,
    #[schemars(range(min = 1))]
    pub adapter_contract_version: u16,
    pub runtime_requirement: ProviderRuntimeRequirementV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128))]
    pub provider_version: Option<String>,
    pub resume: ProviderCapabilityV1,
    pub turns: ProviderCapabilityV1,
    pub permissions: ProviderCapabilityV1,
    pub questions: ProviderCapabilityV1,
    pub diffs: ProviderCapabilityV1,
    pub interruption: ProviderCapabilityV1,
    pub usage: ProviderCapabilityV1,
    pub streaming: ProviderCapabilityV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCapabilitySourceV1 {
    Negotiated,
    AdapterDeclaration,
    ObservedFallback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "strategy", rename_all = "snake_case")]
pub enum ProviderRuntimeRequirementV1 {
    NotPinned,
    Exact {
        #[schemars(length(min = 1, max = 128))]
        version: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProviderCapabilityV1 {
    Supported,
    Partial {
        #[schemars(length(min = 1, max = 1_024))]
        detail: String,
    },
    Unsupported {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(length(min = 1, max = 1_024))]
        detail: Option<String>,
    },
    Unknown {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(length(min = 1, max = 1_024))]
        detail: Option<String>,
    },
}
