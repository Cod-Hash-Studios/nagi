use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginManifestV2 {
    pub manifest_version: PluginManifestVersionV2,
    #[schemars(length(min = 1, max = 120))]
    pub id: String,
    #[schemars(length(min = 1, max = 160))]
    pub name: String,
    #[schemars(length(min = 1, max = 128))]
    pub version: String,
    #[schemars(length(min = 1, max = 128))]
    pub min_nagi_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 2_048))]
    pub description: Option<String>,
    pub runtime: PluginRuntimeV2,
    #[schemars(length(min = 1, max = 4_096))]
    pub entrypoint: String,
    #[serde(default)]
    #[schemars(length(max = 64), inner(length(min = 1, max = 2_048)))]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub contributions: PluginContributionsV2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, schemars::JsonSchema)]
pub struct PluginManifestVersionV2;

impl Serialize for PluginManifestVersionV2 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u8(2)
    }
}

impl<'de> Deserialize<'de> for PluginManifestVersionV2 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u64::deserialize(deserializer)?;
        if value == 2 {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(format!(
                "unsupported plugin manifest version {value}; expected 2"
            )))
        }
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum PluginRuntimeV2 {
    WasiComponent,
    #[default]
    TrustedNative,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginContributionsV2 {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 64))]
    pub commands: Vec<PluginCommandContributionV2>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 16))]
    pub inspector_tabs: Vec<PluginInspectorTabContributionV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginCommandContributionV2 {
    #[schemars(length(min = 1, max = 120))]
    pub id: String,
    #[schemars(length(min = 1, max = 160))]
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 8), inner(length(min = 1, max = 64)))]
    pub contexts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginInspectorTabContributionV2 {
    #[schemars(length(min = 1, max = 120))]
    pub id: String,
    #[schemars(length(min = 1, max = 160))]
    pub title: String,
    #[schemars(length(min = 1, max = 120))]
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginGrantV1 {
    pub schema_version: super::ContractVersionV1,
    #[schemars(length(min = 1, max = 120))]
    pub plugin_id: String,
    #[schemars(length(min = 1, max = 128))]
    pub plugin_version: String,
    pub runtime: PluginRuntimeV2,
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub manifest_sha256: String,
    #[schemars(length(max = 64), inner(length(min = 1, max = 2_048)))]
    pub capabilities: Vec<String>,
    #[schemars(length(min = 1, max = 128))]
    pub approved_by: String,
    pub approved_at_millis: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at_millis: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginLockEntryV1 {
    pub schema_version: super::ContractVersionV1,
    #[schemars(length(min = 1, max = 120))]
    pub plugin_id: String,
    #[schemars(length(min = 1, max = 128))]
    pub plugin_version: String,
    pub runtime: PluginRuntimeV2,
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub manifest_sha256: String,
    #[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))]
    pub package_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"^[0-9a-f]{40}$"))]
    pub resolved_commit: Option<String>,
    #[schemars(length(max = 64), inner(length(min = 1, max = 2_048)))]
    pub requested_capabilities: Vec<String>,
    pub approval: PluginApprovalStateV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginApprovalStateV1 {
    Pending,
    Approved,
    Revoked,
    EscalationBlocked,
}
