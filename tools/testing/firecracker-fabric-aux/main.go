// Node-scoped NIC/NVSwitch allocation. GPUs are allocated only by nvidia.com/gpu.
package main

import (
	"context"
	"fmt"
	"log"
	"net"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
	dp "k8s.io/kubelet/pkg/apis/deviceplugin/v1beta1"
)

const (
	resourceName = "lli.coreweave.com/fabric-aux"
	deviceID     = "s2kxfk44"
	endpoint     = "lli-s2kxfk44-fabric-aux.sock"
)

type endpointDevice struct{ BDF, VendorDevice string }

var fabric = []endpointDevice{
	{"0000:1a:00.0", "0x15b3:0x1021"},
	{"0000:3c:00.0", "0x15b3:0x1021"},
	{"0000:4d:00.0", "0x15b3:0x1021"},
	{"0000:5e:00.0", "0x15b3:0x1021"},
	{"0000:83:00.0", "0x10de:0x22a3"}, {"0000:84:00.0", "0x10de:0x22a3"},
	{"0000:85:00.0", "0x10de:0x22a3"}, {"0000:86:00.0", "0x10de:0x22a3"},
	{"0000:9c:00.0", "0x15b3:0x1021"},
	{"0000:bc:00.0", "0x15b3:0x1021"},
	{"0000:cc:00.0", "0x15b3:0x1021"},
	{"0000:dc:00.0", "0x15b3:0x1021"},
}

type plugin struct {
	dp.UnimplementedDevicePluginServer
	root string
}

// IOMMU group numbers are kernel enumeration results and may change on reboot.
// Resolve them from the stable PCI BDF and verify singleton isolation each time.
func (p *plugin) iommuGroup(d endpointDevice) (string, error) {
	link, err := os.Readlink(filepath.Join(p.root, "sys/bus/pci/devices", d.BDF, "iommu_group"))
	if err != nil {
		return "", err
	}
	group := filepath.Base(link)
	n, err := strconv.ParseUint(group, 10, 32)
	if err != nil || strconv.FormatUint(n, 10) != group {
		return "", fmt.Errorf("invalid group %q for %s", group, d.BDF)
	}
	members, err := os.ReadDir(filepath.Join(p.root, "sys/kernel/iommu_groups", group, "devices"))
	if err != nil || len(members) != 1 || members[0].Name() != d.BDF {
		return "", fmt.Errorf("non-isolated group %s for %s", group, d.BDF)
	}
	return group, nil
}

func (p *plugin) healthy() error {
	for _, d := range fabric {
		path := filepath.Join(p.root, "sys/bus/pci/devices", d.BDF)
		vendor, err := os.ReadFile(filepath.Join(path, "vendor"))
		if err != nil {
			return err
		}
		device, err := os.ReadFile(filepath.Join(path, "device"))
		if err != nil {
			return err
		}
		if strings.TrimSpace(string(vendor))+":"+strings.TrimSpace(string(device)) != d.VendorDevice {
			return fmt.Errorf("unexpected identity %s", d.BDF)
		}
		driver, err := os.Readlink(filepath.Join(path, "driver"))
		if err != nil || filepath.Base(driver) != "vfio-pci" {
			return fmt.Errorf("%s is not VFIO-owned", d.BDF)
		}
		group, err := p.iommuGroup(d)
		if err != nil {
			return err
		}
		info, err := os.Stat(filepath.Join(p.root, "dev/vfio", group))
		if err != nil || info.Mode()&os.ModeCharDevice == 0 {
			return fmt.Errorf("missing VFIO character device %s", group)
		}
	}
	return nil
}

func (*plugin) GetDevicePluginOptions(context.Context, *dp.Empty) (*dp.DevicePluginOptions, error) {
	return &dp.DevicePluginOptions{}, nil
}
func (p *plugin) ListAndWatch(_ *dp.Empty, stream dp.DevicePlugin_ListAndWatchServer) error {
	for {
		health := dp.Healthy
		if err := p.healthy(); err != nil {
			health = dp.Unhealthy
			log.Print(err)
		}
		if err := stream.Send(&dp.ListAndWatchResponse{Devices: []*dp.Device{{ID: deviceID, Health: health}}}); err != nil {
			return err
		}
		select {
		case <-stream.Context().Done():
			return stream.Context().Err()
		case <-time.After(5 * time.Second):
		}
	}
}
func (p *plugin) Allocate(_ context.Context, req *dp.AllocateRequest) (*dp.AllocateResponse, error) {
	if err := p.healthy(); err != nil {
		return nil, err
	}
	if len(req.ContainerRequests) != 1 || len(req.ContainerRequests[0].DevicesIDs) != 1 || req.ContainerRequests[0].DevicesIDs[0] != deviceID {
		return nil, fmt.Errorf("only one NIC/NVSwitch auxiliary allocation is supported")
	}
	// Kubelet persists this response across host reboot. Cache only the stable
	// CDI identity; the per-boot CDI file resolves current IOMMU group numbers.
	return &dp.AllocateResponse{ContainerResponses: []*dp.ContainerAllocateResponse{{
		CDIDevices: []*dp.CDIDevice{{Name: resourceName + "=" + deviceID}},
	}}}, nil
}
func (*plugin) PreStartContainer(context.Context, *dp.PreStartContainerRequest) (*dp.PreStartContainerResponse, error) {
	return &dp.PreStartContainerResponse{}, nil
}

func main() {
	if os.Getenv("NODE_NAME") != deviceID {
		log.Fatal("refusing to serve another host")
	}
	p := &plugin{root: "/host"}
	socket := filepath.Join(dp.DevicePluginPath, endpoint)
	// Re-register after kubelet removes our socket. Never remove arbitrary paths.
	for {
		if err := os.Remove(socket); err != nil && !os.IsNotExist(err) {
			log.Fatal(err)
		}
		lis, err := net.Listen("unix", socket)
		if err != nil {
			log.Fatal(err)
		}
		server := grpc.NewServer()
		dp.RegisterDevicePluginServer(server, p)
		go func() {
			if err := server.Serve(lis); err != nil {
				log.Print(err)
			}
		}()
		ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
		conn, err := grpc.DialContext(ctx, "unix://"+dp.KubeletSocket, grpc.WithTransportCredentials(insecure.NewCredentials()), grpc.WithBlock())
		if err == nil {
			_, err = dp.NewRegistrationClient(conn).Register(ctx, &dp.RegisterRequest{Version: dp.Version, Endpoint: endpoint, ResourceName: resourceName})
			conn.Close()
		}
		cancel()
		if err != nil {
			server.Stop()
			lis.Close()
			time.Sleep(3 * time.Second)
			continue
		}
		log.Print("registered auxiliary fabric (0 GPU, 4 NVSwitch, 8 IB); GPUs use nvidia.com/gpu")
		for {
			time.Sleep(3 * time.Second)
			if _, err := os.Stat(socket); err != nil {
				break
			}
		}
		server.Stop()
		lis.Close()
	}
}
