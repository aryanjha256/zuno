#!/bin/sh
#
# CI only: drive `scripts/install.sh`'s tarball path end to end, as an ordinary user, against
# fake releases built here. Run inside a container with no apt, so the script has to choose the
# tarball by itself — which is half of what is being tested.
#
# **Fake releases, because no published release carries a tarball yet**, and this has to test
# the script *before* a tag exists: the published tarball is checked separately, at tag time, by
# `release.yml`'s `smoke-tarball`. The stand-in binary is a shell script that prints its version,
# laid out exactly as the real archive is, so everything but the binary itself is the real thing.
#
# Its own file rather than inline YAML because it is run as a second user through `su -c`, and a
# script nested in quotes nested in quotes is the shape `installer.yml` records as having cost a
# red build.
#
#   sh test-tarball-install.sh /path/to/repo
#
set -eu

src=${1:?usage: test-tarball-install.sh <repo>}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

fail() { printf '::error::%s\n' "$*" >&2; exit 1; }

# One fake release of version `$1`: the real layout, the real .desktop and icon, a checksum list.
make_release() {
    version=$1
    dir="$work/rel-$version"
    name="zuno-$version-x86_64-linux"
    stage="$dir/stage/$name"
    mkdir -p "$stage/bin"
    printf '#!/bin/sh\necho "zuno stand-in %s"\n' "$version" > "$stage/bin/zuno"
    chmod 755 "$stage/bin/zuno"
    install -Dm644 "$src/assets/dev.zuno.Zuno.desktop" "$stage/share/applications/dev.zuno.Zuno.desktop"
    install -Dm644 "$src/assets/icons/zuno.svg" "$stage/share/icons/hicolor/scalable/apps/dev.zuno.Zuno.svg"
    install -Dm644 "$src/README.md" "$stage/share/doc/zuno/README.md"
    install -Dm644 "$src/LICENSE" "$stage/share/doc/zuno/LICENSE"
    printf '%s\n' "$version" > "$stage/share/doc/zuno/VERSION"
    tar -C "$dir/stage" -czf "$dir/$name.tar.gz" "$name"
    (cd "$dir" && sha256sum -- "$name.tar.gz" > sha256sums.txt)
}

install_version() {
    ZUNO_VERSION=$1 ZUNO_DOWNLOAD_BASE="file://$work/rel-$1" sh "$src/scripts/install.sh"
}

make_release 0.0.1
make_release 0.0.2

# Fresh install. No ZUNO_METHOD: on a system without apt the tarball is the only way in.
out=$(install_version 0.0.1 2>&1)
printf '%s\n' "$out"
case "$out" in
    *"no sudo needed"*) echo "ok: chose the tarball path on its own" ;;
    *) fail "install.sh did not take the tarball path on a system without apt" ;;
esac

test -x "$HOME/.local/bin/zuno" || fail "no executable at ~/.local/bin/zuno"
"$HOME/.local/bin/zuno" | grep -q "stand-in 0.0.1" || fail "the installed binary is not 0.0.1"
test -f "$HOME/.local/share/icons/hicolor/scalable/apps/dev.zuno.Zuno.svg" || fail "no icon"
test -f "$HOME/.local/share/doc/zuno/VERSION" || fail "no VERSION"

# The launcher entry points at the absolute path, or a menu launch can silently find nothing.
desktop="$HOME/.local/share/applications/dev.zuno.Zuno.desktop"
grep -qx "Exec=\"$HOME/.local/bin/zuno\"" "$desktop" \
    || fail "Exec= was not rewritten to the absolute path: $(grep '^Exec=' "$desktop")"

# A re-run of the same version is a no-op, as it is for the .deb.
out=$(install_version 0.0.1 2>&1)
case "$out" in
    *"already the latest"*) echo "ok: re-run is a no-op" ;;
    *) fail "re-running did not report an up-to-date install: $out" ;;
esac

# And an update replaces it.
install_version 0.0.2
"$HOME/.local/bin/zuno" | grep -q "stand-in 0.0.2" || fail "the update did not replace the binary"

echo "ok: the tarball path installs, no-ops and updates"
