#!/usr/bin/env bash
# Build a small ext4 root filesystem image (Alpine Linux minirootfs) on the KVM host:
# ~/microvm-assets/rootfs.ext4
#
# Alpine's /sbin/init is BusyBox init; we give it an inittab that mounts the basics and runs a
# shell on the serial console (ttyS0). `reboot` inside the guest ends the VM.
set -euo pipefail

HOST="${MICROVM_HOST:-beelink}"
SIZE_MIB="${SIZE_MIB:-256}"

ssh "$HOST" bash -s <<EOF
set -euo pipefail
mkdir -p ~/microvm-assets/alpine && cd ~/microvm-assets/alpine

# Find and download the current minirootfs, verifying its checksum.
base=https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases/x86_64
meta=\$(curl -fsSL \$base/latest-releases.yaml | grep -A8 "flavor: alpine-minirootfs")
file=\$(echo "\$meta" | awk '/file:/ {print \$2}')
sha=\$(echo "\$meta" | awk '/sha256:/ {print \$2}')
[ -f "\$file" ] || curl -fsSL -o "\$file" "\$base/\$file"
echo "\$sha  \$file" | sha256sum -c -

# fakeroot makes files root-owned inside the image without needing real root.
rm -rf rootfs && mkdir rootfs
fakeroot bash -c '
  set -euo pipefail
  tar -xzf "'\$file'" -C rootfs
  cat > rootfs/etc/inittab <<INITTAB
::sysinit:/bin/mount -t proc proc /proc
::sysinit:/bin/mount -t sysfs sysfs /sys
::sysinit:/bin/hostname -F /etc/hostname
::sysinit:/bin/echo "microvm guest ready. Type reboot to stop the VM."
ttyS0::respawn:-/bin/sh
::shutdown:/bin/umount -a -r
INITTAB
  echo microvm > rootfs/etc/hostname
  rm -f ../rootfs.ext4
  mkfs.ext4 -q -d rootfs -L rootfs ../rootfs.ext4 ${SIZE_MIB}M
'
rm -rf rootfs
ls -lh ../rootfs.ext4
EOF
