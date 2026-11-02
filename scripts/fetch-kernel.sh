#!/usr/bin/env bash
# Download a minimal guest kernel (Firecracker's CI build) onto the KVM host, and point
# ~/microvm-assets/vmlinux at it. Kept outside the synced repo dir, because run.sh's
# rsync --delete would remove it.
#
# Default: 5.10 "no-acpi". Our VMM provides no ACPI tables, and this build can discover virtio
# devices from the kernel command line (CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES); the 6.1 CI kernel
# can't, it expects ACPI.
set -euo pipefail

HOST="${MICROVM_HOST:-beelink}"
KERNEL="${KERNEL:-firecracker-ci/v1.15/x86_64/vmlinux-5.10.245-no-acpi}"

ssh "$HOST" bash -s <<EOF
set -euo pipefail
mkdir -p ~/microvm-assets && cd ~/microvm-assets
name=\$(basename "$KERNEL")
curl -fsSL -o "\$name"        "https://s3.amazonaws.com/spec.ccfc.min/$KERNEL"
curl -fsSL -o "\$name.config" "https://s3.amazonaws.com/spec.ccfc.min/$KERNEL.config"
ln -sf "\$name" vmlinux
ln -sf "\$name.config" vmlinux.config
file -L vmlinux
EOF
