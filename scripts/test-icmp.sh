#!/usr/bin/env bash
# Run the real-ICMP end-to-end test in a private user+network namespace, so it
# works without root or a permanent CAP_NET_RAW grant.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

# Build outside the namespace, where the cargo registry is reachable.
cargo test --no-run --quiet

if ! unshare -Urn true 2>/dev/null; then
  echo "error: this script needs unprivileged user namespaces (or run it with sudo)" >&2
  exit 1
fi

exec unshare -Urn sh -c '
  set -e
  ip link set lo up
  cd "$1"
  exec cargo test --offline --test e2e -- --ignored --nocapture --test-threads=1
' sh "$root"
