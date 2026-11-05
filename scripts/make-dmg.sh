#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Packs dist/TableCross.app into a disk image with a link to Applications (drag to install).
# Usage: scripts/make-dmg.sh [target.dmg]   (run scripts/bundle-macos.sh first)
set -euo pipefail

cd "$(dirname "$0")/.."
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
OUT="${1:-dist/TableCross-$VERSION-macOS.dmg}"
APP="dist/TableCross.app"
[ -d "$APP" ] || { echo "Missing: $APP (run scripts/bundle-macos.sh first)" >&2; exit 1; }

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
ditto "$APP" "$STAGE/TableCross.app"
ln -s /Applications "$STAGE/Applications"
cp README.md LICENSE assets/fonts/*-LICENSE.txt "$STAGE/"
rm -f "$OUT"
# hdiutil fails now and then on CI runners ("Resource busy"): try again a few times
for attempt in 1 2 3 4 5; do
  if hdiutil create -volname "TableCross $VERSION" -srcfolder "$STAGE" -format UDZO "$OUT"; then
    break
  fi
  [ "$attempt" -lt 5 ] || { echo "hdiutil failed $attempt times" >&2; exit 1; }
  echo "hdiutil failed, trying again (attempt $attempt)" >&2
  rm -f "$OUT"
  sleep $((attempt * 5))
done
echo "Done: $OUT"
