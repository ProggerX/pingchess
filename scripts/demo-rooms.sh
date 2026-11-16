#!/usr/bin/env bash
# Run a rooms hub and two clients in a private user+network namespace: chat,
# start a game, play Fool's Mate, and print both clients' views.
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

unshare -Urn sh -c '
  set -e
  ip link set lo up
  bin="$1/target/debug/pingchess"
  work="$2"

  "$bin" serve --name "demo hub" < /dev/null > "$work/hub.out" 2>&1 &
  hub=$!
  sleep 0.2

  (
    sleep 0.2
    printf "e7e5\nd8h4\n"
    sleep 4.5
    printf "good game\n"
    sleep 1
    printf "/quit\n"
  ) | "$bin" connect 127.0.0.1 --nick bob --ascii --no-color > "$work/bob.out" 2>&1 &
  bob=$!

  (
    sleep 1.5
    printf "/new bob\nf2f3\ng2g4\n"
    sleep 3
    printf "nice game\n"
    sleep 1
    printf "/quit\n"
  ) | "$bin" connect 127.0.0.1 --nick alice --ascii --no-color > "$work/alice.out" 2>&1 &
  alice=$!

  wait "$alice"
  wait "$bob"
  kill "$hub" 2>/dev/null || true
  wait "$hub" 2>/dev/null || true
' sh "$root" "$work"

echo "===================== Alice ====================="
cat "$work/alice.out"
echo
echo "====================== Bob ======================"
cat "$work/bob.out"
echo
echo "==================== Hub log ===================="
cat "$work/hub.out"
