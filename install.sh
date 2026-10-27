#!/bin/sh
# Installs blaze, so that `blaze build`, `blaze test` and `blaze run` work (and the same with
# `cargo blaze`), on a machine with or without Rust:
#
#   curl -fsSL https://ix.dev/blaze | sh
#   curl -fsSL https://ix.dev/blaze | sh -s -- --version 0.2.0
#
# It downloads the release built for this machine (macOS on Apple silicon, Linux on x86-64 or
# ARM64), checks it against the checksums published with it, puts `blaze`, `cargo-blaze` and the
# programs they run in ${CARGO_HOME:-~/.cargo}/bin, and has blaze download the Rust it builds
# with, once (100 to 135 MB; none when rustup already has it). It never asks anything. Running it
# again installs the newest release over the one there.
#
#   --version V        install release V (or BLAZE_VERSION=V) instead of the newest
#   --no-toolchain     download Rust at the first build instead (BLAZE_NO_TOOLCHAIN=1)
#   --verbose          say each step
#   --yes, -y          accepted; nothing is asked anyway
#
# BLAZE_REPO=owner/name installs from another repository's releases. BLAZE_ARCHIVE=file installs
# that archive, a build of blaze laid out as a release's, instead of downloading one.

set -eu

REPO="${BLAZE_REPO:-indexable-inc/blaze}"
# harness/toolchain; harness/toolchain_bump.py writes it
TOOLCHAIN="nightly-2026-10-27"
VERSION="${BLAZE_VERSION:-}"
NO_TOOLCHAIN="${BLAZE_NO_TOOLCHAIN:-}"
ARCHIVE="${BLAZE_ARCHIVE:-}"
VERBOSE=""

say() { printf '%s\n' "$*"; }
step() { if [ -n "$VERBOSE" ]; then printf '  %s\n' "$*"; fi; }
# What went wrong in plain words, then the details for whoever wants them.
fail() {
    printf 'error: %s\n' "$1" >&2
    if [ $# -gt 1 ] && [ -n "$2" ]; then printf '%s\n' "$2" | sed 's/^/  /' >&2; fi
    exit 1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -gt 1 ] || fail "--version needs a version, such as --version 0.2.0"
            VERSION="$2"
            shift 2
            ;;
        --version=*) VERSION="${1#--version=}"; shift ;;
        --no-toolchain) NO_TOOLCHAIN=1; shift ;;
        --verbose | -v) VERBOSE=1; shift ;;
        --yes | -y) shift ;;
        --help | -h)
            sed -n '2,/^$/s/^# \{0,1\}//p' "$0" 2>/dev/null || say "See https://ix.dev/blaze"
            exit 0
            ;;
        *) fail "this installer has no option $1" "It takes --version V, --no-toolchain, --verbose and --yes." ;;
    esac
done

# ---------------------------------------------------------------------------------------------
# Which build this machine takes

os=$(uname -s)
arch=$(uname -m)
case "$os" in
    Darwin)
        # A shell under Rosetta says x86_64 on an Apple silicon Mac.
        if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
            arch=arm64
        fi
        case "$arch" in
            arm64 | aarch64) target=aarch64-apple-darwin; machine="macOS on Apple silicon" ;;
            *) fail "blaze has no build for Intel Macs yet." "It runs on macOS with Apple silicon and on Linux (x86-64, ARM64)." ;;
        esac
        ;;
    Linux)
        case "$arch" in
            x86_64 | amd64) target=x86_64-unknown-linux-gnu; machine="Linux on x86-64" ;;
            aarch64 | arm64) target=aarch64-unknown-linux-gnu; machine="Linux on ARM64" ;;
            *) fail "blaze has no build for Linux on $arch yet." "It runs on Linux on x86-64 and ARM64, and on macOS with Apple silicon." ;;
        esac
        if ldd --version 2>&1 | grep -qi musl; then
            fail "blaze's Linux builds need glibc, and this system uses musl (Alpine?)." \
                "A glibc-based distribution (Debian, Ubuntu, Fedora, ...) or a container of one works."
        fi
        # The newest glibc blaze's Linux builds ask for (GLIBC in .github/scripts/build_release.py).
        glibc_floor=34
        glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null | sed -n 's/^glibc //p')
        glibc_major=${glibc%%.*}
        glibc_minor=${glibc#*.}
        glibc_minor=${glibc_minor%%.*}
        case "$glibc_major.$glibc_minor" in
            *[!0-9.]* | .* | *.) ;;
            *)
                if [ "$glibc_major" -lt 2 ] || { [ "$glibc_major" -eq 2 ] && [ "$glibc_minor" -lt "$glibc_floor" ]; }; then
                    fail "blaze's Linux builds need glibc 2.$glibc_floor or newer, and this system has glibc $glibc." \
                        "RHEL 9, Debian 12, Ubuntu 22.04 and newer have it, as does a container of one."
                fi
                ;;
        esac
        ;;
    MINGW* | MSYS* | CYGWIN* | Windows_NT)
        fail "this installer is for macOS and Linux." "On Windows, install with: cargo +$TOOLCHAIN install --locked cargo-blaze"
        ;;
    *) fail "blaze has no build for $os yet." "It runs on macOS with Apple silicon and on Linux (x86-64, ARM64)." ;;
