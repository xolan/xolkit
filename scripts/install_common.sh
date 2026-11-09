# Shared by the install_<system>.sh scripts, which source it after setting $version: finds the
# release to install and downloads its files, each checked against its .sha256, into $dir.
repo=https://github.com/xolan/xolkit

command -v curl > /dev/null || { echo "installing needs curl" >&2; exit 1; }

dir="$(mktemp -d)"
trap 'rm -rf "$dir"' EXIT

# Prints the tag of release v$version, or of the latest release if v$version isn't released.
release_tag() {
  local status latest
  status="$(curl -sSL --proto '=https' --connect-timeout 30 -o /dev/null -w '%{http_code}' "$repo/releases/tag/v$version")" || {
    echo "couldn't reach $repo" >&2
    return 1
  }
  case "$status" in
  200) echo "v$version" ;;
  404)
    latest="$(curl -fsSL --proto '=https' --connect-timeout 30 -o /dev/null -w '%{url_effective}' "$repo/releases/latest")"
    latest="${latest##*/tag/}"
    [[ "$latest" =~ ^v[0-9] ]] || { echo "couldn't find the latest release" >&2; return 1; }
    echo "xolkit $version has no release; installing the latest release, ${latest#v}." >&2
    echo "$latest"
    ;;
  *) echo "looking up release v$version failed: HTTP $status" >&2; return 1 ;;
  esac
}

# Downloads <file> of release <tag> into $dir.
fetch() {
  local status
  status="$(curl -sSL --proto '=https' --connect-timeout 30 -o "$dir/$2" -w '%{http_code}' "$repo/releases/download/$1/$2")" || {
    echo "couldn't download $2 from release $1" >&2
    return 1
  }
  case "$status" in
  200) ;;
  404) echo "release $1 has no $2" >&2; return 1 ;;
  *) echo "downloading $2 from release $1 failed: HTTP $status" >&2; return 1 ;;
  esac
}

# Downloads <file> and <file>.sha256 of release <tag> into $dir, and checks the one against the
# other. Fails, leaving nothing to install, if they don't match.
download_verified() {
  local tag="$1" file="$2"
  fetch "$tag" "$file" || return 1
  fetch "$tag" "$file.sha256" || return 1
  # The checksum file must name <file> alone, so `sha256sum -c` checks nothing else.
  local sum
  sum="$(cat "$dir/$file.sha256")"
  [[ "$sum" =~ ^[0-9a-f]{64}\ \ (.+)$ && "${BASH_REMATCH[1]}" == "$file" ]] || {
    echo "$file.sha256 from release $tag isn't a checksum of $file" >&2
    return 1
  }
  (cd "$dir" && sha256sum --quiet -c "$file.sha256") || {
    echo "$file from release $tag doesn't match $file.sha256; not installing it" >&2
    return 1
  }
  echo "$file from release $tag matches its checksum."
}
