#!/usr/bin/env bash
# Tag and push a release from the current workspace.
#
# Usage:
#   ./scripts/tag-release.sh    # no arguments; version comes from Cargo.toml,
#                                # branch is always main
#
# Pre-conditions (script enforces):
#   - Working tree clean (no uncommitted or untracked changes).
#   - On the target branch (default: main).
#   - Local branch is fast-forward of origin/<branch> (no diverged history).
#   - Tag does not already exist locally or remotely.
#
# Side effects:
#   - git checkout <branch>
#   - git pull --ff-only
#   - git tag <version>
#   - git push origin <version>          (triggers .github/workflows/release.yml)
#
# Designed to be invoked from inside the running jyc as a /release custom
# command. See config.example.toml for the [[commands]] registration.
# The shell-variant limits (30 s timeout, 8 KiB output cap) apply when
# invoked via jyc; when run from a shell directly, those limits don't.

set -euo pipefail

# Resolve repo root from this script's own location (scripts/ sits directly
# under the repo root) so the script works regardless of the caller's cwd.
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &> /dev/null && pwd)"
REPO_ROOT="$(dirname -- "$SCRIPT_DIR")"
cd "$REPO_ROOT"

log() { printf '\033[1;34m[tag-release]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[tag-release]\033[0m %s\n' "$*" >&2; exit 1; }

# --- Version resolution -------------------------------------------------------

[[ $# -eq 0 ]] || die "this script takes no arguments; the version is read from Cargo.toml"

BRANCH="main"

VERSION="$(awk -F'"' '/^version =/{print $2; exit}' Cargo.toml)"
[[ -n "$VERSION" ]] || die "could not determine version from Cargo.toml"

# release.yml triggers on "v*" tags, so the pushed tag must be v-prefixed.
VERSION="${VERSION#v}"
if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?$ ]]; then
    die "version '$VERSION' is not a valid semver string"
fi
TAG="v$VERSION"

log "version: $VERSION (tag: $TAG)"
log "branch:  $BRANCH"
log "repo:    $REPO_ROOT"

# --- Pre-conditions -----------------------------------------------------------

if [[ -n "$(git status --porcelain)" ]]; then
    die "working tree is dirty; commit or stash first"
fi

if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
    die "tag $TAG already exists locally; delete it first if you really mean to retag"
fi

if git ls-remote --tags origin "refs/tags/$TAG" 2>/dev/null | grep -q "$TAG"; then
    die "tag $TAG already exists on origin"
fi

if ! git show-ref --verify --quiet "refs/heads/$BRANCH"; then
    die "local branch '$BRANCH' does not exist"
fi

# --- Sync to remote -----------------------------------------------------------

CURRENT_BRANCH="$(git symbolic-ref --short HEAD 2>/dev/null || true)"
if [[ "$CURRENT_BRANCH" != "$BRANCH" ]]; then
    log "checking out $BRANCH"
    git checkout "$BRANCH"
fi

log "fetching origin/$BRANCH"
git fetch origin "$BRANCH"

LOCAL_HEAD="$(git rev-parse "$BRANCH")"
REMOTE_HEAD="$(git rev-parse "origin/$BRANCH")"
if [[ "$LOCAL_HEAD" != "$REMOTE_HEAD" ]]; then
    if ! git merge-base --is-ancestor "$REMOTE_HEAD" "$LOCAL_HEAD"; then
        die "local $BRANCH ($LOCAL_HEAD) has diverged from origin/$BRANCH ($REMOTE_HEAD); reconcile first"
    fi
    log "fast-forwarding $BRANCH to origin/$BRANCH"
    git merge --ff-only "origin/$BRANCH"
fi

# --- Tag and push -------------------------------------------------------------

log "creating tag $TAG"
git tag -a "$TAG" -m "Release $TAG"

log "pushing tag $TAG to origin"
git push origin "refs/tags/$TAG"

log "done — release workflow should pick up tag $TAG shortly"
