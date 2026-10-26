//Copyright (c) 2019-2022 Alibaba Cloud
//Copyright (c) 2023 Nubificus Ltd
//
//SPDX-License-Identifier: Apache-2.0

use crate::firecracker::{
    diagnostics::Diagnostics, inner_hypervisor::FC_API_SOCKET_NAME, process, sl,
};
use crate::hypervisor_persist::FirecrackerState;
use crate::MemoryConfig;
use crate::HYPERVISOR_FIRECRACKER;
use crate::{device::DeviceType, VmmState};
use crate::{selinux, HypervisorState};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::Full;
use hyper_util::client::legacy::Client;
use hyperlocal::{UnixClientExt, UnixConnector};
use kata_types::{
    capabilities::{Capabilities, CapabilityBits},
    config::hypervisor::Hypervisor as HypervisorConfig,
};
use nix::sched::{setns, CloneFlags};
use persist::sandbox_persist::Persist;
use std::collections::{BTreeMap, BTreeSet};
use std::process::Stdio;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

unsafe impl Send for FcInner {}
unsafe impl Sync for FcInner {}

#[derive(Debug)]
pub struct FcInner {
    pub(crate) id: String,
    pub(crate) asock_path: String,
    pub(crate) state: VmmState,
    pub(crate) config: HypervisorConfig,
    pub(crate) pid: Option<u32>,
    pub(super) process_start_time: Option<u64>,
    pub(super) attached_drives: BTreeMap<u64, String>,
    pub(super) retired_mounts: BTreeSet<String>,
    pub(super) diagnostics: Diagnostics,
    pub(super) stderr_task: Option<JoinHandle<Result<()>>>,
    pub(super) recovery_task: Option<JoinHandle<()>>,
    pub(crate) vm_path: String,
    pub(crate) netns: Option<String>,
    pub(crate) client: Client<UnixConnector, Full<Bytes>>,
    pub(crate) jailer_root: String,
    pub(crate) jailed: bool,
    pub(crate) run_dir: String,
    pub(crate) pending_devices: Vec<DeviceType>,
    pub(crate) capabilities: Capabilities,
    pub(crate) fc_process: Mutex<Option<Child>>,
    pub(crate) exit_notify: Option<mpsc::Sender<()>>,
}

impl FcInner {
    pub fn new(exit_notify: mpsc::Sender<()>) -> FcInner {
        let mut capabilities = Capabilities::new();
        capabilities
            .set(CapabilityBits::BlockDeviceSupport | CapabilityBits::BlockDeviceHotplugSupport);

        FcInner {
            id: String::default(),
            asock_path: String::default(),
            state: VmmState::NotReady,
            config: Default::default(),
            pid: None,
            process_start_time: None,
            attached_drives: BTreeMap::new(),
            retired_mounts: BTreeSet::new(),
            diagnostics: Diagnostics::default(),
            stderr_task: None,
            recovery_task: None,
            netns: None,
            vm_path: String::default(),
            client: Client::unix(),
            jailer_root: String::default(),
            jailed: false,
            run_dir: String::default(),
            pending_devices: vec![],
            capabilities,
            fc_process: Mutex::new(None),
            exit_notify: Some(exit_notify),
        }
    }

