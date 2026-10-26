//Copyright (c) 2019-2022 Alibaba Cloud
//Copyright (c) 2023 Nubificus Ltd
//
//SPDX-License-Identifier: Apache-2.0

use crate::firecracker::{sl, FcInner};
use crate::{VcpuThreadIds, VmmState, HYPERVISOR_FIRECRACKER};
use anyhow::{anyhow, Context, Result};
use kata_types::capabilities::Capabilities;
use kata_types::config::KATA_PATH;
use std::time::Duration;
use tokio::fs;

pub const FC_API_SOCKET_NAME: &str = "fc.sock";
pub const FC_AGENT_SOCKET_NAME: &str = "kata.hvsock";
pub const ROOT: &str = "root";

const HYBRID_VSOCK_SCHEME: &str = "hvsock";

impl FcInner {
    pub(crate) async fn prepare_vm(
        &mut self,
        id: &str,
        _netns: Option<String>,
        selinux_label: Option<String>,
    ) -> Result<()> {
        debug!(sl(), "Preparing Firecracker");

        if self.check_process().is_ok() {
            return Err(anyhow!("Firecracker is already running"));
        }
        if id.is_empty() || id.contains('/') || id == "." || id == ".." {
            return Err(anyhow!("invalid Firecracker sandbox ID"));
        }
        self.id = id.to_string();

        if !self.config.jailer_path.is_empty() {
            debug!(sl(), "Running jailed");
            self.jailed = true;
            self.jailer_root = KATA_PATH.to_string();
            debug!(sl(), "jailer_root: {:?}", self.jailer_root);
            self.vm_path = [
                self.jailer_root.clone(),
                HYPERVISOR_FIRECRACKER.to_string(),
                id.to_string(),
            ]
            .join("/");
            debug!(sl(), "VM Path: {:?}", self.vm_path);
            self.run_dir = [self.vm_path.clone(), "root".to_string(), "run".to_string()].join("/");
            debug!(sl(), "Rundir: {:?}", self.run_dir);
        } else {
            self.vm_path = [KATA_PATH.to_string(), id.to_string()].join("/");
            debug!(sl(), "VM Path: {:?}", self.vm_path);
            self.run_dir = [self.vm_path.clone(), "run".to_string()].join("/");
            debug!(sl(), "Rundir: {:?}", self.run_dir);
        }
        // We construct the FC API socket path based on the run_dir variable (jailed or
        // non-jailed).
        self.asock_path = [self.run_dir.as_str(), "fc.sock"].join("/");
        debug!(sl(), "Socket Path: {:?}", self.asock_path);

        self.netns = _netns.clone();

        self.config.set_selinux_label(selinux_label);

        let result = async {
            if self.jailed {
                self.remount_jailer_with_exec().await?;
            }
            fs::create_dir_all(self.run_dir.as_str())
                .await
                .context("create Firecracker run directory")?;
            self.prepare_vmm(self.netns.clone()).await?;
            self.prepare_diagnostics(false).await?;
            self.state = VmmState::VmmServerReady;
            self.prepare_vmm_resources().await?;
            self.prepare_hvsock().await?;

            anyhow::Ok(())
        }
        .await;
        if let Err(error) = result {
            let stop = self.stop_vm().await;
            if let Err(stop_error) = stop {
                return Err(error).context(format!(
                    "Firecracker preparation failed; process cleanup also failed: {stop_error:#}"
                ));
            }
            self.cleanup().await.with_context(|| {
                format!("Firecracker preparation failed: {error:#}; cleanup also failed")
            })?;
            self.state = VmmState::NotReady;
            return Err(error).context("prepare Firecracker VM");
        }
        Ok(())
    }

    pub(crate) async fn start_vm(&mut self, _timeout: i32) -> Result<()> {
        debug!(sl(), "Starting sandbox");
        if self.state != VmmState::VmmServerReady {
            return Err(anyhow!("Firecracker VMM is not prepared for VM start"));
        }
        self.check_process()?;
        let body: String = serde_json::json!({
            "action_type": "InstanceStart"
        })
        .to_string();
        if let Err(error) = self
            .request_with_retry(hyper::Method::PUT, "/actions", body)
            .await
        {
            self.stop_vm().await.with_context(|| {
                format!("Firecracker VM start failed: {error:#}; process cleanup also failed")
            })?;
            self.cleanup().await.with_context(|| {
                format!("Firecracker VM start failed: {error:#}; resource cleanup also failed")
            })?;
            return Err(error).context("start Firecracker VM");
        }
        self.state = VmmState::VmRunning;
        Ok(())
    }

