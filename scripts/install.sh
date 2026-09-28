#!/bin/sh
#
# Zuno installer — install or upgrade, one command either way.
#
#   curl -fsSL https://raw.githubusercontent.com/aryanjha256/zuno/main/scripts/install.sh | sh
#
# Deliberately NOT `curl ... | sudo sh`: that would run the download and the version
# resolution as root too. This runs as you, and calls sudo only for the apt line.
#
# **Two ways in, chosen by what the system has.** With apt, the .deb, installed system-wide —
# `apt-get install ./file.deb` is install and upgrade in one, so there is no branch for either.
# Anywhere else — Fedora, openSUSE, Arch — the tarball, unpacked into ~/.local with no sudo at
# all: the same binary, laid out as a prefix. It is an upgrade too, because it overwrites.
#
# **On macOS, Zuno.app into /Applications**, with sudo only when that folder is not writable —
# it is for an admin account, which the first account on a Mac is. Downloaded by curl, which
# sets no quarantine flag, so the ad-hoc signed app opens without a Gatekeeper prompt; the same
# archive fetched by a browser would be blocked. Apple Silicon only.
#
# Knobs:
#   ZUNO_VERSION=0.2.4    install that version instead of the latest (bisecting a regression)
#   ZUNO_ASSUME_YES=1     never prompt; required when there is no terminal
#   ZUNO_FORCE=1          reinstall even when the wanted version is already installed
#   ZUNO_METHOD=tarball   force a method — `deb` or `tarball` — e.g. a no-sudo install on Ubuntu
#   ZUNO_PREFIX=~/.local  where the tarball goes (default ~/.local)
#   ZUNO_APP_DIR=…        where Zuno.app goes on macOS (default /Applications)
#   ZUNO_DOWNLOAD_BASE=…  fetch the assets from here instead of the release — a mirror, or a
#                         local server when testing this script before a release exists
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
# binary dies with "cannot execute", too-old glibc dies inside the loader, and a musl system
# dies with a "not found" that names a file which plainly exists.

require_supported_os() {
    case "$(uname -s)" in
        Linux | Darwin) ;;
        *) die "this installer supports Linux and macOS (found $(uname -s))" ;;
    esac
}

# The SHA-256 of a file, or nothing when there is no tool to compute it. **macOS has no
# `sha256sum`** — it ships `shasum` — and the check below used to skip verification whenever
# `sha256sum` was missing, so a Mac install would have skipped it every time, silently.
sha256_of() {
    if have sha256sum; then
        sha256sum "$1" | cut -d' ' -f1
    elif have shasum; then
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

# `deb` where apt can install one, `tarball` everywhere else — or whatever ZUNO_METHOD says.
install_method() {
    case "${ZUNO_METHOD:-}" in
        deb)
            { have dpkg-query && have apt-get; } \
                || die "ZUNO_METHOD=deb, but this system has no dpkg/apt"
            printf 'deb'
            ;;
        tarball) printf 'tarball' ;;
        "")
            if have dpkg-query && have apt-get; then printf 'deb'; else printf 'tarball'; fi
            ;;
        *) die "ZUNO_METHOD must be deb or tarball, not ${ZUNO_METHOD}" ;;
    esac
}

require_tools() {
    method=$1
    if ! have curl; then
        if [ "$method" = "deb" ]; then
            die "curl is required. Install it with: sudo apt-get install -y curl"
        fi
        die "curl is required. Install it with your package manager and re-run."
    fi
    if [ "$method" = "tarball" ]; then
        have tar || die "tar is required to unpack Zuno. Install it and re-run."
    fi
}

