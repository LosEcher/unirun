#!/usr/bin/env bash
# Gated release driver for unirun.
#
# The release contract is prose in README.md ("Releasing") plus one assertion in
# CI (the tag must equal the crate version). Prose does not fail closed, so this
# script turns the whole sequence into gates: every step runs and must pass
# before anything leaves this machine.
#
#   scripts/release.sh              verify only — changes nothing (default)
#   scripts/release.sh --push       verify, then push main and the version tag
#   scripts/release.sh --publish    ... and `cargo publish` (IRREVERSIBLE)
#
# Nothing is pushed or published unless every gate passes. The published crate
# cannot be deleted, only yanked, which is why `--publish` is a separate,
# explicit flag and why the default is a dry run.
#
# Gates, in order:
#   1. clean working tree, on main
#   2. version agreement: Cargo.toml = planned tag (the assertion CI makes)
#   3. CHANGELOG.md has an entry for this version
#   4. cargo fmt --check
#   5. cargo clippy --all-targets -- -D warnings (default and winrm)
#   6. cargo test (default and winrm)
#   7. cargo publish --dry-run (packages the crate, resolves the payload)
#   8. payload hygiene: no internal file may reach the .crate
set -euo pipefail

cd "$(dirname "$0")/.."
repo_root="$PWD"

push=0
publish=0
for arg in "$@"; do
    case "$arg" in
        --push) push=1 ;;
        --publish) push=1; publish=1 ;;
        -h | --help)
            sed -n '2,20p' "$0"
            exit 0
            ;;
        *)
            echo "unknown argument: $arg (try --help)" >&2
            exit 2
            ;;
    esac
done

step() { printf '\n=== %s\n' "$1"; }
fail() {
    printf '\nrelease refused: %s\n' "$1" >&2
    exit 1
}

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
tag="v${version}"

step "1/8 working tree and branch"
[ -z "$(git status --porcelain)" ] || fail "working tree is dirty; a release is cut from a clean tree"
branch="$(git rev-parse --abbrev-ref HEAD)"
[ "$branch" = "main" ] || fail "on branch '$branch', expected main"
echo "clean tree on main at $(git rev-parse --short HEAD)"

step "2/8 version agreement (Cargo.toml = tag)"
echo "Cargo.toml version: $version   planned tag: $tag"
lock_version="$(awk '/^name = "unirun"$/{found=1; next} found && /^version = /{gsub(/[^0-9.]/, "", $0); print; exit}' Cargo.lock)"
[ "$lock_version" = "$version" ] ||
    fail "Cargo.lock has unirun at '$lock_version', expected '$version' (run cargo build once)"
if git rev-parse -q --verify "refs/tags/${tag}" >/dev/null; then
    fail "tag ${tag} already exists; bump the version instead of re-tagging"
fi

step "3/8 CHANGELOG entry"
grep -q "^## ${version}" CHANGELOG.md ||
    fail "CHANGELOG.md has no '## ${version}' section; a version without release notes is a version nobody can read"
echo "CHANGELOG.md documents ${version}"

step "4/8 cargo fmt --check"
cargo fmt --check

step "5/8 clippy (default + winrm)"
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features winrm -- -D warnings

step "6/8 tests (default + winrm)"
cargo test
cargo test --features winrm

step "7/8 cargo publish --dry-run"
cargo publish --dry-run

step "8/8 payload hygiene"
package_list="$(cargo package --list 2>/dev/null)"
leaked="$(printf '%s\n' "$package_list" | grep -E '^(docs/|AGENTS\.md|\.github/)' || true)"
[ -z "$leaked" ] || fail "internal files would ship in the .crate:
$leaked"
echo "payload: $(printf '%s\n' "$package_list" | wc -l | tr -d ' ') files, no internal leak"

printf '\nAll gates passed for %s (crate %s).\n' "$tag" "$version"

if [ "$push" = 0 ]; then
    cat <<EOF

Dry run: nothing was pushed. To complete the release:

  scripts/release.sh --push        # push main + tag (CI builds the 5 assets)
  scripts/release.sh --publish     # ...and cargo publish (irreversible)

EOF
    exit 0
fi

step "pushing main"
git push origin main

step "tagging ${tag}"
# Annotated, and created only after main is on the remote, so a failed push
# cannot leave a local tag pointing at a commit nobody else can see.
git tag -a "${tag}" -m "unirun ${version}"

step "pushing ${tag}"
git push origin "${tag}"

printf '\nPushed %s. CI now runs test x3 + msrv, then builds 5 platform assets and\ncreates the GitHub Release.\n' "$tag"

if [ "$publish" = 0 ]; then
    cat <<EOF

The crate is NOT published yet. Once CI is green and the release assets exist:

  cd ${repo_root} && cargo publish

EOF
    exit 0
fi

step "cargo publish (irreversible)"
cargo publish
printf '\n%s published. Verify: https://crates.io/crates/unirun\n' "$version"
