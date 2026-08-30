#!/usr/bin/env bash
#
# Cut a release: bump the version, refresh the lock, test, commit, tag.
#
#   scripts/release.sh 0.2.0        explicit version
#   scripts/release.sh patch        0.1.5 -> 0.1.6
#   scripts/release.sh minor        0.1.5 -> 0.2.0
#   scripts/release.sh major        0.1.5 -> 1.0.0
#
#   --push        push the commit and the tag when everything passes
#   --no-test     skip the suite (the release workflow runs it again anyway)
#
# The version is written in exactly one place, `[workspace.package]` in Cargo.toml — both
# members take it with `version.workspace = true`, the engine's User-Agent reads it through
# `env!("CARGO_PKG_VERSION")`, and cargo-deb names the package from it. Nothing else needs
# editing, and the tag is checked against it by the release workflow.
#
# Nothing is pushed without `--push`. A commit and a tag are both local and both undoable;
# a pushed tag builds and publishes a GitHub Release.

set -euo pipefail

die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
step() { printf '\033[36m==>\033[0m %s\n' "$*"; }

root=$(git rev-parse --show-toplevel 2>/dev/null) || die "not inside a git repository"
cd "$root"
[ -f Cargo.toml ] || die "no Cargo.toml at the repository root"

bump=""
push=false
run_tests=true
for arg in "$@"; do
    case "$arg" in
        --push) push=true ;;
        --no-test) run_tests=false ;;
        -h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        -*) die "unknown flag: $arg" ;;
        *) [ -z "$bump" ] || die "give one version, got '$bump' and '$arg'"; bump="$arg" ;;
    esac
done
[ -n "$bump" ] || die "usage: scripts/release.sh <version|patch|minor|major> [--push] [--no-test]"

current=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "zuno") | .version')
[ -n "$current" ] || die "could not read the current version from cargo metadata"

# Resolve a keyword against the current version, or take an explicit one as given.
case "$bump" in
    major|minor|patch)
        IFS=. read -r major minor patch <<<"$current"
        case "$bump" in
            major) version="$((major + 1)).0.0" ;;
            minor) version="$major.$((minor + 1)).0" ;;
            patch) version="$major.$minor.$((patch + 1))" ;;
        esac
        ;;
    *)
        [[ "$bump" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]] \
            || die "'$bump' is not a semver version or one of major|minor|patch"
        version="$bump"
        ;;
esac
tag="v$version"

step "$current -> $version"

# --- preflight, before anything is written ------------------------------------------
#
# Every one of these is cheaper to hit here than after a tag exists: a pushed tag is what
# triggers the publish, and an unpublish is a manual cleanup in two places.

[ -z "$(git status --porcelain)" ] || die "working tree is dirty — commit or stash first"

branch=$(git rev-parse --abbrev-ref HEAD)
[ "$branch" = "main" ] || die "on '$branch', not main"

git rev-parse -q --verify "refs/tags/$tag" >/dev/null && die "tag $tag already exists locally"
if git ls-remote --exit-code --tags origin "$tag" >/dev/null 2>&1; then
    die "tag $tag already exists on origin"
fi

[ "$version" != "$current" ] || die "already at $version"
# Sorts the two versions and complains if the new one is not the later. `sort -V` is
# GNU-only, which is fine: this repo builds Debian packages on Linux.
if [ "$(printf '%s\n%s\n' "$current" "$version" | sort -V | tail -1)" != "$version" ]; then
    die "$version sorts below the current $current"
fi

# --- write --------------------------------------------------------------------------

# Restore the manifests if anything below fails, so a failed run leaves nothing behind.
restore() { git checkout -- Cargo.toml Cargo.lock 2>/dev/null || true; }
trap restore ERR

step "bumping [workspace.package] version"
# Anchored to the section: a blind substitution would also rewrite a dependency pinned to
# the same string, and `[workspace.dependencies]` sits in this same file.
awk -v new="$version" '
    /^\[workspace\.package\]/ { section = 1; print; next }
    /^\[/                     { section = 0 }
    section && /^version[[:space:]]*=/ { sub(/"[^"]*"/, "\"" new "\"") }
    { print }
' Cargo.toml > Cargo.toml.new
mv Cargo.toml.new Cargo.toml

# Proves the edit landed rather than trusting the pattern — an awk that matched nothing
# exits 0 and leaves the file identical.
written=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "zuno") | .version')
[ "$written" = "$version" ] || die "bump did not take: cargo still reports $written"

step "refreshing Cargo.lock"
cargo update -w --quiet

if $run_tests; then
    step "running the suite"
    RUSTFLAGS="-D warnings" cargo test --workspace --quiet
fi

trap - ERR

# --- commit and tag -----------------------------------------------------------------

step "committing"
git add Cargo.toml Cargo.lock
git commit -q -m "release $version"

step "tagging $tag"
git tag -a "$tag" -m "Zuno $version"

if $push; then
    step "pushing"
    git push origin "$branch"
    git push origin "$tag"
    printf '\n\033[32mdone.\033[0m %s is building: %s/actions\n' \
        "$tag" "https://github.com/aryanjha256/zuno"
else
    cat <<EOF

$(printf '\033[32mready.\033[0m') Nothing has been pushed. To publish:

    git push origin $branch
    git push origin $tag

To undo instead:

    git tag -d $tag && git reset --hard HEAD~1
EOF
fi
