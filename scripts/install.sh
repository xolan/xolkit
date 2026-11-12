#!/usr/bin/env bash
# Installs xolkit as a system package: works out which system this is and hands over to the
# install script for it, with the checked-out version and any arguments given here:
# scripts/install_arch.sh for Arch Linux and other pacman-based distributions, and
# scripts/install_debian.sh for Debian, Ubuntu and other apt-based ones, and
# scripts/install_macos.sh for macOS.
set -euo pipefail
scripts="$(cd "$(dirname "$0")" && pwd)"
root="$(dirname "$scripts")"

# The version in [workspace.package] in Cargo.toml.
version="$(awk '
  /^\[/ { section = $0 }
  section == "[workspace.package]" && /^version *=/ { gsub(/.*= *"|".*/, ""); print; exit }
' "$root/Cargo.toml")"
[[ -n "$version" ]] || { echo "no version in $root/Cargo.toml" >&2; exit 1; }

# macOS must be detected before looking for Linux package managers on PATH.
if [[ "$(uname -s)" == Darwin ]]; then
  exec "$scripts/install_macos.sh" "$version" "$@"
fi

# ID and ID_LIKE from os-release, such as "cachyos arch".
distro=""
if [[ "$(uname -s)" == Linux && -r /etc/os-release ]]; then
  distro="$(. /etc/os-release && echo "${ID:-} ${ID_LIKE:-}")"
fi

case " $distro " in
*" arch "*) exec "$scripts/install_arch.sh" "$version" "$@" ;;
*" debian "* | *" ubuntu "*) exec "$scripts/install_debian.sh" "$version" "$@" ;;
esac
# A distribution that doesn't say what it is like, by its package manager.
if command -v pacman > /dev/null; then
  exec "$scripts/install_arch.sh" "$version" "$@"
fi
if command -v apt-get > /dev/null; then
  exec "$scripts/install_debian.sh" "$version" "$@"
fi

echo "This installer supports macOS, Arch Linux, Debian, Ubuntu and distributions based on them." >&2
echo "See Install in README.md for the other ways to install xolkit." >&2
exit 1
