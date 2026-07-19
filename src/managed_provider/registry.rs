use crate::mission::model::ProviderKind;

use super::{
    adapter::{FirstPartyAdapter, ManagedProviderAdapter, ProviderAdapterDescriptor},
    AdapterContractVersion, ManagedProviderError, ProviderCapabilities, ProviderRuntimeVersion,
};

const FULL_MANAGED_CAPABILITIES: ProviderCapabilities = ProviderCapabilities {
    resume: true,
    turns: true,
    interrupt: true,
    permission_attention: true,
    question_attention: true,
    streaming_output: true,
    usage: false,
    diffs: false,
};

static CODEX: FirstPartyAdapter = FirstPartyAdapter::new(
    ProviderAdapterDescriptor {
        provider: ProviderKind::Codex,
        contract_version: AdapterContractVersion::CURRENT,
        capabilities: FULL_MANAGED_CAPABILITIES,
        runtime_version: ProviderRuntimeVersion::NotPinned,
    },
    super::codex::spawn,
);

static CLAUDE_CODE: FirstPartyAdapter = FirstPartyAdapter::new(
    ProviderAdapterDescriptor {
        provider: ProviderKind::ClaudeCode,
        contract_version: AdapterContractVersion::CURRENT,
        capabilities: FULL_MANAGED_CAPABILITIES,
        runtime_version: ProviderRuntimeVersion::NotPinned,
    },
    super::claude::spawn,
);

static OPEN_CODE: FirstPartyAdapter = FirstPartyAdapter::new(
    ProviderAdapterDescriptor {
        provider: ProviderKind::OpenCode,
        contract_version: AdapterContractVersion::CURRENT,
        capabilities: ProviderCapabilities {
            question_attention: false,
            ..FULL_MANAGED_CAPABILITIES
        },
        runtime_version: ProviderRuntimeVersion::Exact(super::opencode::TESTED_VERSION),
    },
    super::opencode::spawn,
);

static ACP: FirstPartyAdapter = FirstPartyAdapter::new(
    ProviderAdapterDescriptor {
        provider: ProviderKind::Acp,
        contract_version: AdapterContractVersion::CURRENT,
        capabilities: ProviderCapabilities {
            resume: true,
            turns: true,
            interrupt: true,
            permission_attention: true,
            question_attention: false,
            streaming_output: true,
            usage: false,
            diffs: false,
        },
        runtime_version: ProviderRuntimeVersion::NotPinned,
    },
    super::acp::spawn_from_executable,
);

static FIRST_PARTY: [&dyn ManagedProviderAdapter; 4] = [&CODEX, &CLAUDE_CODE, &OPEN_CODE, &ACP];

pub(super) fn resolve(
    provider: ProviderKind,
    version: AdapterContractVersion,
) -> Result<&'static dyn ManagedProviderAdapter, ManagedProviderError> {
    FIRST_PARTY
        .iter()
        .copied()
        .find(|adapter| {
            let descriptor = adapter.descriptor();
            descriptor.provider == provider && descriptor.contract_version == version
        })
        .ok_or(ManagedProviderError::UnsupportedAdapterContract {
            provider,
            requested: version,
            supported: AdapterContractVersion::CURRENT,
        })
}
