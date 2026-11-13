// Copyright (c) 2019 ARM Limited
//
// SPDX-License-Identifier: Apache-2.0
//

package types

import (
	"github.com/kata-containers/kata-containers/src/runtime/virtcontainers/pkg/firecracker/client/models"
)

type FcConfig struct {
	BootSource *models.BootSource `json:"boot-source"`

	MachineConfig *models.MachineConfiguration `json:"machine-config"`

	Vsock *models.Vsock `json:"vsock,omitempty"`

	Logger *models.Logger `json:"logger,omitempty"`

	Metrics *models.Metrics `json:"metrics,omitempty"`

	Drives []*models.Drive `json:"drives,omitempty"`

	NetworkInterfaces []*models.NetworkInterface `json:"network-interfaces,omitempty"`

	// Experimental Firecracker PR #6245 configuration, not the upstream
	// feature/vfio branch's differently named device-passthrough API.
	VFIO []FcVFIODevice `json:"vfio,omitempty"`
	// Private cold-boot NUMA and explicit PCI graph APIs.
	NUMA        *FcNUMAConfig  `json:"numa,omitempty"`
	PCITopology *FcPCITopology `json:"pci-topology,omitempty"`
}

type FcNUMAConfig struct {
	Nodes     []FcNUMANode `json:"nodes"`
	Distances [][]uint32   `json:"distances"`
}

type FcNUMANode struct {
	HostNode     uint32   `json:"host_node"`
	VCPUs        []uint32 `json:"vcpus"`
	HostCPUs     []uint32 `json:"host_cpus"`
	MemoryMiB    uint64   `json:"memory_mib"`
	PCIRootSlots []uint32 `json:"-"`
}

type FcVFIODevice struct {
	ID   string `json:"id"`
	Path string `json:"path"`
	// Do not use []uint8: encoding/json serializes byte slices as base64.
	PCIPath    []uint32      `json:"-"`
	PCIAddress *FcPCIAddress `json:"pci_address,omitempty"`
}

// FcPCIAddress is a segment-zero guest BDF, independent of host bus numbers.
type FcPCIAddress struct {
	Bus      uint32 `json:"bus"`
	Device   uint32 `json:"device"`
	Function uint32 `json:"function"`
}

type FcPCIRoot struct {
	Bus     uint32 `json:"bus"`
	LastBus uint32 `json:"last_bus"`
	// Guest proximity domain, not the physical host node ID.
	NUMANode *uint32 `json:"numa_node,omitempty"`
}

type FcPCIBridge struct {
	PCIAddress     FcPCIAddress `json:"pci_address"`
	SecondaryBus   uint32       `json:"secondary_bus"`
	SubordinateBus uint32       `json:"subordinate_bus"`
	Type           string       `json:"type"`
}

type FcPCITopology struct {
	Roots   []FcPCIRoot   `json:"roots"`
	Bridges []FcPCIBridge `json:"bridges"`
}
