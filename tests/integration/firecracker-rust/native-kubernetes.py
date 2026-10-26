#!/usr/bin/env python3
# Copyright (c) 2026 Kata Contributors
# SPDX-License-Identifier: Apache-2.0
"""Validate preconfigured Rust Firecracker handlers through node-local kubectl."""
import argparse
import copy
import http.client
import json
import os
from pathlib import Path
import shlex
import signal
import socket
import struct
import subprocess
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--namespace', required=True)
parser.add_argument('--node', required=True)
parser.add_argument('--handler-prefix', required=True)
parser.add_argument('--expected-shim', required=True, type=Path)
parser.add_argument('--pod-template', required=True, type=Path)
parser.add_argument('--runtimeclass-template', required=True, type=Path)
parser.add_argument('--output', required=True, type=Path)
parser.add_argument('--kubeconfig', type=Path)
parser.add_argument('--owner-annotation', required=True, help='Namespace ownership key=value')
parser.add_argument('--expected-serial')
parser.add_argument('--cycles', default=10, type=int)
parser.add_argument('--memory-mib', default=128, type=int)
parser.add_argument('--vcpus', default=2, type=int)
parser.add_argument('--dns-name', default='kubernetes.default.svc.cluster.local')
parser.add_argument('--crash-cleanup', action='store_true', help='Add VMM-only and VMM/shim crash cleanup lifecycles per variant')
args = parser.parse_args()
assert args.cycles > 0
assert args.expected_shim.is_absolute()
NAMESPACE = args.namespace
NODE = args.node
OUTPUT = args.output
OUTPUT.mkdir(parents=True, exist_ok=True)
if args.kubeconfig:
    os.environ['KUBECONFIG'] = str(args.kubeconfig)


def command(*args, data=None, timeout=150):
    result = subprocess.run(args, input=data, text=True, capture_output=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f'{args}: {result.stdout}\n{result.stderr}')
    return result.stdout


def kubectl(*args, data=None, timeout=150):
    return command('kubectl', '--request-timeout=20s', *args, data=data, timeout=timeout)


class UnixConnection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__('localhost', timeout=10)
        self.path = str(path)

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def request(path, method, target, value=None):
    connection = UnixConnection(path)
    connection.connect()
    pid = struct.unpack('3i', connection.sock.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))[0]
    try:
        body = json.dumps(value) if value is not None else None
        connection.request(method, target, body=body, headers={'Content-Type': 'application/json'})
        response = connection.getresponse()
        data = response.read().decode()
        assert 200 <= response.status < 300, (target, response.status, data)
        return data, pid
    finally:
        connection.close()


def ready_sandboxes():
    info = json.loads(command('crictl', 'pods', '-o', 'json'))
    return [pod for pod in info['items'] if pod.get('metadata', {}).get('namespace') == NAMESPACE
            and pod.get('metadata', {}).get('name', '').startswith('firecracker-rust-native')
            and pod['state'] == 'SANDBOX_READY']


def crash_runtime(name, sid, vmm_pid, shim_pid, kill_shim=True):
    # Freeze the verified task shim before deleting the Pod, preventing normal
    # teardown from erasing the checkpoint we are testing. Pod deletion prevents
    # kubelet from starting a replacement VM after these task processes exit.
    pids = [vmm_pid, shim_pid]
    identities = [Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()[19]
                  for pid in pids]
    fds = [os.pidfd_open(pid) for pid in pids]
    frozen = False
    try:
        assert os.readlink(f'/proc/{shim_pid}/exe') == str(args.expected_shim)
        assert all(Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()[19] == identity
                   for pid, identity in zip(pids, identities))
        checkpoint = Path('/run/kata') / sid / 'state.json'
        saved = json.loads(checkpoint.read_text())
        assert saved['hypervisor']['id'] == sid
        assert saved['hypervisor']['pid'] == vmm_pid
        signal.pidfd_send_signal(fds[1], signal.SIGSTOP)
        frozen = True
        kubectl('delete', 'pod', '-n', NAMESPACE, name, '--wait=false', '--grace-period=0', '--force')
        signal.pidfd_send_signal(fds[0], signal.SIGKILL)
        signal.pidfd_send_signal(fds[1], signal.SIGKILL if kill_shim else signal.SIGCONT)
        frozen = False
        # CRI stop also exercises cleanup retries if containerd's asynchronous
        # shim-exit cleanup and kubelet's deletion arrive in the opposite order.
        command('crictl', '--timeout=10s', 'stopp', sid, timeout=30)
        return {'checkpoint': str(checkpoint), 'vmm_pid': vmm_pid, 'shim_pid': shim_pid,
                'shim_killed': kill_shim,
                'vmm_start_time': identities[0], 'shim_start_time': identities[1]}
    finally:
        if frozen:
            try:
                signal.pidfd_send_signal(fds[1], signal.SIGCONT)
            except ProcessLookupError:
                pass
        for fd in fds:
            os.close(fd)


