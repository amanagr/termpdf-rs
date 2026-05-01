#!/bin/sh
# Fetch the prebuilt libpdfium.so from Bblanchon's pdfium-binaries
# repo and stage it under vendor/. termpdf-rs uses pdfium-render's
# runtime-dlopen path, so the .so just needs to exist on disk —
# we point `Pdfium::bind_to_library` at this absolute path at startup.
#
# Re-run safe: skips download when the file already exists.
set -eu

cd "$(dirname "$0")"
mkdir -p vendor

if [ -f vendor/libpdfium.so ]; then
    echo "vendor/libpdfium.so already present"
    exit 0
fi

# Pin to a specific Chromium release so a future binary-format change
# upstream doesn't silently break us. Bump this when you want a
# newer pdfium; the release pages list the supported chromium tags.
TAG="chromium/7811"
URL="https://github.com/bblanchon/pdfium-binaries/releases/download/${TAG}/pdfium-linux-x64.tgz"

echo "Fetching ${URL}..."
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

curl -fsSL "$URL" -o "$tmp/pdfium.tgz"
tar -xzf "$tmp/pdfium.tgz" -C "$tmp"
cp "$tmp/lib/libpdfium.so" vendor/libpdfium.so

echo "Staged vendor/libpdfium.so"
