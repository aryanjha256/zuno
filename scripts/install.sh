#!/bin/sh
#
# Zuno installer — install or upgrade, one command either way.
#
#   curl -fsSL https://raw.githubusercontent.com/aryanjha256/zuno/main/scripts/install.sh | sh
#
# Deliberately NOT `curl ... | sudo sh`: that would run the download and the version
# resolution as root too. This runs as you, and calls sudo only for the apt line.
#
# There is no install/upgrade branch, because `apt-get install ./file.deb` is already both.
#
# Knobs:
#   ZUNO_VERSION=0.2.4    install that version instead of the latest (bisecting a regression)
#   ZUNO_ASSUME_YES=1     never prompt; required when there is no terminal
#   ZUNO_FORCE=1          reinstall even when the wanted version is already installed
#
set -eu

REPO="aryanjha256/zuno"
RELEASES="https://github.com/$REPO/releases"

WORKDIR=""
# `return 0` is load-bearing: an EXIT trap's last command sets the script's exit status, so
# without it a successful install exits 1 whenever WORKDIR is empty. Verified in sh, dash
# and bash — the install works, the output is correct, and only the status code lies.
cleanup() {
    [ -n "$WORKDIR" ] && rm -rf "$WORKDIR"
    return 0
}
trap cleanup EXIT INT TERM

info() { printf '%s\n' "$*" >&2; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# --- preflight ----------------------------------------------------------------------
#
# Each check exists because its failure is otherwise silent or cryptic: a wrong-arch
# binary dies with "cannot execute", too-old glibc dies inside the loader, and a non-dpkg
# distro gets "apt-get: not found" rather than being told Zuno has no package for it.

require_debian() {
    [ "$(uname -s)" = "Linux" ] || die "this installer supports Linux only (found $(uname -s))"
    if ! have dpkg-query || ! have apt-get; then
        die "Zuno ships a .deb and this system has no dpkg/apt.
Download the package or build from source: $RELEASES/latest"
    fi
}

require_tools() {
    have curl || die "curl is required. Install it with: sudo apt-get install -y curl"
}

detect_arch() {
    arch=$(uname -m)
    case "$arch" in
        x86_64 | amd64) echo "amd64" ;;
        aarch64 | arm64) die "no arm64 package is published yet (found $arch).
Build from source: https://github.com/$REPO" ;;
        *) die "unsupported architecture: $arch" ;;
    esac
}

# The .deb is built on ubuntu-22.04 so dpkg-shlibdeps emits `libc6 (>= 2.35)`. apt would
# refuse the install anyway; checking here turns that into a sentence someone can act on.
require_glibc() {
    have ldd || return 0
    found=$(ldd --version 2>/dev/null | head -n 1 | grep -oE '[0-9]+\.[0-9]+$' || true)
    [ -n "$found" ] || return 0
    if dpkg --compare-versions "$found" lt "2.35"; then
        die "glibc $found is too old — Zuno needs 2.35+ (Ubuntu 22.04+, Debian 12+)"
    fi
    return 0
}

# Vulkan is dlopen'ed by gpui, so a missing ICD is not something apt can resolve from the
# package: `libvulkan1` is a Depends and apt will pull the loader, but `mesa-vulkan-drivers`
# is only a Recommends, because a machine on the proprietary NVIDIA driver already has an
# ICD and must not be forced to pull in Mesa.
warn_about_vulkan() {
    have ldconfig || return 0
    if ldconfig -p 2>/dev/null | grep -q 'libvulkan\.so\.1'; then
        return 0
    fi
    info ""
    info "Note: no Vulkan loader was found, and Zuno renders through Vulkan. If it fails to"
    info "start, install a driver:  sudo apt-get install -y mesa-vulkan-drivers"
    return 0
}

# --- versions -----------------------------------------------------------------------

