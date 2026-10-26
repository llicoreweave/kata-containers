// Copyright (c) 2026 Kata Contributors
// SPDX-License-Identifier: Apache-2.0
use super::diagnostics::LOG_FIFO;
use super::diagnostics::METRICS_FIFO;
use super::{process, FcInner};
use crate::{
    device::DeviceType, BlockConfigModern, BlockDeviceModernHandle, NetworkConfig, VmmState,
};
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

type Requests = Arc<Mutex<Vec<(String, String, Value)>>>;

struct Api {
    _dir: TempDir,
    path: String,
    requests: Requests,
    status: Arc<AtomicUsize>,
    held_backing: Arc<Mutex<Option<(String, std::fs::File)>>>,
    task: JoinHandle<()>,
}

impl Api {
    fn new() -> Self {
        Self::with_path(None)
    }

    fn with_path(path: Option<String>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path =
            path.unwrap_or_else(|| dir.path().join("api.sock").to_string_lossy().into_owned());
        let listener = UnixListener::bind(&path).unwrap();
        let requests: Requests = Arc::default();
        let captured = requests.clone();
        let status = Arc::new(AtomicUsize::new(204));
        let response = status.clone();
        let held_backing = Arc::new(Mutex::new(None::<(String, std::fs::File)>));
        let backing = held_backing.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut data = Vec::new();
                let header_end = loop {
                    let mut buf = [0; 8192];
                    let count = socket.read(&mut buf).await.unwrap();
                    if count == 0 {
                        break None;
                    }
                    data.extend_from_slice(&buf[..count]);
                    if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(end + 4);
                    }
                };
                let Some(end) = header_end else {
                    continue;
                };
                let headers = String::from_utf8(data[..end].to_vec()).unwrap();
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                while data.len() < end + length {
                    let mut buf = [0; 8192];
                    let count = socket.read(&mut buf).await.unwrap();
                    assert_ne!(count, 0);
                    data.extend_from_slice(&buf[..count]);
                }
                let parts: Vec<_> = headers.lines().next().unwrap().split_whitespace().collect();
                captured.lock().unwrap().push((
                    parts[0].into(),
                    parts[1].into(),
                    serde_json::from_slice(&data[end..end + length]).unwrap(),
                ));
                let code = loop {
                    let code = response.load(Ordering::SeqCst);
                    if code != 0 {
                        break code;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                };
                if code == 204 && parts[0] == "PATCH" {
                    let mut held = backing.lock().unwrap();
                    if held.as_ref().is_some_and(|(path, _)| path == parts[1]) {
                        // A successful drive PATCH closes the VMM's old FD.
                        held.take();
                    }
                }
                let body = if code == 204 {
                    ""
                } else {
                    "{\"fault_message\":\"injected failure\"}"
                };
                let response = format!("HTTP/1.1 {code} Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                // A cancellation test can close its socket before the reply.
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        Self {
            _dir: dir,
            path,
            requests,
            status,
            held_backing,
            task,
        }
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn inner(api: &Api, dir: &TempDir) -> FcInner {
    let (notify, _) = mpsc::channel(1);
    let mut fc = FcInner::new(notify);
    fc.asock_path = api.path.clone();
    fc.vm_path = dir.path().to_string_lossy().into_owned();
    fc.state = VmmState::VmmServerReady;
    std::fs::create_dir_all(dir.path().join("root")).unwrap();
    fc
}

fn block(index: u64, path: &str) -> DeviceType {
    DeviceType::BlockModern(
        BlockDeviceModernHandle::new(
            format!("block{index}"),
            BlockConfigModern {
                index,
                path_on_host: path.into(),
                ..Default::default()
            },
        )
        .arc(),
    )
}

#[test]
fn versions() {
    for version in [
        "Firecracker v0.21.1",
        "Firecracker v1.17.0\nRevision: test",
        "Firecracker 1.2.3",
    ] {
        assert!(process::parse_version(version).is_ok(), "{}", version);
    }
    for version in [
        "Firecracker v0.21.0",
        "Firecracker v0.21.1-rc.1",
        "",
        "Firecracker",
        "Firecracker nope",
        "Firecracker vv1.2.3",
        "other 1.2.3",
    ] {
        assert!(process::parse_version(version).is_err(), "{}", version);
    }
}

#[tokio::test]
async fn configured_binary_version_errors() {
    let api = Api::new();
    let dir = tempfile::tempdir().unwrap();
    let mut fc = inner(&api, &dir);
    fc.config.path = dir.path().join("missing").to_string_lossy().into_owned();
    assert!(format!("{:#}", fc.validate_version().await.unwrap_err())
        .contains("read Firecracker version"));
    // Use immutable executables: another concurrent process spawn can briefly
    // inherit a newly written file descriptor and make exec fail with ETXTBSY.
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/firecracker/testdata");
    fc.config.path = fixtures
        .join("version-valid.sh")
        .to_string_lossy()
        .into_owned();
    fc.validate_version().await.unwrap();
    fc.config.path = fixtures
        .join("version-failed.sh")
        .to_string_lossy()
        .into_owned();
    assert!(fc
        .validate_version()
        .await
        .unwrap_err()
        .to_string()
        .contains("failed"));
    fc.config.path = fixtures
        .join("version-old.sh")
        .to_string_lossy()
        .into_owned();
    assert!(fc
        .validate_version()
        .await
        .unwrap_err()
        .to_string()
        .contains("minimum"));
}

#[tokio::test]
async fn block_lifecycle_and_failure_preserve_slot_and_rate_limits() {
    let api = Api::new();
    let dir = tempfile::tempdir().unwrap();
    let mut fc = inner(&api, &dir);
    fc.config.blockdev_info.disk_rate_limiter_bw_max_rate = 4096;
    fc.config.blockdev_info.disk_rate_limiter_ops_max_rate = 42;
    fc.config.blockdev_info.disk_rate_limiter_bw_one_time_burst = Some(8192);
    assert!(fc.add_device(block(0, "/a")).await.is_err());
    assert!(fc.add_device(block(6, "/a")).await.is_err());
    for _ in 0..8 {
        fc.add_device(block(1, "/original")).await.unwrap();
        assert!(fc.add_device(block(1, "/duplicate")).await.is_err());
        api.status.store(400, Ordering::SeqCst);
        let error = fc.remove_device(block(1, "/original")).await.unwrap_err();
        assert!(error.to_string().contains("400"));
        assert!(error.to_string().contains("injected failure"));
        assert_eq!(fc.attached_drives.get(&1).unwrap(), "/original");
        assert!(fc.update_device(block(1, "/replacement")).await.is_err());
        assert_eq!(fc.attached_drives.get(&1).unwrap(), "/original");
        api.status.store(204, Ordering::SeqCst);
        fc.update_device(block(1, "/replacement")).await.unwrap();
        fc.remove_device(block(1, "/replacement")).await.unwrap();
        assert!(fc.attached_drives.is_empty());
    }
    fc.remove_device(block(1, "/a")).await.unwrap();
    let requests = api.requests.lock().unwrap();
    assert_eq!(requests.len(), 8 * 5); // Permanent failures are not retried.
    for (method, path, body) in requests.iter() {
        assert_eq!(
            (method.as_str(), path.as_str()),
            ("PATCH", "/drives/drive1")
        );
        assert_eq!(body["rate_limiter"]["bandwidth"]["size"], 4096);
        assert_eq!(body["rate_limiter"]["bandwidth"]["one_time_burst"], 8192);
        assert_eq!(body["rate_limiter"]["ops"]["size"], 42);
    }
    assert_eq!(
        requests.last().unwrap().2["path_on_host"],
        format!("{}/root/drive1", fc.vm_path)
    );
}

#[tokio::test]
async fn uncertain_patch_quarantines_slot_until_confirmed_removal() {
    let api = Api::new();
    let dir = tempfile::tempdir().unwrap();
    let mut fc = inner(&api, &dir);
    api.status.store(500, Ordering::SeqCst);
    let error = fc.add_device(block(1, "/uncertain")).await.unwrap_err();
    assert!(format!("{error:#}").contains("slot 1 retained"));
    api.status.store(204, Ordering::SeqCst);
    assert!(fc.add_device(block(1, "/replacement")).await.is_err());
    fc.remove_device(block(1, "/uncertain")).await.unwrap();
    fc.add_device(block(1, "/replacement")).await.unwrap();
    fc.remove_device(block(1, "/replacement")).await.unwrap();
    assert!(fc.attached_drives.is_empty());
}

#[tokio::test]
async fn pending_device_removal_and_unsupported_devices() {
    let api = Api::new();
    let dir = tempfile::tempdir().unwrap();
    let mut fc = inner(&api, &dir);
    fc.state = VmmState::NotReady;
    fc.add_device(block(1, "/a")).await.unwrap();
    fc.remove_device(block(1, "/a")).await.unwrap();
    assert!(fc.pending_devices.is_empty());
    fc.remove_device(block(1, "/a")).await.unwrap();
    fc.state = VmmState::VmmServerReady;
    assert!(fc.update_device(block(1, "/a")).await.is_err());
    let network = DeviceType::Network(crate::NetworkDevice::new(
        "net0".into(),
        &NetworkConfig::default(),
    ));
    fc.state = VmmState::VmRunning;
    assert!(fc.remove_device(network.clone()).await.is_err());
    assert!(fc.update_device(network.clone()).await.is_err());
    fc.state = VmmState::VmmServerReady;
    assert!(fc.remove_device(network.clone()).await.is_err());
    fc.state = VmmState::NotReady;
    fc.add_device(network.clone()).await.unwrap();
    assert_eq!(fc.pending_devices.len(), 1);
    fc.remove_device(network).await.unwrap();
    assert!(fc.pending_devices.is_empty());
    assert!(api.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn pending_devices_are_applied_before_instance_start() {
    let dir = tempfile::tempdir().unwrap();
    let id = format!(
        "fc-queue-{}",
        dir.path().file_name().unwrap().to_string_lossy()
    );
    let run = format!("{}/{id}/run", kata_types::config::KATA_PATH);
    std::fs::create_dir_all(&run).unwrap();
    let api = Api::with_path(Some(format!("{run}/fc.sock")));
    let binary = dir.path().join("firecracker");
    std::fs::write(&binary, "#!/bin/sh\nif [ \"$1\" = --version ]; then echo Firecracker v1.17.0; exit 0; fi\nexec sleep 60\n").unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (notify, _) = mpsc::channel(1);
    let mut fc = FcInner::new(notify);
    fc.config.path = binary.to_string_lossy().into_owned();
    fc.config.boot_info.rootfs_type = "ext4".into();
    fc.config.blockdev_info.block_device_driver = crate::VM_ROOTFS_DRIVER_MMIO.into();
    fc.add_device(block(1, "/queued-container")).await.unwrap();
    fc.add_device(DeviceType::Network(crate::NetworkDevice::new(
        "net0".into(),
        &NetworkConfig::default(),
    )))
    .await
    .unwrap();
    assert!(api.requests.lock().unwrap().is_empty());
    fc.prepare_vm(&id, None, None).await.unwrap();
    assert!(fc.pending_devices.is_empty());
    assert_eq!(fc.attached_drives.get(&1).unwrap(), "/queued-container");
    let requests = api.requests.lock().unwrap().clone();
    assert_eq!(requests[requests.len() - 2].1, "/drives/drive1");
    assert_eq!(requests[requests.len() - 1].1, "/network-interfaces/net0");
    fc.start_vm(1).await.unwrap();
    assert_eq!(fc.state, VmmState::VmRunning);
    fc.stop_vm().await.unwrap();
    fc.cleanup().await.unwrap();
    assert!(fc.check().await.is_err());
}

#[tokio::test]
async fn device_manager_retains_failed_detach_and_commits_updates() {
    use crate::device::{
        device_manager::{do_handle_device, do_update_device, DeviceManager},
        DeviceConfig,
    };
    use crate::Hypervisor;
    let api = Api::new();
    let dir = tempfile::tempdir().unwrap();
    let fc = Arc::new(super::Firecracker::new());
    {
        let mut state = fc.inner.write().await;
        state.asock_path = api.path.clone();
        state.vm_path = dir.path().to_string_lossy().into_owned();
        state.state = VmmState::VmmServerReady;
    }
    let manager = tokio::sync::RwLock::new(DeviceManager::new(fc.clone(), None).await.unwrap());
    let mut config = BlockConfigModern {
        path_on_host: "/original".into(),
        driver_option: crate::VM_ROOTFS_DRIVER_MMIO.into(),
        ..Default::default()
    };
    let DeviceType::BlockModern(device) =
        do_handle_device(&manager, &DeviceConfig::BlockCfgModern(config.clone()))
            .await
            .unwrap()
    else {
        panic!("expected block device");
    };
    let id = device.lock().await.device_id.clone();
    assert_eq!(device.lock().await.config.index, 1);
    api.status.store(400, Ordering::SeqCst);
    assert!(manager.write().await.try_remove_device(&id).await.is_err());
    assert_eq!(device.lock().await.attach_count, 1);
    config.index = 1;
    config.path_on_host = "/updated".into();
    assert!(
        do_update_device(&manager, &DeviceConfig::BlockCfgModern(config.clone()))
            .await
            .is_err()
    );
    assert_eq!(device.lock().await.config.path_on_host, "/original");
    api.status.store(204, Ordering::SeqCst);
    do_update_device(&manager, &DeviceConfig::BlockCfgModern(config.clone()))
        .await
        .unwrap();
    assert_eq!(device.lock().await.config.path_on_host, "/updated");
    assert_eq!(device.lock().await.attach_count, 1);
    assert_eq!(device.lock().await.config.virt_path, "/dev/vdb");
    manager.write().await.try_remove_device(&id).await.unwrap();
    for generation in 0..32 {
        config.path_on_host = format!("/next-{generation}");
        let DeviceType::BlockModern(next) =
            do_handle_device(&manager, &DeviceConfig::BlockCfgModern(config.clone()))
                .await
                .unwrap()
        else {
            panic!("expected block device");
        };
        let next = next.lock().await.clone();
        assert_eq!(next.config.index, 1);
        manager
            .write()
            .await
            .try_remove_device(&next.device_id)
            .await
            .unwrap();
    }
    assert_eq!(fc.block_device_start_index().await, 1);
}

#[tokio::test]
async fn network_rate_limits() {
    let api = Api::new();
    let dir = tempfile::tempdir().unwrap();
    let mut fc = inner(&api, &dir);
    fc.add_net_device(&NetworkConfig::default(), "net0".into())
        .await
        .unwrap();
    assert!(api
        .requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .2
        .get("rx_rate_limiter")
        .is_none());
    fc.config.network_info.rx_rate_limiter_max_rate = 80_000_000;
    fc.config.network_info.tx_rate_limiter_max_rate = 8_000;
    fc.add_net_device(&NetworkConfig::default(), "net0".into())
        .await
        .unwrap();
    let body = api.requests.lock().unwrap().last().unwrap().2.clone();
    assert_eq!(body["rx_rate_limiter"]["bandwidth"]["size"], 10_485_760);
    assert_eq!(body["tx_rate_limiter"]["bandwidth"]["size"], 1024);
    assert_eq!(body["rx_rate_limiter"]["bandwidth"]["refill_time"], 1000);
    fc.config.network_info.rx_rate_limiter_max_rate = 1;
    assert!(fc
        .add_net_device(&NetworkConfig::default(), "net0".into())
        .await
        .is_err());
}

struct CountLogs(Arc<AtomicUsize>);
impl slog::Drain for CountLogs {
    type Ok = ();
    type Err = std::convert::Infallible;
    fn log(
        &self,
        record: &slog::Record<'_>,
        _: &slog::OwnedKVList,
    ) -> std::result::Result<(), Self::Err> {
        if record.msg().to_string() == "Firecracker log" {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[tokio::test]
async fn real_fifo_consumption_prometheus_and_reader_shutdown() {
    let count = Arc::new(AtomicUsize::new(0));
    slog_scope::set_global_logger(slog::Logger::root(CountLogs(count.clone()), slog::o!()))
        .cancel_reset();
    let api = Api::new();
    let dir = tempfile::tempdir().unwrap();
    let mut fc = inner(&api, &dir);
    fc.prepare_diagnostics(false).await.unwrap();
    let metrics_path = dir.path().join(METRICS_FIFO);
    let mut metrics = OpenOptions::new().write(true).open(&metrics_path).unwrap();
    let mut logs = OpenOptions::new()
        .write(true)
        .open(dir.path().join(LOG_FIFO))
        .unwrap();
    logs.write_all(b"first VMM log\nsecond VMM log\n").unwrap();
    metrics
        .write_all(b"{\"block\":{\"read_count\":7},")
        .unwrap();
    metrics
        .write_all(b"\"vcpu\":{\"exit_io_in\":3}}\nnot-json\n")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if fc
                .get_hypervisor_metrics()
                .await
                .unwrap()
                .contains("kata_firecracker_block{item=\"read_count\"} 7")
                && count.load(Ordering::SeqCst) == 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    drop(metrics);
    drop(logs);
    fc.diagnostics.stop().await;
    let fds: Vec<_> = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .collect();
    assert!(!fds.contains(&metrics_path));
    assert!(!fds.contains(&dir.path().join(LOG_FIFO)));
    assert_eq!(api.requests.lock().unwrap()[0].1, "/logger");
    assert_eq!(api.requests.lock().unwrap()[1].1, "/metrics");
}

#[tokio::test]
#[ignore = "requires a privileged container for real bind mounts"]
async fn jailed_mount_rollback_detach_and_update() {
    let api = Api::new();
    let dir = tempfile::tempdir().unwrap();
    let mut fc = inner(&api, &dir);
    fc.jailed = true;
    let original = dir.path().join("original");
    let replacement = dir.path().join("replacement");
    std::fs::write(&original, b"original").unwrap();
    std::fs::write(&replacement, b"replacement").unwrap();
    std::fs::write(dir.path().join("root/drive1"), b"").unwrap();
    for _ in 0..4 {
        fc.add_device(block(1, original.to_str().unwrap()))
            .await
            .unwrap();
        let mount = fc.attached_drives.get(&1).unwrap().clone();
        assert_eq!(
            std::fs::read(dir.path().join("root").join(&mount)).unwrap(),
            b"original"
        );
        api.status.store(400, Ordering::SeqCst);
        assert!(fc
            .update_device(block(1, replacement.to_str().unwrap()))
            .await
            .is_err());
        assert_eq!(fc.attached_drives.get(&1), Some(&mount));
        assert!(fc.retired_mounts.is_empty());
        assert!(fc.remove_device(block(1, "")).await.is_err());
        assert!(dir.path().join("root").join(&mount).exists());
        api.status.store(204, Ordering::SeqCst);
        let busy = std::fs::File::open(dir.path().join("root").join(&mount)).unwrap();
        assert!(fc
            .update_device(block(1, replacement.to_str().unwrap()))
            .await
            .is_err());
        assert_eq!(fc.attached_drives.get(&1), Some(&mount));
        assert!(fc.retired_mounts.is_empty());
        assert!(fc.remove_device(block(1, "")).await.is_err());
        assert!(fc
            .add_device(block(1, original.to_str().unwrap()))
            .await
            .is_err());
        drop(busy);
        fc.update_device(block(1, replacement.to_str().unwrap()))
            .await
            .unwrap();
        assert!(!dir.path().join("root").join(&mount).exists());
        fc.remove_device(block(1, "")).await.unwrap();
        assert!(fc.attached_drives.is_empty());
        assert!(std::fs::read(dir.path().join("root/drive1"))
            .unwrap()
            .is_empty());
    }
    fc.cleanup().await.unwrap();
}

#[tokio::test]
async fn preparation_failure_kills_vmm_and_cleans_fifo_directory() {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("firecracker");
    let pid_file = dir.path().join("pid");
    std::fs::write(&binary, format!("#!/bin/sh\nif [ \"$1\" = --version ]; then echo Firecracker v1.17.0; exit 0; fi\necho $$ > '{}'\nexec sleep 60\n", pid_file.display())).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (notify, _) = mpsc::channel(1);
    let mut fc = FcInner::new(notify);
    fc.config.path = binary.to_string_lossy().into_owned();
    let id = format!(
        "fc-failure-{}",
        dir.path().file_name().unwrap().to_string_lossy()
    );
    assert!(fc.prepare_vm(&id, None, None).await.is_err());
    let pid: u32 = std::fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(process::process_start_time(pid).is_err());
    assert!(!std::path::Path::new(&fc.vm_path).exists());
    assert_eq!(fc.state, VmmState::NotReady);
}

#[tokio::test]
async fn instance_start_failure_cleans_process_and_readers() {
    let api = Api::new();
    let dir = tempfile::tempdir().unwrap();
    let mut fc = inner(&api, &dir);
    let child = tokio::process::Command::new("sleep")
        .arg("60")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    fc.pid = Some(pid);
    fc.process_start_time = Some(process::process_start_time(pid).unwrap());
    *fc.fc_process.lock().await = Some(child);
    fc.prepare_diagnostics(false).await.unwrap();
    api.status.store(400, Ordering::SeqCst);
    assert!(fc.start_vm(1).await.is_err());
    assert!(fc.check().await.is_err());
    assert_eq!(fc.state, VmmState::NotReady);
    assert!(!dir.path().exists());
}

#[tokio::test]
async fn cleanup_stops_prepared_server_but_preserves_running_vm() {
    let api = Api::new();
    let dir = tempfile::tempdir().unwrap();
    let mut fc = inner(&api, &dir);
    let child = tokio::process::Command::new("sleep")
        .arg("60")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    fc.pid = Some(pid);
    fc.process_start_time = Some(process::process_start_time(pid).unwrap());
    *fc.fc_process.lock().await = Some(child);
    fc.prepare_diagnostics(false).await.unwrap();
    fc.state = VmmState::VmRunning;
    assert!(fc.cleanup().await.is_err());
    fc.check().await.unwrap();
    fc.state = VmmState::VmmServerReady;
    fc.cleanup().await.unwrap();
    assert!(fc.check().await.is_err());
    assert_eq!(fc.state, VmmState::NotReady);
    assert!(!dir.path().exists());
}

#[tokio::test]
async fn uncertain_attach_keeps_manager_ownership_until_reconciled() {
    use crate::device::{
        device_manager::{do_handle_device, DeviceManager},
        DeviceConfig,
    };
    let api = Api::new();
    let dir = tempfile::tempdir().unwrap();
    let fc = Arc::new(super::Firecracker::new());
    {
        let mut state = fc.inner.write().await;
        state.asock_path = api.path.clone();
        state.vm_path = dir.path().to_string_lossy().into_owned();
        state.state = VmmState::VmmServerReady;
    }
    let manager = tokio::sync::RwLock::new(DeviceManager::new(fc.clone(), None).await.unwrap());
    let config = |path: &str| {
        DeviceConfig::BlockCfgModern(BlockConfigModern {
            path_on_host: path.into(),
            driver_option: crate::VM_ROOTFS_DRIVER_MMIO.into(),
            ..Default::default()
        })
    };
    for _ in 0..8 {
        api.status.store(500, Ordering::SeqCst);
        assert!(do_handle_device(&manager, &config("/uncertain"))
            .await
            .is_err());
        assert_eq!(fc.inner.read().await.attached_drives.len(), 1);
        // Returning an existing handle must not turn an uncertain attach into
        // success or increase its reference count while reconciliation fails.
        assert!(do_handle_device(&manager, &config("/uncertain"))
            .await
            .is_err());
        assert_eq!(fc.inner.read().await.attached_drives.len(), 1);
        api.status.store(204, Ordering::SeqCst);
        let DeviceType::BlockModern(healthy) = do_handle_device(&manager, &config("/healthy"))
            .await
            .unwrap()
        else {
            panic!("expected block device");
        };
        let healthy = healthy.lock().await.clone();
        assert_eq!(healthy.config.index, 1);
        assert_eq!(healthy.attach_count, 1);
        assert_eq!(fc.inner.read().await.attached_drives.len(), 1);
        manager
            .write()
            .await
            .try_remove_device(&healthy.device_id)
            .await
            .unwrap();
        assert!(fc.inner.read().await.attached_drives.is_empty());
    }
}

#[tokio::test]
#[ignore = "requires a privileged container for real bind mounts"]
async fn jailed_uncertain_update_detaches_before_reaping_backings() {
    let api = Api::new();
    let dir = tempfile::tempdir().unwrap();
    let mut fc = inner(&api, &dir);
    fc.jailed = true;
    let original = dir.path().join("original");
    let replacement = dir.path().join("replacement");
    std::fs::write(&original, b"original").unwrap();
    std::fs::write(&replacement, b"replacement").unwrap();
    for index in [1, 2] {
        std::fs::write(dir.path().join(format!("root/drive{index}")), b"").unwrap();
    }
    fc.add_device(block(1, original.to_str().unwrap()))
        .await
        .unwrap();
    let old = fc.attached_drives[&1].clone();
    *api.held_backing.lock().unwrap() = Some((
        "/drives/drive1".into(),
        std::fs::File::open(dir.path().join("root").join(&old)).unwrap(),
    ));
    api.status.store(500, Ordering::SeqCst);
    assert!(fc
        .update_device(block(1, replacement.to_str().unwrap()))
        .await
        .is_err());
    assert_eq!(fc.attached_drives.get(&1), Some(&old));
    assert_eq!(fc.retired_mounts.len(), 1);
    api.status.store(204, Ordering::SeqCst);
    // An uncertain slot must not block operations on an independent slot.
    fc.add_device(block(2, replacement.to_str().unwrap()))
        .await
        .unwrap();
    fc.remove_device(block(2, "")).await.unwrap();
    assert!(api.held_backing.lock().unwrap().is_some());
    fc.remove_device(block(1, "")).await.unwrap();
    assert!(api.held_backing.lock().unwrap().is_none());
    assert!(fc.attached_drives.is_empty());
    assert!(fc.retired_mounts.is_empty());
    assert!(!dir.path().join("root").join(old).exists());
    fc.cleanup().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a privileged container for real bind mounts"]
async fn jailed_block_removal_after_stop_retries_busy_mounts_without_api() {
    let api = Api::new();
    let dir = tempfile::tempdir().unwrap();
    let mut fc = inner(&api, &dir);
    fc.jailed = true;
    let original = dir.path().join("original");
    std::fs::write(&original, b"original").unwrap();
    std::fs::write(dir.path().join("root/drive1"), b"").unwrap();
    fc.add_device(block(1, original.to_str().unwrap()))
        .await
        .unwrap();
    let mount = dir.path().join("root").join(&fc.attached_drives[&1]);
    let busy = std::fs::File::open(&mount).unwrap();
    fc.stop_vm().await.unwrap();
    api.status.store(500, Ordering::SeqCst);
    let requests = api.requests.lock().unwrap().len();
    assert!(fc.remove_device(block(1, "")).await.is_err());
    assert!(fc.attached_drives.contains_key(&1));
    drop(busy);
    fc.remove_device(block(1, "")).await.unwrap();
    assert_eq!(requests, api.requests.lock().unwrap().len());
    assert!(!mount.exists());
    assert!(fc.attached_drives.is_empty());
    fc.cleanup().await.unwrap();
}

#[tokio::test]
async fn block_removal_after_process_exit_does_not_require_api() {
    let api = Api::new();
    let dir = tempfile::tempdir().unwrap();
    let mut fc = inner(&api, &dir);
    let mut child = tokio::process::Command::new("sleep")
        .arg("60")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    fc.pid = Some(pid);
    fc.process_start_time = Some(process::process_start_time(pid).unwrap());
    fc.state = VmmState::VmRunning;
    fc.add_device(block(1, "/backing")).await.unwrap();
    child.kill().await.unwrap();
    api.status.store(500, Ordering::SeqCst);
    let requests = api.requests.lock().unwrap().len();
    fc.remove_device(block(1, "")).await.unwrap();
    assert_eq!(requests, api.requests.lock().unwrap().len());
    assert!(fc.attached_drives.is_empty());
    fc.cleanup().await.unwrap();
}

#[tokio::test]
async fn concurrent_uncertain_attachment_stays_quarantined() {
    use crate::device::{
        device_manager::{do_handle_device, DeviceManager},
        DeviceConfig,
    };
    let api = Api::new();
    api.status.store(500, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let fc = Arc::new(super::Firecracker::new());
    {
        let mut state = fc.inner.write().await;
        state.asock_path = api.path.clone();
        state.vm_path = dir.path().to_string_lossy().into_owned();
        state.state = VmmState::VmmServerReady;
    }
    let manager = Arc::new(tokio::sync::RwLock::new(
        DeviceManager::new(fc.clone(), None).await.unwrap(),
    ));
    let config = BlockConfigModern {
        path_on_host: "/shared-uncertain".into(),
        driver_option: crate::VM_ROOTFS_DRIVER_MMIO.into(),
        ..Default::default()
    };
    // Queue both new_device calls before either caller can try_add_device.
    let held = manager.write().await;
    let first_manager = manager.clone();
    let first_config = config.clone();
    let first = tokio::spawn(async move {
        do_handle_device(&first_manager, &DeviceConfig::BlockCfgModern(first_config)).await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let second_manager = manager.clone();
    let second = tokio::spawn(async move {
        do_handle_device(&second_manager, &DeviceConfig::BlockCfgModern(config)).await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    drop(held);
    let first = first.await.unwrap();
    let second = second.await.unwrap();
    assert!(first.is_err());
    assert!(
        second.is_err(),
        "quarantined attachment returned success: {:?}",
        second
    );
    api.status.store(204, Ordering::SeqCst);
    let DeviceType::BlockModern(device) = do_handle_device(
        &manager,
        &DeviceConfig::BlockCfgModern(BlockConfigModern {
            path_on_host: "/shared-uncertain".into(),
            driver_option: crate::VM_ROOTFS_DRIVER_MMIO.into(),
            ..Default::default()
        }),
    )
    .await
    .unwrap() else {
        panic!("expected block device")
    };
    let device = device.lock().await.clone();
    assert_eq!(device.config.index, 1);
    assert_eq!(device.attach_count, 1);
    manager
        .write()
        .await
        .try_remove_device(&device.device_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn uncertain_update_cannot_be_shared_as_original_backing() {
    use crate::device::{
        device_manager::{do_handle_device, do_update_device, DeviceManager},
        DeviceConfig,
    };
    let api = Api::new();
    let dir = tempfile::tempdir().unwrap();
    let fc = Arc::new(super::Firecracker::new());
    {
        let mut state = fc.inner.write().await;
        state.asock_path = api.path.clone();
        state.vm_path = dir.path().to_string_lossy().into_owned();
        state.state = VmmState::VmmServerReady;
    }
    let manager = tokio::sync::RwLock::new(DeviceManager::new(fc.clone(), None).await.unwrap());
    let config = BlockConfigModern {
        path_on_host: "/original".into(),
        driver_option: crate::VM_ROOTFS_DRIVER_MMIO.into(),
        ..Default::default()
    };
    let DeviceType::BlockModern(original) =
        do_handle_device(&manager, &DeviceConfig::BlockCfgModern(config.clone()))
            .await
            .unwrap()
    else {
        panic!("expected block device")
    };
    api.status.store(500, Ordering::SeqCst);
    let update = do_update_device(
        &manager,
        &DeviceConfig::BlockCfgModern(BlockConfigModern {
            index: 1,
            path_on_host: "/replacement".into(),
            ..config.clone()
        }),
    )
    .await
    .unwrap_err();
    assert!(format!("{update:#}").contains("rollback failed"));
    let requests = api.requests.lock().unwrap().len();
    let reused = do_handle_device(&manager, &DeviceConfig::BlockCfgModern(config.clone())).await;
    assert_eq!(
        api.requests.lock().unwrap().len(),
        requests,
        "reuse made no reconciliation request"
    );
    assert!(
        reused.is_err(),
        "uncertain update returned original backing successfully: {:?}",
        reused
    );
    // Creating an unrelated device must not detach a failed update with live users.
    api.status.store(204, Ordering::SeqCst);
    let DeviceType::BlockModern(other) = do_handle_device(
        &manager,
        &DeviceConfig::BlockCfgModern(BlockConfigModern {
            path_on_host: "/other".into(),
            ..config.clone()
        }),
    )
    .await
    .unwrap() else {
        panic!("expected block device")
    };
    assert_eq!(other.lock().await.config.index, 2);
    assert_eq!(original.lock().await.attach_count, 1);
    assert!(
        do_handle_device(&manager, &DeviceConfig::BlockCfgModern(config.clone()))
            .await
            .is_err()
    );
    // A rejected reconciliation leaves the uncertainty marker intact.
    api.status.store(400, Ordering::SeqCst);
    assert!(do_update_device(
        &manager,
        &DeviceConfig::BlockCfgModern(BlockConfigModern {
            index: 1,
            ..config.clone()
        })
    )
    .await
    .is_err());
    assert!(
        do_handle_device(&manager, &DeviceConfig::BlockCfgModern(config.clone()))
            .await
            .is_err()
    );
    api.status.store(204, Ordering::SeqCst);
    do_update_device(
        &manager,
        &DeviceConfig::BlockCfgModern(BlockConfigModern {
            index: 1,
            ..config.clone()
        }),
    )
    .await
    .unwrap();
    do_handle_device(&manager, &DeviceConfig::BlockCfgModern(config))
        .await
        .unwrap();
    assert_eq!(original.lock().await.attach_count, 2);
    let id = original.lock().await.device_id.clone();
    manager.write().await.try_remove_device(&id).await.unwrap();
    manager.write().await.try_remove_device(&id).await.unwrap();
    let other_id = other.lock().await.device_id.clone();
    manager
        .write()
        .await
        .try_remove_device(&other_id)
        .await
        .unwrap();
    assert!(fc.inner.read().await.attached_drives.is_empty());
}

#[tokio::test]
async fn resource_cleanup_stops_prepared_server_before_network_teardown() {
    use crate::Hypervisor;
    let api = Api::new();
    let dir = tempfile::tempdir().unwrap();
    let fc = super::Firecracker::new();
    let network = DeviceType::Network(crate::NetworkDevice::new(
        "net0".into(),
        &NetworkConfig::default(),
    ));
    {
        let mut state = fc.inner.write().await;
        state.asock_path = api.path.clone();
        state.vm_path = dir.path().to_string_lossy().into_owned();
        state.state = VmmState::VmmServerReady;
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        state.pid = Some(pid);
        state.process_start_time = Some(process::process_start_time(pid).unwrap());
        *state.fc_process.lock().await = Some(child);
    }
    fc.add_device(network.clone()).await.unwrap();
    // This is the hook invoked by VirtSandbox before resource_manager.cleanup.
    fc.prepare_for_cleanup().await.unwrap();
    assert!(fc.check().await.is_err());
    fc.remove_device(network).await.unwrap();
    // Preparing cleanup must not remove files still needed by resource teardown.
    assert!(dir.path().exists());
    fc.cleanup().await.unwrap();
    assert!(!dir.path().exists());
}

#[tokio::test]
async fn resource_cleanup_handles_unexpected_vmm_exit_before_network_teardown() {
    use crate::Hypervisor;
    let api = Api::new();
    let dir = tempfile::tempdir().unwrap();
    let fc = super::Firecracker::new();
    let mut child = tokio::process::Command::new("sleep")
        .arg("60")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    {
        let mut state = fc.inner.write().await;
        state.asock_path = api.path.clone();
        state.vm_path = dir.path().to_string_lossy().into_owned();
        state.state = VmmState::VmRunning;
        state.pid = child.id();
        state.process_start_time = Some(process::process_start_time(child.id().unwrap()).unwrap());
    }
    assert!(fc.prepare_for_cleanup().await.is_err());
    child.kill().await.unwrap();
    fc.prepare_for_cleanup().await.unwrap();
    fc.remove_device(DeviceType::Network(crate::NetworkDevice::new(
        "net0".into(),
        &NetworkConfig::default(),
    )))
    .await
    .unwrap();
    fc.cleanup().await.unwrap();
}

async fn cancelled_block_request_preserves_ownership(update: bool, jailed: bool) {
    use crate::device::{
        device_manager::{do_handle_device, do_update_device, DeviceManager},
        DeviceConfig,
    };
    let api = Api::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("root")).unwrap();
    for name in ["original", "replacement", "root/drive1"] {
        std::fs::write(dir.path().join(name), b"backing").unwrap();
    }
    let fc = Arc::new(super::Firecracker::new());
    {
        let mut state = fc.inner.write().await;
        state.asock_path = api.path.clone();
        state.vm_path = dir.path().to_string_lossy().into_owned();
        state.state = VmmState::VmmServerReady;
        state.jailed = jailed;
    }
    let manager = Arc::new(tokio::sync::RwLock::new(
        DeviceManager::new(fc.clone(), None).await.unwrap(),
    ));
    let config = BlockConfigModern {
        path_on_host: dir.path().join("original").to_string_lossy().into_owned(),
        driver_option: crate::VM_ROOTFS_DRIVER_MMIO.into(),
        ..Default::default()
    };
    if update {
        do_handle_device(&manager, &DeviceConfig::BlockCfgModern(config.clone()))
            .await
            .unwrap();
    }
    let before = api.requests.lock().unwrap().len();
    api.status.store(0, Ordering::SeqCst);
    let pending_manager = manager.clone();
    let mut pending_config = config.clone();
    if update {
        pending_config.index = 1;
        pending_config.path_on_host = dir
            .path()
            .join("replacement")
            .to_string_lossy()
            .into_owned();
    }
    let caller = tokio::spawn(async move {
        let config = DeviceConfig::BlockCfgModern(pending_config);
        if update {
            do_update_device(&pending_manager, &config).await
        } else {
            do_handle_device(&pending_manager, &config)
                .await
                .map(|_| ())
        }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while api.requests.lock().unwrap().len() == before {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    api.status.store(204, Ordering::SeqCst);
    let reused = do_handle_device(&manager, &DeviceConfig::BlockCfgModern(config.clone())).await;
    if update {
        assert!(
            reused.is_err(),
            "cancelled update was reused before reconciliation"
        );
        do_update_device(
            &manager,
            &DeviceConfig::BlockCfgModern(BlockConfigModern {
                index: 1,
                ..config.clone()
            }),
        )
        .await
        .unwrap();
    } else {
        let DeviceType::BlockModern(device) = reused.unwrap() else {
            panic!("expected block device")
        };
        assert_eq!(
            device.lock().await.attach_count,
            1,
            "cancelled attach was counted as a successful user"
        );
        assert!(
            api.requests.lock().unwrap().len() > before + 1,
            "no API reconciliation after cancelled attach"
        );
    }
    let DeviceType::BlockModern(device) =
        do_handle_device(&manager, &DeviceConfig::BlockCfgModern(config))
            .await
            .unwrap()
    else {
        panic!("expected block device")
    };
    let id = device.lock().await.device_id.clone();
    if !update {
        manager.write().await.try_remove_device(&id).await.unwrap();
    }
    manager.write().await.try_remove_device(&id).await.unwrap();
    if update {
        manager.write().await.try_remove_device(&id).await.unwrap();
    }
    assert!(fc.inner.read().await.attached_drives.is_empty());
    assert!(fc.inner.read().await.retired_mounts.is_empty());
    fc.inner.write().await.cleanup().await.unwrap();
}

#[tokio::test]
async fn cancelled_attach_requires_reconciliation() {
    cancelled_block_request_preserves_ownership(false, false).await;
}

#[tokio::test]
async fn cancelled_update_requires_reconciliation() {
    cancelled_block_request_preserves_ownership(true, false).await;
}

#[tokio::test]
#[ignore = "requires a privileged container for real bind mounts"]
async fn jailed_cancelled_requests_preserve_backing_mounts() {
    cancelled_block_request_preserves_ownership(false, true).await;
    cancelled_block_request_preserves_ownership(true, true).await;
}

#[tokio::test]
async fn cancelled_detach_retains_final_reference_for_retry() {
    use crate::device::{
        device_manager::{do_handle_device, DeviceManager},
        DeviceConfig,
    };
    let api = Api::new();
    let dir = tempfile::tempdir().unwrap();
    let fc = Arc::new(super::Firecracker::new());
    {
        let mut state = fc.inner.write().await;
        state.asock_path = api.path.clone();
        state.vm_path = dir.path().to_string_lossy().into_owned();
        state.state = VmmState::VmmServerReady;
    }
    let manager = Arc::new(tokio::sync::RwLock::new(
        DeviceManager::new(fc.clone(), None).await.unwrap(),
    ));
    let config = BlockConfigModern {
        path_on_host: "/original".into(),
        driver_option: crate::VM_ROOTFS_DRIVER_MMIO.into(),
        ..Default::default()
    };
    let DeviceType::BlockModern(device) =
        do_handle_device(&manager, &DeviceConfig::BlockCfgModern(config.clone()))
            .await
            .unwrap()
    else {
        panic!("expected block device")
    };
    let id = device.lock().await.device_id.clone();
    let before = api.requests.lock().unwrap().len();
    api.status.store(0, Ordering::SeqCst);
    let pending_manager = manager.clone();
    let pending_id = id.clone();
    let caller = tokio::spawn(async move {
        pending_manager
            .write()
            .await
            .try_remove_device(&pending_id)
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while api.requests.lock().unwrap().len() == before {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    assert_eq!(device.lock().await.attach_count, 1);
    api.status.store(204, Ordering::SeqCst);
    assert!(
        do_handle_device(&manager, &DeviceConfig::BlockCfgModern(config.clone()))
            .await
            .is_err()
    );
    manager.write().await.try_remove_device(&id).await.unwrap();
    let DeviceType::BlockModern(next) =
        do_handle_device(&manager, &DeviceConfig::BlockCfgModern(config))
            .await
            .unwrap()
    else {
        panic!("expected block device")
    };
    let next = next.lock().await.clone();
    assert_eq!(next.config.index, 1);
    assert_eq!(next.attach_count, 1);
    manager
        .write()
        .await
        .try_remove_device(&next.device_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn process_health_and_vcpu_threads() {
    let mut fc = FcInner::new(mpsc::channel(1).0);
    assert!(fc.check().await.is_err());
    let mut child = tokio::process::Command::new("sleep")
        .arg("60")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    fc.pid = Some(pid);
    fc.process_start_time = Some(process::process_start_time(pid).unwrap());
    fc.check().await.unwrap();
    assert_eq!(fc.get_pids().await.unwrap(), vec![pid]);
    child.kill().await.unwrap();
    assert!(fc.check().await.is_err());
    let (ready, received) = std::sync::mpsc::channel();
    let (finish, done) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("fc_vcpu 3".into())
        .spawn(move || {
            ready.send(nix::unistd::gettid().as_raw() as u32).unwrap();
            done.recv().unwrap();
        })
        .unwrap();
    let tid = received.recv().unwrap();
    fc.pid = Some(std::process::id());
    fc.process_start_time = Some(process::process_start_time(std::process::id()).unwrap());
    assert_eq!(fc.get_thread_ids().await.unwrap().vcpus.get(&3), Some(&tid));
    finish.send(()).unwrap();
    thread.join().unwrap();
}
