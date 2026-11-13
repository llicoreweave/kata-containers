# NUMA-aware PCI passthrough with Firecracker

The Firecracker runtime builds the complete cold-boot configuration from the
pod's allocated devices and host topology before launching the VMM. No separate
CoreWeave configuration generator is required. A device plugin still allocates
the physical devices and supplies their CDI specifications.

This integration requires the companion Firecracker NUMA/PCI implementation in
[`llicoreweave/firecracker`, branch `havock-2250-numa-pci`](https://github.com/llicoreweave/firecracker/tree/havock-2250-numa-pci).
It uses that implementation's `numa` and `vfio[].pci_path` configuration fields;
it is not compatible with an unmodified upstream Firecracker binary.

## Runtime configuration

Use a dedicated containerd handler and RuntimeClass with a compatible guest
kernel/image, Firecracker binary, jailer, and block-device snapshotter. Add:

```toml
[hypervisor.firecracker]
enable_numa = true
cold_plug_vfio = "switch-port"
hot_plug_vfio = "no-port"
enable_hugepages = false

[runtime]
pod_resource_api_sock = "/var/lib/kubelet/pod-resources/kubelet.sock"
static_sandbox_resource_mgmt = true
vfio_mode = "guest-kernel"
```

Without `numa_mapping`, the runtime selects the permitted host nodes containing
the allocated devices. It adds other permitted nodes only if needed for vCPU
capacity. A CPU-only guest uses the first sufficient set of permitted nodes.
Host node IDs are sorted for deterministic guest domain numbering.

To preserve an explicit domain order and include empty PCI domains, add:

```toml
[hypervisor.firecracker]
numa_mapping = ["0", "1"]
```

Each mapping entry must identify one host NUMA node. An explicit domain that
cannot supply permitted local CPUs and memory is rejected rather than silently
removed. NUMA remains disabled unless `enable_numa` or an explicitly permitted
NUMA mapping annotation selects it. Existing flat PCI and MMIO configurations
remain available without NUMA.

## Allocation, planning and launch

1. The device plugin advertises and allocates GPU/NIC resources and publishes
   CDI entries for their VFIO device nodes. Bind devices to `vfio-pci` before
   allocation. The runtime never claims a nearby, unallocated device.
2. During sandbox creation, the shim queries kubelet's PodResources API once
   for the pod. It collects device IDs across containers and resolves CDI
   entries before starting the VM. For NUMA Firecracker guests, explicit
   CPUManager CPU assignments and MemoryManager node assignments from that
   snapshot are retained as placement restrictions.
3. The Firecracker runtime intersects host node CPU membership with the
   process's effective CPU affinity, permitted memory-node mask, OCI sandbox
   CPU/node restrictions, and any explicit kubelet assignments. It does not
   widen those restrictions. Without an explicit kubelet assignment, the
   process and OCI restrictions still apply.
4. It resolves allocated devices' sysfs ancestry and `numa_node`, reconstructs
   their shared bridge prefixes, and assigns deterministic guest root-port
   slots. Every PCI subtree must belong to one selected host NUMA node.
5. Each selected domain receives at least one vCPU and 2 MiB. Additional vCPUs
   are balanced against permitted CPU capacity. Remaining RAM is apportioned
   by vCPU count in 2-MiB units, with exact CPU/RAM totals. CPU or RAM totals do
   not need to divide evenly by the domain count.
6. Host NUMA distances, explicit vCPU affinity, RAM placement, root ownership,
   and device paths are serialized into the VMM configuration. The runtime
   checks PCI path conflicts, root-slot capacity, the 64-bus budget, and the
   agent's ability to represent each guest root bus before publishing the plan.
7. Kata's guest device paths and final NUMA translation use the same plan.
   The complete `fcConfig.json` is written to a private temporary file and
   renamed atomically into place before the jailer/Firecracker is launched.
   The jailer receives the assigned `--vfio-device` paths, selected `--numa-node`
   IDs, and RAM memlock requirement from this configuration.

The VMM reads `/fcConfig.json` inside the jail through `--config-file`, with
`--enable-pci`. It validates host locality again, binds RAM before first touch,
pins vCPUs, creates the static PCI hierarchy, and emits ACPI SRAT/SLIT and PCI
host-bridge `_PXM` entries. The guest kernel discovers NUMA through ACPI.

Planning errors do not publish partial VMM or agent device paths. A missing
NUMA ID, conflicting switch ownership, disjoint resource mask, insufficient
CPU capacity, or exhausted PCI budget fails startup before spawning the VMM.

## Operational constraints

- This is x86_64 cold-boot support: up to 64 vCPUs, 64 PCI buses, host NUMA node
  IDs below 64, and PCI paths containing 2–8 function-zero slots.
- Guest bus numbers are allocated independently of host BDFs. The topology
  reproduces the assigned devices' bridge relationships and NUMA locality;
  it does not clone physical bridge capabilities or link performance.
- Kata's PCI path representation limits roots containing assigned endpoints
  to guest bus numbers `00..1f`; the planner rejects an unrepresentable root.
- Device plugins remain responsible for exclusive device allocation, health,
  reset and release. A shared host RDMA resource is not a VFIO NIC allocation.
  GPU/NIC pairing must be enforced by the allocation layer if required.
- Respect existing NVSwitch/Fabric Manager ownership. Do not include a switch
  already assigned to a separate management VM in a tenant's device list.
- vCPU pinning does not reserve cores exclusively. VM CPU/RAM sizing includes
  Kata's runtime baseline; explicit kubelet CPU assignments must cover every
  pinned vCPU. Memory-node restrictions are enforced, but this planner does
  not implement per-node MemoryManager byte quotas or reserve additional RAM.
- Guest RAM uses anonymous memory, not a hugetlbfs backend. Device hotplug,
  RAM hotplug/reclaim, NUMA snapshots and guest PCI bus renumbering are outside
  this implementation. Use static sandbox resource sizing.

## Source and validation

The planner lives in `src/runtime/virtcontainers/fc_topology.go`,
`fc_numa_host.go`, `fc_numa.go` and `fc_vfio.go`; configuration publication is
in `fc_config.go`. The shim's allocation handoff is in
`src/runtime/pkg/containerd-shim-v2/device_cold_plug.go`.

Regression tests cover sparse host node IDs, uneven CPU/RAM totals, CPU and
memory restrictions, missing locality, incompatible PCI paths, bus limits,
transactional planning, atomic file replacement, and guest-agent mappings.
The dual-NUMA H100 fabric fixture compares all 20 assigned endpoints and the
64-vCPU/64-GiB plan against the previously verified guest configuration.
