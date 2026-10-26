// Copyright (c) 2026 Kata Contributors
// SPDX-License-Identifier: Apache-2.0

// Minimal test-only guest. The controller starts a Linux container in new PID,
// mount, UTS and IPC namespaces using /dev/vdb, then stops it and unmounts its
// rootfs before the Rust runtime detaches the backing drive. No guest agent,
// network access or VM snapshot is needed for this hypervisor regression test.
#define _GNU_SOURCE
#include <errno.h>
#include <linux/vm_sockets.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#define CONTROL_PORT 12345
#define CONTAINER_ROOT "/run/container"

static int container_main(void *unused)
{
    (void)unused;
    if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL) ||
        sethostname("firecracker-container", 21) ||
        chdir(CONTAINER_ROOT) || chroot(".") || chdir("/"))
        return 1;
    execl("/bin/busybox", "busybox", "sleep", "3600", NULL);
    return 1;
}

int main(void)
{
    signal(SIGPIPE, SIG_IGN);
    mount("devtmpfs", "/dev", "devtmpfs", 0, NULL);
    mount("proc", "/proc", "proc", 0, NULL);
    mount("sysfs", "/sys", "sysfs", 0, NULL);
    if (mount("tmpfs", "/run", "tmpfs", 0, NULL) || mkdir(CONTAINER_ROOT, 0700))
        return 1;
    struct sockaddr_vm address = {
        .svm_family = AF_VSOCK, .svm_cid = VMADDR_CID_ANY,
        .svm_port = CONTROL_PORT,
    };
    int listener = socket(AF_VSOCK, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (listener < 0 || bind(listener, (struct sockaddr *)&address, sizeof(address)) ||
        listen(listener, 4))
        return 2;
    void *stack = mmap(NULL, 1024 * 1024, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS | MAP_STACK, -1, 0);
    if (stack == MAP_FAILED)
        return 3;
    pid_t container = -1;
    for (;;) {
        int fd = accept4(listener, NULL, NULL, SOCK_CLOEXEC);
        if (fd < 0)
            continue;
        char command[32] = {0};
        size_t count = 0;
        while (count + 1 < sizeof(command) && read(fd, command + count, 1) == 1) {
            if (command[count++] == '\n')
                break;
        }
        const char *reply = "error\n";
        if (!strcmp(command, "ready\n")) {
            reply = "ready\n";
        } else if (!strcmp(command, "start\n") && container == -1) {
            int mounted = 0;
            // A drive PATCH completes before Linux processes the virtio block
            // capacity-change interrupt for the formerly empty placeholder.
            for (int attempt = 0; attempt < 100; ++attempt) {
                if (!mount("/dev/vdb", CONTAINER_ROOT, "ext4", 0, NULL)) {
                    mounted = 1;
                    break;
                }
                usleep(20000);
            }
            if (mounted) {
                container = clone(container_main, (char *)stack + 1024 * 1024,
                                  CLONE_NEWPID | CLONE_NEWNS | CLONE_NEWUTS |
                                  CLONE_NEWIPC | SIGCHLD, NULL);
                if (container > 0) {
                    usleep(100000);
                    if (waitpid(container, NULL, WNOHANG) == 0)
                        reply = "started\n";
                    else
                        container = -1;
                }
                if (container == -1)
                    umount(CONTAINER_ROOT);
            }
        } else if (!strcmp(command, "stop\n") && container > 0) {
            if (!kill(container, SIGKILL)) {
                while (waitpid(container, NULL, 0) < 0 && errno == EINTR) {}
                container = -1;
                if (!umount(CONTAINER_ROOT))
                    reply = "stopped\n";
            }
        }
        (void)write(fd, reply, strlen(reply));
        close(fd);
    }
}
