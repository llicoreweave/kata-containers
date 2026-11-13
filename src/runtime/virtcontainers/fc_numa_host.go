//go:build linux

// SPDX-License-Identifier: Apache-2.0

package virtcontainers

import (
	"fmt"
	"sort"
	"strconv"
	"strings"

	"github.com/kata-containers/kata-containers/src/runtime/virtcontainers/pkg/cpuset"
	"github.com/kata-containers/kata-containers/src/runtime/virtcontainers/types"
)

func (fc *firecracker) selectNUMANodes(readFile func(string) ([]byte, error), deviceNode func(string) (int, error)) ([]types.GuestNUMANode, error) {
	data, err := readFile("/proc/self/status")
	if err != nil {
		return nil, fmt.Errorf("reading effective host CPU/node restrictions: %w", err)
	}
	fields := map[string]string{}
	for _, line := range strings.Split(string(data), "\n") {
		key, value, found := strings.Cut(line, ":")
		if found {
			fields[key] = strings.TrimSpace(value)
		}
	}
	allowedCPUs, err := fcAllowedSet(fields["Cpus_allowed_list"], fc.sandboxCPUs)
	if err != nil {
		return nil, fmt.Errorf("invalid effective Firecracker CPU set: %w", err)
	}
	allowedMems, err := fcAllowedSet(fields["Mems_allowed_list"], fc.sandboxMems)
	if err != nil {
		return nil, fmt.Errorf("invalid effective Firecracker memory-node set: %w", err)
	}
	allowedCPUs, err = fcAllowedSet(allowedCPUs.String(), fc.config.AllocatedCPUs)
	if err != nil {
		return nil, fmt.Errorf("Firecracker CPU allocation conflicts with effective constraints: %w", err)
	}
	allowedMems, err = fcAllowedSet(allowedMems.String(), fc.config.AllocatedMems)
	if err != nil {
		return nil, fmt.Errorf("Firecracker memory allocation conflicts with effective constraints: %w", err)
	}
	explicit := len(fc.config.NUMAMapping) != 0
	var available []types.GuestNUMANode
	seen := map[int]bool{}
	for _, node := range fc.config.GuestNUMANodes {
		hosts, err := cpuset.Parse(node.HostNodes)
		if err != nil || hosts.Size() != 1 {
			return nil, fmt.Errorf("Firecracker NUMA domains require one host node each")
		}
		host := hosts.ToSlice()[0]
		if host >= 64 || seen[host] {
			return nil, fmt.Errorf("invalid or duplicate Firecracker host node %d", host)
		}
		seen[host] = true
		node.HostNodes = strconv.Itoa(host)
		if !allowedMems.Contains(host) {
			if explicit {
				return nil, fmt.Errorf("explicit host node %d is outside the permitted memory nodes", host)
			}
			continue
		}
		data, err := readFile(fmt.Sprintf("/sys/devices/system/node/node%d/cpulist", host))
		if err != nil {
			return nil, fmt.Errorf("reading CPU membership for host node %d: %w", host, err)
		}
		local, err := cpuset.Parse(strings.TrimSpace(string(data)))
		if err != nil {
			return nil, fmt.Errorf("invalid CPU membership for host node %d: %w", host, err)
		}
		requested, err := cpuset.Parse(strings.TrimSpace(node.HostCPUs))
		if err != nil {
			return nil, fmt.Errorf("invalid configured host CPUs: %w", err)
		}
		permitted := local.Intersection(requested).Intersection(allowedCPUs)
		if permitted.IsEmpty() {
			if explicit {
				return nil, fmt.Errorf("explicit host node %d has no permitted local CPUs", host)
			}
			continue
		}
		node.HostCPUs = permitted.String()
		available = append(available, node)
	}
	if explicit {
		if len(available) == 0 {
			return nil, fmt.Errorf("explicit Firecracker NUMA topology is empty")
		}
		return available, nil
	}
	sort.Slice(available, func(i, j int) bool {
		a, _ := strconv.Atoi(available[i].HostNodes)
		b, _ := strconv.Atoi(available[j].HostNodes)
		return a < b
	})
	required := map[int]bool{}
	for _, device := range fc.fcConfig.VFIO {
		node, err := deviceNode(device.Path)
		if err != nil {
			return nil, err
		}
		required[node] = true
	}
	var selected []types.GuestNUMANode
	selectedIDs := map[int]bool{}
	capacity := 0
	appendNode := func(node types.GuestNUMANode) {
		host, _ := strconv.Atoi(node.HostNodes)
		selected = append(selected, node)
		selectedIDs[host] = true
		cpus, _ := cpuset.Parse(node.HostCPUs)
		capacity += cpus.Size()
	}
	for _, node := range available {
		host, _ := strconv.Atoi(node.HostNodes)
		if required[host] {
			appendNode(node)
		}
	}
	for host := range required {
		if !selectedIDs[host] {
			return nil, fmt.Errorf("allocated PCI device on host NUMA node %d has no permitted CPU/memory domain", host)
		}
	}
	for _, node := range available {
		if capacity >= int(fc.config.NumVCPUs()) {
			break
		}
		host, _ := strconv.Atoi(node.HostNodes)
		if !selectedIDs[host] {
			appendNode(node)
		}
	}
	if len(selected) == 0 || capacity < int(fc.config.NumVCPUs()) {
		return nil, fmt.Errorf("insufficient permitted NUMA CPU capacity for %d vCPUs", fc.config.NumVCPUs())
	}
	sort.Slice(selected, func(i, j int) bool {
		a, _ := strconv.Atoi(selected[i].HostNodes)
		b, _ := strconv.Atoi(selected[j].HostNodes)
		return a < b
	})
	return selected, nil
}

// An absent OCI restriction leaves the effective host restriction intact. An
// empty intersection is an error, never a reason to widen access to the host.
func fcAllowedSet(effective, requested string) (cpuset.CPUSet, error) {
	allowed, err := cpuset.Parse(effective)
	if err != nil || allowed.IsEmpty() {
		return cpuset.CPUSet{}, fmt.Errorf("empty or malformed effective set %q", effective)
	}
	if requested != "" {
		limit, err := cpuset.Parse(requested)
		if err != nil {
			return cpuset.CPUSet{}, err
		}
		allowed = allowed.Intersection(limit)
		if allowed.IsEmpty() {
			return cpuset.CPUSet{}, fmt.Errorf("requested set %q does not intersect effective set %q", requested, effective)
		}
	}
	return allowed, nil
}
