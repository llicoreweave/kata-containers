//go:build linux

// SPDX-License-Identifier: Apache-2.0

package virtcontainers

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"testing"

	"github.com/kata-containers/kata-containers/src/runtime/pkg/device/config"
	models "github.com/kata-containers/kata-containers/src/runtime/virtcontainers/pkg/firecracker/client/models"
	"github.com/kata-containers/kata-containers/src/runtime/virtcontainers/types"
	"github.com/stretchr/testify/require"
)

type fcTestHost struct {
	files map[string]string
	links map[string]string
}

func (h fcTestHost) source() fcTopologySource {
	return fcTopologySource{
		readFile: func(path string) ([]byte, error) {
			value, ok := h.files[path]
			if !ok {
				return nil, fmt.Errorf("%s: %w", path, os.ErrNotExist)
			}
			return []byte(value), nil
		},
		resolve: func(path string) (string, error) {
			value, ok := h.links[path]
			if !ok {
				return "", os.ErrNotExist
			}
			return value, nil
		},
	}
}

func newFCTestPlan(t *testing.T) (*firecracker, fcTestHost) {
	t.Helper()
	fc := &firecracker{config: HypervisorConfig{
		ColdPlugVFIO: config.SwitchPort, NumVCPUsF: 3, MemorySize: 10,
		GuestNUMANodes: []types.GuestNUMANode{
			{HostNodes: "0", HostCPUs: "0,2"}, {HostNodes: "2", HostCPUs: "1,3"},
		},
	}, fcConfig: &types.FcConfig{Drives: make([]*models.Drive, 9), NetworkInterfaces: make([]*models.NetworkInterface, 1)}}
	host := fcTestHost{files: map[string]string{
		"/proc/self/status":                           "Cpus_allowed_list:\t0-3\nMems_allowed_list:\t0,2\n",
		"/sys/devices/system/node/online":             "0,2\n",
		"/sys/devices/system/node/node0/cpulist":      "0,2\n",
		"/sys/devices/system/node/node2/cpulist":      "1,3\n",
		"/sys/devices/system/node/node0/distance":     "10 31\n",
		"/sys/devices/system/node/node2/distance":     "31 10\n",
		"/sys/bus/pci/devices/0000:19:00.0/numa_node": "0\n",
		"/sys/bus/pci/devices/0000:1a:00.0/numa_node": "0\n",
		"/sys/bus/pci/devices/0000:9b:00.0/numa_node": "2\n",
	}, links: map[string]string{
		"/sys/bus/pci/devices/0000:19:00.0": "/sys/devices/pci0000:15/0000:15:01.0/0000:16:00.0/0000:17:01.0/0000:19:00.0",
		"/sys/bus/pci/devices/0000:1a:00.0": "/sys/devices/pci0000:15/0000:15:01.0/0000:16:00.0/0000:17:02.0/0000:1a:00.0",
		"/sys/bus/pci/devices/0000:9b:00.0": "/sys/devices/pci0000:97/0000:97:01.0/0000:98:00.0/0000:99:01.0/0000:9b:00.0",
	}}
	return fc, host
}

func addFCTestDevices(t *testing.T, fc *firecracker, bdfs ...string) {
	t.Helper()
	for _, bdf := range bdfs {
		require.NoError(t, fc.fcAddVFIODevice(&config.VFIODev{ID: bdf, BDF: bdf, Type: config.VFIOPCIDeviceNormalType}))
	}
}

func TestFCPlanAllocatedFabricAndUnevenNUMA(t *testing.T) {
	fc, host := newFCTestPlan(t)
	addFCTestDevices(t, fc, "0000:19:00.0", "0000:1a:00.0", "0000:9b:00.0")
	require.NoError(t, fc.configureTopologyWith(host.source()))
	require.Equal(t, []uint32{13, 0, 1, 0}, fc.fcConfig.VFIO[0].PCIPath)
	require.Equal(t, []uint32{13, 0, 2, 0}, fc.fcConfig.VFIO[1].PCIPath)
	require.Equal(t, "00/0d/00/01/00", fc.vfioDevices[0].GuestPciPath.String())
	require.Equal(t, "05/0e/00/01/00", fc.vfioDevices[2].GuestPciPath.String())
	nodes := fc.fcConfig.NUMA.Nodes
	require.Equal(t, []uint32{0, 1}, nodes[0].VCPUs)
	require.Equal(t, []uint32{0, 2}, nodes[0].HostCPUs)
	require.Equal(t, []uint32{2}, nodes[1].VCPUs)
	require.Equal(t, []uint32{1}, nodes[1].HostCPUs)
	require.Equal(t, uint32(2), nodes[1].HostNode)
	require.Equal(t, uint64(6), nodes[0].MemoryMiB)
	require.Equal(t, uint64(4), nodes[1].MemoryMiB)
	require.Equal(t, [][]uint32{{10, 31}, {31, 10}}, fc.fcConfig.NUMA.Distances)
	require.Contains(t, fc.vfioJailerArgs(), "--numa-node")
	// An identical retry preserves the exact serialized plan and agent mapping.
	before, err := json.Marshal(fc.fcConfig)
	require.NoError(t, err)
	require.NoError(t, fc.configureTopologyWith(host.source()))
	after, err := json.Marshal(fc.fcConfig)
	require.NoError(t, err)
	require.JSONEq(t, string(before), string(after))
}