esac

# ---------------------------------------------------------------------------------------------
# Downloading

if command -v curl >/dev/null 2>&1; then
    fetch() { curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o "$2" "$1"; }
    # Where a URL ends up after its redirects.
    landing() { curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o /dev/null -w '%{url_effective}' "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q -O "$2" "$1"; }
    landing() { wget -q -S --spider "$1" 2>&1 | sed -n 's/^ *[Ll]ocation: *//p' | tail -n 1 | tr -d '\r'; }
else
    fail "downloading blaze needs curl or wget, and neither is installed."
fi

if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
elif command -v openssl >/dev/null 2>&1; then
    sha256() { openssl dgst -sha256 -r "$1" | cut -d ' ' -f 1; }
else
    fail "checking the download needs sha256sum, shasum or openssl, and none is installed."
fi

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t blaze)
trap 'rm -rf "$tmp"' EXIT
trap 'exit 130' INT TERM

if [ -n "$ARCHIVE" ]; then
    [ -f "$ARCHIVE" ] || fail "there is no file $ARCHIVE to install blaze from." "BLAZE_ARCHIVE names a release's archive, such as blaze-v0.2.0-$target.tar.gz."
    asset=$(basename "$ARCHIVE")
    tag=""
    say "Installing blaze from $asset for $machine..."
    cp "$ARCHIVE" "$tmp/$asset"
else
    releases="https://github.com/$REPO/releases"

    # The release: the one asked for, else the newest, else the nightly build before the first.
    if [ -n "$VERSION" ]; then
        tag="v${VERSION#v}"
        [ "$VERSION" = nightly ] && tag=nightly
    else
        latest=$(landing "$releases/latest" 2>/dev/null || true)
        case "$latest" in
            */releases/tag/*) tag="${latest##*/releases/tag/}" ;;
            *) tag=nightly ;;
        esac
    fi
    step "release $tag of $REPO"

    if ! fetch "$releases/download/$tag/SHA256SUMS" "$tmp/SHA256SUMS" 2>"$tmp/error"; then
        if [ -n "$VERSION" ]; then
            fail "there is no blaze release $VERSION." "Releases: $releases"
        fi
        fail "the list of blaze's releases could not be read; is the network up?" \
            "$(cat "$tmp/error")
$releases/download/$tag/SHA256SUMS"
    fi

    # The archive's name: blaze's since the rename, ra-jit's for a release from before it, and the
    # nightly build's without a version.
    asset=""
    for name in "blaze-$tag-$target.tar.gz" "ra-jit-$tag-$target.tar.gz" "ra-jit-$target.tar.gz"; do
        if awk -v n="$name" '$2 == n || $2 == "*" n { found = 1 } END { exit !found }' "$tmp/SHA256SUMS"; then
            asset="$name"
            break
        fi
    done
    [ -n "$asset" ] || fail "blaze release $tag has no build for $machine." "Its files: $releases/tag/$tag"
    want=$(awk -v n="$asset" '$2 == n || $2 == "*" n { print $1; exit }' "$tmp/SHA256SUMS")

    if [ "$tag" = nightly ]; then shown="the nightly build"; else shown="blaze ${tag#v}"; fi
    say "Installing $shown for $machine..."
    step "downloading $asset"
    fetch "$releases/download/$tag/$asset" "$tmp/$asset" 2>"$tmp/error" ||
        fail "the download of blaze stopped before it finished; running this again retries it." \
            "$(cat "$tmp/error")
