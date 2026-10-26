#!/usr/bin/env python3
# Copyright (c) 2026 Kata Contributors
# SPDX-License-Identifier: Apache-2.0

"""Fetch verified Firecracker artifacts and build a small, local test guest."""

import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tarfile
import tempfile
import urllib.request


ROOT = Path(__file__).resolve().parents[3]
OUTPUT = ROOT / "target/firecracker-rust/artifacts"
KERNEL_PREFIX = "firecracker-ci/20261021-1e5fcc27b59f-0"


def download(url, destination):
    with urllib.request.urlopen(url, timeout=60) as response:
        with destination.open("wb") as output:
            shutil.copyfileobj(response, output)


def sha256(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def main():
    OUTPUT.mkdir(parents=True, exist_ok=True)
    arch = platform.machine()
    pins = json.loads((Path(__file__).parent / "kernels.json").read_text())
    if arch not in pins:
        raise RuntimeError(f"unsupported Firecracker architecture: {arch}")
    configured = re.search(
        r'^    firecracker:\n(?:.*\n)*?      version: "([^"]+)"',
        (ROOT / "versions.yaml").read_text(), re.MULTILINE,
    ).group(1)
    version = os.environ.get("FC_TEST_VERSION", configured)
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", version):
        raise RuntimeError(f"invalid Firecracker release: {version}")
    archive = OUTPUT / f"firecracker-{version}-{arch}.tgz"
    url = f"https://github.com/firecracker-microvm/firecracker/releases/download/{version}/{archive.name}"
    checksum = OUTPUT / "release.sha256"
    download(url + ".sha256.txt", checksum)
    expected = checksum.read_text().split()[0]
    download(url, archive)
    if sha256(archive) != expected:
        raise RuntimeError("Firecracker release checksum mismatch")
    # Extract only the two expected regular files, into fixed filenames. Never
    # trust paths or links from the downloaded archive.
    with tarfile.open(archive) as release:
        for name in ("firecracker", "jailer"):
            matches = [entry for entry in release if entry.isfile()
                       and Path(entry.name).name == f"{name}-{version}-{arch}"]
            if len(matches) != 1:
                raise RuntimeError(f"release has no unique {name} binary")
            with release.extractfile(matches[0]) as source:
                with (OUTPUT / name).open("wb") as output:
                    shutil.copyfileobj(source, output)
            (OUTPUT / name).chmod(0o755)
    kernel_url = f"https://s3.amazonaws.com/spec.ccfc.min/{KERNEL_PREFIX}/{arch}/vmlinux-6.1.188"
    kernel = OUTPUT / "vmlinux"
    download(kernel_url, kernel)
    if sha256(kernel) != pins[arch]:
        raise RuntimeError("guest kernel checksum mismatch")
    with tempfile.TemporaryDirectory() as temporary:
        stage = Path(temporary)
        for directory in ("bin", "sbin", "dev", "proc", "sys", "run"):
            (stage / directory).mkdir()
        shutil.copyfile("/bin/busybox", stage / "bin/busybox")
        (stage / "bin/busybox").chmod(0o755)
        subprocess.run(["cc", "-static", "-O2", "-Wall", "-Wextra", "-Werror",
                        str(Path(__file__).parent / "guest-init.c"),
                        "-o", str(stage / "sbin/init")], check=True)
        rootfs = OUTPUT / "rootfs.ext4"
        rootfs.unlink(missing_ok=True)
        subprocess.run(["mkfs.ext4", "-F", "-q", "-d", str(stage), str(rootfs), "32768"], check=True)
    provenance = {
        "kata_revision": subprocess.check_output(
            ["git", "-c", f"safe.directory={ROOT}", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "firecracker_version": version, "architecture": arch,
        "release_url": url, "release_sha256": expected,
        "kernel_url": kernel_url, "kernel_sha256": sha256(kernel),
        "rootfs_sha256": sha256(rootfs),
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
    }
    (OUTPUT / "provenance.json").write_text(json.dumps(provenance, indent=2) + "\n")
    print(json.dumps(provenance, indent=2))


if __name__ == "__main__":
    main()
