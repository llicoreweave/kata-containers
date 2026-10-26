// Copyright (c) 2026 Kata Contributors
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use semver::Version;
use tokio::process::Command;

use super::FcInner;
use crate::VcpuThreadIds;

pub(super) const MIN_VERSION: &str = "0.21.1";

pub(super) fn parse_version(output: &str) -> Result<Version> {
    let mut fields = output.lines().next().unwrap_or_default().split_whitespace();
    if fields.next() != Some("Firecracker") {
        bail!("malformed Firecracker version: {output:?}");
    }
    let version = fields
        .next()
        .ok_or_else(|| anyhow!("missing Firecracker version"))?;
    let version = Version::parse(version.strip_prefix('v').unwrap_or(version))
        .context("malformed Firecracker version")?;
    if version < Version::parse(MIN_VERSION)? {
        bail!("Firecracker {version} is unsupported; minimum supported version is {MIN_VERSION}");
    }
    Ok(version)
}

/// Linux stat's comm field may contain spaces and parentheses.
pub(super) fn process_start_time(pid: u32) -> Result<u64> {
    process_identity(pid)?
        .with_context(|| format!("Firecracker process {pid} is missing or exited"))
}

/// Absence and zombie state confirm exit. Permission or parse errors do not.
pub(super) fn process_identity(pid: u32) -> Result<Option<u64>> {
    if pid == 0 || pid > i32::MAX as u32 {
        bail!("invalid Firecracker PID {pid}");
    }
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read Firecracker process identity"),
    };
    let (_, fields) = stat.rsplit_once(") ").context("malformed process stat")?;
    let fields: Vec<_> = fields.split_whitespace().collect();
    if matches!(fields.first(), Some(&"Z") | Some(&"X") | Some(&"x")) {
        return Ok(None);
    }
    fields
        .get(19)
        .context("missing process start time")?
        .parse()
        .map(Some)
        .context("invalid process start time")
}

impl FcInner {
    pub(super) fn vmm_exited(&self) -> Result<bool> {
        let Some(pid) = self.pid else { return Ok(true) };
        match process_identity(pid)? {
            None => Ok(true),
            Some(start) => Ok(start
                != self
                    .process_start_time
                    .context("missing VMM process identity")?),
        }
    }

    pub(super) async fn validate_version(&self) -> Result<()> {
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            Command::new(&self.config.path)
                .arg("--version")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("Firecracker --version timed out")?
        .with_context(|| format!("read Firecracker version from {}", self.config.path))?;
        if !output.status.success() {
            bail!(
                "Firecracker --version failed: {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        parse_version(
            std::str::from_utf8(&output.stdout).context("non-UTF-8 Firecracker version")?,
        )?;
        Ok(())
    }

    pub(super) fn check_process(&self) -> Result<u32> {
        let pid = self.pid.context("Firecracker has no process PID")?;
        let start = process_start_time(pid)?;
        if self.process_start_time != Some(start) {
            bail!("Firecracker PID {pid} was reused by another process");
        }
        Ok(pid)
    }

    pub(super) fn signal_process(&self, pid: u32) -> Result<()> {
        // Pin the process before checking its identity again to avoid sending a
        // signal to a recycled PID between /proc validation and kill(2).
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error())
                .context("open recovered Firecracker pidfd");
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        self.check_process()?;
        let status = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if status < 0 {
            return Err(std::io::Error::last_os_error()).context("signal recovered Firecracker");
        }
        Ok(())
    }

    pub(super) fn vcpu_threads(&self) -> Result<VcpuThreadIds> {
        let pid = self.check_process()?;
        let mut vcpus = HashMap::new();
        for entry in std::fs::read_dir(format!("/proc/{pid}/task"))? {
            let entry = entry?;
            let comm = match std::fs::read_to_string(entry.path().join("comm")) {
                Ok(comm) => comm,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e).context("read Firecracker thread name"),
            };
            if let Some(index) = comm.trim().strip_prefix("fc_vcpu ") {
                let index: u32 = index.parse().context("invalid Firecracker vCPU index")?;
                let tid: u32 = entry.file_name().to_string_lossy().parse()?;
                if vcpus.insert(index, tid).is_some() {
                    bail!("duplicate Firecracker vCPU index {index}");
                }
            }
        }
        self.check_process()?;
        Ok(VcpuThreadIds { vcpus })
    }
}

pub(super) fn monitor_process(
    pid: u32,
    start_time: u64,
    notify: tokio::sync::mpsc::Sender<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if process_start_time(pid).ok() != Some(start_time) {
                let _ = notify.send(()).await;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
}
