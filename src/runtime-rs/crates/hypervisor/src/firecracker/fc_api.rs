//Copyright (c) 2019-2022 Alibaba Cloud
//Copyright (c) 2023 Nubificus Ltd
//
//SPDX-License-Identifier: Apache-2.0

use crate::{
    firecracker::{
        inner_hypervisor::{FC_AGENT_SOCKET_NAME, ROOT},
        sl, FcInner,
    },
    kernel_param::KernelParams,
    NetworkConfig, Param,
};
use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use dbs_utils::net::MacAddr;
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, Method, Request, Response};
use hyperlocal::Uri;
use kata_sys_util::mount;
use kata_types::config::hypervisor::RateLimiterConfig;
use nix::mount::MsFlags;
use serde_json::json;
use tokio::{fs, fs::File};

const REQUEST_RETRY: u32 = 500;
const FC_KERNEL: &str = "vmlinux";
const FC_ROOT_FS: &str = "rootfs";
pub(super) const DISK_POOL_SIZE: u32 = 6;

#[derive(Debug, thiserror::Error)]
#[error("Firecracker API returned {status}: {body}")]
struct ApiError {
    status: hyper::StatusCode,
    body: String,
}

impl FcInner {
    pub(crate) fn get_resource(&self, src: &str, dst: &str) -> Result<String> {
        if self.jailed {
            self.jail_resource(src, dst)
        } else {
            Ok(src.to_string())
        }
    }

    fn jail_resource(&self, src: &str, dst: &str) -> Result<String> {
        if src.is_empty() || dst.is_empty() {
            return Err(anyhow!("invalid param src {} dst {}", src, dst));
        }

        let jailed_location = [self.vm_path.as_str(), ROOT, dst].join("/");
        mount::bind_mount_unchecked(src, jailed_location.as_str(), false, MsFlags::MS_SLAVE)
            .context("bind_mount ERROR")?;

        let mut abs_path = String::from("/");
        abs_path.push_str(dst);
        Ok(abs_path)
    }

    // Remounting jailer root to ensure it has exec permissions, since firecracker binary will
    // execute from there
    pub(crate) async fn remount_jailer_with_exec(&self) -> Result<()> {
        let localpath = [self.vm_path.clone(), ROOT.to_string()].join("/");
        fs::create_dir_all(&localpath)
            .await
            .context(format!("failed to create directory {:?}", localpath))?;
        mount::bind_mount_unchecked(&localpath, &localpath, false, MsFlags::MS_SHARED)
            .context("bind mount jailer root")?;

        mount::bind_remount(&localpath, false).context("rebind mount jailer root")?;
        Ok(())
    }

    pub(crate) async fn prepare_hvsock(&mut self) -> Result<()> {
        let rel_uds_path = match self.jailed {
            false => [self.vm_path.as_str(), FC_AGENT_SOCKET_NAME].join("/"),
            true => FC_AGENT_SOCKET_NAME.to_string(),
        };

        let body_vsock: String = json!({
            "guest_cid": 3,
            "uds_path": rel_uds_path,
            "vsock_id": ROOT,
        })
        .to_string();

        self.request_with_retry(Method::PUT, "/vsock", body_vsock)
            .await?;
        Ok(())
    }

