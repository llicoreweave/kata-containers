//go:build linux

// SPDX-License-Identifier: Apache-2.0

package virtcontainers

import (
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"

	"github.com/kata-containers/kata-containers/src/runtime/pkg/device/config"
	"github.com/kata-containers/kata-containers/src/runtime/virtcontainers/pkg/cpuset"
	"github.com/kata-containers/kata-containers/src/runtime/virtcontainers/types"
)

// fcTopologySource snapshots only topology metadata. Planning never opens a
// VFIO device or allocates a device that was not already passed to this VM.
type fcTopologySource struct {
	readFile func(string) ([]byte, error)
	resolve  func(string) (string, error)
}

func (fc *firecracker) setSandboxResourceConstraints(cpus, mems string) {
	fc.sandboxCPUs, fc.sandboxMems = cpus, mems
}

// The agent must use the final guest domain order and CPU assignments, including
// automatic node selection, rather than the initial host discovery snapshot.
func (fc *firecracker) guestNUMATopology() []types.GuestNUMANode {
	if fc.fcConfig == nil || fc.fcConfig.NUMA == nil {
		return nil
	}
	nodes := make([]types.GuestNUMANode, len(fc.fcConfig.NUMA.Nodes))
	for i, node := range fc.fcConfig.NUMA.Nodes {
		cpus := cpuset.NewBuilder()
		for _, cpu := range node.HostCPUs {
			cpus.Add(int(cpu))
		}
		nodes[i] = types.GuestNUMANode{HostNodes: strconv.FormatUint(uint64(node.HostNode), 10), HostCPUs: cpus.Result().String()}
	}
	return nodes
}

func (fc *firecracker) configureTopology() error {
	return fc.configureTopologyWith(fcTopologySource{os.ReadFile, filepath.EvalSymlinks})
}

// Build both the VMM configuration and the agent's device paths on copies.
// Invalid host metadata must not publish a partially updated configuration.
func (fc *firecracker) configureTopologyWith(host fcTopologySource) error {
	if fc.fcConfig == nil {
		return fmt.Errorf("Firecracker configuration is not initialized")
	}
	if fc.pciEnabled() && (fc.config.NumVCPUs() == 0 || fc.config.NumVCPUs() > 64) {
		return fmt.Errorf("Firecracker PCI requires 1..64 vCPUs")
	}
	cfg := *fc.fcConfig
	cfg.NUMA = nil
	cfg.PCITopology = nil
	cfg.VFIO = append([]types.FcVFIODevice(nil), fc.fcConfig.VFIO...)
	for i := range cfg.VFIO {
		cfg.VFIO[i].PCIPath = append([]uint32(nil), cfg.VFIO[i].PCIPath...)
	}
	plan := firecracker{
		config: fc.config, fcConfig: &cfg,
		sandboxCPUs: fc.sandboxCPUs, sandboxMems: fc.sandboxMems,
	}
	for _, device := range fc.vfioDevices {
		if device == nil {
			return fmt.Errorf("nil Firecracker VFIO device")
		}
		copy := *device
		plan.vfioDevices = append(plan.vfioDevices, &copy)
	}
	if err := plan.resolveVFIOPathsWith(host.resolve); err != nil {
		return fmt.Errorf("planning Firecracker PCI topology: %w", err)
	}
	if err := plan.configureNUMAFrom(host.readFile); err != nil {
		return fmt.Errorf("planning Firecracker NUMA topology: %w", err)
	}
	fc.fcConfig.VFIO, fc.fcConfig.NUMA, fc.fcConfig.PCITopology = cfg.VFIO, cfg.NUMA, cfg.PCITopology
	for i, device := range plan.vfioDevices {
		fc.vfioDevices[i].GuestPciPath = device.GuestPciPath
	}
	return nil
}

