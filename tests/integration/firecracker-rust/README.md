# Firecracker Rust runtime CI

This suite covers the Rust Firecracker backend and its lifecycle regressions.
All dependencies, builds and tests run inside containers. Run the Docker wrapper
from the repository root:

```bash
bash ci/run-firecracker-rust-tests.sh unit
bash ci/run-firecracker-rust-tests.sh mounts
bash ci/run-firecracker-rust-tests.sh integration
```

For a Kubernetes CI job, build and preload the same Dockerfile image, mount the
checkout at `/kata`, and run `bash ci/run-firecracker-rust-tests-in-container.sh`
with the same `unit`, `mounts` or `integration` argument. The latter two modes
require a privileged pod. Pin the job to a KVM-enabled node, set resource limits,
and run integration versions sequentially. This exercises the backend inside a
CI pod; it does not register a Kata containerd handler or validate the CRI path.

`unit` checks formatting and clippy, generates the shim configuration through
Kata's Makefile, compiles the Rust runtime binaries and runs Firecracker and
device-manager regressions. It includes real FIFO consumption, Prometheus
encoding, live/exited process health, vCPU thread discovery, save/reload,
startup failure cleanup, API rejection and slot reuse. It also checks that the
device manager rejects concurrent reuse of uncertain attachments and updates,
and that cancelled attach, update and final detach operations retain ownership
until an explicit retry reconciles the drive. Cleanup preparation stops the API
server before network teardown and handles unexpected VMM exit. The suite checks that the
namespace-local network runtime can time out stalled I/O, as required by the
Firecracker API deadlines. Formatting and clippy include the hypervisor,
resource manager, virtual-container runtime and runtime handler crates. It uses the workspace's
default Cargo features; Firecracker has no separate Cargo feature.

`mounts` uses a privileged container for real jailed bind mounts. It covers
successful detach/update, failed API operations, busy-mount rollback and retry.
It also tests uncertain attach ownership, detach after failed update/rollback,
release after VM stop, and cleanup from a stale checkpoint after process exit.
It requires no VM or KVM device.

`integration` requires a Linux Docker host with `/dev/kvm`, loop devices and
device mapper. On the host, load `loop` and `dm_mod` if necessary. A missing KVM
device is an error. The workflow runs this job for both the version configured
in `versions.yaml` and v1.12.1. Unit checks and mount tests run on x86_64 and
aarch64; booted-VM jobs use x86_64 KVM runners.

The integration job downloads the official Firecracker/jailer release and
verifies its published SHA-256 checksum. Guest kernels are pinned to an immutable
Firecracker CI artifact prefix and checked against [kernels.json](kernels.json).
The container builds a small ext4 guest using static BusyBox and
[guest-init.c](guest-init.c). This test-only controller starts a container in
new PID, mount, UTS and IPC namespaces, then stops it and unmounts its rootfs.
Commands use the Firecracker vsock proxy; no external guest access is needed.

Both jailed and unjailed VM tests repeat ten container lifecycles in one running
sandbox. The jailed test uses device-mapper-backed root filesystems over isolated
loop devices; the unjailed test uses raw files. Tests verify rejected updates,
successful backing updates, the VMM's open file descriptors, the actual API
configuration returning to the placeholder, released bind mounts, slot reuse,
FIFO logs, Prometheus metrics, vCPU IDs and process recovery. It leaves two
backings attached across VMM exit and verifies stopped-device detach plus
cleanup-only restoration from an earlier checkpoint. Device-mapper
removal must succeed without deferred removal while the VMM remains alive.
Fixture cleanup uses deferred removal only after a failed test, followed by
automatic loop-device cleanup when Docker reaps the test VMM.

The guest container fixture exercises Kata's Rust hypervisor and device manager
directly. It does not install containerd or exercise the CRI shim protocol.

