#!/bin/sh
set -eu

version=${1:?usage: install.sh vVERSION [BIN_DIRECTORY]}
case "$version" in v[0-9]*.[0-9]*.[0-9]*) ;; *) echo "expected a pinned vVERSION" >&2; exit 1 ;; esac
case "$version" in *[!a-zA-Z0-9.-]*) echo "invalid version" >&2; exit 1 ;; esac
destination=${2:-"$HOME/.local/bin"}
case "$(uname -s):$(uname -m)" in
    Linux:x86_64) target=x86_64-unknown-linux-musl ;;
    Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-musl ;;
    Darwin:x86_64) target=x86_64-apple-darwin ;;
    Darwin:arm64) target=aarch64-apple-darwin ;;
    *) echo "unsupported platform; use the release archive directly" >&2; exit 1 ;;
esac
base=${CODEQ_RELEASE_BASE_URL:-https://github.com/tom-draper/codeq/releases/download}
asset="codeq-$version-$target.tar.gz"
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT HUP INT TERM
curl --fail --location --silent --show-error "$base/$version/$asset" -o "$temporary/$asset"
curl --fail --location --silent --show-error "$base/$version/$asset.sha256" -o "$temporary/checksum"
expected=$(awk -v asset="$asset" '$2 == asset {print $1}' "$temporary/checksum")
case "$expected" in *[!0-9a-f]*|'') echo "invalid checksum file" >&2; exit 1 ;; esac
[ "${#expected}" -eq 64 ] || { echo "invalid checksum length" >&2; exit 1; }
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$temporary/$asset" | awk '{print $1}')
else
    actual=$(shasum -a 256 "$temporary/$asset" | awk '{print $1}')
fi
[ "$actual" = "$expected" ] || { echo "checksum mismatch" >&2; exit 1; }
tar -xzf "$temporary/$asset" -C "$temporary" codeq
mkdir -p "$destination"
staged=$(mktemp "$destination/.codeq.XXXXXX")
cp "$temporary/codeq" "$staged"
chmod 755 "$staged"
mv -f "$staged" "$destination/codeq"
"$destination/codeq" --version
echo "Installed to $destination/codeq. Add $destination to PATH if needed."