$releases/download/$tag/$asset"
    got=$(sha256 "$tmp/$asset")
    [ "$got" = "$want" ] ||
        fail "the download is not the file the release published, so it was not installed. Running this again downloads it again." \
            "$asset: sha256 $got, the release's SHA256SUMS says $want"
fi

mkdir "$tmp/unpacked"
# -o: as root, tar gives files the archive's owner, which a container's user namespace may not have.
tar -xzof "$tmp/$asset" -C "$tmp/unpacked" 2>"$tmp/error" ||
    fail "the downloaded archive could not be unpacked." "$(cat "$tmp/error")"
unpacked="$tmp/unpacked/${asset%.tar.gz}"
[ -x "$unpacked/ra-jit" ] || fail "the archive does not have the files blaze needs." "$asset has no ${asset%.tar.gz}/ra-jit"

# ---------------------------------------------------------------------------------------------
# Installing

cargo_home="${CARGO_HOME:-$HOME/.cargo}"
bin="$cargo_home/bin"
mkdir -p "$bin" 2>"$tmp/error" || fail "blaze's folder could not be made." "$(cat "$tmp/error")"

# What `cargo install ra-jit` put there is replaced, and cargo should no longer think it its own.
if [ -f "$cargo_home/.crates.toml" ] && grep -q '^"ra-jit ' "$cargo_home/.crates.toml" && command -v cargo >/dev/null 2>&1; then
    step "removing the ra-jit that cargo installed"
    CARGO_HOME="$cargo_home" cargo uninstall --quiet ra-jit >/dev/null 2>&1 || true
fi

