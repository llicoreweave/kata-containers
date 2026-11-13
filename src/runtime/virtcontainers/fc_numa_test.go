//go:build linux

// SPDX-License-Identifier: Apache-2.0
package virtcontainers

import (
	"encoding/json"
	"fmt"
	"github.com/kata-containers/kata-containers/src/runtime/pkg/device/config"
	"github.com/kata-containers/kata-containers/src/runtime/virtcontainers/types"
	"github.com/stretchr/testify/require"
	"testing"
)

func TestFCNUMAExplicitMapping(t *testing.T) {
	fc := firecracker{config: HypervisorConfig{
		ColdPlugVFIO: config.SwitchPort, NumVCPUsF: 64, MemorySize: 1048576,
		NUMAMapping: []string{"0", "1"}, GuestNUMANodes: []types.GuestNUMANode{
			{HostNodes: "0", HostCPUs: "0-31"}, {HostNodes: "1", HostCPUs: "32-63"},
		},
	}, fcConfig: &types.FcConfig{VFIO: []types.FcVFIODevice{
		{ID: "gpu0", Path: "gpu0", PCIPath: []uint32{13, 0}},
		{ID: "gpu1", Path: "gpu1", PCIPath: []uint32{18, 0}},
	}}}
	readNode := func(path string) (int, error) {
		if path == "gpu0" {
			return 0, nil
		}
		return 1, nil
	}
	fc.vfioDevices = []*config.VFIODev{{ID: "gpu0"}, {ID: "gpu1"}}
	require.NoError(t, fc.configureNUMAWith(readNode))
	require.Equal(t, "00/0d/00", fc.vfioDevices[0].GuestPciPath.String())
	require.Equal(t, "02/12/00", fc.vfioDevices[1].GuestPciPath.String())
	require.Equal(t, uint64(524288), fc.fcConfig.NUMA.Nodes[1].MemoryMiB)
	require.Equal(t, uint32(32), fc.fcConfig.NUMA.Nodes[1].VCPUs[0])
	require.Equal(t, []uint32{18}, fc.fcConfig.NUMA.Nodes[1].PCIRootSlots)
	data, err := json.Marshal(fc.fcConfig)
	require.NoError(t, err)
	require.Contains(t, string(data), `"vcpus":[0,1,2`)
	require.Contains(t, string(data), `"distances":[[10,21],[21,10]]`)
	fc.fcConfig.VFIO[1].PCIPath[0] = 13
	require.Error(t, fc.configureNUMAWith(readNode))
	fc.config.NUMAMapping = nil
	fc.fcConfig.NUMA = nil
	require.NoError(t, fc.configureNUMAWith(readNode))
	require.Nil(t, fc.fcConfig.NUMA)
}

func TestFCNUMAMultipleDomainsAndHostDistances(t *testing.T) {
	fc := firecracker{config: HypervisorConfig{
		ColdPlugVFIO: config.SwitchPort, NumVCPUsF: 6, MemorySize: 12,
		NUMAMapping: []string{"0", "2", "4"}, GuestNUMANodes: []types.GuestNUMANode{
			{HostNodes: "0", HostCPUs: "0-1"}, {HostNodes: "2", HostCPUs: "2-3"}, {HostNodes: "4", HostCPUs: "4-5"},
		},
	}, fcConfig: &types.FcConfig{}}
	require.NoError(t, fc.configureNUMAWith(func(string) (int, error) { return 0, nil }))
	require.Len(t, fc.fcConfig.NUMA.Nodes, 3)
	require.Equal(t, uint64(4), fc.fcConfig.NUMA.Nodes[2].MemoryMiB)
	distances := map[string]string{
		"/sys/devices/system/node/online":         "0,2,4",
		"/sys/devices/system/node/node0/distance": "10 21 31",
		"/sys/devices/system/node/node2/distance": "21 10 21",
		"/sys/devices/system/node/node4/distance": "31 21 10",
	}
	require.NoError(t, fc.configureNUMADistances(func(path string) ([]byte, error) {
		return []byte(distances[path]), nil
	}))
	require.Equal(t, [][]uint32{{10, 21, 31}, {21, 10, 21}, {31, 21, 10}}, fc.fcConfig.NUMA.Distances)
	require.Error(t, fc.configureNUMADistances(func(string) ([]byte, error) { return []byte("10"), nil }))
	require.Error(t, fc.configureNUMADistances(func(string) ([]byte, error) { return nil, fmt.Errorf("unavailable") }))
	fc.config.GuestNUMANodes[1].HostCPUs = "0-1"
	require.Error(t, fc.configureNUMAWith(func(string) (int, error) { return 0, nil }))
}

func TestFCNUMAJailerMetadata(t *testing.T) {
	fc := firecracker{fcConfig: &types.FcConfig{NUMA: &types.FcNUMAConfig{
		Nodes: []types.FcNUMANode{{HostNode: 0}, {HostNode: 2}},
	}}}
	require.Equal(t, []string{"--numa-node", "0", "--numa-node", "2"}, fc.vfioJailerArgs())
	fc.fcConfig.NUMA = nil
	require.Empty(t, fc.vfioJailerArgs())
}
