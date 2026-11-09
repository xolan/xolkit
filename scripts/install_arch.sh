#!/usr/bin/env bash
# Installs the xolkit-bin package on Arch Linux and other pacman-based distributions. Run it
# through scripts/install.sh (`mise run install`). Usage: install_arch.sh <version> [makepkg args]
#
# Downloads the PKGBUILD of release v<version> (the latest release if that version isn't
# released), checks it against the release's PKGBUILD.sha256, and builds and installs it with
# `makepkg -si`.
set -euo pipefail
repo=https://github.com/xolan/xolkit
version="${1:?usage: install_arch.sh <version> [makepkg args]}"
shift

if ! command -v pacman > /dev/null || ! command -v makepkg > /dev/null; then
  echo "installing on Arch needs pacman and makepkg (from the pacman package)" >&2
  exit 1
fi
if [[ $EUID -eq 0 ]]; then
  echo "Run this as your own user, not root: makepkg won't build as root and asks for sudo itself." >&2
  exit 1
fi

dir="$(mktemp -d)"
trap 'rm -rf "$dir"' EXIT

# Downloads <file> of release <tag> into $dir. Prints curl's HTTP status; fails on other errors.
fetch() {
  curl -sSL --proto '=https' -o "$dir/$2" -w '%{http_code}' "$repo/releases/download/$1/$2" || {
    echo "couldn't download $2 from release $1" >&2
    return 1
  }
}

tag="v$version"
status="$(fetch "$tag" PKGBUILD)"
if [[ "$status" == 404 ]]; then
  latest="$(curl -fsSL --proto '=https' -o /dev/null -w '%{url_effective}' "$repo/releases/latest")"
  latest="${latest##*/tag/}"
  [[ "$latest" =~ ^v[0-9] ]] || { echo "couldn't find the latest release" >&2; exit 1; }
  echo "xolkit $version has no release; installing the latest release, ${latest#v}."
  tag="$latest"
  status="$(fetch "$tag" PKGBUILD)"
fi
[[ "$status" == 200 ]] || { echo "downloading PKGBUILD from release $tag failed: HTTP $status" >&2; exit 1; }
status="$(fetch "$tag" PKGBUILD.sha256)"
[[ "$status" == 200 ]] || { echo "downloading PKGBUILD.sha256 from release $tag failed: HTTP $status" >&2; exit 1; }

# The checksum file must name the PKGBUILD alone, so `sha256sum -c` checks nothing else.
grep -Eqx '[0-9a-f]{64}  PKGBUILD' "$dir/PKGBUILD.sha256" || {
  echo "PKGBUILD.sha256 from release $tag isn't a checksum of PKGBUILD" >&2
  exit 1
}
(cd "$dir" && sha256sum --quiet -c PKGBUILD.sha256) || {
  echo "PKGBUILD from release $tag doesn't match PKGBUILD.sha256; not installing it" >&2
  exit 1
}
echo "PKGBUILD from release $tag matches its checksum."

# makepkg checks the release tarball against the checksum in the PKGBUILD.
cd "$dir"
makepkg -si "$@"
