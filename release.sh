#!/bin/bash
# 새 버전을 GitHub 릴리스로 올린다. Homebrew 탭(WWhaleFe/homebrew-tap)이 30분 안에 따라간다.
# 사용: Cargo.toml 의 version 을 올리고 커밋·푸시한 뒤 ./release.sh
set -e
cd "$(dirname "$0")"

VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')"
TAG="v$VERSION"
ZIP="podlift-macos-arm64-$VERSION.zip"

if [ -n "$(git status --porcelain)" ]; then
    echo "커밋하지 않은 변경이 있습니다. 먼저 커밋하세요." >&2
    exit 1
fi
if gh release view "$TAG" >/dev/null 2>&1; then
    echo "$TAG 릴리스가 이미 있습니다. Cargo.toml 의 version 을 올리세요." >&2
    exit 1
fi

./build-app.sh
rm -f "$ZIP"
ditto -c -k --sequesterRsrc --keepParent Podlift.app "$ZIP"
git push
gh release create "$TAG" "$ZIP" --title "Podlift $VERSION" --notes "${1:-Podlift $VERSION}"
rm -f "$ZIP"
echo "✅ $TAG 릴리스를 올렸습니다. brew 는 30분 안에 따라갑니다 (바로 하려면 homebrew-tap 의 Bump 워크플로 실행)."
