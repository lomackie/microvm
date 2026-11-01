#!/usr/bin/env bash
# Download a minimal guest kernel (Firecracker's CI build) onto the KVM host.
# Kept outside the synced repo dir, because run.sh's rsync --delete would remove it.
set -euo pipefail

HOST="${MICROVM_HOST:-beelink}"
KERNEL="${KERNEL:-firecracker-ci/v1.15/x86_64/vmlinux-6.1.155}"

ssh "$HOST" bash -s <<EOF
set -euo pipefail
mkdir -p ~/microvm-assets && cd ~/microvm-assets
curl -fsSL -o vmlinux        "https://s3.amazonaws.com/spec.ccfc.min/$KERNEL"
curl -fsSL -o vmlinux.config "https://s3.amazonaws.com/spec.ccfc.min/$KERNEL.config"
file vmlinux
EOF
