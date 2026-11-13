#!/usr/bin/env bash
# OpenView three-process end-to-end test over a live relay.
#
# Proves the whole stack works across REAL process boundaries and sockets:
#   ovrelay  (relay server, opaque byte-pipe)
#   ovhost   (listener; joins the relay as the RESPONDER by pairing code)
#   ovview   (viewer; dials the relay with the code, handshakes as the CLIENT)
#
# Flow: relay -> host (publishes a fresh 6-digit pairing code) -> view joins
# with that code -> both attach to the same relay session -> X25519 + Ed25519
# + ChaCha20-Poly1305 handshake completes over the tunnel -> the view sends a
# chat line, the host logs the decrypted content and echoes it back -> the
# view observes the echoed line -> clean disconnect on both ends.
#
# Pass criteria (all must hold):
#   * ovview exits 0 and its log shows "connected to peer" and the echoed line
#   * ovhost exits 0 and its log shows "peer connected" and the decrypted line
#
# Usage:  scripts/e2e_relay.sh [PORT]
#   PORT  optional relay port (default 14569); the host's direct UDP path
#         uses PORT+1.
#
# Cross-platform: builds the Windows .exe under git-bash/MSYS, the plain
# binary under Linux/macOS. Run from anywhere; it locates the workspace root.

set -uo pipefail

# --- locate the workspace root (this file lives in scripts/) -------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OV_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN="${CARGO_TARGET_DIR:-$OV_DIR/target}/release"
cd "$OV_DIR" || exit 1

PORT="${1:-14569}"
RELAY="127.0.0.1:$PORT"
TAG="hello-e2e"

# Windows (MSYS/MINGW/Cygwin) builds produce .exe; Unix does not.
case "$(uname -s 2>/dev/null)" in
  MSYS* | MINGW* | CYGWIN*) EXE=".exe" ;;
  *) EXE="" ;;
esac

TMP="$(mktemp -d)"
trap 'killp "$RELAY_PID" "$HOST_PID" "$VIEW_PID" 2>/dev/null; rm -rf "$TMP"' EXIT
RELAY_PID=""; HOST_PID=""; VIEW_PID=""

# taskkill on Windows, plain kill elsewhere (best-effort; stragglers are reaped
# by the process exit logic above before cleanup is even needed).
killp() {
  local p
  for p in "$@"; do
    [ -n "$p" ] || continue
    taskkill //F //PID "$p" >/dev/null 2>&1 || kill "$p" >/dev/null 2>&1 || true
  done
}

# wait_timeout PID SECS -> 0 if it exited (code in $WAIT_CODE), 1 on timeout.
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

echo "=== OpenView e2e: 3-process session over live relay ($RELAY) ==="
echo "building release binaries (ovrelay, ovhost, ovview)..."
cargo build --release -p ovrelay -p ovhost -p ovview >/dev/null 2>&1 \
  || { cargo build --release -p ovrelay -p ovhost -p ovview; fail "cargo build"; }
[ -f "$BIN/ovrelay$EXE" ] || fail "ovrelay$EXE not found in $BIN"

# --- 1. relay ---------------------------------------------------------------
echo "[1/4] starting relay on $RELAY"
"$BIN/ovrelay$EXE" --bind "$RELAY" --timeout 30 >"$TMP/relay.log" 2>&1 &
RELAY_PID=$!
ok=""
for _ in $(seq 1 50); do
  grep -q "listening on" "$TMP/relay.log" 2>/dev/null && { ok=1; break; }
  sleep 0.2
done
[ -n "$ok" ] || fail "relay did not come up: $(cat "$TMP/relay.log" 2>/dev/null)"

# --- 2. host (publishes the pairing code) ----------------------------------
echo "[2/4] starting host (listener) on relay $RELAY"
# Scratch direct-path port (never the default UDP 4570, which a running
# OpenView host on this machine owns), no LAN listener (UDP 4571) and no
# router / STUN traffic.
"$BIN/ovhost$EXE" --listen --relay "$RELAY" --direct-port "$((PORT + 1))" \
  --no-lan --no-upnp --no-stun --data-dir "$TMP/hostdata" >"$TMP/host.log" 2>&1 &
HOST_PID=$!
code=""
for _ in $(seq 1 75); do
  code="$(grep -oE 'pairing code [0-9]{6}' "$TMP/host.log" 2>/dev/null | head -1 | awk '{print $3}')"
  [ -n "$code" ] && break
  sleep 0.2
done
[ -n "$code" ] || fail "host did not publish a pairing code: $(cat "$TMP/host.log" 2>/dev/null)"
echo "      host pairing code: $code"

# --- 3. view (dials the relay with the code, sends $TAG, exits) ------------
echo "[3/4] starting view (client) with code $code; sending '$TAG'"
printf '%s\nexit\n' "$TAG" | "$BIN/ovview$EXE" --code "$code" --relay "$RELAY" \
  >"$TMP/view.log" 2>&1 &
VIEW_PID=$!
wait_timeout "$VIEW_PID" 30 || killp "$VIEW_PID"
VIEW_CODE=$WAIT_CODE

# --- 4. host exits on the client's disconnect ------------------------------
wait_timeout "$HOST_PID" 15 || killp "$HOST_PID"
HOST_CODE=$WAIT_CODE

# --- verify -----------------------------------------------------------------
echo "[4/4] verifying..."
V="$TMP/view.log"; H="$TMP/host.log"
check() { # desc, haystack-file, pattern
  if grep -qE "$3" "$2" 2>/dev/null; then
    echo "  ok  : $1"
  else
    echo "  FAIL: $1  (pattern '$3' not in $(basename "$2"))"
    exit 1
  fi
}

[ "$VIEW_CODE" -eq 0 ] || fail "ovview exit code $VIEW_CODE (want 0). log:\n$(cat "$V")"
[ "$HOST_CODE" -eq 0 ] || fail "ovhost exit code $HOST_CODE (want 0). log:\n$(cat "$H")"
check "ovview: handshake established"      "$V" "connected to peer"
check "ovview: observed host echo of '$TAG'" "$V" "\[peer\] $TAG"
check "ovhost: handshake established"      "$H" "peer connected"
check "ovhost: decrypted + logged '$TAG'"  "$H" "chat: $TAG"

echo
echo "=== E2E PASS: full relay session (handshake + chat round-trip) ==="
echo "view exit=$VIEW_CODE  host exit=$HOST_CODE  code=$code"
echo
echo "view log:";  sed 's/^/  | /' "$V"
echo "host log:";  sed 's/^/  | /' "$H"
exit 0