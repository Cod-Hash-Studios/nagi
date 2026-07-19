use serde::{Deserialize, Serialize};

use super::{ContractVersionV1, MissionViewV1};

/// Host-authored input for a mission inspector contribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginInspectorInputV1 {
    pub schema_version: ContractVersionV1,
    pub mission: MissionViewV1,
}

/// A bounded, theme-safe document returned by a plugin inspector source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginUiDocumentV1 {
    pub schema_version: ContractVersionV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 160))]
    pub summary: Option<String>,
    #[schemars(length(max = 32))]
    pub blocks: Vec<PluginUiBlockV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginUiBlockV1 {
    Section {
        #[schemars(length(min = 1, max = 160))]
        title: String,
        #[schemars(length(max = 64))]
        rows: Vec<PluginUiRowV1>,
    },
    Metrics {
        #[schemars(length(max = 16))]
        items: Vec<PluginUiMetricV1>,
    },
    List {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(length(min = 1, max = 160))]
        title: Option<String>,
        #[schemars(length(max = 64))]
        items: Vec<PluginUiListItemV1>,
    },
    Notice {
        tone: PluginUiToneV1,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(length(min = 1, max = 160))]
        title: Option<String>,
        #[schemars(length(min = 1, max = 2_048))]
        body: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginUiRowV1 {
    #[schemars(length(min = 1, max = 160))]
    pub label: String,
    #[schemars(length(max = 2_048))]
    pub value: String,
    #[serde(default)]
    pub tone: PluginUiToneV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginUiMetricV1 {
    #[schemars(length(min = 1, max = 160))]
    pub label: String,
    #[schemars(length(min = 1, max = 160))]
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 160))]
    pub detail: Option<String>,
    #[serde(default)]
    pub tone: PluginUiToneV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginUiListItemV1 {
    #[schemars(length(min = 1, max = 160))]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 2_048))]
    pub detail: Option<String>,
    #[serde(default)]
    pub tone: PluginUiToneV1,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PluginUiToneV1 {
    #[default]
    Neutral,
    Success,
    Warning,
    Danger,
}