# A musl system reports a glibc-style `ldd` with no version in it, so the floor check below
# would wave it through — and the binary, built against glibc, then fails with "not found" on
# a path that exists. Said plainly instead.
require_not_musl() {
    have ldd || return 0
    if ldd --version 2>&1 | grep -qi musl; then
        die "this system uses musl (Alpine, Void musl…) and Zuno is built against glibc.
Build from source: https://github.com/$REPO"
    fi
    return 0
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

# `$1` < `$2`, as version strings.
#
# **`sort -V`, because `dpkg --compare-versions` exists only on Debian** — and the tarball path
# runs everywhere else. Only ever given plain dotted versions (a glibc release, a Zuno tag with
# its Debian revision stripped), which is where the two agree.
version_lt() {
    [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -n 1)" = "$1" ]
}

# Everything is built on ubuntu-22.04, so the binary needs glibc 2.35+ — dpkg-shlibdeps writes it
# into the .deb as `libc6 (>= 2.35)`, and the tarball carries the same binary with nothing to
# enforce it. apt would refuse the .deb anyway; checking here turns both into a sentence
# someone can act on.
require_glibc() {
    have ldd || return 0
    found=$(ldd --version 2>/dev/null | head -n 1 | grep -oE '[0-9]+\.[0-9]+$' || true)
    [ -n "$found" ] || return 0
    if version_lt "$found" "2.35"; then
        die "glibc $found is too old — Zuno needs 2.35+ (Ubuntu 22.04+, Debian 12+, Fedora 36+)"
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

# Where one release's assets live — the release itself, or `ZUNO_DOWNLOAD_BASE`.
download_base() {
    printf '%s' "${ZUNO_DOWNLOAD_BASE:-$RELEASES/download/v$1}"
}

# The checksum list, fetched first because it also names the assets. Missing is not an error
# here: releases published before it existed have none, and `download` says so.
fetch_sums() {
    version=$1
    dir=$2
    curl -fsSL -o "$dir/sha256sums.txt" "$(download_base "$version")/sha256sums.txt" \
        2>/dev/null || true
}

# The tarball's name. **No Debian revision to guess**, unlike the .deb: it is named after the
# tag, so the constructed name is only a fallback for a list that did not mention it.
tarball_name() {
    version=$1
    sums=$2
    if [ -s "$sums" ]; then
        name=$(grep -oE "zuno-[^ ]*-x86_64-linux\.tar\.gz" "$sums" | head -n 1 || true)
        [ -n "$name" ] && { printf '%s' "$name"; return 0; }
        # Listed sums with no tarball in them: a release from before tarballs shipped. Named
        # here, because the 404 the download would otherwise hit says nothing about why.
        die "Zuno $version was published before the tarball existed, so there is none to install
here. Install a newer version, or on Debian/Ubuntu use ZUNO_METHOD=deb."
    fi
    printf 'zuno-%s-x86_64-linux.tar.gz' "$version"
}

download() {
    version=$1
    file=$2
    dir=$3
    base=$(download_base "$version")

    info "Downloading $file…"
    # HTTPS only — plus `file`, for a `ZUNO_DOWNLOAD_BASE` pointing at a local directory when
    # testing this script before a release exists. Safe to allow: it applies to the URL as
    # given, and curl never follows a *redirect* into `file://`, so the default GitHub download
    # is exactly as strict as `=https` alone.
    curl -fsSL --proto '=https,file' --tlsv1.2 -o "$dir/$file" "$base/$file" \
        || die "could not download $file.
Check that version $version exists: $RELEASES"

    # A mismatch is always fatal. A *missing* checksum file is not, or pinning a release
    # published before this installer existed would be impossible.
    if [ -s "$dir/sha256sums.txt" ]; then
        expected=$(sum_for "$file" "$dir/sha256sums.txt" || true)
        actual=$(sha256_of "$dir/$file")
        [ -n "$expected" ] || die "$file is not listed in sha256sums.txt — refusing to install"
        # A published list and no way to check against it is a refusal, not a skip: the skip is
        # the silent failure `sha256_of` exists to remove.
        [ -n "$actual" ] || die "no sha256sum or shasum to verify $file with — refusing to install"
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

install_via_deb() {
    arch=$1
    wanted=$2
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
    fetch_sums "$wanted" "$WORKDIR"
    file=$(asset_name "$wanted" "$arch" "$WORKDIR/sha256sums.txt")
    package=$(download "$wanted" "$file" "$WORKDIR")
    install_deb "$package" "$downgrade"

    warn_about_vulkan
    info ""
    info "Zuno $wanted is installed. Launch it from your applications menu, or run: zuno"
}

# --- tarball ------------------------------------------------------------------------

PREFIX="${ZUNO_PREFIX:-$HOME/.local}"

# What an unpacked copy says it is. There is no package database to ask, which is why the
# tarball carries `VERSION` at all.
tarball_version() {
    file="$PREFIX/share/doc/zuno/VERSION"
    [ -r "$file" ] || return 0
    head -n 1 "$file" | tr -d '[:space:]'
}

install_tarball() {
    archive=$1
    stage="$WORKDIR/unpacked"
    mkdir -p "$stage"
    tar -xzf "$archive" -C "$stage" --strip-components=1 \
        || die "could not unpack $archive"
    [ -x "$stage/bin/zuno" ] || die "the archive has no bin/zuno — refusing to install it"

    mkdir -p "$PREFIX/bin" "$PREFIX/share/applications" \
        "$PREFIX/share/icons/hicolor/scalable/apps" "$PREFIX/share/doc/zuno" \
        || die "cannot write to $PREFIX. Set ZUNO_PREFIX to a directory you own."

    # **Renamed into place, not copied over.** Writing into an executable that is running fails
    # with "Text file busy", and updating while Zuno is open is exactly when people run this.
    # A rename swaps the directory entry and leaves the running copy's inode alone.
    cp "$stage/bin/zuno" "$PREFIX/bin/.zuno.new"
    chmod 755 "$PREFIX/bin/.zuno.new"
    mv -f "$PREFIX/bin/.zuno.new" "$PREFIX/bin/zuno"

    # **`Exec=` rewritten to the absolute path.** A launcher does not always start apps with
    # ~/.local/bin on PATH, so `Exec=zuno` could be a menu entry that silently does nothing.
    # Quoted, for a home directory with a space in it. Line by line in sh rather than `sed`,
    # whose replacement text would read a `|` or `&` in the path as syntax.
    while IFS= read -r line || [ -n "$line" ]; do
        case "$line" in
            Exec=zuno) printf 'Exec="%s"\n' "$PREFIX/bin/zuno" ;;
            *) printf '%s\n' "$line" ;;
        esac
    done < "$stage/share/applications/dev.zuno.Zuno.desktop" \
        > "$PREFIX/share/applications/dev.zuno.Zuno.desktop"

    cp "$stage/share/icons/hicolor/scalable/apps/dev.zuno.Zuno.svg" \
        "$PREFIX/share/icons/hicolor/scalable/apps/dev.zuno.Zuno.svg"
    cp "$stage/share/doc/zuno/"* "$PREFIX/share/doc/zuno/"

    # So the menu picks the entry up without a logout. Optional: absent on minimal systems,
    # where the next login does the same.
    if have update-desktop-database; then
        update-desktop-database "$PREFIX/share/applications" >/dev/null 2>&1 || true
    fi
}

install_via_tarball() {
    wanted=$1
    current=$(tarball_version)

    # **Before any question is asked**, so a version that has no tarball is refused up front
    # rather than after someone has agreed to a downgrade that cannot happen.
    WORKDIR=$(mktemp -d)
    fetch_sums "$wanted" "$WORKDIR"
    file=$(tarball_name "$wanted" "$WORKDIR/sha256sums.txt")

    if [ -n "$current" ]; then
        if [ "$current" = "$wanted" ] && [ "${ZUNO_FORCE:-0}" != "1" ]; then
            info "Zuno $current is already the latest. Re-run with ZUNO_FORCE=1 to reinstall."
            return 0
        fi
        if version_lt "$wanted" "$current"; then
            confirm "Zuno $current is installed. Downgrade to $wanted?" \
                || die "cancelled."
        else
            info "Zuno $current is installed. Updating to $wanted."
        fi
    else
        info "Installing Zuno $wanted into $PREFIX — no sudo needed."
    fi

    archive=$(download "$wanted" "$file" "$WORKDIR")
    install_tarball "$archive"

    if have ldconfig && ! ldconfig -p 2>/dev/null | grep -q 'libvulkan\.so\.1'; then
        info ""
        info "Note: no Vulkan loader was found, and Zuno renders through Vulkan. If it fails to"
        info "start, install your distribution's Vulkan loader and driver (on Fedora:"
        info "vulkan-loader and mesa-vulkan-drivers)."
    fi

    info ""
    info "Zuno $wanted is installed in $PREFIX."
    case ":$PATH:" in
        *":$PREFIX/bin:"*)
            info "Launch it from your applications menu, or run: zuno"
            ;;
        *)
            info "Launch it from your applications menu, or run: $PREFIX/bin/zuno"
            info "($PREFIX/bin is not on your PATH — add it to run plain \`zuno\`.)"
            ;;
    esac
}

