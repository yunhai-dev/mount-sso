#!/bin/sh
# Build and package the macOS binary into dist/.
#
#   ./scripts/package-macos.sh            # release build for the host target
#   TARGET=x86_64-apple-darwin ./scripts/package-macos.sh
#
# Produces mount-sso-<version>-<target>.tar.gz, the same as a .zip, and a
# SHA256SUMS file next to them. The tarball is what the GitHub release ships.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
target=${TARGET:-$(rustc -vV | sed -n 's/^host: //p')}
name="mount-sso-$version-$target"
dist="$root/dist"
stage="$dist/$name"

echo "==> building $name"
if [ "$target" = "$(rustc -vV | sed -n 's/^host: //p')" ]; then
    cargo build --release
    binary="target/release/mount-sso"
else
    cargo build --release --target "$target"
    binary="target/$target/release/mount-sso"
fi

[ -x "$binary" ] || { echo "missing $binary" >&2; exit 1; }

rm -rf "$stage"
mkdir -p "$stage"
cp "$binary" "$stage/mount-sso"
cp README.md LICENSE "$stage/"
chmod +x "$stage/mount-sso"

echo "==> packaging"
rm -f "$dist/$name.tar.gz" "$dist/$name.zip"
tar -czf "$dist/$name.tar.gz" -C "$dist" "$name"
(cd "$dist" && zip -qr "$name.zip" "$name")
(cd "$dist" && shasum -a 256 "$name.tar.gz" "$name.zip" > SHA256SUMS)

echo "==> done"
ls -lh "$dist/$name.tar.gz" "$dist/$name.zip"
cat "$dist/SHA256SUMS"
