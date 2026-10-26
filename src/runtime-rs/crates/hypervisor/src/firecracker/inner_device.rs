//Copyright (c) 2019-2022 Alibaba Cloud
//Copyright (c) 2019-2022 Ant Group
//Copyright (c) 2023 Nubificus Ltd
//
//SPDX-License-Identifier: Apache-2.0

use super::FcInner;
use crate::firecracker::{
    inner_hypervisor::{FC_AGENT_SOCKET_NAME, ROOT},
    sl,
};
use crate::VmmState;
use crate::{device::DeviceType, HybridVsockConfig, VsockConfig};
use anyhow::{anyhow, Context, Result};
use serde_json::json;

impl FcInner {
    pub(crate) async fn add_device(&mut self, device: DeviceType) -> Result<()> {
        if self.state == VmmState::NotReady {
            info!(sl(), "VMM not ready, queueing device {}", device);

            self.pending_devices.push(device);

            return Ok(());
        }

        debug!(sl(), "Add Device {} ", &device);

        match device {
            DeviceType::BlockModern(block_mod) => {
                let block = block_mod.lock().await.clone();
                if block.config.is_readonly {
                    return Err(anyhow!(
                        "Firecracker writable placeholder cannot accept a read-only block device"
                    ));
                }
                if self.attached_drives.contains_key(&block.config.index) {
                    return Err(anyhow!(
                        "Firecracker drive slot {} is already attached",
                        block.config.index
                    ));
                }
                self.hotplug_block_device(block.config.path_on_host.as_str(), block.config.index)
                    .await
                    .context("add block device")
            }
            DeviceType::Network(network) => self
                .add_net_device(&network.config, network.device_id)
                .await
                .context("add net device"),
            DeviceType::HybridVsock(hvsock) => {
                self.add_hvsock(&hvsock.config).await.context("add vsock")
            }
            DeviceType::Vsock(vsock) => self.add_vsock(&vsock.config).await.context("add vsock"),
            _ => Err(anyhow!("unhandled device: {:?}", device)),
        }
    }

    // Since Firecracker doesn't support sharefs, we patch block devices on pre-start inserted
    // dummy drives
    pub(crate) async fn hotplug_block_device(&mut self, path: &str, id: u64) -> Result<()> {
        match self.patch_container_rootfs(&id.to_string(), path).await {
            Err(error) if self.attached_drives.contains_key(&id) => {
                Err(error.context(crate::BlockAttachmentUncertain { index: id }))
            }
            result => result,
        }
    }

    pub(crate) async fn remove_device(&mut self, device: DeviceType) -> Result<()> {
        if let DeviceType::Network(network) = &device {
            // Firecracker cannot unplug a configured NIC. Once the VMM has
            // stopped, only host TAP teardown (or unqueueing) remains.
            if self.state == VmmState::NotReady {
                self.pending_devices.retain(|pending| {
                    !matches!(pending, DeviceType::Network(pending)
                        if pending.config.host_dev_name == network.config.host_dev_name
                            && pending.config.index == network.config.index)
                });
                return Ok(());
            }
        }
        if let DeviceType::BlockModern(block) = device {
            let block = block.lock().await.clone();
            if self.attached_drives.contains_key(&block.config.index) {
                return self.detach_block_device(block.config.index).await;
            }
            if self.state == VmmState::NotReady {
                let id = block.device_id;
                for index in 0..self.pending_devices.len() {
                    if let DeviceType::BlockModern(pending) = &self.pending_devices[index] {
                        if pending.lock().await.device_id == id {
                            self.pending_devices.remove(index);
                            return Ok(());
                        }
                    }
                }
                // A cancelled caller may retry after the backend already
                // completed removal but before its reference count committed.
                return Ok(());
            }
            Ok(())
        } else {
            Err(anyhow!("Firecracker cannot remove device {device:?}"))
        }
    }

    pub(crate) async fn update_device(&mut self, device: DeviceType) -> Result<()> {
        if self.state == VmmState::NotReady {
            return Err(anyhow!("Firecracker VMM is not ready"));
        }
        if let DeviceType::BlockModern(block) = device {
            let block = block.lock().await.clone();
            if block.config.is_readonly {
                return Err(anyhow!("Firecracker cannot change a drive to read-only"));
            }
            if !self.attached_drives.contains_key(&block.config.index) {
                return Err(anyhow!("Firecracker drive is not attached"));
            }
            self.patch_container_rootfs(&block.config.index.to_string(), &block.config.path_on_host)
                .await
        } else {
            Err(anyhow!("Firecracker cannot update device {device:?}"))
        }
    }

    pub(crate) async fn add_hvsock(&mut self, config: &HybridVsockConfig) -> Result<()> {
        let rel_uds_path = match self.jailed {
            false => [self.vm_path.as_str(), FC_AGENT_SOCKET_NAME].join("/"),
            true => FC_AGENT_SOCKET_NAME.to_string(),
        };
        let body_vsock: String = json!({
            "vsock_id": String::from(ROOT),
            "guest_cid": config.guest_cid,
            "uds_path": rel_uds_path,
        })
        .to_string();

        info!(sl(), "HybridVsock configure: {:?}", &body_vsock);

        self.request_with_retry(hyper::Method::PUT, "/vsock", body_vsock)
            .await?;
        Ok(())
    }

    pub(crate) async fn add_vsock(&mut self, config: &VsockConfig) -> Result<()> {
        let rel_uds_path = match self.jailed {
            false => [self.vm_path.as_str(), FC_AGENT_SOCKET_NAME].join("/"),
            true => FC_AGENT_SOCKET_NAME.to_string(),
        };
        let body_vsock: String = json!({
            "vsock_id": String::from(ROOT),
            "guest_cid": config.guest_cid,
            "uds_path": rel_uds_path,
        })
        .to_string();

        info!(sl(), "HybridVsock configure: {:?}", &body_vsock);

        self.request_with_retry(hyper::Method::PUT, "/vsock", body_vsock)
            .await?;
        Ok(())
    }
}