func (fc *firecracker) resolveVFIOPathsWith(resolve func(string) (string, error)) error {
	if fc.config.ColdPlugVFIO == config.SwitchPort {
		return fc.resolveStaticVFIOPathsWith(resolve)
	}
	return fc.resolveVFIOPaths()
}

// Kata owns the guest graph: bus numbers, port types, forwarding ranges,
// endpoint addresses, and root proximity domains are serialized before launch.
type fcPCIPlanResult struct {
	topology  *types.FcPCITopology
	addresses []types.FcPCIAddress
	paths     []types.PciPath
}

func fcGuestPCIPaths(devices []types.FcVFIODevice, numa *types.FcNUMAConfig) ([]types.PciPath, error) {
	plan, err := fcPlanPCITopology(devices, numa)
	if err != nil {
		return nil, err
	}
	return plan.paths, nil
}

func fcPlanPCITopology(devices []types.FcVFIODevice, numa *types.FcNUMAConfig) (*fcPCIPlanResult, error) {
	parents := map[string]bool{}
	leaves := map[string]bool{}
	ids := map[string]bool{}
	for _, dev := range devices {
		if dev.ID == "" || ids[dev.ID] || len(dev.PCIPath) < 2 || len(dev.PCIPath) > 8 {
			return nil, fmt.Errorf("invalid or duplicate static PCI device %q", dev.ID)
		}
		ids[dev.ID] = true
		for i, slot := range dev.PCIPath {
			if slot >= 32 || (i == 0 && slot == 0) {
				return nil, fmt.Errorf("invalid PCI slot in device %s", dev.ID)
			}
		}
		key := fcPCIPathKey(dev.PCIPath)
		if leaves[key] {
			return nil, fmt.Errorf("duplicate static PCI path for %s", dev.ID)
		}
		leaves[key] = true
		for n := 1; n < len(dev.PCIPath); n++ {
			parents[fcPCIPathKey(dev.PCIPath[:n])] = true
		}
	}
	for key := range parents {
		if leaves[key] {
			return nil, fmt.Errorf("PCI endpoint is also used as a bridge")
		}
	}
	domains := []types.FcNUMANode{{}}
	if numa != nil {
		domains = numa.Nodes
	}
	if len(domains) == 0 || len(domains)+len(parents) > 64 {
		return nil, fmt.Errorf("Firecracker PCI topology exhausts 64 buses")
	}
	rootDomain := map[uint32]int{}
	for domain, node := range domains {
		for _, slot := range node.PCIRootSlots {
			if _, exists := rootDomain[slot]; exists || slot == 0 || slot >= 32 {
				return nil, fmt.Errorf("invalid or duplicate NUMA PCI root slot %d", slot)
			}
			rootDomain[slot] = domain
		}
	}
	paths := make([]types.PciPath, len(devices))
	addresses := make([]types.FcPCIAddress, len(devices))
	topology := &types.FcPCITopology{Roots: []types.FcPCIRoot{}, Bridges: []types.FcPCIBridge{}}
	nextBus := 0
	for domain := range domains {
		if domain > 0 {
			nextBus++
		}
		rootBus := nextBus
		subtrees := map[string][]uint32{}
		for i, dev := range devices {
			if numa != nil {
				owner, exists := rootDomain[dev.PCIPath[0]]
				if !exists {
					return nil, fmt.Errorf("device %s has no NUMA PCI root", dev.ID)
				}
				if owner != domain {
					continue
				}
			}
			slots := make([]types.PciSlot, len(dev.PCIPath)+1)
			var err error
			slots[0], err = types.PciSlotFromInt(rootBus)
			if err != nil {
				return nil, fmt.Errorf("guest root bus %02x cannot be represented by Kata PCI paths: %w", rootBus, err)
			}
			for j, slot := range dev.PCIPath {
				slots[j+1], err = types.PciSlotFromInt(int(slot))
				if err != nil {
					return nil, err
				}
			}
			paths[i], err = types.PciPathFromSlots(slots...)
			if err != nil {
				return nil, err
			}
			for n := 1; n < len(dev.PCIPath); n++ {
				subtrees[fcPCIPathKey(dev.PCIPath[:n])] = dev.PCIPath[:n]
			}
		}
		prefixes := make([][]uint32, 0, len(subtrees))
		for _, prefix := range subtrees {
			prefixes = append(prefixes, prefix)
		}
		sort.Slice(prefixes, func(i, j int) bool {
			a, b := prefixes[i], prefixes[j]
			for n := 0; n < len(a) && n < len(b); n++ {
				if a[n] != b[n] {
					return a[n] < b[n]
				}
			}
			return len(a) < len(b)
		})
		buses := map[string]uint32{}
		for _, prefix := range prefixes {
			nextBus++
			buses[fcPCIPathKey(prefix)] = uint32(nextBus)
		}
		root := types.FcPCIRoot{Bus: uint32(rootBus), LastBus: uint32(nextBus)}
		if numa != nil {
			id := uint32(domain)
			root.NUMANode = &id
		}
		topology.Roots = append(topology.Roots, root)
		for _, prefix := range prefixes {
			primary := uint32(rootBus)
			if len(prefix) > 1 {
				primary = buses[fcPCIPathKey(prefix[:len(prefix)-1])]
			}
			secondary := buses[fcPCIPathKey(prefix)]
			subordinate := secondary
			for key, bus := range buses {
				if strings.HasPrefix(key, fcPCIPathKey(prefix)) && bus > subordinate {
					subordinate = bus
				}
			}
			kind := "root-port"
			if len(prefix) > 1 {
				if len(prefix)%2 == 0 {
					kind = "upstream-port"
				} else {
					kind = "downstream-port"
				}
			}
			topology.Bridges = append(topology.Bridges, types.FcPCIBridge{PCIAddress: types.FcPCIAddress{Bus: primary, Device: prefix[len(prefix)-1]}, SecondaryBus: secondary, SubordinateBus: subordinate, Type: kind})
		}
		for i, dev := range devices {
			if numa != nil && rootDomain[dev.PCIPath[0]] != domain {
				continue
			}
			addresses[i] = types.FcPCIAddress{Bus: buses[fcPCIPathKey(dev.PCIPath[:len(dev.PCIPath)-1])], Device: dev.PCIPath[len(dev.PCIPath)-1]}
		}
	}
	return &fcPCIPlanResult{topology: topology, addresses: addresses, paths: paths}, nil
}

