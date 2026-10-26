# Copyright (c) 2026 Kata Contributors
# SPDX-License-Identifier: Apache-2.0

ARG RUST_VERSION=1.97
FROM rust:${RUST_VERSION}-bookworm
ARG RUST_VERSION

RUN apt-get update && apt-get install -y --no-install-recommends \
    clang cmake libclang-dev libdevmapper-dev libseccomp-dev libssl-dev \
    pkg-config protobuf-compiler python3 shellcheck e2fsprogs busybox-static util-linux \
    && rm -rf /var/lib/apt/lists/* \
    && rustup toolchain install "${RUST_VERSION}" --profile minimal --component rustfmt,clippy

WORKDIR /kata
ENV CARGO_TARGET_DIR=/kata/target/firecracker-rust
