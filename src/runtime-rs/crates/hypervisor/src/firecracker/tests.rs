// Copyright (c) 2026 Kata Contributors
// SPDX-License-Identifier: Apache-2.0
use super::{process, FcInner};
use std::path::Path;
use tokio::sync::mpsc;

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
    let dir = tempfile::tempdir().unwrap();
    let mut fc = FcInner::new(mpsc::channel(1).0);
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