func TestFCPlanAutomaticNodeSelectionAndConstraints(t *testing.T) {
	fc, host := newFCTestPlan(t)
	fc.config.NumVCPUsF = 1
	fc.config.MemorySize = 4
	fc.config.AllocatedCPUs = "3"
	fc.config.AllocatedMems = "2"
	fc.setSandboxResourceConstraints("1-3", "0,2")
	addFCTestDevices(t, fc, "0000:9b:00.0")
	require.NoError(t, fc.configureTopologyWith(host.source()))
	require.Len(t, fc.fcConfig.NUMA.Nodes, 1)
	require.Equal(t, uint32(2), fc.fcConfig.NUMA.Nodes[0].HostNode)
	require.Equal(t, []uint32{3}, fc.fcConfig.NUMA.Nodes[0].HostCPUs)
	require.Equal(t, [][]uint32{{10}}, fc.fcConfig.NUMA.Distances)
	require.Equal(t, "00/0d/00/01/00", fc.vfioDevices[0].GuestPciPath.String())
	require.Len(t, fc.fcConfig.VFIO, 1, "unallocated GPUs and NICs must not be added")
	require.Equal(t, []types.GuestNUMANode{{HostNodes: "2", HostCPUs: "3"}}, fc.guestNUMATopology(), "agent translation must use the final guest domain and pinned CPU")
}

func TestFCPlanCPUOnlyAndExplicitDomains(t *testing.T) {
	fc, host := newFCTestPlan(t)
	fc.config.NumVCPUsF = 1
	require.NoError(t, fc.configureTopologyWith(host.source()))
	require.Len(t, fc.fcConfig.NUMA.Nodes, 1)
	fc.config.NumVCPUsF = 2
	fc.config.NUMAMapping = []string{"0", "2"}
	require.NoError(t, fc.configureTopologyWith(host.source()))
	require.Len(t, fc.fcConfig.NUMA.Nodes, 2, "explicit empty PCI domains must be preserved")
	require.Equal(t, []uint32{0}, fc.fcConfig.NUMA.Nodes[0].VCPUs)
	require.Equal(t, []uint32{1}, fc.fcConfig.NUMA.Nodes[1].VCPUs)
}