# Read the tag from the redirect /releases/latest performs, rather than from the GitHub
# API. Two reasons: the API's unauthenticated limit is 60/hour *per IP*, which an office
# behind one NAT can exhaust, and this needs no JSON parser, so no jq dependency and no
# grepping JSON by hand.
resolve_version() {
    if [ -n "${ZUNO_VERSION:-}" ]; then
        printf '%s' "${ZUNO_VERSION#v}"
        return 0
    fi
    url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$RELEASES/latest" 2>/dev/null) \
        || die "could not reach GitHub to find the latest version. Are you online?"
    case "$url" in
        */tag/*) ;;
        *) die "could not read the latest version from $url" ;;
    esac
    tag=${url##*/tag/}
    printf '%s' "${tag#v}"
}

# dpkg reports cargo-deb's packaged version, which carries a Debian revision: `0.2.5-1`
# against a tag of `0.2.5`. Comparing the two directly reports an up-to-date machine as
# *newer* than the release — verified, not assumed — so the revision is stripped first.
installed_version() {
    raw=$(dpkg-query -W -f='${Version}' zuno 2>/dev/null || true)
    printf '%s' "${raw%%-*}"
}

# --- download -----------------------------------------------------------------------

# The script itself arrives on stdin under `curl | sh`, so the answer is read from
# /dev/tty rather than stdin, which a bare `read` would consume. sudo is unaffected by
# this — it already reads its password from /dev/tty for the same reason.
#
# A read that *fails* is a refusal, never the `[Y/n]` default. `[ -r /dev/tty ]` was the
# first guard and is not one: it stats the file, so it passes with no controlling terminal
# attached, and the failed read then left an empty reply that the default read as **yes**.
# A prompt that cannot be answered must not answer itself.
confirm() {
    [ "${ZUNO_ASSUME_YES:-0}" = "1" ] && return 0
    printf '%s [Y/n] ' "$1" >&2
    # The group's redirect, not the command's: the "cannot open /dev/tty" message comes
    # from the shell rather than from `read`, so a redirect on `read` alone never sees it.
    if ! { read -r reply < /dev/tty; } 2>/dev/null; then
        info ""
        die "no terminal to read an answer from. Re-run with ZUNO_ASSUME_YES=1 to accept."
    fi
    case "$reply" in "" | y | Y | yes | YES) return 0 ;; *) return 1 ;; esac
}

# The asset name carries a Debian revision the git tag does not (`zuno_0.2.5-1_amd64.deb`),
# and guessing it is the kind of thing that silently 404s. `sha256sums.txt` lists the real
# filename beside its hash, so one small file answers both "what is it called" and "did it
# arrive intact". Releases published before that file existed fall back to the constructed
# name, which is why this returns the name rather than requiring the checksum.
asset_name() {
    version=$1
    arch=$2
    sums=$3
    if [ -s "$sums" ]; then
        name=$(grep -oE "zuno_[^ ]*_${arch}\.deb" "$sums" | head -n 1 || true)
        [ -n "$name" ] && { printf '%s' "$name"; return 0; }
    fi
    printf 'zuno_%s-1_%s.deb' "$version" "$arch"
}

