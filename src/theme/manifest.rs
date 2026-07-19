use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThemeManifestV1 {
    pub meta: ThemeMetaV1,
    #[serde(default)]
    pub palette: BTreeMap<String, String>,
    pub semantic: ThemeSemanticV1,
    #[serde(default)]
    pub components: ThemeComponentsV1,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThemeMetaV1 {
    pub name: String,
    pub schema: u32,
    pub appearance: ThemeAppearance,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ThemeAppearance {
    Dark,
    Light,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThemeSemanticV1 {
    pub canvas: String,
    pub panel: String,
    pub text: String,
    pub text_muted: String,
    pub text_bright: Option<String>,
    pub focus: String,
    pub attention: String,
    pub working: String,
    pub proof_fresh: String,
    pub proof_stale: String,
    pub canvas_dim: Option<String>,
    pub text_faint: Option<String>,
    pub special: Option<String>,
    pub done: Option<String>,
    pub caution: Option<String>,
    pub border: Option<String>,
    pub danger: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThemeComponentsV1 {
    pub border: Option<String>,
    pub selection: Option<String>,
    pub density: Option<String>,
    pub motion: Option<String>,
}
