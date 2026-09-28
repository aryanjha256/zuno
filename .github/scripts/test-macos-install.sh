#!/bin/sh
#
# CI only: drive `scripts/install.sh`'s macOS path end to end against fake releases built here,
# the macOS twin of `test-tarball-install.sh`.
#
# **The fake is packed by the real `macos-bundle.sh`**, around a stand-in compiled with `cc` —
# a real Mach-O, so the ad-hoc signature is a real one and `codesign --verify` after the install
# proves the archive and the installer's copy both kept the seal. A shell-script stand-in would
# keep its signature in extended attributes, which is a different path from the one we ship.
#
#   sh test-macos-install.sh /path/to/repo
#
set -eu

src=${1:?usage: test-macos-install.sh <repo>}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

fail() { printf '::error::%s\n' "$*" >&2; exit 1; }

make_release() {
    version=$1
    dir="$work/rel-$version"
    mkdir -p "$dir"
    printf '#include <stdio.h>\nint main(void) { puts("zuno stand-in %s"); return 0; }\n' \
        "$version" > "$dir/stand-in.c"
    cc -o "$dir/zuno" "$dir/stand-in.c"
    sh "$src/scripts/macos-bundle.sh" "$dir/zuno" "$version" "$dir" >/dev/null
    (cd "$dir" && shasum -a 256 "zuno-$version-aarch64-macos.tar.gz" > sha256sums.txt)
}

install_version() {
    ZUNO_VERSION=$1 ZUNO_DOWNLOAD_BASE="file://$work/rel-$1" ZUNO_APP_DIR=$2 \
        sh "$src/scripts/install.sh"
}

make_release 0.0.1
make_release 0.0.2

# An admin-writable folder, as /Applications is for the first account on a Mac: no sudo.
apps="$work/Applications"
mkdir -p "$apps"
# `|| fail "$out"` on every captured run: under `set -e` a bare `out=$(…)` exits the moment the
# install fails, before anything prints it, so the job died with an exit code and no reason.
out=$(install_version 0.0.1 "$apps" 2>&1) || fail "the install failed: $out"
printf '%s\n' "$out"
case "$out" in
    *"Checksum verified."*) echo "ok: verified with shasum" ;;
    *) fail "the checksum was not verified — macOS has no sha256sum" ;;
esac
case "$out" in
    *sudo*) fail "asked for sudo on a writable folder" ;;
esac

app="$apps/Zuno.app"
"$app/Contents/MacOS/zuno" | grep -q "stand-in 0.0.1" || fail "the installed app is not 0.0.1"
codesign --verify --strict --verbose=2 "$app" || fail "the installed app's signature is broken"
[ -z "$(find "$app" -name '._*')" ] || fail "AppleDouble files inside the bundle"

out=$(install_version 0.0.1 "$apps" 2>&1) || fail "the re-run failed: $out"
case "$out" in
    *"already the latest"*) echo "ok: re-run is a no-op" ;;
    *) fail "re-running did not report an up-to-date install: $out" ;;
esac

install_version 0.0.2 "$apps"
"$app/Contents/MacOS/zuno" | grep -q "stand-in 0.0.2" || fail "the update did not replace the app"
if [ -e "$apps/.Zuno.app.old" ] || [ -e "$apps/.Zuno.app.new" ]; then
    fail "left a staging copy"
fi

# A folder this account cannot write, as /Applications is for a standard account: the sudo
# fallback. The runner's sudo needs no password, which is what lets this run unattended.
locked="$work/Locked"
sudo mkdir -p "$locked"
sudo chmod 755 "$locked"
sudo chown root "$locked"
out=$(install_version 0.0.2 "$locked" 2>&1) || fail "the sudo install failed: $out"
printf '%s\n' "$out"
case "$out" in
    *"sudo will ask"*) echo "ok: fell back to sudo" ;;
    *) fail "did not fall back to sudo for a folder it cannot write" ;;
esac
"$locked/Zuno.app/Contents/MacOS/zuno" | grep -q "stand-in 0.0.2" || fail "the sudo install failed"
codesign --verify --strict "$locked/Zuno.app" || fail "the sudo install broke the signature"
sudo rm -rf "$locked"

echo "ok: the macOS path installs, verifies, no-ops, updates and falls back to sudo"
