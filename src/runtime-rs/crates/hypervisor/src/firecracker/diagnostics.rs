// Copyright (c) 2026 Kata Contributors
// SPDX-License-Identifier: Apache-2.0

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use prometheus::{GaugeVec, Opts, Registry, TextEncoder};
use serde_json::{json, Value};
use tokio::{io::unix::AsyncFd, sync::Mutex as AsyncMutex, task::JoinHandle};

use super::{sl, FcInner};

pub(super) const LOG_FIFO: &str = "logs.fifo";
pub(super) const METRICS_FIFO: &str = "metrics.fifo";
const MAX_LINE: usize = 1024 * 1024;

#[derive(Debug, Default)]
pub(super) struct Diagnostics {
    pub(super) metrics: Arc<Mutex<Option<Value>>>,
    tasks: AsyncMutex<Vec<JoinHandle<Result<()>>>>,
}

impl Diagnostics {
    pub(super) async fn stop(&self) {
        for task in self.tasks.lock().await.drain(..) {
            task.abort();
            let _ = task.await;
        }
    }

    fn reader(&self, path: &str, metrics: bool, create: bool) -> Result<JoinHandle<Result<()>>> {
        if create {
            nix::unistd::mkfifo(
                path,
                nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
            )
            .with_context(|| format!("create Firecracker FIFO {path}"))?;
        }
        let meta = std::fs::symlink_metadata(path)?;
        if !meta.file_type().is_fifo() {
            bail!("Firecracker diagnostic path is not a FIFO: {path}");
        }
        // O_RDWR keeps the FIFO alive before the VMM opens its writer. O_NONBLOCK
        // plus AsyncFd makes cancellation independent of a writer and avoids a
        // blocked Tokio filesystem worker on startup failures or shim recovery.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)?;
        let fd = AsyncFd::new(file)?;
        let snapshot = self.metrics.clone();
        let path = path.to_owned();
        Ok(tokio::spawn(async move {
            let result = consume(fd, metrics, snapshot).await;
            if let Err(error) = &result {
                error!(sl(), "Firecracker FIFO reader failed"; "path" => path, "error" => error.to_string());
            }
            result
        }))
    }
}

impl Drop for Diagnostics {
    fn drop(&mut self) {
        for task in self.tasks.get_mut().drain(..) {
            task.abort();
        }
    }
}

async fn consume(
    fd: AsyncFd<File>,
    metrics: bool,
    snapshot: Arc<Mutex<Option<Value>>>,
) -> Result<()> {
    let mut line = Vec::new();
    let mut dropping = false;
    loop {
        let mut ready = fd.readable().await?;
        let mut buffer = [0u8; 8192];
        let len = match ready.try_io(|fd| {
            let mut file = fd.get_ref();
            file.read(&mut buffer)
        }) {
            Ok(result) => result?,
            Err(_) => continue,
        };
        if len == 0 {
            return Ok(());
        }
        for byte in &buffer[..len] {
            if *byte == b'\n' {
                if !dropping {
                    if metrics {
                        match serde_json::from_slice::<Value>(&line) {
                            Ok(value) if value.is_object() => {
                                *snapshot.lock().unwrap() = Some(value)
                            }
                            _ => warn!(sl(), "invalid Firecracker metrics record"),
                        }
                    } else {
                        info!(sl(), "Firecracker log"; "message" => String::from_utf8_lossy(&line).to_string());
                    }
                }
                line.clear();
                dropping = false;
            } else if !dropping {
                if line.len() == MAX_LINE {
                    warn!(sl(), "oversized Firecracker FIFO record");
                    line.clear();
                    dropping = true;
                } else {
                    line.push(*byte);
                }
            }
        }
    }
}

impl FcInner {
    pub(super) async fn prepare_diagnostics(&mut self, recovering: bool) -> Result<()> {
        for (name, metrics) in [(LOG_FIFO, false), (METRICS_FIFO, true)] {
            let host_path = format!("{}/{name}", self.vm_path);
            let task = self.diagnostics.reader(&host_path, metrics, !recovering)?;
            self.diagnostics.tasks.lock().await.push(task);
            if !recovering {
                let path = self.get_resource(&host_path, name)?;
                let (endpoint, body) = if metrics {
                    ("/metrics", json!({"metrics_path": path}))
                } else {
                    (
                        "/logger",
                        json!({"log_path": path, "level": if self.config.debug_info.enable_debug {"Debug"} else {"Info"}, "show_level": true, "show_log_origin": true}),
                    )
                };
                self.request_with_retry(hyper::Method::PUT, endpoint, body.to_string())
                    .await?;
            }
        }
        Ok(())
    }
}

/// Use the same namespace and group/item gauges as the Go integration. A local
/// registry avoids cross-sandbox registration and exports through the runtime's
/// existing get_hypervisor_metrics -> Prometheus endpoint.
pub(super) fn encode_metrics(snapshot: &Value) -> Result<String> {
    let registry = Registry::new();
    if let Some(groups) = snapshot.as_object() {
        for (group, value) in groups {
            if !group
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                continue;
            }
            if !value.is_object() {
                continue;
            }
            let gauge = GaugeVec::new(
                Opts::new(
                    format!("kata_firecracker_{group}"),
                    format!("Firecracker {group} metrics."),
                ),
                &["item"],
            )?;
            collect_metrics(&gauge, "", value);
            registry.register(Box::new(gauge))?;
        }
    }
    Ok(TextEncoder::new().encode_to_string(&registry.gather())?)
}

fn collect_metrics(gauge: &GaugeVec, prefix: &str, value: &Value) {
    if let Some(fields) = value.as_object() {
        for (name, value) in fields {
            let item = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}.{name}")
            };
            if let Some(value) = value.as_f64() {
                gauge.with_label_values(&[&item]).set(value);
            } else if value.is_object() {
                collect_metrics(gauge, &item, value);
            }
        }
    }
}
