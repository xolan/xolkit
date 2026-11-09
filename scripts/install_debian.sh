#!/usr/bin/env bash
# Installs the xolkit package on Debian 12, Ubuntu 22.04 and newer, and other apt-based
# distributions. Run it through scripts/install.sh. Usage: install_debian.sh <version> [apt-get args]
#
# Downloads the .deb of release v<version> (the latest release's if that one has none), checks
# it against the release's .deb.sha256, and installs it with `apt-get install`, which pulls in
# the libraries it needs.
set -euo pipefail
version="${1:?usage: install_debian.sh <version> [apt-get args]}"
shift

if ! command -v apt-get > /dev/null || ! command -v dpkg > /dev/null; then
  echo "installing on Debian needs apt-get and dpkg" >&2
  exit 1
fi
# apt's architecture, not the kernel's: a 32-bit userland can run on a 64-bit kernel.
arch="$(dpkg --print-architecture)"
case "$arch" in
amd64) target=x86_64-unknown-linux-gnu ;;
arm64) target=aarch64-unknown-linux-gnu ;;
*) echo "xolkit has no Debian package for $arch, only amd64 and arm64" >&2; exit 1 ;;
esac
sudo=()
if [[ $EUID -ne 0 ]]; then
  command -v sudo > /dev/null || { echo "installing needs root: run this as root, or install sudo" >&2; exit 1; }
  sudo=(sudo)
fi

. "$(dirname "$0")/install_common.sh"
download_release_file "xolkit-@VERSION@-$target.deb"
deb="$file"

# The package needs the glibc of the system it was built on (see release.yml). Say so plainly
# rather than leave apt to report an unmet libc6 dependency.
need="$(dpkg-deb -f "$dir/$deb" Depends | sed -n 's/.*libc6 (>= \([^)]*\)).*/\1/p')" || need=""
have="$(dpkg-query -W -f '${Version}' "libc6:$arch" 2> /dev/null)" || have=""
if [[ -n "$need" && -n "$have" ]] && ! dpkg --compare-versions "$have" ge "$need"; then
  echo "xolkit needs glibc $need or newer (Debian 12, Ubuntu 22.04 or newer, or a distribution based on them); this system has $have" >&2
  exit 1
fi

# apt reads local packages as its own unprivileged user, which can't read mktemp's folder.
chmod 755 "$dir"
chmod 644 "$dir/$deb"
"${sudo[@]}" apt-get install "$@" "$dir/$deb" || {
  status=$?
  # apt-get exits 100 on errors such as a package it can't locate, 1 when you decline.
  if [[ $status -eq 100 ]]; then
    echo "If apt couldn't locate a package xolkit depends on, run 'sudo apt-get update' and try again." >&2
  fi
  exit "$status"
}
