#!/bin/bash
# Podlift 를 더블클릭 실행 가능한 macOS .app 번들로 만든다.
# 사용: ./build-app.sh            → Podlift.app
#       ./build-app.sh --install  → 만든 뒤 /Applications 에 설치(교체)
set -e
cd "$(dirname "$0")"

VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')"
BUILD="$(git rev-list --count HEAD 2>/dev/null || echo 1)"
APP="Podlift.app"
DEPLOY_TARGET="12.0"

# rustup 툴체인을 PATH 에 올린다 (~/.cargo/bin 이 없는 환경 대비)
TOOLCHAIN_BIN="$(rustup which cargo 2>/dev/null | xargs dirname 2>/dev/null || true)"
[ -n "$TOOLCHAIN_BIN" ] && export PATH="$TOOLCHAIN_BIN:$PATH"

echo "▶ Rust 릴리즈 빌드…"
MACOSX_DEPLOYMENT_TARGET="$DEPLOY_TARGET" cargo build --release

echo "▶ 앱 아이콘…"
ICONSET="AppIcon.iconset"; rm -rf "$ICONSET"; mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
    sips -z $s $s icon.png --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
    sips -z $((s*2)) $((s*2)) icon.png --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o AppIcon.icns; rm -rf "$ICONSET"

echo "▶ $APP 번들 구성…"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/podlift "$APP/Contents/MacOS/podlift"
cp AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>              <string>Podlift</string>
    <key>CFBundleDisplayName</key>       <string>Podlift</string>
    <key>CFBundleIdentifier</key>        <string>com.wwhalefe.podlift</string>
    <key>CFBundleExecutable</key>        <string>podlift</string>
    <key>CFBundlePackageType</key>       <string>APPL</string>
    <key>CFBundleShortVersionString</key><string>${VERSION}</string>
    <key>CFBundleVersion</key>           <string>${BUILD}</string>
    <key>CFBundleIconFile</key>          <string>AppIcon</string>
    <key>LSMinimumSystemVersion</key>    <string>${DEPLOY_TARGET}</string>
    <key>NSHighResolutionCapable</key>   <true/>
    <key>LSApplicationCategoryType</key> <string>public.app-category.music</string>
    <key>NSRemovableVolumesUsageDescription</key><string>연결된 아이팟에서 음악을 읽으려면 외장 볼륨 접근이 필요합니다.</string>
</dict>
</plist>
PLIST
# 개인용 임시 서명 (Gatekeeper 경고 완화)
codesign --force --deep --sign - "$APP" 2>/dev/null || true
echo "✅ 완료: $(pwd)/$APP  (v${VERSION}, build ${BUILD})"

if [ "$1" = "--install" ]; then
    DEST="/Applications/Podlift.app"
    osascript -e 'tell application "Podlift" to quit' 2>/dev/null || true
    sleep 1; pkill -x podlift 2>/dev/null || true
    rm -rf "$DEST"; ditto "$APP" "$DEST"
    xattr -dr com.apple.quarantine "$DEST" 2>/dev/null || true
    echo "✅ 설치: $DEST"
fi
