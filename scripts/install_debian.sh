#!/usr/bin/env bash
# Installs the xolkit package on Debian, Ubuntu and other apt-based distributions. Run it through
# scripts/install.sh (`mise run install`). Usage: install_debian.sh <version> [apt-get args]
#
# Downloads the .deb of release v<version> (the latest release if that version isn't released),
# checks it against the release's .deb.sha256, and installs it with `apt-get install`, which pulls
# in the libraries it needs.
set -euo pipefail
version="${1:?usage: install_debian.sh <version> [apt-get args]}"
shift

if ! command -v apt-get > /dev/null; then
  echo "installing on Debian needs apt-get" >&2
  exit 1
fi
case "$(uname -m)" in
x86_64 | aarch64) target="$(uname -m)-unknown-linux-gnu" ;;
*) echo "xolkit has no Debian package for $(uname -m), only x86_64 and aarch64" >&2; exit 1 ;;
esac
sudo=()
[[ $EUID -eq 0 ]] || sudo=(sudo)

. "$(dirname "$0")/install_common.sh"
tag="$(release_tag)"
deb="xolkit-${tag#v}-$target.deb"
download_verified "$tag" "$deb"

# apt reads local packages as its own unprivileged user, which can't read mktemp's folder.
chmod 755 "$dir"
chmod 644 "$dir/$deb"
"${sudo[@]}" apt-get install "$@" "$dir/$deb"
