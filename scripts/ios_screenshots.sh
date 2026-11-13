#!/usr/bin/env bash
# Take the iOS app screenshots used in the docs (macOS + Xcode only).
#
#   scripts/ios_screenshots.sh [--skip-rust] [--no-dark] [--device NAME]
#
# Steps: build build/OvFFI.xcframework (ios/build_ios.sh --xcframework),
# generate the Xcode project (XcodeGen), build the app (Debug, simulator),
# boot a simulator, set a clean status bar, then launch the app in its
# Debug-only screenshot mode (-OVScreenshotMode: in-memory demo devices,
# nothing saved) and write:
#
#   docs/images/ios-home.png        Home, light
#   docs/images/ios-settings.png    Settings, light (-OVScreenshotSettings)
#   docs/images/ios-home-dark.png   Home, dark (skip with --no-dark)
#
# Options / environment:
#   --skip-rust     reuse an existing ios/build/OvFFI.xcframework (SKIP_RUST=1)
#   --no-dark       no dark-mode screenshot (NO_DARK=1)
#   --device NAME   simulator name to prefer (SIM_NAME, default "iPhone 15");
#                   falls back to iPhone 15-19, then any available iPhone
#   SHOT_DELAY      seconds to wait after launch before a screenshot (default 4)
#   OUT_DIR         output directory (default docs/images)
#
# Requirements: everything ios/build_ios.sh needs (Xcode 15+, XcodeGen,
# Rust with the iOS targets), plus python3 (ships with the Xcode CLT).
set -euo pipefail

usage() {
  sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'
}

