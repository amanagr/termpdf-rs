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
# When bumping TAG, also bump SHA256 — pull it from
# `gh api repos/bblanchon/pdfium-binaries/releases/tags/<tag>` or the
# release page's published checksum file.
TAG="chromium/7811"
SHA256="e76e0a37aefb843d56f04657475ce612157021b1ebc53d801f2fbfcc537ccf64"
URL="https://github.com/bblanchon/pdfium-binaries/releases/download/${TAG}/pdfium-linux-x64.tgz"

echo "Fetching ${URL}..."
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

curl -fsSL "$URL" -o "$tmp/pdfium.tgz"

# Verify the download against the pinned hash before extracting, so
# a compromised upstream / mirror / TLS-MITM can't drop a malicious
# .so into vendor/.
echo "${SHA256}  ${tmp}/pdfium.tgz" | sha256sum -c - >/dev/null || {
    echo "ERROR: pdfium tarball SHA-256 mismatch — aborting." >&2
    echo "       expected: ${SHA256}" >&2
    actual=$(sha256sum "${tmp}/pdfium.tgz" | awk '{print $1}')
    echo "       got:      ${actual}" >&2
    exit 1
}

tar -xzf "$tmp/pdfium.tgz" -C "$tmp"
cp "$tmp/lib/libpdfium.so" vendor/libpdfium.so

echo "Staged vendor/libpdfium.so"
