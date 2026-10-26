// Copyright (c) 2026 Kata Contributors
// SPDX-License-Identifier: Apache-2.0

use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use anyhow::{Context, Result};
use persist::sandbox_persist::Persist;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::RwLock,
};

use super::Firecracker;
use crate::device::{
    device_manager::{do_handle_device, do_update_device, DeviceManager},
    DeviceConfig, DeviceType,
};
use crate::{BlockConfigModern, Hypervisor, HypervisorConfig};

struct LogCounter(Arc<AtomicUsize>, slog::Logger);
impl slog::Drain for LogCounter {
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
        self.1.log(record);
        Ok(())
    }
}

fn holds_backing_file(pid: u32, path: &Path) -> Result<bool> {
    let backing = std::fs::metadata(path)?;
    for fd in std::fs::read_dir(format!("/proc/{pid}/fd"))? {
        if let Ok(meta) = std::fs::metadata(fd?.path()) {
            if (backing.file_type().is_block_device()
                && meta.file_type().is_block_device()
                && meta.rdev() == backing.rdev())
                || (meta.dev() == backing.dev() && meta.ino() == backing.ino())
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn checked(command: &mut Command) -> Result<String> {
    let output = command
        .output()
        .with_context(|| format!("execute {command:?}"))?;
    anyhow::ensure!(
        output.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

struct Backing {
    file: PathBuf,
    path: PathBuf,
    mapper: Option<String>,
    loop_device: Option<String>,
}

impl Backing {
    fn new(dir: &Path, generation: u32, devmapper: bool) -> Result<Self> {
        let file = dir.join(format!("container-{generation}.img"));
        std::fs::copy(std::env::var("FC_TEST_ROOTFS")?, &file)?;
        let mut backing = Self {
            path: file.clone(),
            file,
            mapper: None,
            loop_device: None,
        };
        if devmapper {
            let loop_device = checked(
                Command::new("losetup")
                    .args(["--find", "--show"])
                    .arg(&backing.file),
            )?;
            backing.loop_device = Some(loop_device.clone());
            let name = format!(
                "fc-rust-{}-{generation}",
                dir.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .trim_start_matches('.')
            );
            let sectors = std::fs::metadata(&backing.file)?.len() / 512;
            checked(Command::new("dmsetup").args([
                "--noudevsync",
                "create",
                &name,
                "--addnodeoncreate",
                "--table",
                &format!("0 {sectors} linear {loop_device} 0"),
            ]))?;
            backing.mapper = Some(name.clone());
            // Docker has no udev daemon to create /dev/mapper nodes.
            checked(Command::new("dmsetup").args(["--noudevsync", "mknodes", &name]))?;
            backing.path = Path::new("/dev/mapper").join(&name);
        }
        Ok(backing)
    }

    fn release(&mut self) -> Result<()> {
        // Removing without --deferred must succeed while the sandbox still runs.
        // A leaked VMM FD or jailed mount makes device mapper reject this call.
        if let Some(name) = &self.mapper {
            // Host udev can briefly open a new mapper device for probing. Use
            // dmsetup's bounded retry; a leaked VMM FD still prevents removal.
            checked(Command::new("dmsetup").args(["--noudevsync", "remove", "--retry", name]))?;
            self.mapper = None;
            // Without udev, dmsetup leaves its explicitly created device node.
            std::fs::remove_file(&self.path)?;
        }
        if let Some(device) = &self.loop_device {
            checked(Command::new("losetup").args(["--detach", device]))?;
            self.loop_device = None;
        }
        std::fs::remove_file(&self.file)?;
        Ok(())
    }
}

impl Drop for Backing {
    fn drop(&mut self) {
        // On a failed assertion, defer removal until Docker reaps the test VMM;
        // loop detach marks an in-use loop device for automatic removal.
        if let Some(name) = &self.mapper {
            let _ = Command::new("dmsetup")
                .args(["--noudevsync", "remove", "--deferred", name])
                .output();
            let _ = std::fs::remove_file(&self.path);
        }
        if let Some(device) = &self.loop_device {
            let _ = Command::new("losetup").args(["--detach", device]).output();
        }
    }
}

#[test]
#[ignore = "requires a privileged container, device mapper and the guest rootfs"]
fn devmapper_fixture_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let mut backing = Backing::new(dir.path(), 0, true).unwrap();
    let file = std::fs::File::open(&backing.path).unwrap();
    assert!(
        backing.release().is_err(),
        "an open backing device must prevent removal"
    );
    drop(file);
    backing.release().unwrap();
    assert!(!backing.path.exists());
}

async fn guest_command(fc: &Firecracker, command: &str) -> Result<String> {
    tokio::time::timeout(Duration::from_secs(10), guest_command_inner(fc, command))
        .await
        .context("guest controller request timed out")?
}

async fn guest_command_inner(fc: &Firecracker, command: &str) -> Result<String> {
    let socket = fc.get_agent_socket().await?;
    let path = socket
        .strip_prefix("hvsock://")
        .context("Firecracker vsock scheme")?;
    let mut stream = BufReader::new(UnixStream::connect(path).await?);
    stream.get_mut().write_all(b"CONNECT 12345\n").await?;
    let mut response = String::new();
    stream.read_line(&mut response).await?;
    anyhow::ensure!(
        response.starts_with("OK "),
        "vsock proxy rejected connection: {response}"
    );
    stream
        .get_mut()
        .write_all(format!("{command}\n").as_bytes())
        .await?;
    response.clear();
    stream.read_line(&mut response).await?;
    Ok(response)
}

async fn real_vm(jailed: bool) -> Result<()> {
    let log_count = Arc::new(AtomicUsize::new(0));
    let (logger, _logging_guard) = logging::create_term_logger(slog::Level::Info);
    slog_scope::set_global_logger(slog::Logger::root(
        LogCounter(log_count.clone(), logger),
        slog::o!(),
    ))
    .cancel_reset();
    let dir = tempfile::tempdir()?;
    let mut config = HypervisorConfig {
        path: std::env::var("FC_TEST_BINARY").context("FC_TEST_BINARY is required")?,
        ..Default::default()
    };
    if jailed {
        config.jailer_path =
            std::env::var("FC_TEST_JAILER").context("FC_TEST_JAILER is required")?;
    }
    config.boot_info.kernel =
        std::env::var("FC_TEST_KERNEL").context("FC_TEST_KERNEL is required")?;
    config.boot_info.image =
        std::env::var("FC_TEST_ROOTFS").context("FC_TEST_ROOTFS is required")?;
    config.boot_info.rootfs_type = "ext4".into();
    config.boot_info.kernel_params =
        "console=ttyS0 reboot=k panic=1 root=/dev/vda init=/sbin/init".into();
    config.blockdev_info.block_device_driver = crate::VM_ROOTFS_DRIVER_MMIO.into();
    config.blockdev_info.block_device_cache_set = true;
    config.blockdev_info.disk_rate_limiter_bw_max_rate = 16 * 1024 * 1024;
    config.blockdev_info.disk_rate_limiter_ops_max_rate = 1024;
    config.memory_info.default_memory = 128;
    config.cpu_info.default_vcpus = 2.0;
    config.debug_info.enable_debug = true;
    let fc = Arc::new(Firecracker::new());
    fc.set_hypervisor_config(config).await;
    let id = format!(
        "fc-ci-{}",
        dir.path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .trim_start_matches('.')
    );
    fc.prepare_vm(&id, None, &Default::default(), None).await?;
    let vm_path = fc.inner.read().await.vm_path.clone();
    fc.start_vm(10).await?;
    fc.check().await?;
    let pid = fc.get_pids().await?[0];
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if fc.get_thread_ids().await?.vcpus.len() == 2 {
                return anyhow::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(Ok(response)) =
                tokio::time::timeout(Duration::from_millis(500), guest_command(&fc, "ready")).await
            {
                if response == "ready\n" {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("guest test controller did not become ready")?;
    let manager = RwLock::new(DeviceManager::new(fc.clone(), None).await?);
    for generation in 0..10 {
        let mut backing = Backing::new(dir.path(), generation, jailed)?;
        let path = backing.path.clone();
        let device = do_handle_device(
            &manager,
            &DeviceConfig::BlockCfgModern(BlockConfigModern {
                path_on_host: path.to_string_lossy().into_owned(),
                driver_option: crate::VM_ROOTFS_DRIVER_MMIO.into(),
                ..Default::default()
            }),
        )
        .await?;
        let DeviceType::BlockModern(block) = device else {
            anyhow::bail!("expected a block device");
        };
        let (device_id, index) = {
            let block = block.lock().await;
            (block.device_id.clone(), block.config.index)
        };
        assert_eq!(index, 1, "the released slot must be reused");
        assert!(holds_backing_file(pid, &path)?);
        assert_eq!(guest_command(&fc, "start").await?, "started\n");
        // A rejected update must keep the original backing storage attached.
        let bad = crate::BlockDeviceModernHandle::new(
            device_id.clone(),
            BlockConfigModern {
                index,
                path_on_host: dir.path().join("missing").to_string_lossy().into_owned(),
                ..Default::default()
            },
        );
        assert!(fc
            .update_device(DeviceType::BlockModern(bad.arc()))
            .await
            .is_err());
        assert!(holds_backing_file(pid, &path)?);
        assert_eq!(guest_command(&fc, "stop").await?, "stopped\n");
        let mut replacement = Backing::new(dir.path(), generation + 100, jailed)?;
        do_update_device(
            &manager,
            &DeviceConfig::BlockCfgModern(BlockConfigModern {
                index,
                path_on_host: replacement.path.to_string_lossy().into_owned(),
                ..Default::default()
            }),
        )
        .await?;
        assert!(!holds_backing_file(pid, &path)?);
        assert!(holds_backing_file(pid, &replacement.path)?);
        backing.release()?;
        manager.write().await.try_remove_device(&device_id).await?;
        assert!(
            !holds_backing_file(pid, &replacement.path)?,
            "VMM must release the removed backing FD"
        );
        assert!(fc.inner.read().await.attached_drives.is_empty());
        let mounts = std::fs::read_to_string("/proc/self/mountinfo")?;
        assert!(!mounts
            .lines()
            .any(|line| line.contains(&vm_path) && line.contains("-backing-")));
        let metadata = fc.inner.read().await;
        let placeholder = if jailed {
            "/drive1".to_owned()
        } else {
            format!("{vm_path}/root/drive1")
        };
        // Firecracker's GET configuration confirms the path, beyond bookkeeping.
        let uri: hyper::Uri = hyperlocal::Uri::new(&metadata.asock_path, "/vm/config").into();
        let request =
            hyper::Request::get(uri).body(http_body_util::Full::new(bytes::Bytes::new()))?;
        let response = metadata.send_request(request).await?;
        use http_body_util::BodyExt;
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await?.to_bytes())?;
        assert!(body["drives"]
            .as_array()
            .context("Firecracker drive config")?
            .iter()
            .any(|drive| drive["drive_id"] == "drive1" && drive["path_on_host"] == placeholder));
        drop(metadata);
        assert_eq!(fc.get_pids().await?, vec![pid]);
        fc.check().await?;
        replacement.release()?;
    }
    fc.inner
        .read()
        .await
        .request_with_retry(
            hyper::Method::PUT,
            "/actions",
            "{\"action_type\":\"FlushMetrics\"}".into(),
        )
        .await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let metrics = fc.get_hypervisor_metrics().await?;
            if metrics.contains("kata_firecracker_vmm") && log_count.load(Ordering::SeqCst) > 0 {
                println!("{metrics}");
                return anyhow::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    // Keep two real backings attached across VMM exit. One is removed through
    // the stopped live object; the other through cleanup-only restored state.
    let crash_checkpoint = fc.save().await?;
    let mut stopped_backing = Backing::new(dir.path(), 1000, jailed)?;
    let mut crash_backing = Backing::new(dir.path(), 1001, jailed)?;
    let mut crash_devices = Vec::new();
    for backing in [&stopped_backing, &crash_backing] {
        crash_devices.push(
            do_handle_device(
                &manager,
                &DeviceConfig::BlockCfgModern(BlockConfigModern {
                    path_on_host: backing.path.to_string_lossy().into_owned(),
                    driver_option: crate::VM_ROOTFS_DRIVER_MMIO.into(),
                    ..Default::default()
                }),
            )
            .await?,
        );
    }
    assert_eq!(guest_command(&fc, "start").await?, "started\n");
    let saved = fc.save().await?;
    let json = serde_json::to_string(&saved)?;
    fc.disconnect().await;
    let recovered = Firecracker::restore((), serde_json::from_str(&json)?).await?;
    recovered.check().await?;
    assert_eq!(recovered.get_pids().await?, vec![pid]);
    assert_eq!(recovered.get_thread_ids().await?.vcpus.len(), 2);
    recovered.stop_vm().await?;
    // Reap the original child after stopping it through the recovered PID.
    fc.inner.read().await.wait_vm().await?;
    assert!(recovered.check().await.is_err());
    assert!(Firecracker::restore((), saved.clone()).await.is_err());
    recovered.remove_device(crash_devices.remove(0)).await?;
    stopped_backing.release()?;
    let cleanup = Firecracker::restore_for_cleanup(crash_checkpoint).await?;
    // This checkpoint predates both attached drives. Cleanup must discover
    // the remaining jailed mount without depending on a fresh device list.
    cleanup.cleanup().await?;
    crash_backing.release()?;
    assert!(!Path::new(&vm_path).exists());
    Ok(())
}

#[tokio::test]
#[ignore = "requires KVM and the Firecracker test artifacts"]
async fn real_firecracker_unjailed() {
    real_vm(false).await.unwrap();
}

#[tokio::test]
#[ignore = "requires KVM, a privileged container, jailer and test artifacts"]
async fn real_firecracker_jailed() {
    real_vm(true).await.unwrap();
}