# --- macOS --------------------------------------------------------------------------

APP_DIR="${ZUNO_APP_DIR:-/Applications}"

# **Asked of the hardware, not of `uname -m`**, which reports x86_64 from a shell running under
# Rosetta on an Apple Silicon Mac. Intel Macs have no `hw.optional.arm64` at all.
require_apple_silicon() {
    # By full path: `sysctl` is in /usr/sbin, which a stripped-down PATH can leave out, and a
    # missing command here would read as "this Mac is Intel".
    if [ "$(/usr/sbin/sysctl -n hw.optional.arm64 2>/dev/null || true)" != "1" ]; then
        die "Zuno for macOS is built for Apple Silicon only, and this Mac is Intel.
Build from source: https://github.com/$REPO"
    fi
}

# What the installed app says it is — its own Info.plist, the macOS counterpart of `VERSION`.
app_version() {
    plist="$APP_DIR/Zuno.app/Contents/Info.plist"
    [ -r "$plist" ] || return 0
    /usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$plist" 2>/dev/null || true
}

# The archive's name, read out of the checksum list as the tarball's is.
app_archive_name() {
    version=$1
    sums=$2
    if [ -s "$sums" ]; then
        name=$(grep -oE "zuno-[^ ]*-aarch64-macos\.tar\.gz" "$sums" | head -n 1 || true)
        [ -n "$name" ] && { printf '%s' "$name"; return 0; }
        die "Zuno $version was published before macOS builds existed, so there is none to install."
    fi
    printf 'zuno-%s-aarch64-macos.tar.gz' "$version"
}

