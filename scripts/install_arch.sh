#!/usr/bin/env bash
# Installs the xolkit-bin package on Arch Linux and other pacman-based distributions. Run it
# through scripts/install.sh (`mise run install`). Usage: install_arch.sh <version> [makepkg args]
#
# Downloads the PKGBUILD of release v<version> (the latest release if that version isn't
# released), checks it against the release's PKGBUILD.sha256, and builds and installs it with
# `makepkg -si`.
set -euo pipefail
version="${1:?usage: install_arch.sh <version> [makepkg args]}"
shift

if ! command -v pacman > /dev/null || ! command -v makepkg > /dev/null; then
  echo "installing on Arch needs pacman and makepkg (from the pacman package)" >&2
  exit 1
fi
if ! command -v fakeroot > /dev/null; then
  echo "makepkg needs fakeroot: install base-devel first (sudo pacman -S --needed base-devel)" >&2
  exit 1
fi
if [[ $EUID -eq 0 ]]; then
  echo "Run this as your own user, not root: makepkg won't build as root and asks for sudo itself." >&2
  exit 1
fi

. "$(dirname "$0")/install_common.sh"
tag="$(release_tag)"
download_verified "$tag" PKGBUILD

# makepkg checks the release tarball against the checksum in the PKGBUILD.
cd "$dir"
makepkg -si "$@"
