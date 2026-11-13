//go:build linux

// SPDX-License-Identifier: Apache-2.0

package virtcontainers

import (
	"fmt"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"

	"github.com/kata-containers/kata-containers/src/runtime/pkg/device/config"
	"github.com/kata-containers/kata-containers/src/runtime/virtcontainers/types"
)

// This test-only adapter targets Firecracker PR #6245 at
// 5aa6bd2619d4769c824eb6d65a49bee476cb8a32. Existing MMIO runtimes stay
// unchanged; cold_plug_vfio="root-port" opts a private runtime into PCI.
func (fc *firecracker) pciEnabled() bool {
	return fc.config.ColdPlugVFIO == config.RootPort || fc.config.ColdPlugVFIO == config.SwitchPort
}

func fcValidateVFIOConfig(h *HypervisorConfig) error {
	if h.HotPlugVFIO != "" && h.HotPlugVFIO != config.NoPort {
		return fmt.Errorf("experimental Firecracker VFIO supports cold-plug only")
	}
	if h.ColdPlugVFIO != "" && h.ColdPlugVFIO != config.NoPort && h.ColdPlugVFIO != config.RootPort && h.ColdPlugVFIO != config.SwitchPort {
		return fmt.Errorf("experimental Firecracker VFIO requires cold_plug_vfio=root-port")
	}
	if (h.ColdPlugVFIO == config.RootPort || h.ColdPlugVFIO == config.SwitchPort) && (h.VirtioMem || h.ReclaimGuestFreedMemory) {
		return fmt.Errorf("Firecracker VFIO cannot reclaim or hotplug DMA-pinned guest memory")
	}
	return nil
}

func fcDefaultKernelParams(pci bool) []Param {
	params := make([]Param, 0, len(fcKernelParams))
	for _, param := range fcKernelParams {
		if pci && (param.Key == "pci" || param.Key == "acpi") {
			continue
		}
		params = append(params, param)
	}
	return params
}

var fcVFIOBDF = regexp.MustCompile(`^[0-9a-fA-F]{4}:[0-9a-fA-F]{2}:[0-1][0-9a-fA-F]\.[0-7]$`)

func (fc *firecracker) fcAddVFIODevice(dev *config.VFIODev) error {
	if !fc.pciEnabled() {
		return fmt.Errorf("Firecracker VFIO requires an explicitly enabled PCI runtime")
	}
	if dev == nil || dev.Type != config.VFIOPCIDeviceNormalType || !fcVFIOBDF.MatchString(dev.BDF) || dev.ID == "" {
		return fmt.Errorf("Firecracker VFIO requires a normal PCI device with valid ID and full BDF")
	}
	for _, existing := range fc.vfioDevices {
		if existing.ID == dev.ID || existing.BDF == dev.BDF {
			return fmt.Errorf("duplicate Firecracker VFIO device %s (%s)", dev.ID, dev.BDF)
		}
	}
	fc.vfioDevices = append(fc.vfioDevices, dev)
	fc.fcConfig.VFIO = append(fc.fcConfig.VFIO, types.FcVFIODevice{
		ID: dev.ID, Path: filepath.Join("/sys/bus/pci/devices", dev.BDF),
	})
	return nil
}

func (fc *firecracker) vfioJailerArgs() []string {
	var args []string
	if len(fc.vfioDevices) != 0 {
		args = append(args, "--resource-limit", "memlock="+strconv.FormatUint(uint64(fc.config.MemorySize)<<20, 10))
		for _, dev := range fc.fcConfig.VFIO {
			args = append(args, "--vfio-device", dev.Path)
		}
	}
	if fc.fcConfig != nil && fc.fcConfig.NUMA != nil {
		for _, node := range fc.fcConfig.NUMA.Nodes {
			args = append(args, "--numa-node", strconv.FormatUint(uint64(node.HostNode), 10))
		}
	}
	return args
}

func (fc *firecracker) resolveVFIOPaths() error {
	if fc.config.ColdPlugVFIO == config.SwitchPort {
		return fc.resolveStaticVFIOPaths()
	}
	// In the pinned VMM builder, slot 0 is the host bridge, followed by
	// block disks, network interfaces, VFIO, then vsock and entropy. There
	// is no balloon, pmem or memory-hotplug device in this Kata adapter.
	firstSlot := 1 + len(fc.fcConfig.Drives) + len(fc.fcConfig.NetworkInterfaces)
	paths := make([]types.PciPath, len(fc.vfioDevices))
	for i := range fc.vfioDevices {
		slot, err := types.PciSlotFromInt(firstSlot + i)
		if err != nil {
			return err
		}
		paths[i], err = types.PciPathFromSlots(slot)
		if err != nil {
			return err
		}
	}
	for i, dev := range fc.vfioDevices {
		dev.GuestPciPath = paths[i]
	}
	return nil
}

// Resolve only the ancestry of explicitly allocated endpoints. Host bridges and
// unallocated siblings are not assigned to the VM. Guest bus numbering is a VMM detail.
func (fc *firecracker) resolveStaticVFIOPaths() error {
	return fc.resolveStaticVFIOPathsWith(filepath.EvalSymlinks)
}

func (fc *firecracker) resolveStaticVFIOPathsWith(resolve func(string) (string, error)) error {
	physical := make([][]string, len(fc.vfioDevices))
	rootSet := map[string]bool{}
	for i, device := range fc.vfioDevices {
		resolved, err := resolve(filepath.Join("/sys/bus/pci/devices", device.BDF))
		if err != nil {
			return fmt.Errorf("resolving allocated PCI ancestry: %w", err)
		}
		for _, component := range strings.Split(resolved, "/") {
			if fcVFIOBDF.MatchString(component) {
				if !strings.HasSuffix(component, ".0") {
					return fmt.Errorf("static topology prototype requires function zero: %s", component)
				}
				physical[i] = append(physical[i], component)
			}
		}
		if len(physical[i]) < 2 {
			return fmt.Errorf("allocated device lacks PCI bridge ancestry: %s", device.BDF)
		}
		rootSet[physical[i][0]] = true
	}
	roots := make([]string, 0, len(rootSet))
	for root := range rootSet {
		roots = append(roots, root)
	}
	sort.Strings(roots)
	// Reserve the two root-bus slots used by vsock and entropy after VFIO attach.
	first := 3 + len(fc.fcConfig.Drives) + len(fc.fcConfig.NetworkInterfaces)
	if first+len(roots) > 32 {
		return fmt.Errorf("static topology exhausts root-bus slots")
	}
	for i, device := range fc.vfioDevices {
		rootIndex := sort.SearchStrings(roots, physical[i][0])
		path := []uint32{uint32(first + rootIndex)}
		for _, component := range physical[i][1:] {
			slot, err := strconv.ParseUint(component[8:10], 16, 8)
			if err != nil {
				return err
			}
			path = append(path, uint32(slot))
		}
		// The pinned private guest agent treats the first component of paths
		// with >=3 elements as the PCI root-complex bus. Explicitly include
		// bus 00 for its device-tree protocol; the VMM path has no such prefix.
		slots := make([]types.PciSlot, len(path)+1)
		rootBus, err := types.PciSlotFromInt(0)
		if err != nil {
			return err
		}
		slots[0] = rootBus
		for j, number := range path {
			slot, err := types.PciSlotFromInt(int(number))
			if err != nil {
				return err
			}
			slots[j+1] = slot
		}
		guestPath, err := types.PciPathFromSlots(slots...)
		if err != nil {
			return err
		}
		device.GuestPciPath = guestPath
		fc.fcConfig.VFIO[i].PCIPath = path
	}
	return nil
}
