package main

import (
	"context"
	"fmt"
	dp "k8s.io/kubelet/pkg/apis/deviceplugin/v1beta1"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestExactInventory(t *testing.T) {
	counts := map[string]int{}
	bdfs := map[string]bool{}
	for _, d := range fabric {
		if bdfs[d.BDF] {
			t.Fatal("duplicate identity")
		}
		if d.BDF == "0000:9d:00.0" {
			t.Fatal("management NIC must remain on host")
		}
		bdfs[d.BDF] = true
		counts[d.VendorDevice]++
	}
	if len(fabric) != 12 || counts["0x10de:0x2330"] != 0 || counts["0x10de:0x22a3"] != 4 || counts["0x15b3:0x1021"] != 8 {
		t.Fatal(counts)
	}
}
func TestFailClosedWithoutVFIO(t *testing.T) {
	p := &plugin{root: t.TempDir()}
	if p.healthy() == nil {
		t.Fatal("missing fabric accepted")
	}
	if _, err := p.Allocate(context.Background(), &dp.AllocateRequest{}); err == nil {
		t.Fatal("invalid allocation accepted")
	}
}

func fixtureGroup(t *testing.T, root, bdf, group string, extra bool) {
	t.Helper()
	pci := filepath.Join(root, "sys/bus/pci/devices", bdf)
	members := filepath.Join(root, "sys/kernel/iommu_groups", group, "devices")
	if err := os.MkdirAll(pci, 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(members, 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(members, bdf), nil, 0600); err != nil {
		t.Fatal(err)
	}
	if extra {
		if err := os.WriteFile(filepath.Join(members, "0000:9d:00.0"), nil, 0600); err != nil {
			t.Fatal(err)
		}
	}
	link := filepath.Join(pci, "iommu_group")
	os.Remove(link)
	if err := os.Symlink("/sys/kernel/iommu_groups/"+group, link); err != nil {
		t.Fatal(err)
	}
}
func TestIOMMUGroupRenumberedAfterReboot(t *testing.T) {
	p := &plugin{root: t.TempDir()}
	d := fabric[0]
	for _, group := range []string{"46", "205"} {
		fixtureGroup(t, p.root, d.BDF, group, false)
		got, err := p.iommuGroup(d)
		if err != nil || got != group {
			t.Fatalf("group=%s got=%s err=%v", group, got, err)
		}
	}
}
func TestRefuseSharedIOMMUGroup(t *testing.T) {
	p := &plugin{root: t.TempDir()}
	fixtureGroup(t, p.root, fabric[0].BDF, "205", true)
	if _, err := p.iommuGroup(fabric[0]); err == nil {
		t.Fatal("shared group accepted")
	}
}

func TestAllocationCachesStableCDIIdentityAfterGroupRenumber(t *testing.T) {
	p := &plugin{root: t.TempDir()}
	for _, offset := range []int{100, 200} {
		for i, d := range fabric {
			group := fmt.Sprint(offset + i)
			fixtureGroup(t, p.root, d.BDF, group, false)
			pci := filepath.Join(p.root, "sys/bus/pci/devices", d.BDF)
			identity := strings.Split(d.VendorDevice, ":")
			for j, name := range []string{"vendor", "device"} {
				if err := os.WriteFile(filepath.Join(pci, name), []byte(identity[j]), 0600); err != nil {
					t.Fatal(err)
				}
			}
			driver := filepath.Join(pci, "driver")
			os.Remove(driver)
			if err := os.Symlink("/sys/bus/pci/drivers/vfio-pci", driver); err != nil {
				t.Fatal(err)
			}
			vfio := filepath.Join(p.root, "dev/vfio")
			if err := os.MkdirAll(vfio, 0755); err != nil {
				t.Fatal(err)
			}
			if err := os.Symlink("/dev/null", filepath.Join(vfio, group)); err != nil {
				t.Fatal(err)
			}
		}
		response, err := p.Allocate(context.Background(), &dp.AllocateRequest{
			ContainerRequests: []*dp.ContainerAllocateRequest{{DevicesIDs: []string{deviceID}}},
		})
		if err != nil {
			t.Fatal(err)
		}
		got := response.ContainerResponses[0]
		if len(got.Devices) != 0 || len(got.CDIDevices) != 1 || got.CDIDevices[0].Name != resourceName+"="+deviceID {
			t.Fatalf("unstable allocation response: %+v", got)
		}
	}
}