namespace = json.loads(kubectl('get', 'namespace', NAMESPACE, '-o', 'json'))
owner_key, owner_value = args.owner_annotation.split('=', 1)
assert namespace['metadata'].get('annotations', {}).get(owner_key) == owner_value
node = json.loads(kubectl('get', 'node', NODE, '-o', 'json'))
assert any(condition['type'] == 'Ready' and condition['status'] == 'True'
           for condition in node['status']['conditions'])
if args.expected_serial:
    assert Path('/sys/class/dmi/id/product_serial').read_text().strip() == args.expected_serial
base = json.loads(args.pod_template.read_text())
assert base['metadata']['namespace'] == NAMESPACE
assert base['spec']['nodeName'] == NODE
assert not ready_sandboxes(), 'Previous native test sandbox is still active'
results = []
cycles = args.cycles
for variant in ['jailed', 'unjailed']:
    handler = args.handler_prefix + ('' if variant == 'jailed' else '-unjailed')
    runtime = json.loads(args.runtimeclass_template.read_text())
    runtime['metadata']['name'] = handler
    runtime['handler'] = handler
    kubectl('apply', '--dry-run=server', '-f', '-', data=json.dumps(runtime))
    kubectl('apply', '-f', '-', data=json.dumps(runtime))
    total_cycles = cycles + 2 * int(args.crash_cleanup)
    for cycle in range(total_cycles):
        pod = copy.deepcopy(base)
        name = f'firecracker-rust-native-{variant}-{cycle}'
        pod['metadata']['name'] = name
        pod['spec']['runtimeClassName'] = handler
        record = {'variant': variant, 'cycle': cycle, 'pod': name, 'handler': handler,
                  'cleanup_mode': ('normal' if cycle < cycles else
                                   'vmm-crash' if cycle == cycles else 'crash')}
        results.append(record)
        print(f'Starting native {variant} pod cycle {cycle + 1}/{total_cycles} ({record["cleanup_mode"]} cleanup)', flush=True)
        kubectl('apply', '--dry-run=server', '-f', '-', data=json.dumps(pod))
        kubectl('apply', '-f', '-', data=json.dumps(pod))
        try:
            kubectl('wait', '-n', NAMESPACE, '--for=condition=Ready', 'pod/' + name, '--timeout=120s')
            status = json.loads(kubectl('get', 'pod', '-n', NAMESPACE, name, '-o', 'json'))
            (OUTPUT / (name + '.json')).write_text(json.dumps(status, indent=2) + '\n')
            record['pod_uid'] = status['metadata']['uid']
            record['pod_ip'] = status['status'].get('podIP')
            logs = kubectl('logs', '-n', NAMESPACE, name)
            assert 'native-kata-firecracker-rust-ready' in logs, logs
            (OUTPUT / (name + '.log')).write_text(logs)
            record['guest_exec'] = kubectl('exec', '-n', NAMESPACE, name, '--', '/bin/sh', '-c',
                'set -eu; echo rootfs-rw-ok > /tmp/firecracker-rust-ci; cat /tmp/firecracker-rust-ci; uname -r; ip -4 addr show eth0; cat /etc/resolv.conf; nslookup ' + shlex.quote(args.dns_name))
            assert 'rootfs-rw-ok' in record['guest_exec']
            sandboxes = ready_sandboxes()
            assert len(sandboxes) == 1, sandboxes
            sid = sandboxes[0]['id']
            record['sandbox_id'] = sid
            vm_dir = Path('/run/kata') / ('firecracker' if variant == 'jailed' else '') / sid
            api = vm_dir / ('root/run/fc.sock' if variant == 'jailed' else 'run/fc.sock')
            config, pid = request(api, 'GET', '/machine-config')
            record['vmm_pid'] = pid
            parent = next(line.split()[1] for line in Path(f'/proc/{pid}/status').read_text().splitlines() if line.startswith('PPid:'))
            record['shim_pid'] = int(parent)
            record['shim_executable'] = os.readlink(f'/proc/{parent}/exe')
            assert record['shim_executable'] == str(args.expected_shim), record['shim_executable']
            record['machine_config'] = json.loads(config)
            assert record['machine_config']['mem_size_mib'] == args.memory_mib, config
            assert record['machine_config']['vcpu_count'] == args.vcpus, config
            threads = [thread.joinpath('comm').read_text().strip() for thread in Path(f'/proc/{pid}/task').iterdir()]
            assert sorted(thread for thread in threads if thread.startswith('fc_vcpu ')) == sorted(f'fc_vcpu {index}' for index in range(args.vcpus)), threads
            record['vcpu_threads'] = [thread for thread in threads if thread.startswith('fc_vcpu ')]
            config, _ = request(api, 'GET', '/vm/config')
            record['vm_config'] = json.loads(config)
            drives = record['vm_config']['drives']
            assert any(drive['drive_id'] != 'rootfs' and drive.get('cache_type') == 'Writeback' for drive in drives), drives
            interfaces = record['vm_config'].get('network-interfaces', record['vm_config'].get('network_interfaces', []))
            assert interfaces and all(interface.get('rx_rate_limiter') and interface.get('tx_rate_limiter') for interface in interfaces), interfaces
            request(api, 'PUT', '/actions', {'action_type': 'FlushMetrics'})
            monitor = Path('/run/kata') / sid / 'shim-monitor.sock'
            deadline = time.monotonic() + 15
            while True:
                metrics, _ = request(monitor, 'GET', '/metrics')
                if 'kata_firecracker_vmm' in metrics:
                    break
                assert time.monotonic() < deadline, 'Firecracker metrics missing from native shim endpoint'
                time.sleep(0.1)
            (OUTPUT / (name + '-metrics.prom')).write_text(metrics)
            record['metrics_exported'] = True
            forwarded = command('journalctl', '--no-pager', '-o', 'cat', '--since=-5min', '_PID=' + str(record['shim_pid']), '--grep=Firecracker log')
            assert forwarded.strip(), 'Native Firecracker FIFO logs missing from shim journal'
            (OUTPUT / (name + '-firecracker-fifo.log')).write_text(forwarded)
            record['fifo_logs_forwarded'] = True
            if cycle >= cycles:
                record['crash_cleanup'] = crash_runtime(name, sid, pid, record['shim_pid'],
                                                       kill_shim=record['cleanup_mode'] == 'crash')
            record['result'] = 'features-passed'
            print(f'Native {variant} cycle {cycle + 1}: guest exec, DNS, vCPUs, cache/rate options, metrics passed', flush=True)
        except BaseException as error:
            record['result'] = 'failed'
            record['error'] = str(error)
            (OUTPUT / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
            try:
                description = kubectl('describe', 'pod', '-n', NAMESPACE, name)
            except RuntimeError as describe_error:
                description = str(describe_error)
            (OUTPUT / (name + '-failure-describe.log')).write_text(description)
            raise
        finally:
            kubectl('delete', 'pod', '-n', NAMESPACE, name, '--ignore-not-found=true', '--wait=true', '--timeout=90s', timeout=120)
            deadline = time.monotonic() + 90
            while ready_sandboxes():
                assert time.monotonic() < deadline, 'Native test sandbox remained active after deletion'
                time.sleep(1)
        try:
            deadline = time.monotonic() + 90
            while (vm_dir.exists() or (Path('/run/kata') / sid).exists()
                   or Path(f'/proc/{pid}').exists()):
                assert time.monotonic() < deadline, f'Native runtime cleanup leaked resources: {sid}'
                time.sleep(0.2)
        except BaseException as error:
            record['result'] = 'failed'
            record['error'] = str(error)
            (OUTPUT / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
            raise
        record['vm_directory_cleaned'] = True
        record['result'] = 'passed'
        (OUTPUT / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
print(f'Passed {len(results)} native Kubernetes pod lifecycles', flush=True)
