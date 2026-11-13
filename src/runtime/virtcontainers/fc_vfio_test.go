//go:build linux

// SPDX-License-Identifier: Apache-2.0

package virtcontainers

import (
	"encoding/json"
	"testing"

	"github.com/kata-containers/kata-containers/src/runtime/pkg/device/config"
	"github.com/kata-containers/kata-containers/src/runtime/virtcontainers/pkg/firecracker/client/models"
	"github.com/kata-containers/kata-containers/src/runtime/virtcontainers/types"
	"github.com/stretchr/testify/require"
)

func TestFCVFIOOptIn(t *testing.T) {
	for _, h := range []HypervisorConfig{{}, {ColdPlugVFIO: config.NoPort}, {ColdPlugVFIO: config.RootPort}, {ColdPlugVFIO: config.SwitchPort}} {
		fc := firecracker{fcConfig: &types.FcConfig{}}
		require.NoError(t, fc.setConfig(&h))
		require.Equal(t, h.ColdPlugVFIO == config.RootPort || h.ColdPlugVFIO == config.SwitchPort, fc.pciEnabled())
	}
	for _, h := range []HypervisorConfig{
		{HotPlugVFIO: config.RootPort},
		{ColdPlugVFIO: config.SwitchPort, VirtioMem: true},
		{ColdPlugVFIO: config.RootPort, VirtioMem: true},
		{ColdPlugVFIO: config.RootPort, ReclaimGuestFreedMemory: true},
	} {
		require.Error(t, fcValidateVFIOConfig(&h))
	}
}

func TestFCVFIOKernelDefaults(t *testing.T) {
	before := append([]Param(nil), fcKernelParams...)
	pci := fcDefaultKernelParams(true)
	for _, p := range pci {
		require.NotEqual(t, "pci", p.Key)
		require.NotEqual(t, "acpi", p.Key)
	}
	pci[0].Value = "changed"
	require.Equal(t, before, fcKernelParams)
	require.Equal(t, before, fcDefaultKernelParams(false))
}

func TestFCVFIODevicesAndPaths(t *testing.T) {
	fc := firecracker{config: HypervisorConfig{ColdPlugVFIO: config.RootPort, MemorySize: 16384}, fcConfig: &types.FcConfig{}}
	dev := &config.VFIODev{ID: "gpu0", BDF: "0000:19:00.0", Type: config.VFIOPCIDeviceNormalType}
	require.NoError(t, fc.fcAddVFIODevice(dev))
	require.Error(t, fc.fcAddVFIODevice(dev))
	require.Error(t, fc.fcAddVFIODevice(nil))
	require.Error(t, fc.fcAddVFIODevice(&config.VFIODev{ID: "unsafe", BDF: "../vfio", Type: config.VFIOPCIDeviceNormalType}))
	require.Error(t, fc.fcAddVFIODevice(&config.VFIODev{ID: "mdev", BDF: "0000:19:00.1", Type: config.VFIOPCIDeviceMediatedType}))
	fc.fcConfig.Drives = make([]*models.Drive, 9)
	fc.fcConfig.NetworkInterfaces = make([]*models.NetworkInterface, 1)
	require.NoError(t, fc.resolveVFIOPaths())
	require.Equal(t, "0b", dev.GuestPciPath.String())
	require.Equal(t, []string{"--resource-limit", "memlock=17179869184", "--vfio-device", "/sys/bus/pci/devices/0000:19:00.0"}, fc.vfioJailerArgs())
	data, err := json.Marshal(fc.fcConfig)
	require.NoError(t, err)
	require.Contains(t, string(data), `"vfio":[{"id":"gpu0","path":"/sys/bus/pci/devices/0000:19:00.0","pci_address":{"bus":0,"device":11,"function":0}}]`)
	fc.fcConfig.Drives = make([]*models.Drive, 31)
	require.Error(t, fc.resolveVFIOPaths())

	regular := firecracker{fcConfig: &types.FcConfig{}}
	require.Error(t, regular.fcAddVFIODevice(dev))
	require.Empty(t, regular.vfioJailerArgs())
}