# The recorded hash for one file.
#
# Compares the *name* field rather than matching the whole line, because how that name is spelled
# depends on how the checksums were generated: `sha256sum ./*.deb` writes `./zuno_…deb`, a bare
# glob writes `zuno_…deb`, and binary mode prefixes a `*`. A line match worked against every
# release that had no checksum file and failed on the first one that did — which is the worst
# possible moment for it, and is exactly when it failed.
sum_for() {
    want=$1
    sums_path=$2
    while read -r sum_hash sum_name; do
        sum_name=${sum_name#./}
        sum_name=${sum_name#\*}
        if [ "$sum_name" = "$want" ]; then
            printf '%s' "$sum_hash"
            return 0
        fi
    done < "$sums_path"
    return 1
}

download() {
    version=$1
    arch=$2
    dir=$3
    base="$RELEASES/download/v${version}"

    curl -fsSL -o "$dir/sha256sums.txt" "$base/sha256sums.txt" 2>/dev/null || true
    file=$(asset_name "$version" "$arch" "$dir/sha256sums.txt")

    info "Downloading $file…"
    curl -fsSL --proto '=https' --tlsv1.2 -o "$dir/$file" "$base/$file" \
        || die "could not download $file.
Check that version $version exists: $RELEASES"

    # A mismatch is always fatal. A *missing* checksum file is not, or pinning a release
    # published before this installer existed would be impossible.
    if [ -s "$dir/sha256sums.txt" ] && have sha256sum; then
        expected=$(sum_for "$file" "$dir/sha256sums.txt" || true)
        actual=$(sha256sum "$dir/$file" | cut -d' ' -f1)
        [ -n "$expected" ] || die "$file is not listed in sha256sums.txt — refusing to install"
        [ "$expected" = "$actual" ] || die "checksum mismatch on $file — refusing to install"
        info "Checksum verified."
    else
        warn "no checksum published for $version — skipping verification"
    fi

    printf '%s' "$dir/$file"
}

# Asked for *before* the download rather than after it, so the password prompt arrives
# while you are still watching the terminal instead of behind seven megabytes of wait.
# `sudo -v` also turns "sudo cannot authenticate" into its own message — folded into the
# apt failure below, it read as a stale package cache and sent the retry down the wrong path.
SUDO=""
ensure_root() {
    if [ "$(id -u)" = "0" ]; then
        return 0
    fi
    have sudo || die "need root to install, and sudo is not available. Re-run as root."
    info "Installing system-wide — sudo may ask for your password."
    sudo -v || die "could not get root via sudo.
Under \`curl | sh\` sudo reads the password from the terminal, so this needs one.
Re-run as root, or download the .deb yourself: $RELEASES/latest"
    SUDO="sudo"
    return 0
}

install_deb() {
    package=$1
    downgrade=$2

    extra=""
    [ "$downgrade" = "1" ] && extra="--allow-downgrades"

    # apt-get rather than apt, which prints "does not have a stable CLI interface" when
    # scripted. Retried once behind `apt-get update`, because resolving the package's
    # dependencies needs a populated cache and a long-idle machine has a stale one.
    #
    # The first attempt's stderr is held rather than discarded: discarding it hides the real
    # cause behind a "retrying" message, and printing it makes a recovered failure look like
    # a broken install. So it is shown only if the retry fails too.
    log="$WORKDIR/apt.log"
    # shellcheck disable=SC2086
    if ! $SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y $extra "$package" 2>"$log"; then
        info "Refreshing package lists and retrying…"
        $SUDO env DEBIAN_FRONTEND=noninteractive apt-get update >/dev/null 2>&1 || true
        # shellcheck disable=SC2086
        if ! $SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y $extra "$package"; then
            [ -s "$log" ] && cat "$log" >&2
            die "the install failed. The package is at $package if you want to inspect it."
        fi
    fi
}

main() {
    require_debian
    require_tools
    require_glibc
    arch=$(detect_arch)

    wanted=$(resolve_version)
    current=$(installed_version)
    downgrade=0

    if [ -n "$current" ]; then
        if [ "$current" = "$wanted" ] && [ "${ZUNO_FORCE:-0}" != "1" ]; then
            info "Zuno $current is already the latest. Re-run with ZUNO_FORCE=1 to reinstall."
            return 0
        fi
        if dpkg --compare-versions "$current" gt "$wanted"; then
            downgrade=1
            confirm "Zuno $current is installed. Downgrade to $wanted?" \
                || die "cancelled."
        else
            info "Zuno $current is installed. Updating to $wanted."
        fi
    else
        info "Installing Zuno $wanted."
    fi

    ensure_root

    WORKDIR=$(mktemp -d)
    package=$(download "$wanted" "$arch" "$WORKDIR")
    install_deb "$package" "$downgrade"

    warn_about_vulkan
    info ""
    info "Zuno $wanted is installed. Launch it from your applications menu, or run: zuno"
}

main "$@"
