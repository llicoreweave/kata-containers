// Copyright (c) 2026 Kata Contributors
// SPDX-License-Identifier: Apache-2.0
use super::diagnostics::LOG_FIFO;
use super::{process, FcInner};
use crate::VmmState;
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
    #[allow(dead_code)]
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
    let mut logs = OpenOptions::new()
        .write(true)
        .open(dir.path().join(LOG_FIFO))
        .unwrap();
    logs.write_all(b"first VMM log\nsecond VMM log\n").unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while count.load(Ordering::SeqCst) != 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    drop(logs);
    fc.diagnostics.stop().await;
    let fds: Vec<_> = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .collect();
    assert!(!fds.contains(&dir.path().join(LOG_FIFO)));
    assert_eq!(api.requests.lock().unwrap()[0].1, "/logger");
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