Results are saved under `target/firecracker-rust/results/`; downloaded artifact
URLs, hashes, Kata revision and Rust version are recorded in
`target/firecracker-rust/artifacts/provenance.json`. GitHub Actions uploads the
logs and provenance on success or failure. Set `FC_TEST_VERSION=v1.12.1` to select
that release locally. The workflow's four job names are required by Gatekeeper.

For a host that has device mapper but no KVM, the fixture itself can be checked
after preparing artifacts inside the image:

```bash
docker run --rm -v "$PWD:/kata" kata-firecracker-rust-ci:local \
  python3 tests/integration/firecracker-rust/prepare.py
docker run --rm --privileged -v "$PWD:/kata" \
  -e FC_TEST_ROOTFS=/kata/target/firecracker-rust/artifacts/rootfs.ext4 \
  kata-firecracker-rust-ci:local \
  cargo test --locked -p hypervisor \
  firecracker::integration_tests::devmapper_fixture_lifecycle -- --ignored --nocapture
```

## Native Kubernetes validation

[`native-kubernetes.py`](native-kubernetes.py) adds node-local CRI and Kata agent
coverage after the container CI passes. Run it as root on a dedicated KVM node
with `kubectl`, `crictl`, `journalctl`, and access to its Kubernetes API. Configure
two private containerd handlers using the CI-built Rust shim, the devmapper
snapshotter and the packaged Firecracker/kernel/Kata agent image. Their names
must be `<prefix>` (jailed) and `<prefix>-unjailed`. Enable
`static_sandbox_resource_mgmt = true` in both Rust runtime configurations;
Firecracker cannot hotplug CPUs or memory. This opt-in harness does not install
handlers or change the node image.

Supply JSON templates for a RuntimeClass and a single-container BusyBox pod.
The pod template must specify the test namespace and node, no service account
token or host volumes, a 2-CPU/128-MiB container limit, and this command:
`["/bin/sh", "-c", "uname -a; echo native-kata-firecracker-rust-ready; exec sleep 3600"]`.
Set the RuntimeClass overhead to 512 MiB so host shim/VMM overhead has room
without increasing the 128-MiB guest. Set both hypervisor configurations to two
vCPUs and 128 MiB, enable debug logging, use Writeback block cache, and configure
receive/transmit network limiters. Apply any node-specific selectors/tolerations
to the templates. Annotate the dedicated namespace with an ownership marker.

```bash
sudo python3 tests/integration/firecracker-rust/native-kubernetes.py \
  --namespace firecracker-rust-ci --node "$(hostname)" \
  --handler-prefix kata-firecracker-rust-ci \
  --expected-shim /opt/kata-rust-ci/containerd-shim-kata-v2 \
  --pod-template /tmp/firecracker-rust-pod.json \
  --runtimeclass-template /tmp/firecracker-rust-runtimeclass.json \
  --owner-annotation example.org/ci-task=firecracker-rust \
  --output /tmp/firecracker-rust-native-results
```

The harness creates and deletes ten pods per variant, one VM at a time. It
checks the VMM's parent executable to prove the Rust shim was used, then verifies
guest execution, rootfs writes, DNS, machine sizing, vCPU threads, cache/limiter
configuration, native shim Prometheus metrics and forwarded Firecracker FIFO
logs. It also requires VMM exit and VM-directory removal after each deletion.
Results and per-pod evidence are retained, including failure descriptions.
`--cycles`, `--dns-name`, `--kubeconfig` and `--expected-serial` customize the run.
Use a fully qualified DNS name with BusyBox. Add `--crash-cleanup` to run one
two extra lifecycles per variant: freeze the verified task shim, delete its Pod
to prevent replacement, then kill the VMM through an identity-checked pidfd.
One lifecycle resumes the shim to exercise unexpected VMM exit; the other also
kills the shim to exercise containerd's separate delete/cleanup path. Both must
release the VM and sandbox directories. This requires permission to signal
those task processes. Remove the two test RuntimeClasses,
namespace and temporary containerd handlers after collecting evidence.

See the [implementation comparison and validation record](../../../docs/design/firecracker-rust-parity.md).
