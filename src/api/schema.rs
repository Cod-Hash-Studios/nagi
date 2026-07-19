use std::borrow::Cow;

use serde::{Deserialize, Serialize};

pub mod agents;
pub mod attention;
pub mod common;
pub mod events;
pub mod integrations;
pub mod missions;
pub mod panes;
pub mod plugin_v2;
pub mod plugins;
pub mod proof;
pub mod providers;
pub mod response;
pub mod server;
pub mod session;
pub mod tabs;
pub mod ui_contributions;
pub mod workspaces;
pub mod worktrees;

pub use agents::*;
#[allow(
    unused_imports,
    reason = "standalone product projections are published before endpoint adoption"
)]
pub use attention::*;
pub use common::*;
pub use events::*;
pub use integrations::*;
pub use missions::*;
pub use panes::*;
pub use plugin_v2::*;
pub use plugins::*;
#[allow(
    unused_imports,
    reason = "standalone product projections are published before endpoint adoption"
)]
pub use proof::*;
#[allow(
    unused_imports,
    reason = "standalone product projections are published before endpoint adoption"
)]
pub use providers::*;
pub use response::*;
pub use server::*;
pub use session::*;
pub use tabs::*;
pub use ui_contributions::*;
pub use workspaces::*;
pub use worktrees::*;

/// Numeric marker embedded in every first-generation product projection.
///
/// Keeping the version as a type, rather than a freely writable integer,
/// prevents a value with a newer shape from being mislabeled as V1.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContractVersionV1;

impl Serialize for ContractVersionV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u8(1)
    }
}

impl<'de> Deserialize<'de> for ContractVersionV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let version = u64::deserialize(deserializer)?;
        if version == 1 {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(format!(
                "unsupported product contract version {version}; expected 1"
            )))
        }
    }
}

impl schemars::JsonSchema for ContractVersionV1 {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        "ContractVersionV1".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "integer",
            "const": 1
        })
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Request {
    pub id: String,
    #[serde(flatten)]
    pub method: Method,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "method", content = "params")]