func fcPCIPathKey(path []uint32) string {
	var key strings.Builder
	for _, slot := range path {
		key.WriteString(strconv.FormatUint(uint64(slot), 16))
		key.WriteByte('/')
	}
	return key.String()
}

func (fc *firecracker) assignStaticGuestPaths(numa *types.FcNUMAConfig) error {
	plan, err := fcPlanPCITopology(fc.fcConfig.VFIO, numa)
	if err != nil {
		return err
	}
	if len(fc.vfioDevices) == 0 {
		fc.fcConfig.PCITopology = plan.topology
		for i := range fc.fcConfig.VFIO {
			address := plan.addresses[i]
			fc.fcConfig.VFIO[i].PCIAddress = &address
		}
		return nil // A configuration can be validated before device objects exist.
	}
	if len(fc.vfioDevices) != len(plan.paths) {
		return fmt.Errorf("Firecracker VFIO configuration and device inventory differ")
	}
	for i, dev := range fc.vfioDevices {
		if dev.ID != fc.fcConfig.VFIO[i].ID {
			return fmt.Errorf("Firecracker VFIO device order differs for %s", dev.ID)
		}
	}
	fc.fcConfig.PCITopology = plan.topology
	for i, dev := range fc.vfioDevices {
		address := plan.addresses[i]
		fc.fcConfig.VFIO[i].PCIAddress = &address
		dev.GuestPciPath = plan.paths[i]
	}
	return nil
}