    pub(crate) async fn prepare_vmm(&mut self, netns: Option<String>) -> Result<()> {
        self.validate_version().await?;
        let exit_notify = self.exit_notify.clone().context("no exit notify")?;
        let netns_file = netns
            .as_ref()
            .map(std::fs::File::open)
            .transpose()
            .context("open Firecracker network namespace")?;
        let mut cmd: Command;
        self.netns = netns.clone();
        match self.jailed {
            true => {
                debug!(sl(), "Running Jailed");
                cmd = Command::new(&self.config.jailer_path);
                let api_socket = ["/run/", FC_API_SOCKET_NAME].join("/");
                let args = [
                    "--id",
                    &self.id,
                    "--gid",
                    "0",
                    "--uid",
                    "0",
                    "--exec-file",
                    &self.config.path,
                    "--chroot-base-dir",
                    &self.jailer_root,
                    "--",
                    "--api-sock",
                    &api_socket,
                ];
                cmd.args(args);
            }
            false => {
                debug!(sl(), "Running non-Jailed");
                cmd = Command::new(&self.config.path);
                cmd.args(["--api-sock", &self.asock_path]);
            }
        }
        if self.config.security_info.disable_seccomp {
            cmd.arg("--no-seccomp");
        }
        debug!(sl(), "Exec: {:?}", cmd);

        selinux::apply_exec_label(
            &mut cmd,
            self.config.effective_selinux_label(),
            "Firecracker",
        )?;

        // Make sure we're in the correct Network Namespace
        unsafe {
            let _pre = cmd.pre_exec(move || {
                if let Some(netns_file) = &netns_file {
                    setns(netns_file, CloneFlags::CLONE_NEWNET).map_err(std::io::Error::from)?;
                }
                Ok(())
            });
        }

        let mut child = cmd
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn Firecracker")?;

        let stderr = child.stderr.take().context("missing Firecracker stderr")?;
        self.stderr_task = Some(tokio::spawn(log_fc_stderr(stderr)));

        match child.id() {
            Some(id) => {
                let cur_tid = nix::unistd::gettid().as_raw() as u32;
                info!(
                    sl(),
                    "VMM spawned successfully: PID: {:?}, current TID: {:?}", id, cur_tid
                );
                self.pid = Some(id);
                match process::process_start_time(id) {
                    Ok(start) => self.process_start_time = Some(start),
                    Err(error) => {
                        let _ = child.kill().await;
                        return Err(error);
                    }
                }
            }
            None => {
                let exit_status = child.wait().await?;
                error!(sl(), "Process exited, status: {:?}", exit_status);
                return Err(anyhow!("fc vmm start failed with: {:?}", exit_status));
            }
        };

        *self.fc_process.lock().await = Some(child);
        self.recovery_task = Some(process::monitor_process(
            self.pid.context("missing Firecracker PID")?,
            self.process_start_time
                .context("missing Firecracker start time")?,
            exit_notify,
        ));

        Ok(())
    }

    pub(crate) fn hypervisor_config(&self) -> HypervisorConfig {
        debug!(sl(), "[Firecracker]: Hypervisor config");
        self.config.clone()
    }

    pub(crate) fn set_hypervisor_config(&mut self, config: HypervisorConfig) {
        debug!(sl(), "[Firecracker]: Set Hypervisor config");
        self.config = config;
    }

    pub(crate) fn resize_memory(&mut self, new_mem_mb: u32) -> Result<(u32, MemoryConfig)> {
        warn!(
            sl(),
            "memory size unchanged, requested: {:?} Not implemented", new_mem_mb
        );
        Ok((
            0,
            MemoryConfig {
                ..Default::default()
            },
        ))
    }

    pub(crate) fn set_capabilities(&mut self, flag: CapabilityBits) {
        self.capabilities.add(flag);
    }

    pub(crate) fn set_guest_memory_block_size(&mut self, size: u32) {
        warn!(
            sl(),
            "guest memory block size unchanged, requested: {:?}, Not implemented", size
        );
    }

    pub(crate) fn guest_memory_block_size_mb(&self) -> u32 {
        warn!(sl(), "guest memory block size Not implemented");
        0
    }
}

async fn log_fc_stderr(stderr: ChildStderr) -> Result<()> {
    info!(sl!(), "starting reading fc stderr");

    let stderr_reader = BufReader::new(stderr);
    let mut stderr_lines = stderr_reader.lines();

    while let Some(buffer) = stderr_lines
        .next_line()
        .await
        .context("next_line() failed on fc stderr")?
    {
        info!(sl!(), "fc stderr: {:?}", buffer);
    }

    info!(sl!(), "finished reading fc stderr");
    Ok(())
}

#[async_trait]
impl Persist for FcInner {
    type State = HypervisorState;
    type ConstructorArgs = mpsc::Sender<()>;