// Request enums are short-lived wire values; keeping variants direct preserves
// the simple serde shape and avoids boxing churn across every caller.
#[allow(clippy::large_enum_variant)]
pub enum Method {
    #[serde(rename = "ping")]
    Ping(PingParams),
    #[serde(rename = "server.stop")]
    ServerStop(EmptyParams),
    #[serde(rename = "server.live_handoff")]
    ServerLiveHandoff(ServerLiveHandoffParams),
    #[serde(rename = "server.reload_config")]
    ServerReloadConfig(EmptyParams),
    #[serde(rename = "server.agent_manifests")]
    ServerAgentManifests(EmptyParams),
    #[serde(rename = "server.reload_agent_manifests")]
    ServerReloadAgentManifests(EmptyParams),
    #[serde(rename = "notification.show")]
    NotificationShow(NotificationShowParams),
    #[serde(rename = "mission.create")]
    MissionCreate(MissionCreateParams),
    #[serde(rename = "mission.list")]
    MissionList(EmptyParams),
    #[serde(rename = "mission.get")]
    MissionGet(MissionTarget),
    #[serde(rename = "mission.configure")]
    MissionConfigure(MissionConfigureParams),
    #[serde(rename = "mission.start")]
    MissionStart(MissionStartParams),
    #[serde(rename = "mission.respond")]
    MissionRespond(MissionRespondParams),
    #[serde(rename = "mission.proof.get")]
    MissionProofGet(MissionTarget),
    #[serde(rename = "mission.handoff.preview")]
    MissionHandoffPreview(MissionHandoffPreviewParams),
    #[serde(rename = "mission.handoff.start")]
    MissionHandoffStart(MissionHandoffStartParams),
    #[serde(rename = "mission.close")]
    MissionClose(MissionTarget),
    #[serde(rename = "attention.list")]
    AttentionList(AttentionListParams),
    #[serde(rename = "attention.get")]
    AttentionGet(AttentionTarget),
    #[serde(rename = "client.window_title.set")]
    ClientWindowTitleSet(ClientWindowTitleSetParams),
    #[serde(rename = "client.window_title.clear")]
    ClientWindowTitleClear(EmptyParams),
    #[serde(rename = "session.snapshot")]
    SessionSnapshot(EmptyParams),
    #[serde(rename = "workspace.create")]
    WorkspaceCreate(WorkspaceCreateParams),
    #[serde(rename = "workspace.list")]
    WorkspaceList(EmptyParams),
    #[serde(rename = "workspace.get")]
    WorkspaceGet(WorkspaceTarget),
    #[serde(rename = "workspace.focus")]
    WorkspaceFocus(WorkspaceTarget),
    #[serde(rename = "workspace.rename")]
    WorkspaceRename(WorkspaceRenameParams),
    #[serde(rename = "workspace.move")]
    WorkspaceMove(WorkspaceMoveParams),
    #[serde(rename = "workspace.report_metadata")]
    WorkspaceReportMetadata(WorkspaceReportMetadataParams),
    #[serde(rename = "workspace.close")]
    WorkspaceClose(WorkspaceTarget),
    #[serde(rename = "worktree.list")]
    WorktreeList(WorktreeListParams),
    #[serde(rename = "worktree.create")]
    WorktreeCreate(WorktreeCreateParams),
    #[serde(rename = "worktree.open")]
    WorktreeOpen(WorktreeOpenParams),
    #[serde(rename = "worktree.remove")]
    WorktreeRemove(WorktreeRemoveParams),
    #[serde(rename = "tab.create")]
    TabCreate(TabCreateParams),
    #[serde(rename = "tab.list")]
    TabList(TabListParams),
    #[serde(rename = "tab.get")]
    TabGet(TabTarget),
    #[serde(rename = "tab.focus")]
    TabFocus(TabTarget),
    #[serde(rename = "tab.rename")]
    TabRename(TabRenameParams),
    #[serde(rename = "tab.move")]
    TabMove(TabMoveParams),
    #[serde(rename = "tab.close")]
    TabClose(TabTarget),
    #[serde(rename = "agent.list")]
    AgentList(EmptyParams),
    #[serde(rename = "agent.get")]
    AgentGet(AgentTarget),
    #[serde(rename = "agent.read")]
    AgentRead(AgentReadParams),
    #[serde(rename = "agent.explain")]
    AgentExplain(AgentTarget),
    #[serde(rename = "agent.send")]
    AgentSend(AgentSendParams),
    #[serde(rename = "agent.rename")]
    AgentRename(AgentRenameParams),
    #[serde(rename = "agent.focus")]
    AgentFocus(AgentTarget),
    #[serde(rename = "agent.start")]
    AgentStart(AgentStartParams),
    #[serde(rename = "pane.split")]
    PaneSplit(PaneSplitParams),
    #[serde(rename = "pane.swap")]
    PaneSwap(PaneSwapParams),
    #[serde(rename = "pane.move")]
    PaneMove(PaneMoveParams),
    #[serde(rename = "pane.zoom")]
    PaneZoom(PaneZoomParams),
    #[serde(rename = "pane.layout")]
    PaneLayout(PaneLayoutParams),
    #[serde(rename = "pane.process_info")]
    PaneProcessInfo(PaneProcessInfoParams),
    #[serde(rename = "layout.export")]
    LayoutExport(LayoutExportParams),
    #[serde(rename = "layout.apply")]
    LayoutApply(LayoutApplyParams),
    #[serde(rename = "layout.set_split_ratio")]
    LayoutSetSplitRatio(LayoutSetSplitRatioParams),
    #[serde(rename = "pane.neighbor")]
    PaneNeighbor(PaneNeighborParams),
    #[serde(rename = "pane.edges")]
    PaneEdges(PaneEdgesParams),
    #[serde(rename = "pane.focus_direction")]
    PaneFocusDirection(PaneFocusDirectionParams),
    #[serde(rename = "pane.resize")]
    PaneResize(PaneResizeParams),
    #[serde(rename = "pane.list")]
    PaneList(PaneListParams),
    #[serde(rename = "pane.current")]
    PaneCurrent(PaneCurrentParams),
    #[serde(rename = "pane.get")]
    PaneGet(PaneTarget),
    #[serde(rename = "pane.focus")]
    PaneFocus(PaneTarget),
    #[serde(rename = "pane.rename")]
    PaneRename(PaneRenameParams),
    #[serde(rename = "pane.send_text")]
    PaneSendText(PaneSendTextParams),
    #[serde(rename = "pane.send_keys")]
    PaneSendKeys(PaneSendKeysParams),
    #[serde(rename = "pane.send_input")]
    PaneSendInput(PaneSendInputParams),
    #[serde(rename = "pane.read")]
    PaneRead(PaneReadParams),
    #[serde(rename = "pane.graphics.set")]
    PaneGraphicsSet(PaneGraphicsSetParams),
    #[serde(rename = "pane.graphics.clear")]
    PaneGraphicsClear(PaneGraphicsClearParams),
    #[serde(rename = "pane.graphics.info")]
    PaneGraphicsInfo(PaneTarget),
    #[serde(rename = "pane.graphics.stream")]
    #[schemars(skip)]
    PaneGraphicsStream(PaneGraphicsStreamParams),
    #[serde(skip)]
    #[schemars(skip)]
    PaneGraphicsStreamSet(PaneGraphicsSetParams),
    #[serde(skip)]
    #[schemars(skip)]
    PaneGraphicsStreamOpen(PaneGraphicsStreamParams),
    #[serde(skip)]
    #[schemars(skip)]
    PaneGraphicsStreamClose(PaneGraphicsStreamParams),
    #[serde(rename = "pane.report_agent")]
    PaneReportAgent(PaneReportAgentParams),
    #[serde(rename = "pane.report_agent_session")]
    PaneReportAgentSession(PaneReportAgentSessionParams),
    #[serde(rename = "pane.report_metadata")]
    PaneReportMetadata(PaneReportMetadataParams),
    #[serde(rename = "pane.clear_agent_authority")]
    PaneClearAgentAuthority(PaneClearAgentAuthorityParams),
    #[serde(rename = "pane.release_agent")]
    PaneReleaseAgent(PaneReleaseAgentParams),
    #[serde(rename = "pane.close")]
    PaneClose(PaneTarget),
    #[serde(rename = "popup.close")]
    PopupClose(EmptyParams),
    #[serde(rename = "events.subscribe")]
    EventsSubscribe(EventsSubscribeParams),
    #[serde(rename = "events.wait")]
    EventsWait(EventsWaitParams),
    #[serde(rename = "pane.wait_for_output")]
    PaneWaitForOutput(PaneWaitForOutputParams),
    #[serde(rename = "integration.install")]
    IntegrationInstall(IntegrationInstallParams),
    #[serde(rename = "integration.uninstall")]
    IntegrationUninstall(IntegrationUninstallParams),
    #[serde(rename = "plugin.link")]
    PluginLink(PluginLinkParams),
    #[serde(rename = "plugin.list")]
    PluginList(PluginListParams),
    #[serde(rename = "plugin.unlink")]
    PluginUnlink(PluginUnlinkParams),
    #[serde(rename = "plugin.enable")]
    PluginEnable(PluginSetEnabledParams),
    #[serde(rename = "plugin.disable")]
    PluginDisable(PluginSetEnabledParams),
    #[serde(rename = "plugin.capability.approve")]
    PluginCapabilityApprove(PluginCapabilityApproveParams),
    #[serde(rename = "plugin.capability.revoke")]
    PluginCapabilityRevoke(PluginSetEnabledParams),
    #[serde(rename = "plugin.action.list")]
    PluginActionList(PluginActionListParams),
    #[serde(rename = "plugin.action.invoke")]
    PluginActionInvoke(PluginActionInvokeParams),
    #[serde(rename = "plugin.log.list")]
    PluginLogList(PluginLogListParams),
    #[serde(rename = "plugin.pane.open")]
    PluginPaneOpen(PluginPaneOpenParams),
    #[serde(rename = "plugin.pane.focus")]
    PluginPaneFocus(PluginPaneFocusParams),
    #[serde(rename = "plugin.pane.close")]
    PluginPaneClose(PluginPaneCloseParams),
}

#[cfg(test)]
mod tests;
