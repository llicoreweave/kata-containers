// Copyright (c) 2025 NVIDIA CORPORATION.
//
// SPDX-License-Identifier: Apache-2.0
//

package containerdshim

import (
	"context"
	"fmt"
	"net"
	"strings"

	"github.com/kata-containers/kata-containers/src/runtime/pkg/device/config"
	vc "github.com/kata-containers/kata-containers/src/runtime/virtcontainers"
	"github.com/kata-containers/kata-containers/src/runtime/virtcontainers/pkg/cpuset"
	"github.com/opencontainers/runtime-spec/specs-go"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
	podresourcesv1 "k8s.io/kubelet/pkg/apis/podresources/v1"
)

const (
	// containerd CRI annotations
	nameAnnotation      = "io.kubernetes.cri.sandbox-name"
	namespaceAnnotation = "io.kubernetes.cri.sandbox-namespace"

	// CRI-O annotations
	crioNameAnnotation      = "io.kubernetes.cri-o.KubeName"
	crioNamespaceAnnotation = "io.kubernetes.cri-o.Namespace"
)

// coldPlugDevices handles cold plug of CDI devices into the sandbox
// kubelet's PodResources API is used for determining the devices to be
// cold plugged, if so configured. Otherwise, cdi annotations can be used for
// covering stand alone scenarios.
func coldPlugDevices(ctx context.Context, s *service, ociSpec *specs.Spec) error {
	if s.config.HypervisorConfig.ColdPlugVFIO == config.NoPort {
		// device cold plug is not enabled
		shimLog.Debug("cold_plug_vfio not enabled, skip device cold plug")
		return nil
	}

	kubeletSock := s.config.PodResourceAPISock
	if kubeletSock != "" {
		return coldPlugWithAPI(ctx, s, ociSpec)
	}

	shimLog.Debug("config.PodResourceAPISock not set, skip k8s based device cold plug")

	// Here we deal with CDI devices that are cold-plugged
	// for the single_container (nerdctl, podman, ...) use-case.
	// We can provide additional directories where to search for
	// CDI specs if needed. immutable OS's only have specific
	// directories where applications can write too. For instance /opt/cdi
	_, err := config.WithCDI(ociSpec.Annotations, []string{}, ociSpec)
	if err != nil {
		return fmt.Errorf("CDI device injection failed: %w", err)
	}
	return nil
}

func coldPlugWithAPI(ctx context.Context, s *service, ociSpec *specs.Spec) error {
	ann := ociSpec.Annotations
	resources, err := getPodResourceSnapshot(ctx, s.config.PodResourceAPISock, ann)
	if err != nil {
		return err
	}
	if err := s.constrainFirecrackerResources(resources); err != nil {
		return err
	}
	devices := podResourceDeviceSpec(resources)

	if len(devices) == 0 {
		shimLog.WithField("pod", debugPodID(ann)).Debug("No devices found in Pod Resources, skip cold plug")
		return nil
	}

	err = config.InjectCDIDevices(ociSpec, devices)
	if err != nil {
		return fmt.Errorf("cold plug: CDI device injection failed: %w", err)
	}

	return nil
}

