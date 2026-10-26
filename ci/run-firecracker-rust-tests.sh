#!/usr/bin/env bash
# Copyright (c) 2026 Kata Contributors
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${root_dir}"
mode="${1:-unit}"
case "${mode}" in
    unit|mounts|integration) ;;
    *) echo "Usage: $0 [unit|mounts|integration]" >&2; exit 2 ;;
esac
rust_version="$(sed -n 's/^channel = "\([^"]*\)"/\1/p' rust-toolchain.toml)"
test -n "${rust_version}"
image="${FC_CI_IMAGE:-kata-firecracker-rust-ci:local}"
docker build --build-arg "RUST_VERSION=${rust_version}" -t "${image}" \
    -f tests/integration/firecracker-rust/Dockerfile .
mkdir -p target/firecracker-rust/results
options=(--rm --init --mount "type=bind,source=${root_dir},target=/kata"
    --mount "type=volume,source=kata-firecracker-rust-registry,target=/usr/local/cargo/registry"
    --mount "type=volume,source=kata-firecracker-rust-git,target=/usr/local/cargo/git"
    --env RUST_BACKTRACE=1 --env GIT_CONFIG_COUNT=1
    --env GIT_CONFIG_KEY_0=safe.directory --env GIT_CONFIG_VALUE_0=/kata)
if [[ "${mode}" != unit ]]; then
    options+=(--privileged)
fi
options+=(--env "FC_TEST_VERSION=${FC_TEST_VERSION:-}")
docker run "${options[@]}" "${image}" bash ci/run-firecracker-rust-tests-in-container.sh "${mode}" \
    2>&1 | tee "target/firecracker-rust/results/${mode}.log"
