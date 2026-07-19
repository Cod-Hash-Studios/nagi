use crate::api::schema::{PluginUiBlockV1, PluginUiDocumentV1};
use crate::app::{state::PluginInspectorRefreshRequest, App};

const MAX_DOCUMENT_BYTES: usize = 64 * 1024;
const MAX_TOTAL_ITEMS: usize = 256;

pub(crate) fn parse_plugin_ui_document(stdout: &str) -> Result<PluginUiDocumentV1, String> {
    if stdout.len() > MAX_DOCUMENT_BYTES {
        return Err(format!(
            "plugin inspector document exceeds {MAX_DOCUMENT_BYTES} bytes"
        ));
    }
    let document: PluginUiDocumentV1 = serde_json::from_str(stdout.trim())
        .map_err(|error| format!("invalid plugin inspector document: {error}"))?;
    validate_document(&document)?;
    Ok(document)
}

fn validate_document(document: &PluginUiDocumentV1) -> Result<(), String> {
    if document.blocks.is_empty() {
        return Err("plugin inspector document must contain at least one block".to_owned());
    }
    if let Some(summary) = &document.summary {
        validate_text(summary, 160, false, "summary")?;
    }
    let mut total_items = 0usize;
    for block in &document.blocks {
        match block {
            PluginUiBlockV1::Section { title, rows } => {
                validate_text(title, 160, false, "section title")?;
                total_items = total_items.saturating_add(rows.len());
                for row in rows {
                    validate_text(&row.label, 160, false, "row label")?;
                    validate_text(&row.value, 2_048, true, "row value")?;
                }
            }
            PluginUiBlockV1::Metrics { items } => {
                total_items = total_items.saturating_add(items.len());
                for item in items {
                    validate_text(&item.label, 160, false, "metric label")?;
                    validate_text(&item.value, 160, false, "metric value")?;
                    if let Some(detail) = &item.detail {
                        validate_text(detail, 160, false, "metric detail")?;
                    }
                }
            }
            PluginUiBlockV1::List { title, items } => {
                if let Some(title) = title {
                    validate_text(title, 160, false, "list title")?;
                }
                total_items = total_items.saturating_add(items.len());
                for item in items {
                    validate_text(&item.title, 160, false, "list item title")?;
                    if let Some(detail) = &item.detail {
                        validate_text(detail, 2_048, true, "list item detail")?;
                    }
                }
            }
            PluginUiBlockV1::Notice { title, body, .. } => {
                total_items = total_items.saturating_add(1);
                if let Some(title) = title {
                    validate_text(title, 160, false, "notice title")?;
                }
                validate_text(body, 2_048, true, "notice body")?;
            }
        }
        if total_items > MAX_TOTAL_ITEMS {
            return Err(format!(
                "plugin inspector document exceeds {MAX_TOTAL_ITEMS} structured items"
            ));
        }
    }
    Ok(())
}

fn validate_text(
    value: &str,
    max_chars: usize,
    multiline: bool,
    field: &str,
) -> Result<(), String> {
    let chars = value.chars().count();
    if value.trim().is_empty() || chars > max_chars {
        return Err(format!(
            "plugin inspector {field} must contain 1 to {max_chars} characters"
        ));
    }
    if value
        .chars()
        .any(|character| character.is_control() && !(multiline && matches!(character, '\n' | '\t')))
    {
        return Err(format!(
            "plugin inspector {field} contains unsafe control characters"
        ));
    }
    Ok(())
}

impl App {
    pub(crate) fn process_plugin_inspector_refresh_request(&mut self) {
        let Some(request) = self.state.request_plugin_inspector_refresh.take() else {
            return;
        };
        self.state.plugin_inspector_document = None;
        self.state.plugin_inspector_error = None;
        self.state.plugin_inspector_log_id = None;

        match self.start_plugin_inspector_refresh(&request) {
            Ok(log_id) => self.state.plugin_inspector_log_id = Some(log_id),
            Err(message) => self.state.plugin_inspector_error = Some(message),
        }
    }

    fn start_plugin_inspector_refresh(
        &mut self,
        request: &PluginInspectorRefreshRequest,
    ) -> Result<String, String> {
        let mission = self
            .state
            .mission_views
            .iter()
            .find(|mission| mission.mission_id == request.mission_id)
            .cloned()
            .ok_or_else(|| "Mission no longer exists".to_owned())?;
        let plugin = self
            .state
            .installed_plugins
            .get(&request.plugin_id)
            .cloned()
            .ok_or_else(|| "Plugin is no longer installed".to_owned())?;
        if !plugin.enabled || !super::plugin_manifest_available(&plugin) {
            return Err("Plugin is disabled or its manifest is unavailable".to_owned());
        }
        let tab = plugin
            .inspector_tabs
            .iter()
            .find(|tab| tab.id == request.tab_id && tab.source == request.source)
            .ok_or_else(|| "Plugin inspector tab changed; reopen the mission".to_owned())?;
        if !plugin
            .requested_capabilities
            .iter()
            .any(|capability| capability == "mission.read")
        {
            return Err("Plugin inspector tab requires mission.read".to_owned());
        }
        let action = plugin
            .actions
            .iter()
            .find(|action| action.id == tab.source)
            .cloned()
            .ok_or_else(|| "Plugin inspector source command is unavailable".to_owned())?;
        let input = serde_json::to_string(&crate::api::schema::PluginInspectorInputV1 {
            schema_version: crate::api::schema::ContractVersionV1,
            mission,
        })
        .map_err(|error| format!("Plugin inspector input failed: {error}"))?;
        let mut context = self.current_plugin_context("tui.plugin.inspector");
        context.invocation_source = Some("mission_inspector".to_owned());
        let log = self
            .start_plugin_command(
                &plugin,
                Some(action.id),
                None,
                action.command,
                &context,
                Some(input),
            )
            .map_err(|(_, message)| message)?;
        Ok(log.log_id)
    }

