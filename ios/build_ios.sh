#!/usr/bin/env bash
# Build the OpenView iOS app on macOS: the Rust core as an XCFramework, the
# Xcode project (XcodeGen), and the app itself.
#
#   ./build_ios.sh --sim          simulator .app (no signing; Apple silicon + Intel)
#   ./build_ios.sh --device       unsigned device .ipa (re-sign with Sideloadly / AltStore)
#   ./build_ios.sh --ipa          signed archive + .ipa export (needs DEVELOPMENT_TEAM,
#                                 see RemoteView/Config/Base.xcconfig)
#   ./build_ios.sh --test         unit tests on an iOS simulator
#   ./build_ios.sh --xcframework  only build build/OvFFI.xcframework
#
# Requirements: Xcode 15+, XcodeGen (`brew install xcodegen`), Rust via
# rustup with the iOS targets:
#   rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
#
# Environment overrides:
#   CONFIGURATION   Xcode configuration (default Release; Debug for --test)
#   DESTINATION     xcodebuild -destination for --sim / --test
#   CARGO_TARGET_DIR  where cargo builds (default <repo>/target)
#
# Outputs go to ios/build/ (git-ignored): OvFFI.xcframework, DerivedData/,
# OpenView-unsigned.ipa, OpenView.xcarchive, export/.
set -euo pipefail

usage() {
  sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'
}

MODE="${1:-}"
case "${MODE}" in
  --sim|--device|--ipa|--test|--xcframework) ;;
  -h|--help) usage; exit 0 ;;
  *) usage >&2; exit 2 ;;
esac

IOS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${IOS_DIR}/.." && pwd)"
APP_DIR="${IOS_DIR}/RemoteView"
BUILD_DIR="${IOS_DIR}/build"
HEADERS_DIR="${IOS_DIR}/OvFFI/include"
XCFRAMEWORK="${BUILD_DIR}/OvFFI.xcframework"
DERIVED="${BUILD_DIR}/DerivedData"
if [[ "${MODE}" == "--test" ]]; then
  CONFIGURATION="${CONFIGURATION:-Debug}"
else
  CONFIGURATION="${CONFIGURATION:-Release}"
fi
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${REPO_DIR}/target}"
export CARGO_TARGET_DIR
# Rust objects carry the same minimum iOS version as the app (no linker
# "built for newer iOS version" warnings). Keep in sync with project.yml.
export IPHONEOS_DEPLOYMENT_TARGET=15.6

DEVICE_TARGET=aarch64-apple-ios
SIM_TARGETS=(aarch64-apple-ios-sim x86_64-apple-ios)

