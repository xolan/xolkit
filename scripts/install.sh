#!/usr/bin/env bash
# Installs xolkit as a system package: works out which system this is and hands over to the
# install script for it, with the checked-out version and any arguments given here. For now only
# Arch Linux and other pacman-based distributions (scripts/install_arch.sh) are supported.
set -euo pipefail
scripts="$(cd "$(dirname "$0")" && pwd)"
root="$(dirname "$scripts")"

# The version in [workspace.package] in Cargo.toml.
version="$(awk '
  /^\[/ { section = $0 }
  section == "[workspace.package]" && /^version *=/ { gsub(/.*= *"|".*/, ""); print; exit }
' "$root/Cargo.toml")"
[[ -n "$version" ]] || { echo "no version in $root/Cargo.toml" >&2; exit 1; }

# ID and ID_LIKE from os-release, such as "cachyos arch".
distro=""
if [[ "$(uname -s)" == Linux && -r /etc/os-release ]]; then
  distro="$(. /etc/os-release && echo "${ID:-} ${ID_LIKE:-}")"
fi

case " $distro " in
*" arch "*) exec "$scripts/install_arch.sh" "$version" "$@" ;;
esac
# A pacman-based distribution that doesn't say it is like Arch.
if command -v pacman > /dev/null; then
  exec "$scripts/install_arch.sh" "$version" "$@"
fi

echo "mise run install supports only Arch Linux and other pacman-based distributions for now." >&2
echo "See Install in README.md for the other ways to install xolkit." >&2
exit 1
