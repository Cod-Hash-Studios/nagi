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
    pub border: Option<ThemeBorderStyle>,
    pub selection: Option<ThemeSelectionStyle>,
    pub density: Option<ThemeDensity>,
    pub motion: Option<ThemeMotion>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ThemeComponents {
    pub border: ThemeBorderStyle,
    pub selection: ThemeSelectionStyle,
    pub density: ThemeDensity,
    pub motion: ThemeMotion,
}

impl From<ThemeComponentsV1> for ThemeComponents {
    fn from(components: ThemeComponentsV1) -> Self {
        Self {
            border: components.border.unwrap_or_default(),
            selection: components.selection.unwrap_or_default(),
            density: components.density.unwrap_or_default(),
            motion: components.motion.unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ThemeBorderStyle {
    #[default]
    Soft,
    Rounded,
    Plain,
    Ascii,
}

impl ThemeBorderStyle {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Soft => "soft",
            Self::Rounded => "rounded",
            Self::Plain => "plain",
            Self::Ascii => "ascii",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ThemeSelectionStyle {
    #[default]
    Rail,
    Fill,
}

impl ThemeSelectionStyle {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Rail => "rail",
            Self::Fill => "fill",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ThemeDensity {
    Compact,
    #[default]
    Comfortable,
}

impl ThemeDensity {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Comfortable => "comfortable",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ThemeMotion {
    None,
    #[default]
    Subtle,
}

impl ThemeMotion {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Subtle => "subtle",
        }
    }
}
