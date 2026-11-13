#!/usr/bin/env bash
# OpenView LAN end-to-end test: an unattended host and a viewer on the same
# machine, no relay at all.
#
#   ovhost   unattended (PIN), LAN listener on a scratch UDP port, announced
#            by mDNS as <device-id>._openview._udp.local; its relay address
#            points at a closed port, so only the LAN path can work
#   ovview   --id <device-id>: mDNS lookup, PIN handshake over direct UDP,
#            sends a chat line, reads the host's echo, exits
#
# Pass criteria (all must hold):
#   * ovview exits 0; its log shows the mDNS lookup result, "connected to
#     peer" and the echoed line
#   * ovhost logs "LAN viewer", "peer connected" and the decrypted line
#
# Usage:  scripts/e2e_lan.sh [PORT] [--no-mdns]
#   PORT       scratch base port (default 24600): direct path PORT, LAN
#              listener PORT+1, the unreachable relay PORT+9. Never the
#              defaults 4569-4571 a running OpenView owns.
#   --no-mdns  skip the mDNS lookup (ovview --lan-addr 127.0.0.1:PORT+1),
#              for machines where multicast is filtered.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OV_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN="${CARGO_TARGET_DIR:-$OV_DIR/target}/release"
cd "$OV_DIR" || exit 1

PORT="24600"
MDNS=1
for a in "$@"; do
  case "$a" in
    --no-mdns) MDNS="" ;;
    *) PORT="$a" ;;
  esac
done
DIRECT_PORT="$PORT"
LAN_PORT="$((PORT + 1))"
DEAD_RELAY="127.0.0.1:$((PORT + 9))"
PIN="8642"
TAG="hello-lan-e2e"

case "$(uname -s 2>/dev/null)" in
  MSYS* | MINGW* | CYGWIN*) EXE=".exe" ;;
  *) EXE="" ;;
esac

TMP="$(mktemp -d)"
trap 'killp "$HOST_PID" "$VIEW_PID" 2>/dev/null; rm -rf "$TMP"' EXIT
HOST_PID=""; VIEW_PID=""

killp() {
  local p
  for p in "$@"; do
    [ -n "$p" ] || continue
    taskkill //F //PID "$p" >/dev/null 2>&1 || kill "$p" >/dev/null 2>&1 || true
  done
}

wait_timeout() {
  local pid="$1" secs="$2" n=0
  WAIT_CODE=124
  while [ "$n" -lt "$((secs * 5))" ]; do
    if ! kill -0 "$pid" 2>/dev/null; then
      wait "$pid"; WAIT_CODE=$?
      return 0
    fi
    sleep 0.2; n=$((n + 1))
  done
  return 1
}

fail() { echo "E2E FAIL: $*" >&2; exit 1; }

echo "=== OpenView e2e: LAN session without a relay (UDP $LAN_PORT) ==="
echo "building release binaries (ovhost, ovview)..."
cargo build --release -p ovhost -p ovview >/dev/null 2>&1 \
  || { cargo build --release -p ovhost -p ovview; fail "cargo build"; }

# --- 1. host -----------------------------------------------------------------
echo "[1/3] starting unattended host (LAN UDP $LAN_PORT, direct UDP $DIRECT_PORT)"
OPENVIEW_PIN="$PIN" "$BIN/ovhost$EXE" --unattended --relay "$DEAD_RELAY" \
  --direct-port "$DIRECT_PORT" --lan-port "$LAN_PORT" --no-upnp --no-stun \
  --data-dir "$TMP/hostdata" --dry-input --fx-dry-run \
  >"$TMP/host.out" 2>"$TMP/host.log" &
HOST_PID=$!
dev=""
for _ in $(seq 1 50); do
  dev="$(grep -oE '"device_id","id":"[0-9]+"' "$TMP/host.out" 2>/dev/null | grep -oE '[0-9]+' | head -1)"
  [ -n "$dev" ] && grep -q '"event":"lan"' "$TMP/host.out" && break
  sleep 0.2
done
[ -n "$dev" ] || fail "host did not report a device id: $(cat "$TMP/host.log")"
grep -q '"event":"lan","port":'"$LAN_PORT" "$TMP/host.out" \
  || fail "host LAN listener not up: $(cat "$TMP/host.out")"
echo "      host device id: $dev"

# --- 2. viewer ---------------------------------------------------------------
if [ -n "$MDNS" ]; then
  echo "[2/3] ovview --id $dev (mDNS lookup)"
  LAN_ARGS=()
else
  echo "[2/3] ovview --id $dev --lan-addr 127.0.0.1:$LAN_PORT"
  LAN_ARGS=(--lan-addr "127.0.0.1:$LAN_PORT")
fi
printf '%s\nexit\n' "$TAG" | OPENVIEW_PIN="$PIN" "$BIN/ovview$EXE" --id "$dev" \
  "${LAN_ARGS[@]}" --no-window >"$TMP/view.out" 2>"$TMP/view.log" &
VIEW_PID=$!
wait_timeout "$VIEW_PID" 30 || killp "$VIEW_PID"
VIEW_CODE=$WAIT_CODE

# --- 3. host serves one session, then exits ----------------------------------
wait_timeout "$HOST_PID" 20 || killp "$HOST_PID"

echo "[3/3] verifying..."
V="$TMP/view.log"; H="$TMP/host.log"
check() {
  if grep -qE "$3" "$2" 2>/dev/null; then
    echo "  ok  : $1"
  else
    echo "  FAIL: $1  (pattern '$3' not in $(basename "$2"))"
    echo "view log:"; sed 's/^/  | /' "$V"
    echo "host log:"; sed 's/^/  | /' "$H"
    exit 1
  fi
}
[ "$VIEW_CODE" -eq 0 ] || fail "ovview exit code $VIEW_CODE (want 0). log:\n$(cat "$V")"
if [ -n "$MDNS" ]; then
  check "ovview: found the host by mDNS"   "$V" "found .* at .*:$LAN_PORT"
fi
check "ovview: handshake established"      "$V" "connected to peer"
check "ovview: observed host echo of '$TAG'" "$V" "\[peer\] $TAG"
check "ovhost: LAN viewer accepted"        "$H" "LAN viewer"
check "ovhost: handshake established"      "$H" "peer connected"
check "ovhost: decrypted + logged '$TAG'"  "$H" "chat: $TAG"

echo
echo "=== E2E PASS: LAN session without a relay (PIN handshake + chat round-trip) ==="
exit 0
