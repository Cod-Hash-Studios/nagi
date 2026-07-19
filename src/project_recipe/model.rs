use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub(crate) const PROJECT_SCHEMA_V1: u16 = 1;
pub(crate) const DEFAULT_COMMAND_TIMEOUT_SECONDS: u64 = 300;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectContract {
    pub(crate) schema: u16,
    #[serde(default)]
    pub(crate) worktree: WorktreeContract,
    #[serde(default)]
    pub(crate) setup: Option<CommandContract>,
    #[serde(default)]
    pub(crate) services: BTreeMap<String, ServiceContract>,
    #[serde(default)]
    pub(crate) checks: Vec<CheckContract>,
    #[serde(default)]
    pub(crate) cleanup: Vec<CommandContract>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorktreeContract {
    #[serde(default = "default_worktree_location")]
    pub(crate) location: String,
    #[serde(default = "default_worktree_base")]
    pub(crate) base: String,
    #[serde(default)]
    pub(crate) copy_ignored: Vec<String>,
}

impl Default for WorktreeContract {
    fn default() -> Self {
        Self {
            location: default_worktree_location(),
            base: default_worktree_base(),
            copy_ignored: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommandContract {
    pub(crate) command: Vec<String>,
    #[serde(default = "default_command_timeout_seconds")]
    pub(crate) timeout_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServiceContract {
    pub(crate) command: Vec<String>,
    pub(crate) port_env: String,
    pub(crate) health: String,
    #[serde(default = "default_command_timeout_seconds")]
    pub(crate) timeout_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckContract {
    pub(crate) id: String,
    pub(crate) command: Vec<String>,
    #[serde(default = "default_command_timeout_seconds")]
    pub(crate) timeout_seconds: u64,
    #[serde(default)]
    pub(crate) covers: Vec<String>,
}

fn default_worktree_location() -> String {
    ".worktrees".to_owned()
}

fn default_worktree_base() -> String {
    "HEAD".to_owned()
}

const fn default_command_timeout_seconds() -> u64 {
    DEFAULT_COMMAND_TIMEOUT_SECONDS
}