func TestFCPlanFailuresDoNotPublishPartialState(t *testing.T) {
	tests := map[string]func(*firecracker, *fcTestHost){
		"missing ancestry": func(_ *firecracker, h *fcTestHost) { delete(h.links, "/sys/bus/pci/devices/0000:19:00.0") },
		"wrong endpoint": func(_ *firecracker, h *fcTestHost) {
			h.links["/sys/bus/pci/devices/0000:19:00.0"] = h.links["/sys/bus/pci/devices/0000:1a:00.0"]
		},
		"unknown locality":            func(_ *firecracker, h *fcTestHost) { h.files["/sys/bus/pci/devices/0000:19:00.0/numa_node"] = "-1" },
		"unavailable node":            func(fc *firecracker, _ *fcTestHost) { fc.sandboxMems = "2" },
		"disjoint pod CPUs":           func(fc *firecracker, _ *fcTestHost) { fc.sandboxCPUs = "9" },
		"disjoint allocation":         func(fc *firecracker, _ *fcTestHost) { fc.config.AllocatedCPUs = "9" },
		"insufficient CPUs":           func(fc *firecracker, _ *fcTestHost) { fc.config.NumVCPUsF = 5 },
		"insufficient RAM":            func(fc *firecracker, _ *fcTestHost) { fc.config.MemorySize = 2 },
		"odd RAM":                     func(fc *firecracker, _ *fcTestHost) { fc.config.MemorySize = 9 },
		"malformed status":            func(_ *firecracker, h *fcTestHost) { h.files["/proc/self/status"] = "Cpus_allowed_list: 0-3\n" },
		"missing status":              func(_ *firecracker, h *fcTestHost) { delete(h.files, "/proc/self/status") },
		"bad distance after planning": func(_ *firecracker, h *fcTestHost) { h.files["/sys/devices/system/node/node2/distance"] = "31 20" },
		"wrong distance columns":      func(_ *firecracker, h *fcTestHost) { h.files["/sys/devices/system/node/node2/distance"] = "31 10 40" },
		"cross-node switch":           func(_ *firecracker, h *fcTestHost) { h.files["/sys/bus/pci/devices/0000:1a:00.0/numa_node"] = "2" },
		"explicit excluded domain": func(fc *firecracker, _ *fcTestHost) {
			fc.config.NUMAMapping = []string{"0", "2"}
			fc.sandboxCPUs = "0,2"
		},
	}
	for name, damage := range tests {
		t.Run(name, func(t *testing.T) {
			fc, host := newFCTestPlan(t)
			addFCTestDevices(t, fc, "0000:19:00.0", "0000:1a:00.0", "0000:9b:00.0")
			require.NoError(t, fc.configureTopologyWith(host.source()))
			before, err := json.Marshal(fc.fcConfig)
			require.NoError(t, err)
			paths := []string{}
			for _, device := range fc.vfioDevices {
				paths = append(paths, device.GuestPciPath.String())
			}
			damage(fc, &host)
			require.Error(t, fc.configureTopologyWith(host.source()))
			after, err := json.Marshal(fc.fcConfig)
			require.NoError(t, err)
			require.JSONEq(t, string(before), string(after))
			for i, device := range fc.vfioDevices {
				require.Equal(t, paths[i], device.GuestPciPath.String())
			}
		})
	}
}

func TestFCPlanIndependentOfDeviceOrder(t *testing.T) {
	var expected map[string]string
	for _, order := range [][]string{{"0000:19:00.0", "0000:1a:00.0", "0000:9b:00.0"}, {"0000:9b:00.0", "0000:1a:00.0", "0000:19:00.0"}} {
		fc, host := newFCTestPlan(t)
		addFCTestDevices(t, fc, order...)
		require.NoError(t, fc.configureTopologyWith(host.source()))
		actual := map[string]string{}
		for _, device := range fc.vfioDevices {
			actual[device.ID] = device.GuestPciPath.String()
		}
		if expected == nil {
			expected = actual
		} else {
			require.Equal(t, expected, actual)
		}
	}
}

func TestFCGuestPCIPathBudgetsAndConflicts(t *testing.T) {
	for _, paths := range [][][]uint32{
		{{0, 0}}, {{1, 32}}, {{1}}, {{1, 0}, {1, 0}}, {{1, 0}, {1, 0, 0}},
		{{1, 0, 0, 0, 0, 0, 0, 0, 0}},
	} {
		var devices []types.FcVFIODevice
		for i, path := range paths {
			devices = append(devices, types.FcVFIODevice{ID: fmt.Sprint(i), PCIPath: path})
		}
		_, err := fcGuestPCIPaths(devices, nil)
		require.Error(t, err, "%v", paths)
	}
	var devices []types.FcVFIODevice
	for i := uint32(1); i <= 9; i++ {
		devices = append(devices, types.FcVFIODevice{ID: fmt.Sprint(i), PCIPath: []uint32{i, 0, 0, 0, 0, 0, 0, 0}})
	}
	_, err := fcGuestPCIPaths(devices, nil)
	require.NoError(t, err, "63 bridge buses plus the root fit exactly")
	devices = append(devices, types.FcVFIODevice{ID: "overflow", PCIPath: []uint32{10, 0}})
	_, err = fcGuestPCIPaths(devices, nil)
	require.ErrorContains(t, err, "64 buses")
	numa := &types.FcNUMAConfig{Nodes: []types.FcNUMANode{{PCIRootSlots: []uint32{1, 2, 3, 4, 5}}, {PCIRootSlots: []uint32{6, 7, 8, 9}}}}
	_, err = fcGuestPCIPaths(devices[:9], numa)
	require.ErrorContains(t, err, "64 buses")
	_, err = fcGuestPCIPaths(devices[:6], numa)
	require.ErrorContains(t, err, "root bus")
}

