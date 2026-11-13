//go:build linux

// SPDX-License-Identifier: Apache-2.0
package virtcontainers

import (
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"

	"github.com/kata-containers/kata-containers/src/runtime/pkg/device/config"
	"github.com/kata-containers/kata-containers/src/runtime/virtcontainers/pkg/cpuset"
	"github.com/kata-containers/kata-containers/src/runtime/virtcontainers/types"
)

// Explicit opt-in: auto-discovered GuestNUMANodes must not change older runtimes.
func (fc *firecracker) configureNUMA() error {
	err := fc.configureNUMAWith(func(path string) (int, error) {
		value, err := os.ReadFile(filepath.Join(path, "numa_node"))
		if err != nil {
			return 0, err
		}
		return strconv.Atoi(strings.TrimSpace(string(value)))
	})
	if err != nil || fc.fcConfig.NUMA == nil {
		return err
	}
	return fc.configureNUMADistances(os.ReadFile)
}

func (fc *firecracker) configureNUMADistances(readFile func(string) ([]byte, error)) error {
	data, err := readFile("/sys/devices/system/node/online")
	if err != nil {
		return fmt.Errorf("reading online NUMA nodes: %w", err)
	}
	online, err := cpuset.Parse(strings.TrimSpace(string(data)))
	if err != nil || online.IsEmpty() {
		return fmt.Errorf("invalid online NUMA nodes")
	}
	// Linux node_read_distance emits columns in online-node order, which may
	// contain holes in physical node IDs (drivers/base/node.c).
	columns := map[uint32]int{}
	for index, host := range online.ToSlice() {
		columns[uint32(host)] = index
	}
	for i, node := range fc.fcConfig.NUMA.Nodes {
		data, err := readFile(fmt.Sprintf("/sys/devices/system/node/node%d/distance", node.HostNode))
		if err != nil {
			return fmt.Errorf("reading host NUMA distance: %w", err)
		}
		distances := strings.Fields(string(data))
		for j, remote := range fc.fcConfig.NUMA.Nodes {
			column, found := columns[remote.HostNode]
			if !found || column >= len(distances) {
				return fmt.Errorf("missing host NUMA distance to node %d", remote.HostNode)
			}
			value, err := strconv.ParseUint(distances[column], 10, 8)
			if err != nil || (i == j && value != 10) || (i != j && value <= 10) {
				return fmt.Errorf("invalid host NUMA distance from %d to %d", node.HostNode, remote.HostNode)
			}
			fc.fcConfig.NUMA.Distances[i][j] = uint32(value)
		}
	}
	return nil
}

func (fc *firecracker) configureNUMAWith(deviceNode func(string) (int, error)) error {
	if len(fc.config.NUMAMapping) == 0 {
		return nil
	}
	if fc.config.ColdPlugVFIO != config.SwitchPort {
		return fmt.Errorf("private Firecracker NUMA requires the static switch-port runtime")
	}
	nodes := fc.config.GuestNUMANodes
	count := len(nodes)
	if count == 0 || count > 64 || fc.config.NumVCPUs()%uint32(count) != 0 || fc.config.MemorySize%uint32(2*count) != 0 {
		return fmt.Errorf("NUMA CPU and memory totals must divide equally across explicit nodes")
	}
	perNode := fc.config.NumVCPUs() / uint32(count)
	if perNode == 0 || fc.config.NumVCPUs() > 64 || fc.config.MemorySize == 0 {
		return fmt.Errorf("NUMA requires 1..=64 vCPUs and nonzero memory")
	}
	numa := &types.FcNUMAConfig{Distances: make([][]uint32, count)}
	hostToGuest := map[int]int{}
	usedCPUs := map[int]bool{}
	for id, node := range nodes {
		hostNodes, err := cpuset.Parse(node.HostNodes)
		if err != nil || hostNodes.Size() != 1 {
			return fmt.Errorf("one host node required per private Firecracker NUMA domain")
		}
		host := hostNodes.ToSlice()[0]
		if host >= 64 {
			return fmt.Errorf("host NUMA node %d exceeds Firecracker nodemask", host)
		}
		if _, exists := hostToGuest[host]; exists {
			return fmt.Errorf("duplicate host NUMA node %d", host)
		}
		hostToGuest[host] = id
		cpus, err := cpuset.Parse(node.HostCPUs)
		if err != nil || cpus.Size() < int(perNode) {
			return fmt.Errorf("insufficient CPUs for NUMA node %d", id)
		}
		guest := types.FcNUMANode{HostNode: uint32(host), MemoryMiB: uint64(fc.config.MemorySize) / uint64(count), PCIRootSlots: []uint32{}}
		for vcpu, cpu := range cpus.ToSlice()[:perNode] {
			if cpu >= 1024 || usedCPUs[cpu] {
				return fmt.Errorf("invalid or duplicate NUMA host CPU %d", cpu)
			}
			usedCPUs[cpu] = true
			guest.VCPUs = append(guest.VCPUs, uint32(id)*perNode+uint32(vcpu))
			guest.HostCPUs = append(guest.HostCPUs, uint32(cpu))
		}
		numa.Nodes = append(numa.Nodes, guest)
		numa.Distances[id] = make([]uint32, count)
		for remote := range nodes {
			if remote == id {
				numa.Distances[id][remote] = 10
			} else {
				numa.Distances[id][remote] = 21
			}
		}
	}
	rootNode := map[uint32]int{}
	for _, dev := range fc.fcConfig.VFIO {
		host, err := deviceNode(dev.Path)
		if err != nil {
			return fmt.Errorf("reading device NUMA node: %w", err)
		}
		guest, found := hostToGuest[host]
		if !found || len(dev.PCIPath) < 2 {
			return fmt.Errorf("device %s has no covered NUMA/static PCI path", dev.Path)
		}
		root := dev.PCIPath[0]
		if previous, exists := rootNode[root]; exists {
			if previous != guest {
				return fmt.Errorf("PCI subtree %d crosses host NUMA domains", root)
			}
		} else {
			rootNode[root] = guest
			numa.Nodes[guest].PCIRootSlots = append(numa.Nodes[guest].PCIRootSlots, root)
		}
	}
	// Mirror the VMM's root-bus numbering. Each domain owns a root bus,
	// followed by its lexically ordered bridge-prefix buses.
	nextBus := 0
	for id := range numa.Nodes {
		if id != 0 {
			nextBus++
		}
		rootBus := nextBus
		parents := map[string]bool{}
		for i, dev := range fc.fcConfig.VFIO {
			if rootNode[dev.PCIPath[0]] != id {
				continue
			}
			for n := 1; n < len(dev.PCIPath); n++ {
				parents[fmt.Sprint(dev.PCIPath[:n])] = true
			}
			if i >= len(fc.vfioDevices) {
				continue
			} // synthetic configuration tests
			slots := make([]types.PciSlot, len(dev.PCIPath)+1)
			var err error
			slots[0], err = types.PciSlotFromInt(rootBus)
			if err != nil {
				return err
			}
			for j, slot := range dev.PCIPath {
				slots[j+1], err = types.PciSlotFromInt(int(slot))
				if err != nil {
					return err
				}
			}
			fc.vfioDevices[i].GuestPciPath, err = types.PciPathFromSlots(slots...)
			if err != nil {
				return err
			}
		}
		nextBus += len(parents)
		if nextBus >= 64 {
			return fmt.Errorf("NUMA PCI topology exhausts 64 buses")
		}
	}
	fc.fcConfig.NUMA = numa
	return nil
}
