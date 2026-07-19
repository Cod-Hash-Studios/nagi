use std::{collections::BTreeMap, path::PathBuf};

use tokio::sync::mpsc;

use crate::mission::model::ProviderKind;

mod acp;
mod adapter;
mod claude;
mod codex;
mod opencode;
mod probe;
mod registry;

#[cfg(test)]
mod conformance;

pub(crate) use acp::AcpEndpoint;
pub(crate) use adapter::{
    AdapterContractVersion, ProviderAdapterDescriptor, ProviderCapabilities, ProviderRuntimeVersion,
};
pub(crate) use claude::TESTED_VERSION as CLAUDE_TESTED_VERSION;
pub(crate) use codex::TESTED_VERSION as CODEX_TESTED_VERSION;
pub(crate) use opencode::TESTED_VERSION as OPENCODE_TESTED_VERSION;
pub(crate) use probe::probe_protocol;

const COMMAND_CHANNEL_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SandboxAccess {
    ReadOnly,
    #[allow(
        dead_code,
        reason = "workspace writes stay closed until interactive consent is public"
    )]
    WorkspaceWriteConfirmed,
}

impl SandboxAccess {
    const fn codex_value(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWriteConfirmed => "workspace-write",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StartOrResume {
    pub(crate) run_id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) resume_session_id: Option<String>,
    pub(crate) initial_input: String,
    pub(crate) sandbox: SandboxAccess,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(
    dead_code,
    reason = "provider replies stay closed until interactive consent is public"
)]
pub(crate) enum ProviderResponse {
    Approve,
    ApproveForSession,
    Decline,
    Answers(BTreeMap<String, Vec<String>>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProviderCommand {
    StartOrResume(StartOrResume),
    #[allow(
        dead_code,
        reason = "follow-up turns are staged behind the public mission lifecycle"
    )]
    SendTurn {
        input: String,
    },
    #[allow(
        dead_code,
        reason = "provider replies stay closed until interactive consent is public"
    )]
    Respond {
        token: ResponseToken,
        response: ProviderResponse,
    },
    #[allow(
        dead_code,
        reason = "interrupt control is staged behind the public mission lifecycle"
    )]
    Interrupt,
    #[allow(
        dead_code,
        reason = "handoff quiescing is staged until managed actors own live writes"
    )]
    Quiesce,
    Shutdown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResponseToken {
    rpc_id: RpcId,
    method: String,
    request_id: String,
}

impl ResponseToken {
    pub(crate) fn request_id(&self) -> &str {
        &self.request_id
    }

    #[cfg(test)]
    pub(crate) fn for_test(request_id: u64, method: impl Into<String>) -> Self {
        let rpc_id = RpcId::Number(request_id);
        Self {
            request_id: rpc_id.audit_id(),
            rpc_id,
            method: method.into(),
        }
    }