    pub(crate) async fn prepare_vmm_resources(&mut self) -> Result<()> {
        let mut kernel_params = KernelParams::new(self.config.debug_info.enable_debug);
        kernel_params.push(Param::new("pci", "off"));
        kernel_params.push(Param::new("iommu", "off"));
        let mut rootfs_params = KernelParams::new_rootfs_kernel_params(
            &self.config.boot_info.kernel_verity_params,
            &self.config.blockdev_info.block_device_driver,
            &self.config.boot_info.rootfs_type,
            true,
        )?;
        kernel_params.append(&mut rootfs_params);
        kernel_params.append(&mut KernelParams::from_string(
            &self.config.boot_info.kernel_params,
        ));
        let mut parameters = String::new().to_owned();

        if let Ok(param) = &kernel_params.to_string() {
            parameters.push_str(&param.to_string());
        }

        let kernel = self
            .get_resource(&self.config.boot_info.kernel, FC_KERNEL)
            .context("get resource KERNEL")?;
        let rootfs = self
            .get_resource(&self.config.boot_info.image, FC_ROOT_FS)
            .context("get resource ROOTFS")?;

        let body_config: String = json!({
            "mem_size_mib": self.config.memory_info.default_memory,
            "vcpu_count": self.config.cpu_info.default_vcpus.ceil() as u8,
        })
        .to_string();
        let body_kernel: String = json!({
            "kernel_image_path": kernel,
            "boot_args": parameters,
        })
        .to_string();

        let body_rootfs: String = json!({
            "drive_id": "rootfs",
            "path_on_host": rootfs,
            "is_root_device": false,
            "is_read_only": true
        })
        .to_string();

        info!(sl(), "Before first request");
        self.request_with_retry(Method::PUT, "/boot-source", body_kernel)
            .await?;
        self.request_with_retry(Method::PUT, "/machine-config", body_config)
            .await?;
        self.request_with_retry(Method::PUT, "/drives/rootfs", body_rootfs)
            .await?;

        let abs_path = [&self.vm_path, ROOT].join("/");

        let rel_path = "/".to_string();
        fs::create_dir_all(&abs_path)
            .await
            .context(format!("failed to create directory {:?}", abs_path))?;

        // We create some placeholder drives to be used for patching block devices while the vmm is
        // running, as firecracker does not support device hotplug.
        for i in 1..DISK_POOL_SIZE {
            let full_path_name = format!("{abs_path}/drive{i}");

            File::create(&full_path_name)
                .await
                .context(format!("failed to create file {:?}", full_path_name))?;

            let path_on_host = match self.jailed {
                false => abs_path.clone(),
                true => rel_path.clone(),
            };
            let body = json!({
                "drive_id": format!("drive{}",i),
                "path_on_host": format!("{}/drive{}", path_on_host, i),
                "is_root_device": false,
                "is_read_only": false
            });

            self.request_with_retry(Method::PUT, &format!("/drives/drive{i}"), body.to_string())
                .await?;
        }

        Ok(())
    }
    pub(crate) async fn patch_container_rootfs(
        &mut self,
        drive_id: &str,
        drive_path: &str,
    ) -> Result<()> {
        let index: u64 = drive_id
            .parse()
            .context("invalid Firecracker drive index")?;
        self.validate_drive_index(index)?;
        let previous_backing = self.attached_drives.get(&index).cloned();
        let name = if self.jailed {
            let root = format!("{}/{}", self.vm_path, ROOT);
            let placeholder = tempfile::Builder::new()
                .prefix(&format!("drive{index}-backing-"))
                .tempfile_in(&root)
                .context("create jailed backing mount target")?;
            let (_, path) = placeholder
                .keep()
                .context("keep jailed backing mount target")?;
            let name = path
                .file_name()
                .context("backing mount name")?
                .to_string_lossy()
                .into_owned();
            if let Err(error) = self.get_resource(drive_path, &name) {
                let _ = std::fs::remove_file(path);
                return Err(error).context("jail container backing storage");
            }
            name
        } else {
            drive_path.to_owned()
        };
        let path = if self.jailed {
            format!("/{name}")
        } else {
            name.clone()
        };
        // A cancelled future cannot run rollback. Retain the candidate before
        // issuing PATCH so a later detach still owns every possible backing.
        self.retain_uncertain_backing(index, name.clone());
        if let Err(error) = self.patch_drive(index, &path).await {
            // A lost response can follow a committed PATCH. Restore the previous
            // path before releasing the new mount; quarantine the slot if the
            // VMM cannot confirm rollback. Client validation rejections leave
            // the backing unchanged; server and transport errors may not.
            let rejected = error
                .downcast_ref::<ApiError>()
                .is_some_and(|error| error.status.is_client_error());
            if !rejected {
                let previous = previous_backing
                    .as_ref()
                    .map(|name| {
                        if self.jailed {
                            format!("/{name}")
                        } else {
                            name.clone()
                        }
                    })
                    .unwrap_or_else(|| {
                        if self.jailed {
                            format!("/drive{index}")
                        } else {
                            format!("{}/{ROOT}/drive{index}", self.vm_path)
                        }
                    });
                if let Err(rollback) = self.patch_drive(index, &previous).await {
                    self.retain_uncertain_backing(index, name);
                    return Err(error)
                        .context(format!("drive state uncertain; slot {index} retained because rollback failed: {rollback:#}"))
                        .context(crate::BlockAttachmentUncertain { index });
                }
            }
            if previous_backing.is_none() {
                self.attached_drives.remove(&index);
            }
            if self.jailed {
                self.retired_mounts.insert(name);
                if let Err(cleanup) = self.reap_retired_mounts(index) {
                    return Err(error).context(format!(
                        "drive patch failed; mount rollback also failed: {cleanup:#}"
                    ));
                }
            }
            return Err(error);
        }
        if self.jailed {
            let old = previous_backing;
            // PATCH has now closed any earlier VMM backing FD. Reap only this
            // slot, before releasing the canonical old mount needed for rollback.
            let cleanup = self
                .reap_retired_mounts_except(index, Some(&name))
                .and_then(|()| match &old {
                    Some(old) => self.release_backing_mount(old),
                    None => Ok(()),
                });
            if let Err(error) = cleanup {
                let previous = old
                    .as_ref()
                    .map(|name| format!("/{name}"))
                    .unwrap_or_else(|| format!("/drive{index}"));
                match self.patch_drive(index, &previous).await {
                    Ok(()) => {
                        if old.is_none() {
                            self.attached_drives.remove(&index);
                        }
                        self.retired_mounts.insert(name);
                    }
                    Err(rollback) => {
                        self.retain_uncertain_backing(index, name);
                        return Err(error)
                            .context(format!(
                                "updated drive retained; rollback failed: {rollback:#}"
                            ))
                            .context(crate::BlockAttachmentUncertain { index });
                    }
                }
                self.reap_retired_mounts(index)?;
                return Err(error);
            }
        }
        self.retired_mounts.remove(&name);
        self.attached_drives.insert(index, name);
        Ok(())
    }