    pub(crate) async fn stop_vm(&mut self) -> Result<()> {
        let result = async {
            let mut child = self.fc_process.lock().await;
            if let Some(child) = child.as_mut() {
                child.kill().await.context("kill Firecracker")
            } else if !self.vmm_exited()? {
                self.signal_process(self.check_process()?)?;
                tokio::time::timeout(Duration::from_secs(5), async {
                    while !self.vmm_exited()? {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    anyhow::Ok(())
                })
                .await
                .context("wait for recovered Firecracker to exit")?
            } else {
                Ok(())
            }
        }
        .await;
        result?;
        self.diagnostics.stop().await;
        if let Some(task) = self.stderr_task.take() {
            task.abort();
            let _ = task.await;
        }
        if let Some(task) = self.recovery_task.take() {
            task.abort();
            let _ = task.await;
        }
        if let Some(sender) = &self.exit_notify {
            let _ = sender.try_send(());
        }
        self.state = VmmState::NotReady;
        Ok(())
    }

    pub(crate) async fn wait_vm(&self) -> Result<i32> {
        let mut child = self.fc_process.lock().await;
        if let Some(mut child) = child.take() {
            return Ok(child.wait().await?.code().unwrap_or(0));
        }
        // A restored VMM is not this shim's child and cannot be waitpid'd.
        while self.check_process().is_ok() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(0)
    }

    pub(crate) fn pause_vm(&self) -> Result<()> {
        warn!(sl(), "Pause VM: Not implemented");
        Ok(())
    }

    pub(crate) async fn save_vm(&self) -> Result<()> {
        warn!(sl(), "Save VM: Not implemented");
        Ok(())
    }
    pub(crate) fn resume_vm(&self) -> Result<()> {
        warn!(sl(), "Resume VM: Not implemented");
        Ok(())
    }

    pub(crate) async fn get_agent_socket(&self) -> Result<String> {
        debug!(sl(), "Get kata-agent socket");
        let vsock_path = match self.jailed {
            false => [self.vm_path.as_str(), FC_AGENT_SOCKET_NAME].join("/"),
            true => [self.vm_path.as_str(), ROOT, FC_AGENT_SOCKET_NAME].join("/"),
        };
        Ok(format!("{HYBRID_VSOCK_SCHEME}://{vsock_path}"))
    }

    pub(crate) async fn disconnect(&mut self) {
        self.diagnostics.stop().await;
        if let Some(task) = self.stderr_task.take() {
            task.abort();
            let _ = task.await;
        }
        if let Some(task) = self.recovery_task.take() {
            task.abort();
            let _ = task.await;
        }
    }
    pub(crate) async fn get_thread_ids(&self) -> Result<VcpuThreadIds> {
        self.vcpu_threads()
    }

    pub(crate) async fn get_pids(&self) -> Result<Vec<u32>> {
        Ok(vec![self.check_process()?])
    }

    pub(crate) async fn get_vmm_master_tid(&self) -> Result<u32> {
        self.check_process()
    }
    pub(crate) async fn get_ns_path(&self) -> Result<String> {
        let pid = self.check_process()?;
        Ok(format!("/proc/{pid}/ns"))
    }

    pub(crate) async fn prepare_for_cleanup(&mut self) -> Result<()> {
        // Generic sandbox setup can fail after prepare_vm spawned the API
        // server but before InstanceStart was attempted. Reap that server too.
        if self.state == VmmState::VmmServerReady {
            self.stop_vm().await?;
        }
        if !self.vmm_exited()? {
            return Err(anyhow!("cannot clean up a running Firecracker process"));
        }
        self.state = VmmState::NotReady;
        Ok(())
    }

    pub(crate) async fn cleanup(&mut self) -> Result<()> {
        self.prepare_for_cleanup().await?;
        self.diagnostics.stop().await;
        self.cleanup_resource()
    }

    pub(crate) async fn resize_vcpu(&self, old_vcpu: u32, new_vcpu: u32) -> Result<(u32, u32)> {
        warn!(sl(), "Resize vCPU: Not implemented");
        Ok((old_vcpu, new_vcpu))
    }

    pub(crate) async fn check(&self) -> Result<()> {
        self.check_process().map(|_| ())
    }

    pub(crate) async fn get_jailer_root(&self) -> Result<String> {
        debug!(sl(), "Get Jailer Root");
        Ok(self.jailer_root.clone())
    }

    pub(crate) async fn capabilities(&self) -> Result<Capabilities> {
        debug!(sl(), "Capabilities");
        Ok(self.capabilities.clone())
    }

    pub(crate) async fn get_hypervisor_metrics(&self) -> Result<String> {
        warn!(sl(), "Get Hypervisor Metrics: Not implemented");
        Ok(String::new())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_unsupported_hypervisor_metrics_are_empty() {
        let (exit_notify, _exit_waiter) = mpsc::channel(1);
        let firecracker = FcInner::new(exit_notify);

        for _ in 0..3 {
            assert_eq!(firecracker.get_hypervisor_metrics().await.unwrap(), "");
        }
    }
}
