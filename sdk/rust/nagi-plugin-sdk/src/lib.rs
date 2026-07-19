use std::io::{self, Read, Write};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

pub const CONTRACT_VERSION: u8 = 1;
pub const MAX_INPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub plugin_id: String,
    pub action_id: Option<String>,
    pub context: serde_json::Value,
}

impl Invocation {
    pub fn from_env() -> Result<Self, String> {
        let plugin_id =
            std::env::var("NAGI_PLUGIN_ID").map_err(|_| "NAGI_PLUGIN_ID is missing".to_owned())?;
        let action_id = std::env::var("NAGI_PLUGIN_ACTION_ID").ok();
        let raw = std::env::var("NAGI_PLUGIN_CONTEXT_JSON").unwrap_or_else(|_| "{}".to_owned());
        let context = serde_json::from_str(&raw)
            .map_err(|error| format!("NAGI_PLUGIN_CONTEXT_JSON is invalid: {error}"))?;
        Ok(Self {
            plugin_id,
            action_id,
            context,
        })
    }
}

pub fn read_json<T: DeserializeOwned>(input: impl Read) -> Result<T, String> {
    let mut bytes = Vec::new();
    input
        .take((MAX_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("plugin input read failed: {error}"))?;
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(format!("plugin input exceeds {MAX_INPUT_BYTES} bytes"));
    }
    serde_json::from_slice(&bytes).map_err(|error| format!("plugin input JSON is invalid: {error}"))
}

pub fn write_document(document: &UiDocument, mut output: impl Write) -> Result<(), String> {
    document.validate()?;
    serde_json::to_writer(&mut output, document)
        .map_err(|error| format!("plugin document serialization failed: {error}"))?;
    output
        .write_all(b"\n")
        .map_err(|error| format!("plugin document write failed: {error}"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectorInput<T = serde_json::Value> {
    pub schema_version: u8,
    pub mission: T,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiDocument {
    pub schema_version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub blocks: Vec<UiBlock>,
}

impl UiDocument {
    pub fn new(blocks: Vec<UiBlock>) -> Self {
        Self {
            schema_version: CONTRACT_VERSION,
            summary: None,
            blocks,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != CONTRACT_VERSION {
            return Err("unsupported UI document schema version".to_owned());
        }
        if self.blocks.len() > 32 {
            return Err("UI document exceeds 32 blocks".to_owned());
        }
        if self
            .summary
            .as_ref()
            .is_some_and(|summary| summary.len() > 160)
        {
            return Err("UI document summary exceeds 160 bytes".to_owned());
        }
        for block in &self.blocks {
            block.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiBlock {
    Section {
        title: String,
        rows: Vec<UiRow>,
    },
    Metrics {
        items: Vec<UiMetric>,
    },
    List {
        title: Option<String>,
        items: Vec<UiListItem>,
    },
    Notice {
        tone: Tone,
        title: Option<String>,
        body: String,
    },
}

impl UiBlock {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Section { title, rows } => {
                bounded(title, 1, 160, "section title")?;
                if rows.len() > 64 {
                    return Err("section exceeds 64 rows".to_owned());
                }
                for row in rows {
                    bounded(&row.label, 1, 160, "row label")?;
                    bounded(&row.value, 0, 2048, "row value")?;
                }
            }
            Self::Metrics { items } => {
                if items.len() > 16 {
                    return Err("metrics block exceeds 16 items".to_owned());
                }
                for item in items {
                    bounded(&item.label, 1, 160, "metric label")?;
                    bounded(&item.value, 1, 160, "metric value")?;
                }
            }
            Self::List { title, items } => {
                if let Some(title) = title {
                    bounded(title, 1, 160, "list title")?;
                }
                if items.len() > 64 {
                    return Err("list exceeds 64 items".to_owned());
                }
            }
            Self::Notice { title, body, .. } => {
                if let Some(title) = title {
                    bounded(title, 1, 160, "notice title")?;
                }
                bounded(body, 1, 2048, "notice body")?;
            }
        }
        Ok(())
    }
}

fn bounded(value: &str, min: usize, max: usize, label: &str) -> Result<(), String> {
    if value.len() < min || value.len() > max || value.chars().any(char::is_control) {
        Err(format!("{label} must contain {min}..={max} safe bytes"))
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiRow {
    pub label: String,
    pub value: String,
    #[serde(default)]
    pub tone: Tone,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiMetric {
    pub label: String,
    pub value: String,
    pub detail: Option<String>,
    #[serde(default)]
    pub tone: Tone,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiListItem {
    pub title: String,
    pub detail: Option<String>,
    #[serde(default)]
    pub tone: Tone,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    #[default]
    Neutral,
    Success,
    Warning,
    Danger,
}

pub fn stdin_json<T: DeserializeOwned>() -> Result<T, String> {
    read_json(io::stdin().lock())
}
pub fn stdout_document(document: &UiDocument) -> Result<(), String> {
    write_document(document, io::stdout().lock())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_host_compatible_bounded_documents() {
        let document = UiDocument::new(vec![UiBlock::Notice {
            tone: Tone::Success,
            title: Some("Checks".to_owned()),
            body: "All green".to_owned(),
        }]);
        let mut output = Vec::new();
        write_document(&document, &mut output).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output).unwrap()["schema_version"],
            1
        );
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("\"type\":\"notice\""));
    }

    #[test]
    fn rejects_unsafe_control_text() {
        let document = UiDocument::new(vec![UiBlock::Notice {
            tone: Tone::Danger,
            title: None,
            body: "bad\u{0007}".to_owned(),
        }]);
        assert!(document.validate().unwrap_err().contains("safe bytes"));
    }

    #[test]
    fn rejects_input_larger_than_the_host_contract() {
        let mut input = vec![b' '; 1024 * 1024];
        input.extend_from_slice(b"{}");

        let error = read_json::<serde_json::Value>(input.as_slice()).unwrap_err();

        assert!(error.contains("exceeds 1048576 bytes"), "{error}");
    }
}