    pub(super) fn validate_drive_index(&self, index: u64) -> Result<()> {
        if index == 0 || index >= u64::from(DISK_POOL_SIZE) {
            return Err(anyhow!(
                "Firecracker drive index {index} is outside the container slot pool 1..{}",
                DISK_POOL_SIZE
            ));
        }
        Ok(())
    }

    fn retain_uncertain_backing(&mut self, index: u64, name: String) {
        if let std::collections::btree_map::Entry::Vacant(entry) = self.attached_drives.entry(index)
        {
            entry.insert(name);
        } else if self.jailed {
            // Preserve the device manager's canonical backing. Either it or
            // this candidate may still be open until a later confirmed PATCH.
            self.retired_mounts.insert(name);
        }
    }

    pub(super) fn reap_retired_mounts(&mut self, index: u64) -> Result<()> {
        self.reap_retired_mounts_except(index, None)
    }

    fn reap_retired_mounts_except(&mut self, index: u64, protected: Option<&str>) -> Result<()> {
        let prefix = format!("drive{index}-backing-");
        for name in self
            .retired_mounts
            .clone()
            .into_iter()
            .filter(|name| name.starts_with(&prefix) && Some(name.as_str()) != protected)
        {
            self.release_backing_mount(&name)?;
            self.retired_mounts.remove(&name);
        }
        Ok(())
    }

