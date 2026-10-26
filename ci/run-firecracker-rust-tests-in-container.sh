#!/usr/bin/env bash
# Copyright (c) 2026 Kata Contributors
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
case "${1:-unit}" in
    unit)
        cargo fmt -p hypervisor -p resource -p virt_container -p runtimes -- --check
        cargo clippy --locked -p hypervisor -p resource -p virt_container -p runtimes --all-targets -- -D warnings
        make -C src/runtime-rs LIBC=gnu crates/shim/src/config.rs
        CARGO_PROFILE_DEV_DEBUG=0 cargo build --locked -p runtime-rs --bins
        cargo test --locked -p hypervisor firecracker:: -- --nocapture
        cargo test --locked -p hypervisor device::device_manager:: -- --nocapture
        cargo test --locked -p resource manager_inner::tests::network_runtime_bounds_stalled_api_io -- --nocapture
        cargo test --locked -p virt_container sandbox_persist::tests:: -- --nocapture
        ;;
    mounts)
        cargo test --locked -p hypervisor firecracker::tests::jailed_ -- --ignored --nocapture
        ;;
    integration)
        if [[ ! -c /dev/kvm ]]; then
            echo "Firecracker integration tests require /dev/kvm; refusing to skip VM validation." >&2
            exit 1
        fi
        if [[ -z "${FC_TEST_VERSION:-}" ]]; then unset FC_TEST_VERSION; fi
        python3 tests/integration/firecracker-rust/prepare.py
        export FC_TEST_BINARY="${PWD}/target/firecracker-rust/artifacts/firecracker"
        export FC_TEST_JAILER="${PWD}/target/firecracker-rust/artifacts/jailer"
        export FC_TEST_KERNEL="${PWD}/target/firecracker-rust/artifacts/vmlinux"
        export FC_TEST_ROOTFS="${PWD}/target/firecracker-rust/artifacts/rootfs.ext4"
        cargo test --locked -p hypervisor firecracker::integration_tests:: -- --ignored --test-threads=1 --nocapture
        ;;
    *) echo "Usage: $0 [unit|mounts|integration]" >&2; exit 2 ;;
esac
