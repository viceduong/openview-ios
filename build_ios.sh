#!/usr/bin/env bash
# OpenView iOS build — public CI mirror (v0.28).
#
# The Rust FFI staticlib (libs/libovffi.a, built for aarch64-apple-ios-sim) is
# prebuilt and committed to this mirror, so CI does NOT run cargo. This script
# just regenerates the Xcode project and builds.
#
#   bash build_ios.sh          # device .ipa via archive + export (needs Apple account)
#   bash build_ios.sh --sim    # simulator .app, no signing — what CI runs
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SIM=0
[[ "${1:-}" == "--sim" ]] && SIM=1

command -v xcodegen >/dev/null 2>&1 || {
  echo "xcodegen not found — install with: brew install xcodegen" >&2; exit 1;
}
[[ -f "${DIR}/libs/libovffi.a" ]] || {
  echo "error: ${DIR}/libs/libovffi.a not found (prebuilt Rust staticlib)" >&2; exit 1;
}

echo "==> xcodegen (${DIR})"
(cd "${DIR}" && xcodegen generate --spec project.yml)

if [[ "${SIM}" -eq 1 ]]; then
  # Simulator build: no signing, produces a runnable .app.
  DEST="${DESTINATION:-generic/platform=iOS Simulator}"
  echo "==> xcodebuild (simulator: ${DEST})"
  (cd "${DIR}" && xcodebuild -project OpenView.xcodeproj \
    -scheme OpenView -configuration Release -sdk iphonesimulator \
    -destination "${DEST}" CODE_SIGNING_ALLOWED=NO build)
  APP="$(find ~/Library/Developer/Xcode/DerivedData \
    -path '*Release-iphonesimulator/OpenView.app' -type d | head -1)"
  [[ -n "${APP}" ]] || { echo "error: built .app not found in DerivedData" >&2; exit 1; }
  echo "==> done: ${APP}"
  echo "    run: xcrun simctl install booted '${APP}' && xcrun simctl launch booted com.openview.remoteview"
  exit 0
fi

# Device build: archive, then export an .ipa (needs a valid Apple dev account).
echo "==> xcodebuild archive (device)"
(cd "${DIR}" && xcodebuild -project OpenView.xcodeproj \
  -scheme OpenView -configuration Release -sdk iphoneos \
  -destination 'generic/platform=iOS' \
  -archivePath "${DIR}/build/OpenView.xcarchive" archive)

cat > "${DIR}/ExportOptions.plist" <<'EOF'
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

echo "==> xcodebuild -exportArchive"
(cd "${DIR}" && xcodebuild -exportArchive \
  -archivePath "${DIR}/build/OpenView.xcarchive" \
  -exportOptionsPlist "${DIR}/ExportOptions.plist" \
  -exportPath "${DIR}/build")

IPA="${DIR}/build/OpenView.ipa"
[[ -f "${IPA}" ]] || IPA="$(find "${DIR}/build" -name '*.ipa' | head -1)"
echo "==> done: ${IPA}"