SKIP_RUST="${SKIP_RUST:-0}"
NO_DARK="${NO_DARK:-0}"
SIM_NAME="${SIM_NAME:-iPhone 15}"
SHOT_DELAY="${SHOT_DELAY:-4}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-rust) SKIP_RUST=1 ;;
    --no-dark) NO_DARK=1 ;;
    --device)
      [[ $# -ge 2 ]] || { usage >&2; exit 2; }
      SIM_NAME="$2"
      shift
      ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
  shift
done

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IOS_DIR="${REPO_DIR}/ios"
APP_DIR="${IOS_DIR}/OpenView"
BUILD_DIR="${IOS_DIR}/build"
XCFRAMEWORK="${BUILD_DIR}/OvFFI.xcframework"
DERIVED="${BUILD_DIR}/ScreenshotsDerivedData"
OUT_DIR="${OUT_DIR:-${REPO_DIR}/docs/images}"
DEMO_RELAY="wss://relay.example.com/relay"

log() { echo "==> $*"; }
die() { echo "error: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "$1 not found. $2"; }

[[ "$(uname -s)" == "Darwin" ]] || die "this script needs macOS with Xcode"
need xcrun "Install Xcode 15 or newer."
need xcodebuild "Install Xcode 15 or newer."
need xcodegen "Install it with: brew install xcodegen"
need python3 "Install the Xcode command line tools (xcode-select --install)."

# 1. Rust core as an XCFramework (device + simulator slices).
if [[ "${SKIP_RUST}" == "1" && -d "${XCFRAMEWORK}" ]]; then
  log "skipping the Rust build, reusing ${XCFRAMEWORK}"
else
  [[ "${SKIP_RUST}" == "1" ]] && log "${XCFRAMEWORK} missing, building it anyway"
  log "building ${XCFRAMEWORK} (ios/build_ios.sh --xcframework)"
  "${IOS_DIR}/build_ios.sh" --xcframework
fi

# 2. Xcode project.
log "xcodegen generate (${APP_DIR})"
(cd "${APP_DIR}" && xcodegen generate --spec project.yml)

# 3. Pick a simulator: SIM_NAME, else the newest iPhone 15-19, else any iPhone.
log "looking for an available simulator (preferred: ${SIM_NAME})"
UDID="$(xcrun simctl list devices available -j | SIM_NAME="${SIM_NAME}" python3 -c '
import json, os, re, sys

want = os.environ["SIM_NAME"]
devices = json.load(sys.stdin)["devices"]

def runtime_version(rt):
    # com.apple.CoreSimulator.SimRuntime.iOS-17-5 -> (17, 5)
    m = re.search(r"iOS-(\d+)(?:-(\d+))?", rt)
    return (int(m.group(1)), int(m.group(2) or 0)) if m else None

phones = []
for rt, devs in devices.items():
    ver = runtime_version(rt)
    if ver is None:
        continue
    for d in devs:
        if d.get("isAvailable", True) and d["name"].startswith("iPhone"):
            phones.append((ver, d["name"], d["udid"]))
phones.sort(reverse=True)  # newest runtime first

def first(pred):
    return next((u for v, n, u in phones if pred(n)), None)

udid = (first(lambda n: n == want)
        or first(lambda n: re.match(r"iPhone 1[5-9]\b", n) is not None)
        or first(lambda n: True))
print(udid or "")
')"
[[ -n "${UDID}" ]] || die "no available iPhone simulator (install one in Xcode > Settings > Platforms)"
SIM_DESC="$(xcrun simctl list devices available | grep -F "${UDID}" | head -1 | sed 's/^ *//')"
log "simulator: ${SIM_DESC}"

# 4. Build the app (Debug: the screenshot mode is compiled into Debug only).
log "xcodebuild Debug for the simulator -> ${DERIVED}"
(cd "${APP_DIR}" && xcodebuild -project OpenView.xcodeproj -scheme OpenView \
  -configuration Debug -sdk iphonesimulator -destination "platform=iOS Simulator,id=${UDID}" \
  -derivedDataPath "${DERIVED}" CODE_SIGNING_ALLOWED=NO build)
APP="${DERIVED}/Build/Products/Debug-iphonesimulator/OpenView.app"
[[ -d "${APP}" ]] || die "built app not found at ${APP}"
BUNDLE_ID="$(/usr/libexec/PlistBuddy -c 'Print CFBundleIdentifier' "${APP}/Info.plist")"
log "app: ${APP} (${BUNDLE_ID})"

# 5. Boot the simulator (remember whether we did, to shut it down again).
STATE="$(xcrun simctl list devices | grep -F "${UDID}" | head -1 | sed -E 's/.*\(([A-Za-z ]+)\) *$/\1/')"
WE_BOOTED=0
if [[ "${STATE}" != "Booted" ]]; then
  log "booting the simulator"
  xcrun simctl boot "${UDID}" 2>/dev/null || true
  WE_BOOTED=1
fi
xcrun simctl bootstatus "${UDID}" -b >/dev/null
ORIG_APPEARANCE="$(xcrun simctl ui "${UDID}" appearance 2>/dev/null || echo light)"

cleanup() {
  set +e
  xcrun simctl terminate "${UDID}" "${BUNDLE_ID:-org.openview.openview}" >/dev/null 2>&1
  xcrun simctl status_bar "${UDID}" clear >/dev/null 2>&1
  case "${ORIG_APPEARANCE}" in
    light|dark) xcrun simctl ui "${UDID}" appearance "${ORIG_APPEARANCE}" >/dev/null 2>&1 ;;
  esac
  if [[ "${WE_BOOTED}" == "1" ]]; then
    echo "==> shutting the simulator down"
    xcrun simctl shutdown "${UDID}" >/dev/null 2>&1
  fi
}
trap cleanup EXIT

log "status bar override (9:41, full battery and signal)"
xcrun simctl status_bar "${UDID}" override --time 9:41 --batteryState charged \
  --batteryLevel 100 --cellularBars 4 --wifiBars 3

log "installing the app"
xcrun simctl install "${UDID}" "${APP}"
mkdir -p "${OUT_DIR}"

# Launch in screenshot mode and capture. The -ov.* arguments land in the
# (in-memory) argument domain of UserDefaults: they override the app's
# stored relay / appearance for this launch only and are never saved.
# UserDefaults reads launch arguments as `-key value` pairs, so the app's
# flags get a value too (YES) and cannot swallow the next `-ov.*` key.
shoot() {
  local out="$1"
  shift
  xcrun simctl terminate "${UDID}" "${BUNDLE_ID}" >/dev/null 2>&1 || true
  xcrun simctl launch "${UDID}" "${BUNDLE_ID}" -OVScreenshotMode YES \
    -ov.defaultRelayURL "${DEMO_RELAY}" -ov.appearance system "$@" >/dev/null
  sleep "${SHOT_DELAY}"
  xcrun simctl io "${UDID}" screenshot --type=png "${out}" >/dev/null
  log "wrote ${out}"
  xcrun simctl terminate "${UDID}" "${BUNDLE_ID}" >/dev/null 2>&1 || true
}

log "appearance: light"
xcrun simctl ui "${UDID}" appearance light
shoot "${OUT_DIR}/ios-home.png"
shoot "${OUT_DIR}/ios-settings.png" -OVScreenshotSettings YES

if [[ "${NO_DARK}" != "1" ]]; then
  log "appearance: dark"
  xcrun simctl ui "${UDID}" appearance dark
  shoot "${OUT_DIR}/ios-home-dark.png"
fi

log "done"