    pub(crate) fn finish_plugin_inspector_command(
        &mut self,
        log: &crate::api::schema::PluginCommandLogInfo,
    ) {
        if self.state.plugin_inspector_log_id.as_deref() != Some(log.log_id.as_str()) {
            return;
        }
        self.state.plugin_inspector_log_id = None;
        if log.status != crate::api::schema::PluginCommandStatus::Succeeded {
            self.state.plugin_inspector_document = None;
            self.state.plugin_inspector_error = Some(
                log.error
                    .clone()
                    .or_else(|| {
                        log.stderr
                            .clone()
                            .filter(|stderr| !stderr.trim().is_empty())
                    })
                    .unwrap_or_else(|| "Plugin inspector command failed".to_owned()),
            );
            return;
        }
        match parse_plugin_ui_document(log.stdout.as_deref().unwrap_or_default()) {
            Ok(document) => {
                self.state.plugin_inspector_document = Some(document);
                self.state.plugin_inspector_error = None;
            }
            Err(error) => {
                self.state.plugin_inspector_document = None;
                self.state.plugin_inspector_error = Some(error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bounded_theme_safe_documents() {
        let document = parse_plugin_ui_document(
            r#"{
              "schema_version": 1,
              "summary": "Review is ready",
              "blocks": [
                {"type":"metrics","items":[{"label":"Risk","value":"Low","tone":"success"}]},
                {"type":"section","title":"Checks","rows":[{"label":"CI","value":"Passing","tone":"success"}]},
                {"type":"list","title":"Files","items":[{"title":"src/main.rs","detail":"No concern"}]},
                {"type":"notice","tone":"neutral","body":"Generated by the review plugin"}
              ]
            }"#,
        )
        .unwrap();

        assert_eq!(document.blocks.len(), 4);
    }

    #[test]
    fn rejects_oversized_unknown_and_terminal_control_payloads() {
        let oversized = format!(
            r#"{{"schema_version":1,"blocks":[{{"type":"notice","tone":"neutral","body":"{}"}}]}}"#,
            "x".repeat(MAX_DOCUMENT_BYTES)
        );
        assert!(parse_plugin_ui_document(&oversized)
            .unwrap_err()
            .contains("exceeds"));

        let unknown = r#"{"schema_version":1,"blocks":[],"html":"<script>"}"#;
        assert!(parse_plugin_ui_document(unknown)
            .unwrap_err()
            .contains("unknown field"));

        let escape = r#"{"schema_version":1,"blocks":[{"type":"notice","tone":"danger","body":"\u001b[2J"}]}"#;
        assert!(parse_plugin_ui_document(escape)
            .unwrap_err()
            .contains("unsafe control"));
    }

    #[test]
    fn completed_inspector_commands_publish_only_valid_documents() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.plugin_inspector_log_id = Some("plugin-log-1".into());
        app.finish_plugin_inspector_command(&crate::api::schema::PluginCommandLogInfo {
            log_id: "plugin-log-1".into(),
            plugin_id: "example.review".into(),
            action_id: Some("review-current".into()),
            event: None,
            command: vec!["wasi-component".into()],
            status: crate::api::schema::PluginCommandStatus::Succeeded,
            started_unix_ms: 1,
            finished_unix_ms: Some(2),
            exit_code: Some(0),
            stdout: Some(
                r#"{"schema_version":1,"blocks":[{"type":"notice","tone":"success","body":"Ready"}]}"#
                    .into(),
            ),
            stderr: Some(String::new()),
            error: None,
        });
        assert!(app.state.plugin_inspector_log_id.is_none());
        assert!(app.state.plugin_inspector_document.is_some());
        assert!(app.state.plugin_inspector_error.is_none());

        app.state.plugin_inspector_log_id = Some("plugin-log-2".into());
        app.finish_plugin_inspector_command(&crate::api::schema::PluginCommandLogInfo {
            log_id: "plugin-log-2".into(),
            plugin_id: "example.review".into(),
            action_id: Some("review-current".into()),
            event: None,
            command: vec!["wasi-component".into()],
            status: crate::api::schema::PluginCommandStatus::Succeeded,
            started_unix_ms: 3,
            finished_unix_ms: Some(4),
            exit_code: Some(0),
            stdout: Some(r#"{"schema_version":1,"blocks":[],"html":"unsafe"}"#.into()),
            stderr: Some(String::new()),
            error: None,
        });
        assert!(app.state.plugin_inspector_document.is_none());
        assert!(app
            .state
            .plugin_inspector_error
            .as_deref()
            .is_some_and(|error| error.contains("unknown field")));
    }
}
