# Shared by the install_<system>.sh scripts, which source it after setting $version: finds the
# release to install from and downloads a file of it, checked against its .sha256, into $dir.
repo=https://github.com/xolan/xolkit

command -v curl > /dev/null || { echo "installing needs curl" >&2; exit 1; }

dir="$(mktemp -d)"
trap 'rm -rf "$dir"' EXIT

# Succeeds if release <tag> has <file>, fails if it doesn't, and exits on any other answer.
has_file() {
  local status
  # One byte is enough to tell; the asset host answers 206, or 200 if it ignores the range.
  status="$(curl -sSL --proto '=https' --connect-timeout 30 -r 0-0 -o /dev/null -w '%{http_code}' "$repo/releases/download/$1/$2")" || {
    echo "couldn't reach $repo" >&2
    exit 1
  }
  case "$status" in
  200 | 206) return 0 ;;
  404) return 1 ;;
  *) echo "looking for $2 in release $1 failed: HTTP $status" >&2; exit 1 ;;
  esac
}

# Prints the tag of the latest release.
latest_tag() {
  local latest
  latest="$(curl -fsSL --proto '=https' --connect-timeout 30 -o /dev/null -w '%{url_effective}' "$repo/releases/latest")" || {
    echo "couldn't look up the latest release" >&2
    return 1
  }
  latest="${latest##*/tag/}"
  [[ "$latest" =~ ^v[0-9] ]] || { echo "couldn't find the latest release" >&2; return 1; }
  echo "$latest"
}

# Downloads <name> and its .sha256 from release v$version, or from the latest release if
# v$version doesn't have it (not released, or released before that file was), and checks them.
# @VERSION@ in <name> stands for the release's version. Sets $tag and $file.
download_release_file() {
  tag="v$version"
  file="${1//@VERSION@/$version}"
  if ! has_file "$tag" "$file"; then
    tag="$(latest_tag)"
    echo "No $file in a v$version release (not released, or released without it); installing from the latest release, ${tag#v}." >&2
    file="${1//@VERSION@/${tag#v}}"
  fi
  download_verified "$tag" "$file"
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