func TestFCConfigPublication(t *testing.T) {
	fc, host := newFCTestPlan(t)
	addFCTestDevices(t, fc, "0000:19:00.0", "0000:9b:00.0")
	require.NoError(t, fc.configureTopologyWith(host.source()))
	dir := t.TempDir()
	fc.fcConfigPath = filepath.Join(dir, "fcConfig.json")
	victim := filepath.Join(t.TempDir(), "untouched")
	require.NoError(t, os.WriteFile(victim, []byte("keep"), 0600))
	require.NoError(t, os.Symlink(victim, fc.fcConfigPath))
	require.NoError(t, fc.writeConfig())
	data, err := os.ReadFile(victim)
	require.NoError(t, err)
	require.Equal(t, "keep", string(data))
	data, err = os.ReadFile(fc.fcConfigPath)
	require.NoError(t, err)
	var actual types.FcConfig
	require.NoError(t, json.Unmarshal(data, &actual))
	expected, err := json.Marshal(fc.fcConfig)
	require.NoError(t, err)
	require.JSONEq(t, string(expected), string(data))
	require.Equal(t, fc.fcConfig.PCITopology, actual.PCITopology)
	info, err := os.Lstat(fc.fcConfigPath)
	require.NoError(t, err)
	require.Equal(t, os.FileMode(0640), info.Mode().Perm())
	require.True(t, info.Mode().IsRegular())
	fc.fcConfigPath = filepath.Join(dir, "directory")
	require.NoError(t, os.Mkdir(fc.fcConfigPath, 0700))
	require.Error(t, fc.writeConfig())
	entries, err := os.ReadDir(dir)
	require.NoError(t, err)
	var names []string
	for _, entry := range entries {
		names = append(names, entry.Name())
	}
	sort.Strings(names)
	require.Equal(t, []string{"directory", "fcConfig.json"}, names)
}

func TestFCPlanRecordedDualNUMAFabric(t *testing.T) {
	var fixture struct {
		Files        map[string]string  `json:"files"`
		Links        map[string]string  `json:"links"`
		ExpectedNUMA types.FcNUMAConfig `json:"expected_numa"`
		Devices      []struct {
			ID        string   `json:"id"`
			Path      string   `json:"path"`
			PCIPath   []uint32 `json:"pci_path"`
			GuestPath string   `json:"guest_path"`
		} `json:"devices"`
	}
	data, err := os.ReadFile("testdata/firecracker/dual-numa-fabric.json")
	require.NoError(t, err)
	require.NoError(t, json.Unmarshal(data, &fixture))
	fc := &firecracker{config: HypervisorConfig{ColdPlugVFIO: config.SwitchPort, NumVCPUsF: 64, MemorySize: 65536},
		fcConfig: &types.FcConfig{Drives: make([]*models.Drive, 9), NetworkInterfaces: make([]*models.NetworkInterface, 1)}}
	for _, node := range fixture.ExpectedNUMA.Nodes {
		name := fmt.Sprint(node.HostNode)
		fc.config.GuestNUMANodes = append(fc.config.GuestNUMANodes, types.GuestNUMANode{
			HostNodes: name, HostCPUs: fixture.Files["/sys/devices/system/node/node"+name+"/cpulist"],
		})
	}
	for _, device := range fixture.Devices {
		addFCTestDevices(t, fc, device.ID)
	}
	require.NoError(t, fc.configureTopologyWith(fcTestHost{fixture.Files, fixture.Links}.source()))
	require.Equal(t, fixture.ExpectedNUMA.Distances, fc.fcConfig.NUMA.Distances)
	for i, node := range fixture.ExpectedNUMA.Nodes {
		actual := fc.fcConfig.NUMA.Nodes[i]
		actual.PCIRootSlots = nil
		require.Equal(t, node, actual)
	}
	require.Len(t, fc.fcConfig.VFIO, 20)
	require.Len(t, fc.fcConfig.PCITopology.Bridges, 38)
	require.Equal(t, uint32(0), fc.fcConfig.PCITopology.Roots[0].Bus)
	require.Equal(t, uint32(16), fc.fcConfig.PCITopology.Roots[0].LastBus)
	require.Equal(t, uint32(17), fc.fcConfig.PCITopology.Roots[1].Bus)
	require.Equal(t, uint32(39), fc.fcConfig.PCITopology.Roots[1].LastBus)
	for _, dev := range fc.fcConfig.VFIO {
		if dev.ID == "0000:19:00.0" {
			require.Equal(t, uint32(3), dev.PCIAddress.Bus)
		}
		if dev.ID == "0000:9b:00.0" {
			require.Equal(t, uint32(26), dev.PCIAddress.Bus)
		}
	}
	for i, expected := range fixture.Devices {
		require.Equal(t, expected.Path, fc.fcConfig.VFIO[i].Path)
		require.Equal(t, expected.PCIPath, fc.fcConfig.VFIO[i].PCIPath)
		require.Equal(t, expected.GuestPath, fc.vfioDevices[i].GuestPciPath.String())
	}
}