    fn release_backing_mount(&self, name: &str) -> Result<()> {
        let path = format!("{}/{ROOT}/{name}", self.vm_path);
        // A failed unmount leaves the slot allocated and is retried by remove.
        // EINVAL means an earlier attempt already unmounted this target.
        match nix::mount::umount(path.as_str()) {
            Ok(()) | Err(nix::errno::Errno::EINVAL) | Err(nix::errno::Errno::ENOENT) => (),
            Err(error) => {
                return Err(error).with_context(|| format!("release backing mount {path}"))
            }
        }
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            // The backing storage is released. An empty target file can be
            // removed during final cleanup without retaining the drive slot.
            Err(error) => {
                warn!(sl(), "failed to remove empty backing mount target"; "path" => path, "error" => error.to_string());
                Ok(())
            }
        }
    }

    pub(super) async fn detach_block_device(&mut self, index: u64) -> Result<()> {
        self.validate_drive_index(index)?;
        let name = self
            .attached_drives
            .get(&index)
            .context("Firecracker drive is not attached")?
            .clone();
        let placeholder = if self.jailed {
            format!("/drive{index}")
        } else {
            format!("{}/{ROOT}/drive{index}", self.vm_path)
        };
        // The placeholder is a separate inode: release the VMM's original FD
        // first, then the bind mount. Never report success before both complete.
        let stopped =
            self.state == crate::VmmState::NotReady || (self.pid.is_some() && self.vmm_exited()?);
        if !stopped {
            self.patch_drive(index, &placeholder).await?;
        } else if !self.vmm_exited()? {
            return Err(anyhow!(
                "cannot release backing storage while Firecracker is alive"
            ));
        }
        if self.jailed {
            self.reap_retired_mounts(index)?;
            self.release_backing_mount(&name)?;
        }
        self.attached_drives.remove(&index);
        Ok(())
    }

    async fn patch_drive(&self, index: u64, path: &str) -> Result<()> {
        let block = &self.config.blockdev_info;
        let rate = RateLimiterConfig::new(
            block.disk_rate_limiter_bw_max_rate,
            block.disk_rate_limiter_ops_max_rate,
            block.disk_rate_limiter_bw_one_time_burst,
            block.disk_rate_limiter_ops_one_time_burst,
        );
        let mut body = json!({ "drive_id": format!("drive{index}"), "path_on_host": path });
        if let Some(rate) = rate {
            body["rate_limiter"] = serde_json::to_value(rate)?;
        }
        self.request_with_retry(
            Method::PATCH,
            &format!("/drives/drive{index}"),
            body.to_string(),
        )
        .await
    }

    pub(crate) async fn add_net_device(
        &mut self,
        config: &NetworkConfig,
        device_id: String,
    ) -> Result<()> {
        let g_mac = match &config.guest_mac {
            Some(mac) => MacAddr::from_bytes(&mac.0).ok(),
            None => None,
        };
        let body: String = json!({
            "iface_id": &device_id,
            "guest_mac": g_mac,
            "host_dev_name": &config.host_dev_name

        })
        .to_string();
        self.request_with_retry(
            Method::PUT,
            &["/network-interfaces/", &device_id].concat(),
            body,
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn request_with_retry(
        &self,
        method: Method,
        uri: &str,
        data: String,
    ) -> Result<()> {
        let url: hyper::Uri = Uri::new(&self.asock_path, uri).into();
        self.send_request_with_retry(method, url, data).await
    }

    pub(crate) async fn send_request_with_retry(
        &self,
        method: Method,
        uri: hyper::Uri,
        data: String,
    ) -> Result<()> {
        debug!(sl(), "METHOD: {:?}", method.clone());
        debug!(sl(), "URI: {:?}", uri.clone());
        debug!(sl(), "DATA: {:?}", data.clone());
        let mut last_error = None;
        for _count in 0..REQUEST_RETRY {
            let req = Request::builder()
                .method(method.clone())
                .uri(uri.clone())
                .header("Accept", "application/json")
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from(data.clone())))?;

            match tokio::time::timeout(std::time::Duration::from_secs(2), self.send_request(req))
                .await
                .context("Firecracker API request timed out")?
            {
                Ok(resp) => {
                    debug!(sl(), "Request sent, resp: {:?}", resp);
                    return Ok(());
                }
                Err(resp) => {
                    debug!(sl(), "Request sent with error, resp: {:?}", resp);
                    // API validation failures are permanent, and must reach callers.
                    if !resp.is::<hyper_util::client::legacy::Error>() {
                        return Err(resp);
                    }
                    last_error = Some(resp);
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    continue;
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("Firecracker request was not attempted")))
            .with_context(|| format!("Firecracker API unavailable after {REQUEST_RETRY} attempts"))
    }

    pub(crate) async fn send_request(
        &self,
        req: Request<Full<Bytes>>,
    ) -> Result<Response<Incoming>> {
        let resp = self.client.request(req).await?;

        let status = resp.status();
        debug!(sl(), "Request RESPONSE {:?} {:?}", &status, resp);
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.into_body().collect().await?.to_bytes();
        Err(ApiError {
            status,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
        .into())
    }
    pub(crate) fn cleanup_resource(&self) -> Result<()> {
        if self.jailed {
            let mut backings: std::collections::BTreeSet<_> = self
                .attached_drives
                .values()
                .chain(self.retired_mounts.iter())
                .cloned()
                .collect();
            // The sandbox checkpoint predates later container hotplug. Once
            // VMM exit is confirmed, discover our generated targets as well so
            // a crashed shim cannot leave uncheckpointed mounts behind.
            match std::fs::read_dir(format!("{}/{ROOT}", self.vm_path)) {
                Ok(entries) => {
                    for entry in entries {
                        let entry = entry?;
                        let name = entry.file_name().to_string_lossy().into_owned();
                        if (1..DISK_POOL_SIZE)
                            .any(|index| name.starts_with(&format!("drive{index}-backing-")))
                        {
                            if entry.file_type()?.is_symlink() {
                                return Err(anyhow!(
                                    "refusing symlinked Firecracker backing target {name}"
                                ));
                            }
                            backings.insert(name);
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error).context("discover Firecracker backing mounts"),
            }
            for name in backings {
                self.umount_jail_resource(&name)?;
            }
            for name in [
                super::diagnostics::LOG_FIFO,
                super::diagnostics::METRICS_FIFO,
                FC_KERNEL,
                FC_ROOT_FS,
                "",
            ] {
                self.umount_jail_resource(name)?;
            }
        }
        match std::fs::remove_dir_all(&self.vm_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("remove Firecracker VM directory"),
        }
    }

    pub(crate) fn umount_jail_resource(&self, jailed_path: &str) -> Result<()> {
        let path = match jailed_path {
            // Handle final case to umount the bind-mounted `/run/kata/firecracker/{id}/root` dir
            "" => [self.vm_path.clone(), ROOT.to_string()].join("/"),
            // Handle generic case to umount the bind-mounted
            // `/run/kata/firecracker/{id}/root/asset` file/dir
            _ => [
                self.vm_path.clone(),
                ROOT.to_string(),
                jailed_path.to_string(),
            ]
            .join("/"),
        };
        match nix::mount::umount(path.as_str()) {
            Ok(()) | Err(nix::errno::Errno::EINVAL) | Err(nix::errno::Errno::ENOENT) => Ok(()),
            Err(error) => Err(error).with_context(|| format!("umount path {path}")),
        }
    }
}