    async fn save(&self) -> Result<Self::State> {
        self.check_process()?;
        Ok(HypervisorState {
            hypervisor_type: HYPERVISOR_FIRECRACKER.to_string(),
            pid: self.pid.map(|pid| pid as i32),
            api_socket: self.asock_path.clone(),
            firecracker: Some(FirecrackerState {
                process_start_time: self
                    .process_start_time
                    .context("cannot save Firecracker without process identity")?,
                boot_id: process::boot_id()?,
                running: self.state == VmmState::VmRunning,
                attached_drives: self.attached_drives.clone(),
                retired_mounts: self.retired_mounts.clone(),
            }),
            id: self.id.clone(),
            vm_path: self.vm_path.clone(),
            config: self.hypervisor_config(),
            jailed: self.jailed,
            jailer_root: self.jailer_root.clone(),
            run_dir: self.run_dir.clone(),
            netns: self.netns.clone(),
            ..Default::default()
        })
    }
    async fn restore(exit_notify: mpsc::Sender<()>, hypervisor_state: Self::State) -> Result<Self> {
        Self::restore_state(exit_notify, hypervisor_state, false).await
    }
}

impl FcInner {
    pub(super) async fn restore_for_cleanup(
        exit_notify: mpsc::Sender<()>,
        state: HypervisorState,
    ) -> Result<Self> {
        Self::restore_state(exit_notify, state, true).await
    }

    async fn restore_state(
        exit_notify: mpsc::Sender<()>,
        hypervisor_state: HypervisorState,
        cleanup_only: bool,
    ) -> Result<Self> {
        let saved = hypervisor_state
            .firecracker
            .context("saved Firecracker state lacks process recovery metadata")?;
        let pid = hypervisor_state
            .pid
            .filter(|pid| *pid > 0)
            .context("saved Firecracker state has no valid PID")? as u32;
        let same_boot = saved.boot_id == process::boot_id()?;
        let identity = if same_boot {
            process::process_identity(pid)?
        } else {
            None
        };
        let live = match identity {
            Some(start) if start == saved.process_start_time => true,
            Some(_) => {
                return Err(anyhow!(
                    "saved Firecracker process identity no longer matches PID {pid}"
                ))
            }
            None if cleanup_only => false,
            None => {
                return Err(anyhow!(
                    "saved Firecracker process {pid} is missing or exited"
                ))
            }
        };
        let mut inner = Self::new(exit_notify.clone());
        inner.id = hypervisor_state.id;
        inner.asock_path = hypervisor_state.api_socket;
        if live && !cleanup_only && inner.asock_path.is_empty() {
            return Err(anyhow!("saved Firecracker state has no API socket"));
        }
        inner.state = if !live {
            VmmState::NotReady
        } else if saved.running {
            VmmState::VmRunning
        } else {
            VmmState::VmmServerReady
        };
        inner.vm_path = hypervisor_state.vm_path;
        inner.config = hypervisor_state.config;
        inner.netns = hypervisor_state.netns;
        inner.pid = live.then_some(pid);
        inner.process_start_time = live.then_some(saved.process_start_time);
        inner.attached_drives = saved.attached_drives;
        inner.retired_mounts = saved.retired_mounts;
        inner.jailed = hypervisor_state.jailed;
        inner.jailer_root = hypervisor_state.jailer_root;
        inner.run_dir = hypervisor_state.run_dir;
        if live {
            if !cleanup_only {
                inner
                    .prepare_diagnostics(true)
                    .await
                    .context("recover Firecracker FIFO readers")?;
            }
            inner.recovery_task = Some(process::monitor_process(
                pid,
                saved.process_start_time,
                exit_notify,
            ));
        } else {
            // Cleanup restores metadata, not an unrelated/recycled process.
            // Do not open FIFOs or retain a PID that could later be reused.
            let _ = exit_notify.try_send(());
        }
        Ok(inner)
    }
}

impl Drop for FcInner {
    fn drop(&mut self) {
        if let Some(task) = &self.stderr_task {
            task.abort();
        }
        if let Some(task) = &self.recovery_task {
            task.abort();
        }
    }
}
