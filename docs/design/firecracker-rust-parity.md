# Firecracker Rust parity implementation

The comparison below uses Go and Rust from that same recorded revision:

| Capability | Go implementation | Previous Rust behavior | Implemented Rust behavior |
| --- | --- | --- | --- |
| Block removal and update | `fc.go`: patch removed drives to their pre-created placeholder and release the jailed mount. | `remove_device` and `update_device` returned success without changing the VMM. | Keep placeholders separate from backing mounts; PATCH first, then release the original mount. Updates stage a distinct backing mount, retain block rate limits, roll back busy mounts and preserve slot ownership on failures. The device manager commits a backing change only after hypervisor success. |
| VMM metrics | `fc.go`, `fc_metrics.go`: consume a metrics FIFO and export `kata_firecracker` group/item gauges. | Returned an empty metrics string. | Configure `/metrics`, consume complete JSON FIFO records and export group/item gauges through the existing runtime metrics interface. Keep the last valid sample after malformed input. |
| VMM logging | `fc.go`: configure the dedicated logger and consume its FIFO. | Only stderr was read. | Configure `/logger`, consume the dedicated FIFO and retain stderr. Readers use nonblocking descriptors and cancellable Tokio tasks, including before the VMM opens a writer. Startup failures stop/reap the VMM, close readers and clean the jail. |
| Health and vCPU identification | `fc.go`: probe the VMM PID and enumerate `fc_vcpu` threads. | Health always succeeded and the thread map was empty. | Reject missing, exited, zombie or reused processes; enumerate individual vCPU thread IDs from `/proc`. Derive process exit notification from process state rather than stderr closure. |
| Version validation | `fc.go`: parse the configured binary's version and require at least `0.21.1`. | No version check. | Run the configured binary with `--version`, enforce the same semantic-version minimum and report malformed output, command failure, unreadable paths and timeout. |
| Network limits and block cache | `fc.go`: configure receive/transmit limiters and placeholder cache mode. | Omitted both; block PATCH already supported disk limits. | Convert network rates using Go's bit-to-byte and decimal-to-binary scaling, set receive/transmit bandwidth token buckets and apply `Writeback`/`Unsafe` to placeholders when configured. Preserve disk bandwidth/operations limits and bursts on PATCH. |
| Process recovery | `fc.go`: save/load the VMM PID. | Restore discarded the PID and initialized `NotReady`. | Persist PID, process start time, host boot ID, API socket, running/ready state, active backing mounts and pending mount cleanup. Reopen existing FIFOs, restore capabilities and monitor the existing process. Reject absent or exited saved processes and identity mismatches; use a `pidfd` when stopping a recovered process. |

Source references:
[Go Firecracker](https://github.com/kata-containers/kata-containers/blob/aab745271b496f999d65bb158c6cd8417616b77b/src/runtime/virtcontainers/fc.go),
[Go metrics](https://github.com/kata-containers/kata-containers/blob/aab745271b496f999d65bb158c6cd8417616b77b/src/runtime/virtcontainers/fc_metrics.go),
[previous Rust backend](https://github.com/kata-containers/kata-containers/tree/aab745271b496f999d65bb158c6cd8417616b77b/src/runtime-rs/crates/hypervisor/src/firecracker).

Two supporting bugs also needed correction: queued devices were never applied
before VM start, and reusing a released device index advanced the fresh-index
counter. Firecracker now reserves index zero for its guest image so the first
container drive maps to `/dev/vdb`. The existing five-slot container pool is
unchanged, and out-of-range requests fail explicitly. Other hypervisors keep
their existing initial block index and reject the newly exposed block-backing
update operation unless they opt in.

Native containerd validation exposed two additional integration requirements.
The resource manager's namespace-local Tokio runtime must enable timers for
Firecracker API deadlines; a regression verifies a stalled TCP read times out.
Firecracker also declares that it owns its boot image, so generic sandbox setup
does not attach the same read-only image again as a container drive. Cleanup of
a prepared, unstarted API server now stops and reaps it; cleanup still rejects a
live running VM until it has been explicitly stopped. Network teardown after
VM stop removes the host TAP without attempting unsupported NIC hot-unplug;
removal from a prepared or running VMM still fails explicitly. Native runtime
configurations use Firecracker's supported static resource sizing mode.

An unsuccessful operation does not release a slot prematurely. Client validation rejections
preserve the original mapping. If a transport failure leaves a PATCH outcome
uncertain and rollback cannot be confirmed, the backend retains the slot and
mounts for reconciliation/removal or final cleanup. It never reports that
operation as successful. Empty backing mount targets may remain until final
cleanup if unlink fails after a successful unmount; the host backing storage has
already been released in that case.

Recovery is process-state recovery, not a VM snapshot. It requires a live saved
process on the same boot with matching start time and the existing VM directory.
Older Rust state files that lack a PID/process identity cannot recover a VMM and
now return an explicit error. Recovered processes are not children of the new
shim, so their exit code is unavailable and recovery reports zero after exit,
matching the existing wrapper's fallback behavior. Stopping a recovered process
requires Linux `pidfd` support.

Read-only container drives are explicitly rejected because the existing
placeholder pool is writable and Firecracker cannot change that property with
PATCH. The guest boot image remains read-only. Adding read-only container slots
is a separate capability; silently exposing such a drive as writable is unsafe.
VM snapshots, pause/resume, and live CPU/memory resizing remain outside this work.

Review follow-up: cleanup restoration is separate from live recovery. The
containerd delete path can restore resource ownership after confirmed process
exit without opening FIFOs or retaining a recyclable PID. A mismatched live
PID still fails. Cleanup runtime settings are checkpointed alongside the active
hypervisor settings, so the separate delete process needs neither the original
containerd options nor a default runtime configuration file. Agent credentials
are not copied into this additional checkpoint data. Firecracker checkpoints are stored at the generic sandbox
path read by containerd, and final jail cleanup discovers generated backing
mounts added after that checkpoint.

Uncertain initial attaches retain their device-manager handle, reference count
and allocated index. Later device creation retries their detach; unreconciled
slots stay reserved and cannot be reported as successful shared attachments.
Updates retain the canonical old backing until success. Detach PATCHes first,
then reaps only that slot's retired mounts. Confirmed VM exit permits backing
cleanup without an API request; failed unmounts still retain ownership for retry.

## Lifecycle review follow-up

Device lookup, attach and the returned device snapshot execute under one manager
write lock. Uncertain initial attaches cannot become successful shared handles.
Uncertain updates retain the existing users and block new sharing until an
explicit update or final detach reconciles the backing. Creating an unrelated
device only retries failed initial attaches, never an update with live users.

The manager records pending block operations before awaiting the backend. The
backend retains candidate backing mounts before sending PATCH, including when
its future is cancelled. Final detach retains its last reference until backend
removal completes and commits the count and returned index together. Backend
block removal is idempotent so a cancelled caller can retry a completed detach.
These invariants are exercised by concurrent and cancelled-request regressions,
including real jailed bind mounts.

Before resource teardown, VirtSandbox invokes the hypervisor cleanup preparation
hook. Firecracker stops a prepared API server before network teardown and moves
an exited VMM to NotReady even if exit occurred before stop_vm. Running guests
remain guarded against cleanup. Native Kubernetes crash validation includes both
unexpected VMM exit with a surviving shim and simultaneous VMM/shim exit.
