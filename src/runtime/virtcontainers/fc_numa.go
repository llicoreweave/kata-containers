//go:build linux

// SPDX-License-Identifier: Apache-2.0
package virtcontainers

import (
	"fmt"
	"path/filepath"
	"sort"
	"strconv"
	"strings"

	"github.com/kata-containers/kata-containers/src/runtime/pkg/device/config"
	"github.com/kata-containers/kata-containers/src/runtime/virtcontainers/pkg/cpuset"
	"github.com/kata-containers/kata-containers/src/runtime/virtcontainers/types"
)

// NUMA remains opt-in through enable_numa. The host node mapping can be
// explicit, or selected here from the allocated devices and allowed resources.
func (fc *firecracker) configureNUMAFrom(readFile func(string) ([]byte, error)) error {
	if len(fc.config.GuestNUMANodes) == 0 && len(fc.config.NUMAMapping) == 0 {
		return nil
	}
	deviceNodes := map[string]int{}
	readNode := func(path string) (int, error) {
		if node, ok := deviceNodes[path]; ok {
			return node, nil
		}
		data, err := readFile(filepath.Join(path, "numa_node"))
		if err != nil {
			return 0, err
		}
		node, err := strconv.Atoi(strings.TrimSpace(string(data)))
		if err != nil || node < 0 || node >= 64 {
			return 0, fmt.Errorf("invalid or unknown NUMA locality for %s: %q", path, strings.TrimSpace(string(data)))
		}
		deviceNodes[path] = node
		return node, nil
	}
	nodes, err := fc.selectNUMANodes(readFile, readNode)
	if err != nil {
		return err
	}
	fc.config.GuestNUMANodes = nodes
	if err := fc.configureNUMAWith(readNode); err != nil {
		return err
	}
	return fc.configureNUMADistances(readFile)
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
		if len(distances) != online.Size() {
			return fmt.Errorf("host NUMA distance row for node %d has %d columns, expected %d", node.HostNode, len(distances), online.Size())
		}
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
	if len(fc.config.NUMAMapping) == 0 && len(fc.config.GuestNUMANodes) == 0 {
		return nil
	}
	if fc.config.ColdPlugVFIO != config.SwitchPort {
		return fmt.Errorf("Firecracker NUMA requires cold_plug_vfio=switch-port")
	}
	nodes := fc.config.GuestNUMANodes
	count := len(nodes)
	cpus := fc.config.NumVCPUs()
	if count == 0 || count > 64 || cpus < uint32(count) || cpus > 64 || fc.config.MemorySize%2 != 0 || fc.config.MemorySize < uint32(2*count) {
		return fmt.Errorf("Firecracker NUMA requires 1..64 domains, at least one vCPU and 2 MiB per domain, up to 64 vCPUs, and even memory_mib")
	}
	capacities := make([]int, count)
	for i, node := range nodes {
		available, err := cpuset.Parse(node.HostCPUs)
		if err != nil || available.IsEmpty() {
			return fmt.Errorf("invalid host CPUs for NUMA domain %d", i)
		}
		capacities[i] = available.Size()
	}
	cpuCounts, err := fcDistributeVCPUs(int(cpus), capacities)
	if err != nil {
		return err
	}
	memory := fcDistributeMemory(uint64(fc.config.MemorySize), cpuCounts)
	vcpu := uint32(0)
	numa := &types.FcNUMAConfig{Distances: make([][]uint32, count)}
	hostToGuest := map[int]int{}
	usedCPUs := map[int]bool{}
	for id, node := range nodes {
		hostNodes, err := cpuset.Parse(node.HostNodes)
		if err != nil || hostNodes.Size() != 1 {
			return fmt.Errorf("one host node required per Firecracker NUMA domain")
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
		if err != nil || cpus.Size() < cpuCounts[id] {
			return fmt.Errorf("insufficient CPUs for NUMA node %d", id)
		}
		guest := types.FcNUMANode{HostNode: uint32(host), MemoryMiB: memory[id], PCIRootSlots: []uint32{}}
		for _, cpu := range cpus.ToSlice()[:cpuCounts[id]] {
			if cpu >= 1024 || usedCPUs[cpu] {
				return fmt.Errorf("invalid or duplicate NUMA host CPU %d", cpu)
			}
			usedCPUs[cpu] = true
			guest.VCPUs = append(guest.VCPUs, vcpu)
			vcpu++
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
	if err := fc.assignStaticGuestPaths(numa); err != nil {
		return err
	}
	for i := range numa.Nodes {
		sort.Slice(numa.Nodes[i].PCIRootSlots, func(a, b int) bool { return numa.Nodes[i].PCIRootSlots[a] < numa.Nodes[i].PCIRootSlots[b] })
	}
	fc.fcConfig.NUMA = numa
	return nil
}

// Give each domain one vCPU, then balance occupancy against the available
// CPU capacity. Integer comparisons keep results deterministic without floats.
func fcDistributeVCPUs(total int, capacities []int) ([]int, error) {
	counts := make([]int, len(capacities))
	capacity := 0
	for i, available := range capacities {
		if available < 1 {
			return nil, fmt.Errorf("NUMA domain %d has no permitted host CPUs", i)
		}
		counts[i] = 1
		capacity += available
	}
	if total < len(counts) || total > capacity {
		return nil, fmt.Errorf("cannot place %d vCPUs on %d NUMA domains with %d permitted host CPUs", total, len(counts), capacity)
	}
	for remaining := total - len(counts); remaining > 0; remaining-- {
		best := -1
		for i, available := range capacities {
			if counts[i] < available && (best < 0 || counts[i]*capacities[best] < counts[best]*available) {
				best = i
			}
		}
		counts[best]++
	}
	return counts, nil
}

// Reserve 2 MiB per domain, then distribute the rest proportionally to vCPUs
// using largest remainders. The result exactly matches the VM's RAM total.
func fcDistributeMemory(total uint64, cpus []int) []uint64 {
	memory := make([]uint64, len(cpus))
	weights := uint64(0)
	for _, count := range cpus {
		weights += uint64(count)
	}
	units := total/2 - uint64(len(cpus))
	remainders := make([]uint64, len(cpus))
	assigned := uint64(0)
	for i, count := range cpus {
		share := units * uint64(count)
		memory[i] = 2 * (1 + share/weights)
		remainders[i] = share % weights
		assigned += memory[i]
	}
	for assigned < total {
		best := 0
		for i := range remainders {
			if remainders[i] > remainders[best] {
				best = i
			}
		}
		memory[best] += 2
		remainders[best] = 0
		assigned += 2
	}
	return memory
}
