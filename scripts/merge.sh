#!/usr/bin/bash

BRANCH=$(git branch --show-current)
gh pr merge --squash --delete-branch
git checkout main
git pull
git branch -D "$BRANCH"
