#!/usr/bin/env bash
# Play a scripted Fool's Mate between two local peers over real ICMP, inside a
# private user+network namespace so no root privileges are needed.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

if ! unshare -Urn true 2>/dev/null; then
  echo "error: this demo needs unprivileged user namespaces (or run it with sudo)" >&2
  exit 1
fi

cargo build --quiet

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cat >"$work/white.moves" <<'EOF'
f2f3
g2g4
EOF

cat >"$work/black.moves" <<'EOF'
e7e5
d8h4
EOF

unshare -Urn sh -c '
  set -e
  ip link set lo up
  bin="$1/target/debug/pingchess"
  work="$2"
  "$bin" host --name alice --ascii --no-color < "$work/white.moves" > "$work/host.out" &
  host_pid=$!
  sleep 0.2
  "$bin" join 127.0.0.1 --name bob --ascii --no-color < "$work/black.moves" > "$work/join.out"
  wait "$host_pid"
' sh "$root" "$work"

echo "================ Alice (host, White) ================"
cat "$work/host.out"
echo
echo "================= Bob (joiner, Black) ================="
cat "$work/join.out"