die() { echo "error: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "$1 not found. $2"; }

need cargo "Install Rust from https://rustup.rs"
need xcodebuild "Install Xcode 15 or newer."
need lipo "Install the Xcode command line tools (xcode-select --install)."

# 1. Rust staticlib for the device and both simulator architectures.
if command -v rustup >/dev/null 2>&1; then
  installed="$(rustup target list --installed)"
  for t in "${DEVICE_TARGET}" "${SIM_TARGETS[@]}"; do
    grep -qx "${t}" <<<"${installed}" || die "Rust target ${t} missing: rustup target add ${t}"
  done
fi
for t in "${DEVICE_TARGET}" "${SIM_TARGETS[@]}"; do
  echo "==> cargo build -p ovffi --release --target ${t}"
  cargo build --manifest-path "${REPO_DIR}/Cargo.toml" -p ovffi --release --locked --target "${t}"
done

# 2. One fat simulator library (arm64 + x86_64), then the XCFramework with
#    the C header and the OvFFI module map.
mkdir -p "${BUILD_DIR}/ovffi-sim"
SIM_LIB="${BUILD_DIR}/ovffi-sim/libovffi.a"
echo "==> lipo simulator slices -> ${SIM_LIB}"
lipo -create \
  "${CARGO_TARGET_DIR}/aarch64-apple-ios-sim/release/libovffi.a" \
  "${CARGO_TARGET_DIR}/x86_64-apple-ios/release/libovffi.a" \
  -output "${SIM_LIB}"

[[ -f "${HEADERS_DIR}/shim.h" && -f "${HEADERS_DIR}/module.modulemap" ]] \
  || die "headers missing in ${HEADERS_DIR}"
echo "==> xcodebuild -create-xcframework -> ${XCFRAMEWORK}"
rm -rf "${XCFRAMEWORK}"
xcodebuild -create-xcframework \
  -library "${CARGO_TARGET_DIR}/${DEVICE_TARGET}/release/libovffi.a" -headers "${HEADERS_DIR}" \
  -library "${SIM_LIB}" -headers "${HEADERS_DIR}" \
  -output "${XCFRAMEWORK}"

[[ "${MODE}" == "--xcframework" ]] && { echo "==> done: ${XCFRAMEWORK}"; exit 0; }

# 3. Xcode project from the XcodeGen spec.
need xcodegen "Install it with: brew install xcodegen"
echo "==> xcodegen generate (${APP_DIR})"
(cd "${APP_DIR}" && xcodegen generate --spec project.yml)

xcb() {
  (cd "${APP_DIR}" && xcodebuild -project OpenView.xcodeproj -scheme OpenView \
    -configuration "${CONFIGURATION}" -derivedDataPath "${DERIVED}" "$@")
}

# 4. The app.
case "${MODE}" in
  --sim)
    # Generic destination: no particular simulator needs to be installed;
    # Release builds both arm64 and x86_64.
    xcb -sdk iphonesimulator -destination "${DESTINATION:-generic/platform=iOS Simulator}" \
      CODE_SIGNING_ALLOWED=NO build
    APP="${DERIVED}/Build/Products/${CONFIGURATION}-iphonesimulator/OpenView.app"
    [[ -d "${APP}" ]] || die "built app not found at ${APP}"
    echo "==> done: ${APP}"
    echo "    run: xcrun simctl install booted '${APP}' && xcrun simctl launch booted \$(/usr/libexec/PlistBuddy -c 'Print CFBundleIdentifier' '${APP}/Info.plist')"
    ;;
  --device)
    # Unsigned: no Apple account needed. Re-sign the .ipa with Sideloadly or
    # AltStore (free Apple ID) before installing.
    xcb -sdk iphoneos -destination "generic/platform=iOS" CODE_SIGNING_ALLOWED=NO build
    APP="${DERIVED}/Build/Products/${CONFIGURATION}-iphoneos/OpenView.app"
    [[ -d "${APP}" ]] || die "built app not found at ${APP}"
    IPA="${BUILD_DIR}/OpenView-unsigned.ipa"
    rm -rf "${BUILD_DIR}/Payload" "${IPA}"
    mkdir -p "${BUILD_DIR}/Payload"
    cp -R "${APP}" "${BUILD_DIR}/Payload/OpenView.app"
    (cd "${BUILD_DIR}" && zip -qry "$(basename "${IPA}")" Payload)
    rm -rf "${BUILD_DIR}/Payload"
    echo "==> done: ${IPA}"
    ;;
  --ipa)
    # Signed: automatic signing with the team from Config/Local.xcconfig
    # (DEVELOPMENT_TEAM = ...).
    ARCHIVE="${BUILD_DIR}/OpenView.xcarchive"
    xcb -sdk iphoneos -destination "generic/platform=iOS" -archivePath "${ARCHIVE}" archive
    EXPORT_OPTIONS="${BUILD_DIR}/ExportOptions.plist"
    cat > "${EXPORT_OPTIONS}" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>method</key>
  <string>development</string>
  <key>signingStyle</key>
  <string>automatic</string>
</dict>
</plist>
EOF
    xcodebuild -exportArchive -archivePath "${ARCHIVE}" \
      -exportOptionsPlist "${EXPORT_OPTIONS}" -exportPath "${BUILD_DIR}/export"
    echo "==> done: $(find "${BUILD_DIR}/export" -name '*.ipa' | head -1)"
    ;;
  --test)
    xcb -sdk iphonesimulator \
      -destination "${DESTINATION:-platform=iOS Simulator,name=iPhone 15}" test
    ;;
esac
