// Copyright (c) 2026 CoreWeave, Inc.
// SPDX-License-Identifier: Apache-2.0

package containerdshim

import (
	"testing"

	"github.com/kata-containers/kata-containers/src/runtime/pkg/oci"
	vc "github.com/kata-containers/kata-containers/src/runtime/virtcontainers"
	"github.com/kata-containers/kata-containers/src/runtime/virtcontainers/types"
	"github.com/stretchr/testify/require"
	podresourcesv1 "k8s.io/kubelet/pkg/apis/podresources/v1"
)

func TestFirecrackerPodResourceConstraints(t *testing.T) {
	s := &service{config: &oci.RuntimeConfig{HypervisorType: vc.FirecrackerHypervisor,
		HypervisorConfig: vc.HypervisorConfig{GuestNUMANodes: []types.GuestNUMANode{{HostNodes: "0"}}},
	}}
	resources := &podresourcesv1.PodResources{Containers: []*podresourcesv1.ContainerResources{
		{Name: "app", CpuIds: []int64{4, 2}, Devices: []*podresourcesv1.ContainerDevices{{ResourceName: "nvidia.com/gpu", DeviceIds: []string{"gpu0"}}},
			Memory: []*podresourcesv1.ContainerMemory{{Topology: &podresourcesv1.TopologyInfo{Nodes: []*podresourcesv1.NUMANode{{ID: 2}}}}}},
		{Name: "sidecar", CpuIds: []int64{2, 6}, Devices: []*podresourcesv1.ContainerDevices{{ResourceName: "example.com/nic", DeviceIds: []string{"nic0"}}},
			Memory: []*podresourcesv1.ContainerMemory{{Topology: &podresourcesv1.TopologyInfo{Nodes: []*podresourcesv1.NUMANode{{ID: 0}, {ID: 2}}}}}},
	}}
	require.NoError(t, s.constrainFirecrackerResources(resources))
	require.Equal(t, "2,4,6", s.config.HypervisorConfig.AllocatedCPUs)
	require.Equal(t, "0,2", s.config.HypervisorConfig.AllocatedMems)
	require.Equal(t, []string{"nvidia.com/gpu=gpu0", "example.com/nic=nic0"}, podResourceDeviceSpec(resources))
	resources.Containers[1].CpuIds = []int64{-1}
	require.Error(t, s.constrainFirecrackerResources(resources))
	require.Equal(t, "2,4,6", s.config.HypervisorConfig.AllocatedCPUs, "failed snapshots must not replace a valid allocation")
	resources.Containers[1].CpuIds = nil
	resources.Containers[1].Memory[0].Topology.Nodes[0].ID = 64
	require.Error(t, s.constrainFirecrackerResources(resources))
	resources.Containers = nil
	require.NoError(t, s.constrainFirecrackerResources(resources))
	require.Empty(t, s.config.HypervisorConfig.AllocatedCPUs, "an unmanaged cpuset remains unspecified")
	require.Empty(t, s.config.HypervisorConfig.AllocatedMems)
}

func TestFirecrackerPodResourceConstraintsOptIn(t *testing.T) {
	for _, hypervisor := range []vc.HypervisorType{vc.QemuHypervisor, vc.FirecrackerHypervisor} {
		s := &service{config: &oci.RuntimeConfig{HypervisorType: hypervisor}}
		resources := &podresourcesv1.PodResources{Containers: []*podresourcesv1.ContainerResources{{CpuIds: []int64{-1}}}}
		require.NoError(t, s.constrainFirecrackerResources(resources), "other hypervisors and UMA Firecracker ignore the NUMA planner's inputs")
		require.Empty(t, s.config.HypervisorConfig.AllocatedCPUs)
	}
}