// getPodResourceSnapshot fetches the pod's resources before sandbox startup.
// CRI does not include device allocations in its RunPodSandbox request.
func getPodResourceSnapshot(ctx context.Context, socket string, ann map[string]string) (*podresourcesv1.PodResources, error) {
	podName, podNs := getPodIdentifiers(ann)

	// create dialer for unix socket
	dialer := func(ctx context.Context, target string) (net.Conn, error) {
		// need this workaround to avoid duplicate prefix
		addr := strings.TrimPrefix(target, "unix://")
		return (&net.Dialer{}).DialContext(ctx, "unix", addr)
	}

	target := fmt.Sprintf("unix://%s", socket)
	conn, err := grpc.NewClient(
		target,
		grpc.WithTransportCredentials(insecure.NewCredentials()),
		grpc.WithContextDialer(dialer),
		grpc.WithDefaultCallOptions(grpc.MaxCallRecvMsgSize(16*1024*1024)),
	)

	if err != nil {
		return nil, fmt.Errorf("cold plug: failed to connect to kubelet: %w", err)
	}
	defer conn.Close()

	// create client
	client := podresourcesv1.NewPodResourcesListerClient(conn)

	// get all pod resources
	prr := &podresourcesv1.GetPodResourcesRequest{
		PodName:      podName,
		PodNamespace: podNs,
	}
	resp, err := client.Get(ctx, prr)
	if err != nil {
		return nil, fmt.Errorf("cold plug: GetPodResources failed for pod(%s) in namespace(%s): %w", podName, podNs, err)
	}
	podRes := resp.PodResources
	if podRes == nil {
		return nil, fmt.Errorf("cold plug: PodResources is nil")
	}

	return podRes, nil
}

func podResourceDeviceSpec(podRes *podresourcesv1.PodResources) []string {
	var devices []string
	for _, container := range podRes.Containers {
		for _, d := range container.Devices {
			shimLog.WithField("container", container.Name).Debugf("Pod Resources Device: %s = %v\n",
				d.ResourceName, d.DeviceIds)
			cdiDevs := formatCDIDevIDs(d.ResourceName, d.DeviceIds)
			devices = append(devices, cdiDevs...)
		}
	}

	return devices
}

// Keep kubelet's explicit CPUManager/MemoryManager assignments with the same
// device snapshot used for cold plug. This does not reserve additional CPUs or
// memory, and must not change resource behavior for other hypervisors.
func (s *service) constrainFirecrackerResources(resources *podresourcesv1.PodResources) error {
	if s.config.HypervisorType != vc.FirecrackerHypervisor || len(s.config.HypervisorConfig.GuestNUMANodes) == 0 {
		return nil
	}
	cpus, mems := cpuset.NewCPUSet(), cpuset.NewCPUSet()
	for _, container := range resources.Containers {
		for _, cpu := range container.CpuIds {
			if cpu < 0 || cpu >= 1024 {
				return fmt.Errorf("invalid allocated Firecracker host CPU %d", cpu)
			}
			cpus = cpus.Union(cpuset.NewCPUSet(int(cpu)))
		}
		for _, memory := range container.Memory {
			if memory.Topology == nil {
				continue
			}
			for _, node := range memory.Topology.Nodes {
				if node.ID < 0 || node.ID >= 64 {
					return fmt.Errorf("invalid allocated Firecracker host memory node %d", node.ID)
				}
				mems = mems.Union(cpuset.NewCPUSet(int(node.ID)))
			}
		}
	}
	s.config.HypervisorConfig.AllocatedCPUs = cpus.String()
	s.config.HypervisorConfig.AllocatedMems = mems.String()
	return nil
}

// formatCDIDevIDs formats the way CDI package expects
func formatCDIDevIDs(specName string, devIDs []string) []string {
	var result []string
	for _, id := range devIDs {
		result = append(result, fmt.Sprintf("%s=%s", specName, id))
	}
	return result
}

// getPodIdentifiers returns the pod name and namespace from annotations.
// It first checks containerd CRI annotations, then falls back to CRI-O annotations.
func getPodIdentifiers(ann map[string]string) (podName, podNamespace string) {
	podName = ann[nameAnnotation]
	podNamespace = ann[namespaceAnnotation]

	// Fall back to CRI-O annotations if containerd annotations are empty
	if podName == "" {
		podName = ann[crioNameAnnotation]
	}
	if podNamespace == "" {
		podNamespace = ann[crioNamespaceAnnotation]
	}

	return podName, podNamespace
}

func debugPodID(ann map[string]string) string {
	podName, podNamespace := getPodIdentifiers(ann)
	return fmt.Sprintf("%s/%s", podNamespace, podName)
}