    fn for_external(request_id: &serde_json::Value, method: impl Into<String>) -> Option<Self> {
        let rpc_id = RpcId::from_json(request_id)?;
        Some(Self {
            request_id: rpc_id.audit_id(),
            rpc_id,
            method: method.into(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AttentionClass {
    CommandApproval,
    FileChangeApproval,
    PermissionApproval,
    UserInput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProviderAttention {
    pub(crate) token: ResponseToken,
    pub(crate) class: AttentionClass,
    pub(crate) thread_id: String,
    pub(crate) turn_id: String,
    pub(crate) item_id: String,
    pub(crate) requested_action: String,
    pub(crate) questions: Vec<ProviderQuestion>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProviderQuestion {
    /// Exact key expected by the provider response protocol.
    pub(crate) id: String,
    pub(crate) header: String,
    pub(crate) prompt: String,
    pub(crate) options: Vec<ProviderQuestionOption>,
    pub(crate) multiple: bool,
    pub(crate) custom_allowed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProviderQuestionOption {
    pub(crate) label: String,
    pub(crate) description: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProviderEvent {
    Ready {
        run_id: String,
        session_id: String,
    },
    Working {
        run_id: String,
        turn_id: String,
    },
    OutputDelta {
        run_id: String,
        turn_id: String,
        text: String,
    },
    AttentionRequested {
        run_id: String,
        attention: ProviderAttention,
    },
    ResponseResolved {
        run_id: String,
        request_id: String,
    },
    TurnCompleted {
        run_id: String,
        turn_id: String,
        outcome: TurnOutcome,
    },
    TransportFailed {
        run_id: String,
        reason: TransportFailure,
    },
    Stopped {
        run_id: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TurnOutcome {
    Completed,
    Interrupted,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransportFailure {
    Spawn,
    Protocol,
    Timeout,
    Disconnected,
    CommandRejected,
    DeliveryUnknown,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum RpcId {
    Number(u64),
    String(String),
}

impl RpcId {
    fn from_json(value: &serde_json::Value) -> Option<Self> {
        value
            .as_u64()
            .map(Self::Number)
            .or_else(|| value.as_str().map(|value| Self::String(value.to_owned())))
    }

    fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Number(value) => serde_json::Value::from(*value),
            Self::String(value) => serde_json::Value::from(value.clone()),
        }
    }

    fn audit_id(&self) -> String {
        match self {
            Self::Number(value) => format!("number:{value}"),
            Self::String(value) => format!("string:{value}"),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ManagedProviderHandle {
    commands: mpsc::Sender<ProviderCommand>,
}

impl ManagedProviderHandle {
    pub(crate) fn try_send(&self, command: ProviderCommand) -> Result<(), ManagedProviderError> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => ManagedProviderError::Busy,
                mpsc::error::TrySendError::Closed(_) => ManagedProviderError::Disconnected,
            })
    }

    #[cfg(test)]
    pub(crate) fn for_test(capacity: usize) -> (Self, mpsc::Receiver<ProviderCommand>) {
        let (commands, receiver) = mpsc::channel(capacity);
        (Self { commands }, receiver)
    }
}

pub(crate) struct ManagedProviderSupervisor;

impl ManagedProviderSupervisor {
    #[allow(
        dead_code,
        reason = "adapter descriptors are inspected by the external conformance harness"
    )]
    pub(crate) fn descriptor(
        provider: ProviderKind,
        contract_version: AdapterContractVersion,
    ) -> Result<ProviderAdapterDescriptor, ManagedProviderError> {
        registry::resolve(provider, contract_version).map(|adapter| adapter.descriptor())
    }

    pub(crate) fn spawn(
        provider: ProviderKind,
        executable: Option<PathBuf>,
        events: mpsc::Sender<ProviderEvent>,
    ) -> Result<ManagedProviderHandle, ManagedProviderError> {
        if provider == ProviderKind::Acp {
            return Err(ManagedProviderError::AcpEndpointUnavailable);
        }
        Self::spawn_with_contract(
            provider,
            AdapterContractVersion::CURRENT,
            executable,
            events,
        )
    }

    pub(crate) fn spawn_acp(
        endpoint: AcpEndpoint,
        events: mpsc::Sender<ProviderEvent>,
    ) -> Result<ManagedProviderHandle, ManagedProviderError> {
        let (commands, command_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        acp::spawn(endpoint, command_rx, events);
        Ok(ManagedProviderHandle { commands })
    }

    pub(crate) fn spawn_with_contract(
        provider: ProviderKind,
        contract_version: AdapterContractVersion,
        executable: Option<PathBuf>,
        events: mpsc::Sender<ProviderEvent>,
    ) -> Result<ManagedProviderHandle, ManagedProviderError> {
        let adapter = registry::resolve(provider, contract_version)?;
        let (commands, command_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        adapter.spawn(executable, command_rx, events);
        Ok(ManagedProviderHandle { commands })
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ManagedProviderError {
    #[error(
        "provider {provider:?} does not support adapter contract {requested}; supported contract is {supported}"
    )]
    UnsupportedAdapterContract {
        provider: ProviderKind,
        requested: AdapterContractVersion,
        supported: AdapterContractVersion,
    },
    #[error("ACP provider is not configured; set providers.acp.command in config.toml")]
    AcpEndpointUnavailable,
    #[error("managed provider command queue is full")]
    Busy,
    #[error("managed provider command channel is disconnected")]
    Disconnected,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_ids_preserve_their_wire_type_and_have_distinct_audit_ids() {
        let numeric = RpcId::from_json(&serde_json::json!(7)).unwrap();
        let textual = RpcId::from_json(&serde_json::json!("7")).unwrap();

        assert_eq!(numeric.to_json(), serde_json::json!(7));
        assert_eq!(textual.to_json(), serde_json::json!("7"));
        assert_ne!(numeric.audit_id(), textual.audit_id());
    }
}
