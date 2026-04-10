---
name: dev-workflow
description: Trunk-based development workflow - branching, fix/feature flow, release process, commit conventions. Use when planning development work, creating branches, merging, or preparing releases.
---

## Development Flow: Trunk-Based with Short-Lived Branches

main is always deployable. No release branches. Tag main directly for releases.

```
main ──●──●──●──●──●──●──●──●──● (always deployable)
        \   /   \     /
    fix/x  feat/a  feat/b
   (hours)  (1-2 days)
```

## For Fixes

1. `git checkout -b fix/<description>` from main
2. Fix the issue
3. Run tests: `cargo test`
4. Build clean: `cargo build --release` (zero warnings)
5. Commit with `fix:` prefix
6. Merge to main: `git checkout main && git merge fix/<name> --no-ff`
7. Push: `git push origin main`
8. Deploy if needed

## For Features

1. `git checkout -b feat/<description>` from main
2. Develop in small increments, commit frequently
3. If main has new commits, rebase: `git rebase main`
4. Run tests and build clean before merge
5. Merge to main: `git checkout main && git merge feat/<name> --no-ff`
6. Push

## For Releases

1. Bump version in `Cargo.toml`
2. Update `CHANGELOG.md` with all changes since last release
3. Update `DESIGN.md` if architecture changed
4. Commit: `chore: prepare release vX.Y.Z`
5. Tag: `git tag -a vX.Y.Z -m "vX.Y.Z: summary"`
6. Push: `git push origin main --tags`

## Critical Rules

- Fixes ALWAYS go to main first — never fix on a feature branch
- Feature branches rebase on main regularly — don't let them diverge
- Keep feature branches short (1-2 days) — break large features into smaller merges
- Every commit on main must build with zero warnings
- Run `cargo test` before every merge to main
- Never force-push to main

## Commit Message Convention

- `fix:` — bug fix
- `feat:` — new feature
- `chore:` — maintenance, cleanup, version bump
- `docs:` — documentation only
- `refactor:` — code restructuring without behavior change

## Version Numbering

- Format: `MAJOR.MINOR.PATCH` (e.g., 0.1.5)
- Bump PATCH for fixes and small features
- Bump MINOR for significant features or breaking changes
- Bump MAJOR for fundamental architecture changes
