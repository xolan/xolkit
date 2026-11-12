#!/usr/bin/env bash
# Installs the verified macOS release binary for the current user. Run through install.sh.
# Usage: install_macos.sh <version> [bin-directory]
set -euo pipefail
version="${1:?usage: install_macos.sh <version> [bin-directory]}"
shift
[[ $# -le 1 ]] || { echo "usage: install_macos.sh <version> [bin-directory]" >&2; exit 1; }
bin_dir="${1:-$HOME/.local/bin}"
[[ "$(uname -s)" == Darwin ]] || { echo "this installer needs macOS" >&2; exit 1; }
case "$(uname -m)" in
arm64) target=aarch64-apple-darwin ;;
x86_64) target=x86_64-apple-darwin ;;
*) echo "xolkit has no macOS build for $(uname -m)" >&2; exit 1 ;;
esac
major="$(sw_vers -productVersion)"
major="${major%%.*}"
[[ "$major" -ge 15 ]] || { echo "xolkit needs macOS 15 or newer" >&2; exit 1; }

. "$(dirname "$0")/install_common.sh"
download_release_file "xolkit-@VERSION@-$target.tar.gz"
tar -xzf "$dir/$file" -C "$dir"
folder="${file%.tar.gz}"
mkdir -p "$bin_dir"
# Rename over the old executable, which may still be running.
staged="$(mktemp "$bin_dir/.xolkit.XXXXXX")"
trap 'rm -f "$staged"; rm -rf "$dir"' EXIT
install -m 755 "$dir/$folder/xolkit" "$staged"
mv -f "$staged" "$bin_dir/xolkit"
echo "Installed xolkit ${tag#v} to $bin_dir/xolkit"
case ":$PATH:" in
*":$bin_dir:"*) ;;
*) echo "Add $bin_dir to your PATH to run xolkit from your terminal." ;;
esac
