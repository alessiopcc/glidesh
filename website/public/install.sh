#!/bin/sh
# Installs the glidesh binary from a GitHub release, on Linux and macOS.
#
#   curl -fsSL https://glidesh.netlify.app/install.sh | sh
#
# Options are environment variables; run with --help to list them.

set -eu

usage() {
    cat <<'EOF'
Install glidesh from its GitHub release, checked against the release's SHA-256 sums.

  curl -fsSL https://glidesh.netlify.app/install.sh | sh
  curl -fsSL https://glidesh.netlify.app/install.sh | GLIDESH_VERSION=v2.0.0 sh

Environment:
  GLIDESH_VERSION      release tag to install, such as v2.0.0 (default: the latest)
  GLIDESH_INSTALL_DIR  directory for the binary (default: ~/.local/bin, or
                       /usr/local/bin when run as root). Never uses sudo: to install
                       into a system directory, run the script as root.
  GLIDESH_BASE_URL     a mirror laid out like the releases URL
                       (default: https://github.com/alessiopcc/glidesh/releases)

Running it again replaces the binary, which is how to upgrade. Supported: Linux and
macOS, on x86_64 and arm64. On Windows use Scoop: scoop install alessiopcc/glidesh
(after: scoop bucket add alessiopcc https://github.com/alessiopcc/scoop-bucket).
EOF
}

say() {
    printf 'glidesh-install: %s\n' "$1"
}

fail() {
    printf 'glidesh-install: error: %s\n' "$1" >&2
    exit 1
}

fetch() {
    if command -v curl >/dev/null 2>&1; then
        case "$1" in
        https://*) curl --proto '=https' --tlsv1.2 -fsSL "$1" -o "$2" ;;
        *) curl -fsSL "$1" -o "$2" ;;
        esac
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        fail "neither curl nor wget is installed"
    fi || fail "could not download $1"
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        fail "neither sha256sum nor shasum is installed, so the download cannot be verified"
    fi
}

# The release's target triples for this machine, preferred first. Linux lists the static
# musl build before the glibc one, which releases before 2.0 shipped instead.
targets() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$arch" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) fail "no glidesh build for the $arch processor" ;;
    esac
    case "$os" in
    Linux) echo "$arch-unknown-linux-musl $arch-unknown-linux-gnu" ;;
    Darwin)
        # An x86_64 shell under Rosetta reports x86_64 on an Apple silicon Mac.
        if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null)" = 1 ]; then
            arch=aarch64
        fi
        echo "$arch-apple-darwin"
        ;;
    MINGW* | MSYS* | CYGWIN*) fail "on Windows, install with Scoop: scoop install alessiopcc/glidesh" ;;
    *) fail "no glidesh build for $os" ;;
    esac
}

main() {
    case "${1:-}" in
    -h | --help)
        usage
        return 0
        ;;
    "") ;;
    *) fail "unknown argument: $1 (options are environment variables; see --help)" ;;
    esac

    base=${GLIDESH_BASE_URL:-https://github.com/alessiopcc/glidesh/releases}
    base=${base%/}
    if [ -n "${GLIDESH_VERSION:-}" ]; then
        sums_url="$base/download/$GLIDESH_VERSION/checksums-sha256.txt"
    else
        sums_url="$base/latest/download/checksums-sha256.txt"
    fi
    if [ -n "${GLIDESH_INSTALL_DIR:-}" ]; then
        dir=$GLIDESH_INSTALL_DIR
    elif [ "$(id -u)" = 0 ]; then
        dir=/usr/local/bin
    else
        dir="$HOME/.local/bin"
    fi

    wanted=$(targets)
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT

    fetch "$sums_url" "$tmp/sums"

    # The sums file names every archive of the release, tag included, so it also says which
    # version "latest" is.
    asset=""
    for target in $wanted; do
        asset=$(awk -v suffix="-$target.tar.gz" '
            index($2, "glidesh-") == 1 && substr($2, length($2) - length(suffix) + 1) == suffix {
                print $2
                exit
            }' "$tmp/sums")
        if [ -n "$asset" ]; then break; fi
    done
    [ -n "$asset" ] || fail "the release has no archive for $wanted"
    expected=$(awk -v name="$asset" '$2 == name { print $1; exit }' "$tmp/sums")
    tag=${asset#glidesh-}
    tag=${tag%"-$target.tar.gz"}

    say "downloading $asset"
    fetch "$base/download/$tag/$asset" "$tmp/$asset"
    actual=$(sha256_of "$tmp/$asset")
    [ "$actual" = "$expected" ] || fail "checksum mismatch for $asset: expected $expected, got $actual"

    tar -xzf "$tmp/$asset" -C "$tmp" glidesh || fail "could not unpack $asset"
    mkdir -p "$dir" || fail "cannot create $dir"
    # Copied beside the target and renamed over it, so a running glidesh is replaced whole.
    cp "$tmp/glidesh" "$dir/.glidesh.new" || fail "cannot write to $dir"
    chmod 755 "$dir/.glidesh.new"
    mv -f "$dir/.glidesh.new" "$dir/glidesh"

    say "installed $("$dir/glidesh" --version) to $dir/glidesh"
    case ":$PATH:" in
    *":$dir:"*)
        found=$(command -v glidesh 2>/dev/null || true)
        if [ -n "$found" ] && [ "$found" != "$dir/glidesh" ]; then
            say "note: $found comes first on PATH, so 'glidesh' still runs that one"
        fi
        ;;
    *) say "note: $dir is not on PATH; add it, e.g. export PATH=\"$dir:\$PATH\"" ;;
    esac
}

# Everything runs from here, so a download cut short runs nothing.
main "$@"
