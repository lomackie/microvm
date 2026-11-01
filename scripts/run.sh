#!/usr/bin/env bash
# Sync this repo to the KVM host and run the VMM there.
# Usage: scripts/run.sh [vmm args...]   (override host/dir with MICROVM_HOST / MICROVM_DIR)
set -euo pipefail

HOST="${MICROVM_HOST:-beelink}"
DIR="${MICROVM_DIR:-microvm}"

cd "$(dirname "$0")/.."
rsync -az --delete --exclude target/ --exclude .git/ ./ "$HOST:$DIR/"
ssh "$HOST" "cd $DIR && cargo run --release -q -p vmm -- $*"