# sudo only when this account cannot write the app or the folder it goes in: a standard
# (non-admin) account, or a Zuno.app an earlier install left owned by root.
app_sudo() {
    SUDO=""
    if { [ -d "$APP_DIR" ] || mkdir -p "$APP_DIR" 2>/dev/null; } \
        && [ -w "$APP_DIR" ] \
        && { [ ! -e "$APP_DIR/Zuno.app" ] || [ -w "$APP_DIR/Zuno.app" ]; }; then
        return 0
    fi
    have sudo || die "$APP_DIR is not writable and sudo is not available.
Set ZUNO_APP_DIR to a folder you own, e.g. ZUNO_APP_DIR=~/Applications."
    info "$APP_DIR is not writable by this account — sudo will ask for your password."
    sudo -v || die "could not get root via sudo.
Under \`curl | sh\` sudo reads the password from the terminal, so this needs one.
Or install where you can write: ZUNO_APP_DIR=~/Applications."
    SUDO="sudo"
    $SUDO mkdir -p "$APP_DIR"
}

install_app() {
    archive=$1
    stage="$WORKDIR/unpacked"
    mkdir -p "$stage"
    tar -xzf "$archive" -C "$stage" || die "could not unpack $archive"
    [ -x "$stage/Zuno.app/Contents/MacOS/zuno" ] \
        || die "the archive has no Zuno.app — refusing to install it"

    app_sudo

    # **Swapped in by rename, as the Linux binary is**, so an update while Zuno is open leaves
    # the running copy alone. `ditto` rather than `cp -R`, because it is the copy macOS defines
    # as preserving a bundle exactly — the code signature seals every file in it.
    dest="$APP_DIR/Zuno.app"
    new="$APP_DIR/.Zuno.app.new"
    old="$APP_DIR/.Zuno.app.old"
    $SUDO rm -rf "$new" "$old"
    $SUDO ditto "$stage/Zuno.app" "$new"
    if [ -e "$dest" ]; then
        $SUDO mv "$dest" "$old"
    fi
    $SUDO mv "$new" "$dest"
    $SUDO rm -rf "$old"
}

install_via_app() {
    wanted=$1
    current=$(app_version)

    WORKDIR=$(mktemp -d)
    fetch_sums "$wanted" "$WORKDIR"
    file=$(app_archive_name "$wanted" "$WORKDIR/sha256sums.txt")

    if [ -n "$current" ]; then
        if [ "$current" = "$wanted" ] && [ "${ZUNO_FORCE:-0}" != "1" ]; then
            info "Zuno $current is already the latest. Re-run with ZUNO_FORCE=1 to reinstall."
            return 0
        fi
        if version_lt "$wanted" "$current"; then
            confirm "Zuno $current is installed. Downgrade to $wanted?" \
                || die "cancelled."
        else
            info "Zuno $current is installed. Updating to $wanted."
        fi
    else
        info "Installing Zuno $wanted into $APP_DIR."
    fi

    archive=$(download "$wanted" "$file" "$WORKDIR")
    install_app "$archive"

    info ""
    info "Zuno $wanted is installed in $APP_DIR. Open it from Launchpad or Spotlight, or run:"
    info "  open -a Zuno"
}

main_macos() {
    case "${ZUNO_METHOD:-}" in
        "" | app) ;;
        *) die "on macOS Zuno installs as an app; ZUNO_METHOD=${ZUNO_METHOD} is for Linux" ;;
    esac
    have curl || die "curl is required, and ships with macOS — is it on your PATH?"
    require_apple_silicon
    wanted=$(resolve_version)
    install_via_app "$wanted"
}

main() {
    require_supported_os
    if [ "$(uname -s)" = "Darwin" ]; then
        main_macos
        return 0
    fi
    method=$(install_method)
    require_tools "$method"
    require_not_musl
    require_glibc
    arch=$(detect_arch)
    wanted=$(resolve_version)

    case "$method" in
        deb) install_via_deb "$arch" "$wanted" ;;
        tarball) install_via_tarball "$wanted" ;;
    esac
}

main "$@"
