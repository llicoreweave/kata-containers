//go:build linux

// SPDX-License-Identifier: Apache-2.0
package virtcontainers

import (
	"encoding/json"
	"fmt"
	"github.com/kata-containers/kata-containers/src/runtime/pkg/device/config"
	models "github.com/kata-containers/kata-containers/src/runtime/virtcontainers/pkg/firecracker/client/models"
	"github.com/kata-containers/kata-containers/src/runtime/virtcontainers/types"
	"github.com/stretchr/testify/require"
	"path/filepath"
	"testing"
)

func TestFCStaticTopologyPaths(t *testing.T) {
	fc := firecracker{config: HypervisorConfig{ColdPlugVFIO: config.SwitchPort}, fcConfig: &types.FcConfig{}}
	fc.fcConfig.Drives = make([]*models.Drive, 9)
	fc.fcConfig.NetworkInterfaces = make([]*models.NetworkInterface, 1)
	for _, bdf := range []string{"0000:19:00.0", "0000:1a:00.0", "0000:83:00.0"} {
		require.NoError(t, fc.fcAddVFIODevice(&config.VFIODev{ID: bdf, BDF: bdf, Type: config.VFIOPCIDeviceNormalType}))
	}
	ancestors := map[string]string{
		"0000:19:00.0": "/sys/devices/pci0000:15/0000:15:01.0/0000:16:00.0/0000:17:01.0/0000:19:00.0",
		"0000:1a:00.0": "/sys/devices/pci0000:15/0000:15:01.0/0000:16:00.0/0000:17:02.0/0000:1a:00.0",
		"0000:83:00.0": "/sys/devices/pci0000:80/0000:80:01.0/0000:81:00.0/0000:82:00.0/0000:83:00.0",
	}
	resolve := func(path string) (string, error) { return ancestors[filepath.Base(path)], nil }
	require.NoError(t, fc.resolveStaticVFIOPathsWith(resolve))
	require.Equal(t, []uint32{13, 0, 1, 0}, fc.fcConfig.VFIO[0].PCIPath)
	require.Equal(t, []uint32{13, 0, 2, 0}, fc.fcConfig.VFIO[1].PCIPath)
	require.Equal(t, []uint32{14, 0, 0, 0}, fc.fcConfig.VFIO[2].PCIPath)
	wire, err := json.Marshal(fc.fcConfig.VFIO[0])
	require.NoError(t, err)
	require.Contains(t, string(wire), `"pci_address":{"bus":3,"device":0,"function":0}`)
	require.Equal(t, "00/0d/00/01/00", fc.vfioDevices[0].GuestPciPath.String())
	ancestors["0000:19:00.0"] = "/sys/devices/pci0000:15/0000:15:01.0/0000:19:00.1"
	require.Error(t, fc.resolveStaticVFIOPathsWith(resolve))
	require.Error(t, fc.resolveStaticVFIOPathsWith(func(string) (string, error) { return "", fmt.Errorf("unavailable sysfs") }))
}
