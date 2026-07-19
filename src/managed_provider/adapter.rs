use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::mission::model::ProviderKind;

use super::{ProviderCommand, ProviderEvent};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct AdapterContractVersion(u16);

impl AdapterContractVersion {
    pub(crate) const CURRENT: Self = Self(1);

    #[allow(
        dead_code,
        reason = "unknown versions are constructed by the external conformance harness"
    )]
    pub(crate) const fn new(version: u16) -> Self {
        Self(version)
    }
}

impl std::fmt::Display for AdapterContractVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProviderCapabilities {
    pub(crate) resume: bool,
    pub(crate) turns: bool,
    pub(crate) interrupt: bool,
    pub(crate) permission_attention: bool,
    pub(crate) question_attention: bool,
    pub(crate) streaming_output: bool,
    pub(crate) usage: bool,
    pub(crate) diffs: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderRuntimeVersion {
    NotPinned,
    Exact(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProviderAdapterDescriptor {
    pub(crate) provider: ProviderKind,
    pub(crate) contract_version: AdapterContractVersion,
    pub(crate) capabilities: ProviderCapabilities,
    pub(crate) runtime_version: ProviderRuntimeVersion,
}

pub(super) type SpawnAdapter =
    fn(Option<PathBuf>, mpsc::Receiver<ProviderCommand>, mpsc::Sender<ProviderEvent>);

pub(super) trait ManagedProviderAdapter: Sync {
    fn descriptor(&self) -> ProviderAdapterDescriptor;

    fn spawn(
        &self,
        executable: Option<PathBuf>,
        commands: mpsc::Receiver<ProviderCommand>,
        events: mpsc::Sender<ProviderEvent>,
    );
}

pub(super) struct FirstPartyAdapter {
    descriptor: ProviderAdapterDescriptor,
    spawn: SpawnAdapter,
}

impl FirstPartyAdapter {
    pub(super) const fn new(descriptor: ProviderAdapterDescriptor, spawn: SpawnAdapter) -> Self {
        Self { descriptor, spawn }
    }
}

impl ManagedProviderAdapter for FirstPartyAdapter {
    fn descriptor(&self) -> ProviderAdapterDescriptor {
        self.descriptor
    }

    fn spawn(
        &self,
        executable: Option<PathBuf>,
        commands: mpsc::Receiver<ProviderCommand>,
        events: mpsc::Sender<ProviderEvent>,
    ) {
        (self.spawn)(executable, commands, events);
    }
}
