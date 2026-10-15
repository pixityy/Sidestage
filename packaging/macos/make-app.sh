#!/usr/bin/env bash
# Builds Sidestage.app and a zip of it.
#
#   packaging/macos/make-app.sh [--universal | --target <rust-target>] [--out <dir>]
#
# Defaults: the host architecture, output in target/macos-app/. --universal builds
# aarch64-apple-darwin and x86_64-apple-darwin and joins them with lipo (both targets must be
# installed: rustup target add aarch64-apple-darwin x86_64-apple-darwin).
#
# Signing: ad hoc by default, which runs on the Mac that built it. macOS may ask for audio
# capture permission again after every rebuild, because an ad-hoc signature changes with the
# binary. For distribution set SIDESTAGE_SIGN_IDENTITY to a "Developer ID Application: ..."
# identity: the app is then signed with the hardened runtime and Sidestage.entitlements, ready
# for notarization (xcrun notarytool submit <zip> --wait; xcrun stapler staple Sidestage.app).
#
# The deployment target is macOS 14.2, the first release with Core Audio process taps.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="$root/target/macos-app"
targets=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --universal) targets=(aarch64-apple-darwin x86_64-apple-darwin); shift ;;
        --target) targets=("${2:?--target needs a value}"); shift 2 ;;
        --out) out="${2:?--out needs a value}"; shift 2 ;;
        -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "make-app.sh must run on macOS (it needs the Xcode command line tools)" >&2
    exit 1
fi
for tool in cargo codesign ditto lipo plutil; do
    command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 1; }
done
if [[ ${#targets[@]} -eq 0 ]]; then
    case "$(uname -m)" in
        arm64) targets=(aarch64-apple-darwin) ;;
        x86_64) targets=(x86_64-apple-darwin) ;;
        *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
    esac
fi

# The zip is written from inside $out, so a relative --out must be made absolute first.
mkdir -p "$out"
out="$(cd "$out" && pwd)"

export MACOSX_DEPLOYMENT_TARGET=14.2
metadata="$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml")"
version="$(sed -nE 's/.*"name":"sidestage","version":"([^"]+)".*/\1/p' <<<"$metadata")"
[[ -n "$version" ]] || { echo "could not read the sidestage version from cargo metadata" >&2; exit 1; }
# Honours CARGO_TARGET_DIR and build.target-dir.
target_dir="$(sed -nE 's/.*"target_directory":"([^"]+)".*/\1/p' <<<"$metadata")"
[[ -n "$target_dir" ]] || { echo "could not read the target directory from cargo metadata" >&2; exit 1; }

binaries=()
for target in "${targets[@]}"; do
    echo "==> building sidestage $version for $target"
    cargo build --release --locked --manifest-path "$root/Cargo.toml" -p sidestage --target "$target"
    binaries+=("$target_dir/$target/release/sidestage")
done

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
if [[ ${#binaries[@]} -gt 1 ]]; then
    lipo -create -output "$work/sidestage" "${binaries[@]}"
    arch="universal"
else
    cp "${binaries[0]}" "$work/sidestage"
    arch="${targets[0]%%-*}"
fi

app="$out/Sidestage.app"
echo "==> assembling $app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
install -m 755 "$work/sidestage" "$app/Contents/MacOS/sidestage"
sed "s/@VERSION@/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist" >/dev/null
printf 'APPL????' > "$app/Contents/PkgInfo"

if [[ -n "${SIDESTAGE_SIGN_IDENTITY:-}" ]]; then
    echo "==> signing with $SIDESTAGE_SIGN_IDENTITY (hardened runtime)"
    codesign --force --timestamp --options runtime \
        --entitlements "$here/Sidestage.entitlements" \
        --sign "$SIDESTAGE_SIGN_IDENTITY" "$app"
else
    echo "==> signing ad hoc"
    codesign --force --sign - --entitlements "$here/Sidestage.entitlements" "$app"
fi
codesign --verify --strict --verbose=2 "$app"

zip="$out/Sidestage-$version-macos-$arch.zip"
rm -f "$zip"
(cd "$out" && ditto -c -k --sequesterRsrc --keepParent Sidestage.app "$zip")
echo "==> $zip"
