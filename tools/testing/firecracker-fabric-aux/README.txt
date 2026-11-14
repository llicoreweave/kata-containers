Experimental Firecracker full-fabric validation helper

This optional, standalone device plugin is the node-scoped helper used to
validate the companion Kata/Firecracker NUMA/PCI branches on an eight-H100 host.
It is not a production allocator. Its node identity and PCI inventory are fixed
in main.go and publish-cdi.py. Review those values before using another host.
It makes no changes to Kata runtime policy or the Firecracker VMM.

GPU ownership stays with the existing NVIDIA sandbox device plugin through
nvidia.com/gpu. This helper advertises one lli.coreweave.com/fabric-aux unit
containing eight NIC functions and four NVSwitch functions, with zero GPUs.
The workload requests nvidia.com/gpu: 8 and lli.coreweave.com/fabric-aux: 1,
with equal extended-resource requests and limits.

The host must already be prepared for VFIO by its node image/payload. Native
NVIDIA consumers, including DCGM, must be drained before converting a live host.
This helper neither unbinds native drivers nor stops services or workloads.
Respect any existing NVSwitch/Fabric Manager ownership. Independent extended
resource names do not provide mutual exclusion with another switch allocator.

Setup contract:
1. Prepare and exclusively reserve the physical devices for the experiment.
2. Run publish-cdi.py as root on the configured host. It validates the twelve
   auxiliary identities, VFIO bindings and singleton IOMMU groups, then writes
   /etc/cdi/lli-havock2250-fabric-aux.json atomically. It includes no GPU handles.
3. Run the plugin on that host with NODE_NAME set to the configured deviceID.
   Mount /var/lib/kubelet/device-plugins read/write, host /sys at /host/sys
   read-only, and host /dev/vfio at /host/dev/vfio read-only. The plugin needs
   only metadata access to its read-only host mounts.
4. Allocate the normal GPU resource and the auxiliary resource to the same pod.
   Kata resolves their CDI references before VM creation.

IOMMU group numbers are resolved from stable PCI BDFs, not hard-coded. The
Allocate response contains only the stable CDI reference, because kubelet may
cache that response across restart. Regenerate the CDI file when the host's
VFIO group identities change. Allocation cannot be used to acquire extra GPUs.

Build and test inside a Linux container using the Go version in go.mod:
  go test -v ./...
  python3 -m unittest -v test_publish_cdi
  CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -o fabric-aux-device-plugin .

Five regression tests cover the GPU-free inventory, missing VFIO state,
IOMMU group renumbering, shared-group rejection and a stable cached CDI response.
The Go sources are copied unchanged from the helper tested on hardware.
Four Python fixture tests validate JSON publication, GPU substitution rejection,
shared-group rejection and protection of an existing CDI mapping.

Validation: Kata runtime ae1e1cf1194d3e5be639395b3f96779b96b8e060 and Firecracker
f646783893717c22c7946e614027a3a3b05d59fe booted 64 vCPUs / 64 GiB in two NUMA
domains with 20 assigned PCI functions. After a host reboot and manual restaging,
CUDA passed on 8/8 GPUs, CPU-memory RDMA on 8/8 NICs and GPU-memory RDMA write/read
on all 64 GPU/NIC pairs, with CUDA checking the received bytes. Transfers were
2 MiB each way over local RC loopback, not remote-host throughput benchmarks.
Host affinity and physical 32-GiB RAM placement per NUMA node were also verified.

The test did not demonstrate automatic cloud reboot recovery. Node provisioning
cleared the locally staged runtime, guest assets and image cache. Cloud delivery
requires a versioned node image/payload, registry images and virtualization host
configuration. The abandoned local-file bootstrap experiment is not included.
