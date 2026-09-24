#!/usr/bin/env bash
# Squash-merge the current branch's PR and get back to a clean main.
set -euo pipefail

BRANCH=$(git branch --show-current)
# Without -e a failed merge (red CI, conflict, no PR) would still fall through
# to the branch delete below, reporting a merge that never happened.
gh pr merge --squash --delete-branch
git checkout main
git pull
git branch -D "$BRANCH" || true