func TestFCNUMAResourceAccounting(t *testing.T) {
	for _, capacities := range [][]int{{1, 63}, {32, 32}, {1, 1, 62}, {3, 5, 9}} {
		capacity := 0
		for _, n := range capacities {
			capacity += n
		}
		for total := len(capacities); total <= capacity && total <= 64; total++ {
			counts, err := fcDistributeVCPUs(total, capacities)
			require.NoError(t, err)
			sum := 0
			for i, n := range counts {
				require.Greater(t, n, 0)
				require.LessOrEqual(t, n, capacities[i])
				sum += n
			}
			require.Equal(t, total, sum)
			for _, ram := range []uint64{uint64(2 * len(counts)), uint64(2*len(counts) + 2), 65536, 65538} {
				memory := fcDistributeMemory(ram, counts)
				var allocated uint64
				for _, size := range memory {
					require.GreaterOrEqual(t, size, uint64(2))
					require.Zero(t, size%2)
					allocated += size
				}
				require.Equal(t, ram, allocated)
			}
		}
	}
	_, err := fcDistributeVCPUs(4, []int{1, 2})
	require.Error(t, err)
}

func TestFCTopologyOptInDoesNotReadNUMAMetadata(t *testing.T) {
	fc, host := newFCTestPlan(t)
	fc.config.GuestNUMANodes = nil
	addFCTestDevices(t, fc, "0000:19:00.0")
	source := host.source()
	source.readFile = func(string) ([]byte, error) { t.Fatal("UMA must not read NUMA metadata"); return nil, nil }
	require.NoError(t, fc.configureTopologyWith(source))
	require.Nil(t, fc.fcConfig.NUMA)
	require.Nil(t, fc.guestNUMATopology())
	require.NotEmpty(t, fc.fcConfig.VFIO[0].PCIPath)
	require.Equal(t, "00/0d/00/01/00", fc.vfioDevices[0].GuestPciPath.String())
}

// A shared wire fixture is parsed independently by Firecracker's Rust tests.
// Regenerate explicitly when changing the contract, then review both copies.
func TestFCExplicitPCIContract(t *testing.T) {
	fc, host := newFCTestPlan(t)
	addFCTestDevices(t, fc, "0000:19:00.0", "0000:1a:00.0", "0000:9b:00.0")
	require.NoError(t, fc.configureTopologyWith(host.source()))
	contract := struct {
		Topology *types.FcPCITopology `json:"pci-topology"`
		VFIO     []types.FcVFIODevice `json:"vfio"`
		NUMA     *types.FcNUMAConfig  `json:"numa"`
	}{fc.fcConfig.PCITopology, fc.fcConfig.VFIO, fc.fcConfig.NUMA}
	wire, err := json.MarshalIndent(contract, "", "  ")
	require.NoError(t, err)
	if output := os.Getenv("KATA_FC_PCI_FIXTURE_OUTPUT"); output != "" {
		require.NoError(t, os.WriteFile(output, append(wire, '\n'), 0600))
		return
	}
	expected, err := os.ReadFile("testdata/firecracker/explicit-pci-contract.json")
	require.NoError(t, err)
	require.JSONEq(t, string(expected), string(wire))
	require.NotContains(t, string(wire), "pci_path")
	require.NotContains(t, string(wire), "pci_root_slots")
	require.Equal(t, &types.FcPCIAddress{Bus: 3, Device: 0}, fc.fcConfig.VFIO[0].PCIAddress)
	require.Equal(t, &types.FcPCIAddress{Bus: 4, Device: 0}, fc.fcConfig.VFIO[1].PCIAddress)
	require.Equal(t, &types.FcPCIAddress{Bus: 8, Device: 0}, fc.fcConfig.VFIO[2].PCIAddress)
}
