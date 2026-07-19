use nagi_plugin_sdk::{
    stdin_json, stdout_document, InspectorInput, Tone, UiBlock, UiDocument, UiListItem,
};
use serde_json::Value;

fn main() -> Result<(), String> {
    let _: InspectorInput<Value> = stdin_json()?;
    let recipe = std::fs::read_to_string("/workspace/.nagi/project.toml").ok();
    let services = recipe.as_deref().map(parse_services).unwrap_or_default();
    let block = if services.is_empty() {
        UiBlock::Notice {
            tone: Tone::Warning,
            title: Some("No declared services".into()),
            body:
                "Add services to .nagi/project.toml. This plugin never starts processes on its own."
                    .into(),
        }
    } else {
        UiBlock::List {
            title: Some("Project recipe".into()),
            items: services
                .into_iter()
                .map(|title| UiListItem {
                    title,
                    detail: Some("Declared, not auto-started".into()),
                    tone: Tone::Neutral,
                })
                .collect(),
        }
    };
    let mut document = UiDocument::new(vec![block]);
    document.summary = Some("Local services from the trusted project recipe".into());
    stdout_document(&document)
}

fn parse_services(recipe: &str) -> Vec<String> {
    recipe
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            line.strip_prefix("[services.")?
                .strip_suffix(']')
                .map(str::to_owned)
        })
        .take(32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extracts_only_service_tables() {
        assert_eq!(
            parse_services("[setup]\n[services.web]\n[services.api]\n"),
            ["web", "api"]
        );
    }
}
