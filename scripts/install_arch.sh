#!/usr/bin/env bash
# Installs the xolkit-bin package on Arch Linux and other pacman-based distributions. Run it
# through scripts/install.sh (`mise run install`). Usage: install_arch.sh <version> [makepkg args]
#
# Downloads the PKGBUILD of release v<version> (the latest release's if that one has none),
# checks it against the release's PKGBUILD.sha256, and builds and installs it with
# `makepkg -si`.
set -euo pipefail
version="${1:?usage: install_arch.sh <version> [makepkg args]}"
shift

if ! command -v pacman > /dev/null || ! command -v makepkg > /dev/null; then
  echo "installing on Arch needs pacman and makepkg (from the pacman package)" >&2
  exit 1
fi
# makepkg builds for `uname -m`, and the PKGBUILD has the release's two architectures.
case "$(uname -m)" in
x86_64 | aarch64) ;;
*) echo "xolkit has no package for $(uname -m), only x86_64 and aarch64" >&2; exit 1 ;;
esac
if ! command -v fakeroot > /dev/null; then
  echo "makepkg needs fakeroot: install base-devel first (sudo pacman -S --needed base-devel)" >&2
  exit 1
fi
if [[ $EUID -eq 0 ]]; then
  echo "Run this as your own user, not root: makepkg won't build as root and asks for sudo itself." >&2
  exit 1
fi

. "$(dirname "$0")/install_common.sh"
download_release_file PKGBUILD

# makepkg checks the release tarball against the checksum in the PKGBUILD.
cd "$dir"
makepkg -si "$@"