# Each program is copied beside its place and renamed into it, so that a daemon running the
# one before goes on running it, and is replaced by the next command.
put() {
    cp "$1" "$bin/.$2.blaze-new"
    chmod 755 "$bin/.$2.blaze-new"
    mv -f "$bin/.$2.blaze-new" "$bin/$2"
}
for program in "$unpacked"/*; do
    if [ -f "$program" ] && [ -x "$program" ]; then
        step "installing $(basename "$program")"
        put "$program" "$(basename "$program")"
    fi
done
# A release from before the rename has no cargo-blaze; this one runs its ra-jit the same way.
if [ ! -x "$unpacked/cargo-blaze" ]; then
    cat >"$tmp/cargo-blaze" <<EOF
#!/bin/sh
# \`cargo blaze\`, for a release from before the rename: runs the ra-jit installed beside it.
[ "\${1-}" = blaze ] && shift
exec "$bin/ra-jit" cargo "\$@"
EOF
    put "$tmp/cargo-blaze" cargo-blaze
fi

# ---------------------------------------------------------------------------------------------
# The Rust blaze builds with

rust_note=""
if [ -x "$unpacked/blaze" ]; then
    # blaze downloads its own copy, once, unless rustup has the one it builds with.
    if [ -n "$NO_TOOLCHAIN" ]; then
        step "Rust $TOOLCHAIN is downloaded at the first build"
    elif "$bin/blaze" toolchain >"$tmp/toolchain"; then
        step "$(cat "$tmp/toolchain")"
    else
        fail "blaze is installed, but the Rust it builds with could not be downloaded; the first build tries again."
    fi
else
    # A release from before blaze had its own copy builds with rustup's copy of the Rust it was
    # built for, which its tag's harness/toolchain names: not $TOOLCHAIN, which is this
    # installer's, and the binary refuses any other rustc.
    if [ -z "$tag" ]; then
        TOOLCHAIN=""
        rust_note="This archive has no blaze program, so it builds with rustup's copy of the Rust named in
harness/toolchain of the commit it was built from. Install that with:

    rustup toolchain install <that toolchain> --component rust-src"
    else
        fetch "https://raw.githubusercontent.com/$REPO/$tag/harness/toolchain" "$tmp/toolchain" 2>"$tmp/error" ||
            fail "blaze $tag is installed, but which Rust it builds with could not be read." \
                "$(cat "$tmp/error")
https://raw.githubusercontent.com/$REPO/$tag/harness/toolchain"
        TOOLCHAIN=$(tr -d ' \r\n' <"$tmp/toolchain")
        [ -n "$TOOLCHAIN" ] || fail "blaze $tag is installed, but its harness/toolchain names no Rust."
    fi
    has_toolchain() {
        rustup run "$TOOLCHAIN" rustc --version >/dev/null 2>&1 &&
            rustup component list --toolchain "$TOOLCHAIN" --installed 2>/dev/null | grep -q '^rust-src'
    }
    if [ -n "$NO_TOOLCHAIN" ] || [ -z "$TOOLCHAIN" ]; then
        step "leaving Rust alone"
    elif command -v rustup >/dev/null 2>&1; then
        if has_toolchain; then
            step "Rust $TOOLCHAIN is installed"
        else
            say "Installing Rust $TOOLCHAIN, which blaze builds with..."
            # What rustup says of a step after the install, such as its own place, is no failure
            # when the toolchain is there.
            rustup toolchain install "$TOOLCHAIN" --profile minimal --component rust-src >"$tmp/rustup" 2>&1 ||
                has_toolchain ||
                fail "rustup could not install Rust $TOOLCHAIN, which blaze builds with. blaze itself is installed." \
                    "$(tail -n 5 "$tmp/rustup")
Try: rustup toolchain install $TOOLCHAIN --component rust-src"
        fi
    else
        rust_note="This release of blaze builds with Rust $TOOLCHAIN, which needs rustup (https://rustup.rs).
Once it is installed, run:

    rustup toolchain install $TOOLCHAIN --component rust-src"
    fi
fi

# Rust links a program with the system's C compiler, which blaze does not bring.
linker_note=""
if [ "$os" = Darwin ]; then
    xcode-select -p >/dev/null 2>&1 ||
        linker_note="blaze links programs with Apple's command line tools, which are not installed. Install them with:

    xcode-select --install"
elif ! command -v cc >/dev/null 2>&1; then
    linker_note="blaze links programs with the system's C compiler (cc), and none is installed. Install one:

    sudo apt install gcc libc6-dev     (Debian, Ubuntu)
    sudo dnf install gcc               (Fedora)"
fi

# ---------------------------------------------------------------------------------------------
# What to do next

if [ -x "$unpacked/blaze" ]; then run="blaze run"; else run="cargo blaze run"; fi
case ":${PATH}:" in
    *":$bin:"*) path_note="" ;;
    *)
        # Under $HOME it reads as ~, which every shell expands.
        case "$bin" in "$HOME"/*) shown_bin="\$HOME/${bin#"$HOME"/}" ;; *) shown_bin="$bin" ;; esac
        path_note="Your shell finds blaze once your PATH has $shown_bin, which it does not yet. Add it with:

    export PATH=\"$shown_bin:\$PATH\"

and put that line in your shell's profile (~/.zshrc, ~/.bashrc) to keep it."
        ;;
esac

say ""
case "$tag" in
    "") say "blaze is installed." ;;
    nightly) say "blaze (nightly build) is installed." ;;
    *) say "blaze ${tag#v} is installed." ;;
esac
if [ -n "$rust_note" ]; then say ""; say "$rust_note"; fi
if [ -n "$linker_note" ]; then say ""; say "$linker_note"; fi
if [ -n "$path_note" ]; then say ""; say "$path_note"; fi
say ""
say "In a Rust project, run:"
say ""
say "    $run"
if [ "$run" = "blaze run" ] && command -v cargo >/dev/null 2>&1; then
    say ""
    say "(cargo blaze run is the same.)"
fi
