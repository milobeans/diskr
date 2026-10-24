#!/bin/sh
set -eu

# Release packaging fills this value so every download uses one immutable tag.
release_version=''
prefix=''
work=''
staged=''
nested_staged=''

usage() {
    cat <<'EOF'
Install a checksum-verified native diskr release on macOS 15 or newer.

Usage: sh install-diskr.sh [--version X.Y.Z] [--prefix /absolute/path]

  --version VERSION  Release to install; defaults to this installer's release
  --prefix PATH      Install into PATH/bin (default: $HOME/.local)
  --help             Show this help

No administrator access is requested and shell startup files are not changed.
EOF
}

fail() { printf '%s\n' "diskr install: $*" >&2; exit 1; }

cleanup() {
    if [ -n "$staged" ]; then rm -f "$staged"; fi
    if [ -n "$nested_staged" ] && [ -f "$nested_staged" ]; then
        rm -f "$nested_staged"
    fi
    if [ -n "$work" ]; then rm -rf "$work"; fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP

while [ "$#" -gt 0 ]; do
    case "$1" in
        --help|-h) usage; exit 0 ;;
        --version|--prefix)
            [ "$#" -ge 2 ] || fail "$1 requires a value"
            option=$1
            shift
            if [ "$option" = --version ]; then release_version=${1#v}; else prefix=$1; fi
            ;;
        *) fail "unknown option: $1 (use --help)" ;;
    esac
    shift
done

printf '%s\n' "$release_version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' ||
    fail 'specify a release with --version X.Y.Z'
[ "$(uname -s)" = Darwin ] || fail 'diskr requires macOS'
major=$(sw_vers -productVersion | cut -d . -f 1)
case "$major" in ''|*[!0-9]*) fail 'could not read the macOS version' ;; esac
[ "$major" -ge 15 ] || fail 'prebuilt releases require macOS 15 or newer'
case "$(uname -m)" in
    arm64) target=aarch64-apple-darwin ;;
    x86_64) target=x86_64-apple-darwin ;;
    *) fail 'unsupported Mac architecture' ;;
esac

if [ -z "$prefix" ]; then
    [ -n "${HOME:-}" ] || fail 'HOME is unset; specify --prefix /absolute/path'
    prefix=$HOME/.local
fi
case "$prefix" in /*) ;; *) fail '--prefix must be an absolute path' ;; esac
prefix=${prefix%/}
[ -n "$prefix" ] || fail '--prefix cannot be the filesystem root'

for tool in curl shasum tar install mktemp; do
    command -v "$tool" >/dev/null 2>&1 || fail "required tool not found: $tool"
done
work=$(mktemp -d "${TMPDIR:-/tmp}/diskr-install.XXXXXXXX")
archive="diskr-$target.tar.gz"
base="https://github.com/milobeans/diskr/releases/download/v$release_version"
download() {
    curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
        --retry 2 --output "$work/$1" "$base/$1"
}
printf 'Downloading diskr %s for %s\n' "$release_version" "$target"
download "$archive"
download SHA256SUMS
checksum=$(awk -v name="$archive" '$2 == name { print $1 }' "$work/SHA256SUMS")
[ "${#checksum}" -eq 64 ] || fail 'release checksums must contain exactly one archive checksum'
printf '%s\n' "$checksum" | grep -Eq '^[0-9a-fA-F]{64}$' ||
    fail 'release checksums must contain exactly one valid archive checksum'
(cd "$work" && printf '%s  %s\n' "$checksum" "$archive" | shasum -a 256 -c -) ||
    fail 'archive checksum verification failed; nothing was installed'

mkdir "$work/extracted"
tar -xzf "$work/$archive" -C "$work/extracted" diskr LICENSE licenses
binary=$work/extracted/diskr
[ -f "$binary" ] && [ ! -L "$binary" ] || fail 'archive has no regular executable'
[ -x "$binary" ] || fail 'archive executable is not executable'
actual_version=$("$binary" --version) || fail 'downloaded executable cannot run on this Mac'
[ "$actual_version" = "diskr $release_version" ] || fail 'archive version does not match the requested release'

destination=$prefix/bin/diskr
[ ! -d "$destination" ] || fail "$destination is a directory"
mkdir -p "$prefix/bin" "$prefix/share/diskr/$release_version"
cp -R "$work/extracted/LICENSE" "$work/extracted/licenses" "$prefix/share/diskr/$release_version/"
staged=$(mktemp "$prefix/bin/.diskr.XXXXXXXX")
install -m 755 "$binary" "$staged"
# BSD mv can nest a source in a directory created after the preflight check.
nested_staged=$destination/${staged##*/}
mv -fh "$staged" "$destination"
[ -f "$destination" ] && [ ! -L "$destination" ] && [ -x "$destination" ] ||
    fail 'installation destination changed; no regular executable was installed'
installed_version=$("$destination" --version) || fail 'installed executable cannot run on this Mac'
[ "$installed_version" = "diskr $release_version" ] ||
    fail 'installed executable version does not match the requested release'
staged=''
nested_staged=''

printf 'Installed %s at %s\n' "$installed_version" "$destination"
printf 'License notices: %s/share/diskr/%s\n' "$prefix" "$release_version"
case ":${PATH:-}:" in
    *":$prefix/bin:"*) printf 'Start with: diskr ~/Downloads\n' ;;
    *)
        printf 'Add this directory to your shell PATH: %s/bin\n' "$prefix"
        printf 'Or run the installed executable by its full path: %s\n' "$destination"
        ;;
esac
