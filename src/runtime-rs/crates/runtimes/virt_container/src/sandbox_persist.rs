// Copyright (c) 2019-2022 Alibaba Cloud
// Copyright (c) 2019-2022 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//

use hypervisor::hypervisor_persist::HypervisorState;
use kata_types::config::{Runtime, TomlConfig};
use resource::resource_persist::ResourceState;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct SandboxState {
    pub sandbox_type: String,
    pub resource: Option<ResourceState>,
    pub hypervisor: Option<HypervisorState>,
    #[serde(default)]
    pub vmm_exit_confirmed: bool,
    /// Cleanup runs without the original containerd runtime options. Persist
    /// the runtime settings it needs, rather than depending on a default TOML
    /// file (which may describe a different handler or no longer exist).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_config: Option<Runtime>,
}

impl SandboxState {
    pub fn cleanup_config(&self) -> Option<TomlConfig> {
        let mut config = TomlConfig {
            runtime: self.runtime_config.clone()?,
            ..Default::default()
        };
        if let Some(hypervisor) = &self.hypervisor {
            config.hypervisor.insert(
                config.runtime.hypervisor_name.clone(),
                hypervisor.config.clone(),
            );
        }
        Some(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_config_round_trips_without_handler_options_or_agent_secrets() {
        let runtime = Runtime {
            hypervisor_name: "private-firecracker".into(),
            keep_abnormal: true,
            ..Default::default()
        };
        let mut hypervisor = HypervisorState::default();
        hypervisor.config.memory_info.default_memory = 128;
        let state = SandboxState {
            sandbox_type: "virt_container".into(),
            resource: None,
            hypervisor: Some(hypervisor),
            vmm_exit_confirmed: false,
            runtime_config: Some(runtime),
        };
        let encoded = serde_json::to_value(state).unwrap();
        let restored: SandboxState = serde_json::from_value(encoded.clone()).unwrap();
        let config = restored.cleanup_config().unwrap();
        assert!(config.runtime.keep_abnormal);
        assert_eq!(
            config.hypervisor["private-firecracker"]
                .memory_info
                .default_memory,
            128
        );
        assert!(config.agent.is_empty());
        let mut legacy = encoded;
        legacy.as_object_mut().unwrap().remove("runtime_config");
        assert!(serde_json::from_value::<SandboxState>(legacy)
            .unwrap()
            .cleanup_config()
            .is_none());
    }
}
