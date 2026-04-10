---
name: bump-jyc-version
description: |
  Bump JYC version by generating changelog from git log and updating version files.
  Use when: bumping version, releasing new version, updating changelog, preparing release.
triggers:
  - bump version
  - bump jyc version
  - bump jyc
  - release version
  - 发布版本
  - 更新版本
  - update changelog
  - prepare release
---

# JYC Version Bump Skill

This skill automates the version bump process for JYC, including changelog generation from git log and multi-file version updates.

## Prerequisites

Before running this skill, ensure:

1. **Working directory is clean** (no uncommitted changes)
   ```bash
   git status
   ```

2. **You are on the correct branch** (main, or a feature/release branch)
   ```bash
   git branch --show-current
   ```

3. **The jyc repository exists at `./jyc/`**

## Workflow

### Phase 1: Detect Branch State

**Step 1.1: Get current branch and last tag**

```bash
cd jyc
CURRENT_BRANCH=$(git rev-parse --abbrev-ref HEAD)
LAST_TAG=$(git describe --tags --abbrev=0 2>/dev/null || echo "v0.0.0")
echo "Current branch: $CURRENT_BRANCH"
echo "Last tag: $LAST_TAG"
```

**Step 1.2: Check working directory status**

```bash
if ! git diff-index --quiet HEAD --; then
    echo "ERROR: Working directory has uncommitted changes"
    git status --short
    exit 1
fi
```

**Step 1.3: Handle different branch types**

**Case A: Current branch is `main`**

```bash
if [ "$CURRENT_BRANCH" = "main" ]; then
    # Check for unmerged feature branches
    UNMERGED=$(git branch -a --merged origin/main | grep -E "feat/|feature/" || true)
    if [ -n "$UNMERGED" ]; then
        echo "WARNING: The following feature branches are merged to origin/main:"
        echo "$UNMERGED"
        echo ""
        read -p "Continue with main branch bump? (yes/no): " CONFIRM
        if [ "$CONFIRM" != "yes" ]; then
            exit 0
        fi
    fi
    
    TARGET_BRANCH="main"
    RELEASE_MODE="production"
fi
```

**Case B: Current branch is `feat/*` or `feature/*`**

```bash
elif [[ "$CURRENT_BRANCH" =~ ^(feat|feature)/ ]]; then
    echo "Current branch is a feature branch: $CURRENT_BRANCH"
    echo ""
    echo "Select an option:"
    echo "1) Merge to main first, then bump version (RECOMMENDED for production release)"
    echo "2) Bump version on current feature branch (for testing/pre-release)"
    echo "3) Create a new release branch from current state"
    echo "4) Cancel"
    echo ""
    read -p "Enter choice [1-4]: " CHOICE
    
    case $CHOICE in
        1)
            # Merge to main
            git checkout main
            git pull origin main
            git merge --no-ff "$CURRENT_BRANCH" -m "Merge $CURRENT_BRANCH into main"
            TARGET_BRANCH="main"
            RELEASE_MODE="production"
            ;;
        2)
            TARGET_BRANCH="$CURRENT_BRANCH"
            RELEASE_MODE="prerelease"
            SUGGESTED_VERSION_SUFFIX="-beta.1+$(echo $CURRENT_BRANCH | tr '/' '-')"
            ;;
        3)
            read -p "Enter release branch name (e.g., release/v0.2.0): " RELEASE_BRANCH
            git checkout -b "$RELEASE_BRANCH"
            TARGET_BRANCH="$RELEASE_BRANCH"
            RELEASE_MODE="release-branch"
            ;;
        4)
            echo "Cancelled"
            exit 0
            ;;
    esac
fi
```

**Case C: Current branch is `release/*`**

```bash
elif [[ "$CURRENT_BRANCH" =~ ^release/ ]]; then
    echo "Continuing on release branch: $CURRENT_BRANCH"
    TARGET_BRANCH="$CURRENT_BRANCH"
    RELEASE_MODE="release-branch"
fi
```

### Phase 2: Generate Changelog from Git Log

**Step 2.1: Get commits since last tag**

```bash
# Get all commits since last tag
COMMITS=$(git log ${LAST_TAG}..HEAD --pretty=format:"%H|%s|%b" --reverse)

if [ -z "$COMMITS" ]; then
    echo "WARNING: No commits found since $LAST_TAG"
    echo "Did you forget to tag the previous release?"
    read -p "Continue anyway? (yes/no): " CONFIRM
    if [ "$CONFIRM" != "yes" ]; then
        exit 0
    fi
fi
```

**Step 2.2: Parse and categorize commits**

```bash
# Initialize changelog sections
ADDED=""
FIXED=""
CHANGED=""
REMOVED=""
PERFORMANCE=""
SECURITY=""
DEPRECATED=""
OTHER=""

# Parse each commit
while IFS= read -r line; do
    HASH=$(echo "$line" | cut -d'|' -f1)
    SUBJECT=$(echo "$line" | cut -d'|' -f2)
    BODY=$(echo "$line" | cut -d'|' -f3-)
    
    # Parse conventional commit type
    if [[ "$SUBJECT" =~ ^(feat|fix|chore|docs|refactor|perf|test|style|build|ci|revert|deprecate|remove|security)(\(.+\))?(!)?:[[:space:]]*(.+)$ ]]; then
        TYPE="${BASH_REMATCH[1]}"
        SCOPE="${BASH_REMATCH[2]}"
        BREAKING="${BASH_REMATCH[3]}"
        MESSAGE="${BASH_REMATCH[4]}"
        
        # Format entry
        ENTRY="- **${MESSAGE}**"
        if [ -n "$SCOPE" ]; then
            ENTRY="${ENTRY} — ${SCOPE}"
        fi
        if [ -n "$BODY" ]; then
            # Add body as sub-bullets
            ENTRY="${ENTRY}
$(echo "$BODY" | sed 's/^/  - /')"
        fi
        if [ -n "$BREAKING" ]; then
            ENTRY="${ENTRY} ⚠️ **BREAKING CHANGE**"
        fi
        
        # Categorize
        case "$TYPE" in
            feat)
                ADDED="${ADDED}${ENTRY}\n"
                ;;
            fix|security)
                FIXED="${FIXED}${ENTRY}\n"
                ;;
            chore|docs|refactor|style|build|ci|test)
                CHANGED="${CHANGED}${ENTRY}\n"
                ;;
            perf)
                PERFORMANCE="${PERFORMANCE}${ENTRY}\n"
                ;;
            deprecate)
                DEPRECATED="${DEPRECATED}${ENTRY}\n"
                ;;
            remove)
                REMOVED="${REMOVED}${ENTRY}\n"
                ;;
        esac
    else
        # Non-conventional commit, put in OTHER
        OTHER="${OTHER}- ${SUBJECT}\n"
    fi
done <<< "$COMMITS"
```

**Step 2.3: Suggest version bump based on changes**

```bash
# Parse current version
CURRENT_VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*= *"\(.*\)".*/\1/')
CURRENT_MAJOR=$(echo $CURRENT_VERSION | cut -d. -f1)
CURRENT_MINOR=$(echo $CURRENT_VERSION | cut -d. -f2)
CURRENT_PATCH=$(echo $CURRENT_VERSION | cut -d. -f3 | cut -d- -f1)

# Determine bump type
BUMP_TYPE="patch"  # default

if [ -n "$DEPRECATED" ] || [ -n "$REMOVED" ]; then
    BUMP_TYPE="major"
elif [ -n "$ADDED" ]; then
    BUMP_TYPE="minor"
elif [ -n "$FIXED" ] || [ -n "$CHANGED" ]; then
    BUMP_TYPE="patch"
fi

# Calculate new version
case $BUMP_TYPE in
    major)
        NEW_MAJOR=$((CURRENT_MAJOR + 1))
        NEW_VERSION="${NEW_MAJOR}.0.0"
        ;;
    minor)
        NEW_MINOR=$((CURRENT_MINOR + 1))
        NEW_VERSION="${CURRENT_MAJOR}.${NEW_MINOR}.0"
        ;;
    patch)
        NEW_PATCH=$((CURRENT_PATCH + 1))
        NEW_VERSION="${CURRENT_MAJOR}.${CURRENT_MINOR}.${NEW_PATCH}"
        ;;
esac

# Handle prerelease for feature branches
if [ "$RELEASE_MODE" = "prerelease" ]; then
    NEW_VERSION="${NEW_VERSION}-beta.1+$(echo $CURRENT_BRANCH | tr '/' '-')"
fi

echo "📊 Version Analysis:"
echo "   Current: $CURRENT_VERSION"
echo "   Suggested: $NEW_VERSION (bump: $BUMP_TYPE)"
echo ""
echo "   Breaking changes: $([ -n \"$REMOVED\" ] && echo \"Yes\" || echo \"No\")"
echo "   New features: $([ -n \"$ADDED\" ] && echo \"Yes\" || echo \"No\")"
echo "   Bug fixes: $([ -n \"$FIXED\" ] && echo \"Yes\" || echo \"No\")"
```

### Phase 3: User Review and Editing

**Step 3.1: Present generated changelog for review**

```bash
# Build full changelog entry
TODAY=$(date +%Y-%m-%d)

cat > /tmp/changelog_draft.md << EOF
## [${NEW_VERSION}] - ${TODAY}

EOF

[ -n "$ADDED" ] && cat >> /tmp/changelog_draft.md << EOF
### Added

${ADDED}

EOF

[ -n "$FIXED" ] && cat >> /tmp/changelog_draft.md << EOF
### Fixed

${FIXED}

EOF

[ -n "$CHANGED" ] && cat >> /tmp/changelog_draft.md << EOF
### Changed

${CHANGED}

EOF

[ -n "$PERFORMANCE" ] && cat >> /tmp/changelog_draft.md << EOF
### Performance

${PERFORMANCE}

EOF

[ -n "$REMOVED" ] && cat >> /tmp/changelog_draft.md << EOF
### Removed

${REMOVED}

EOF

[ -n "$DEPRECATED" ] && cat >> /tmp/changelog_draft.md << EOF
### Deprecated

${DEPRECATED}

EOF

[ -n "$SECURITY" ] && cat >> /tmp/changelog_draft.md << EOF
### Security

${SECURITY}

EOF

cat >> /tmp/changelog_draft.md << EOF
---
Generated from commits: ${LAST_TAG}..HEAD
Branch: ${CURRENT_BRANCH}
EOF

# Display to user
echo "==========================================="
echo "  📋 GENERATED CHANGELOG DRAFT"
echo "==========================================="
cat /tmp/changelog_draft.md
echo ""
echo "==========================================="
echo ""
echo "⚠️  This is a DRAFT. You can:"
echo "   [e] Edit the changelog manually before proceeding"
echo "   [c] Continue with this draft as-is"
echo "   [r] Regenerate with different options"
echo "   [q] Quit"
echo ""
read -p "Your choice [e/c/r/q]: " REVIEW_CHOICE

case $REVIEW_CHOICE in
    e)
        echo "Opening editor..."
        ${EDITOR:-nano} /tmp/changelog_draft.md
        ;;
    c)
        echo "Continuing with current draft..."
        ;;
    r)
        echo "Regenerating..."
        # Loop back to regeneration
        ;;
    q)
        echo "Cancelled."
        exit 0
        ;;
esac
```

### Phase 4: Apply Changes

**Step 4.1: Update Cargo.toml**

```bash
# Update version in Cargo.toml
sed -i "s/^version = \"[^\"]*\"/version = \"${NEW_VERSION}\"/" Cargo.toml

# Verify the change
echo "Updated Cargo.toml:"
grep "^version" Cargo.toml | head -1
```

**Step 4.2: Update CHANGELOG.md**

```bash
# Insert new changelog entry after the header (line 4)
CHANGELOG_FILE="CHANGELOG.md"

# Create temp file with new content
head -4 "$CHANGELOG_FILE" > /tmp/changelog_new.md
echo "" >> /tmp/changelog_new.md
cat /tmp/changelog_draft.md >> /tmp/changelog_new.md
echo "" >> /tmp/changelog_new.md
tail -n +5 "$CHANGELOG_FILE" >> /tmp/changelog_new.md

# Replace original file
mv /tmp/changelog_new.md "$CHANGELOG_FILE"

echo "✓ Updated CHANGELOG.md"
```

**Step 4.3: Git operations**

```bash
# Stage all changes
git add Cargo.toml CHANGELOG.md

# Create commit
git commit -m "chore: prepare release v${NEW_VERSION}

$(cat /tmp/changelog_draft.md | grep -E '^-|^###' | head -20)"

# Create tag
git tag -a "v${NEW_VERSION}" -m "Release v${NEW_VERSION}"

echo "✓ Created commit and tag v${NEW_VERSION}"
echo ""
echo "Next steps:"
echo "  git push origin main"
echo "  git push origin v${NEW_VERSION}"
```

### Phase 5: Summary

**Display final summary to user:**

```bash
cat << EOF

========================================
  ✅ VERSION BUMP COMPLETE
========================================

Version Change:
  ${CURRENT_VERSION} → ${NEW_VERSION}

Files Modified:
  ✓ Cargo.toml
  ✓ CHANGELOG.md

Git Operations:
  ✓ Created commit: chore: prepare release v${NEW_VERSION}
  ✓ Created tag: v${NEW_VERSION}

Branch State:
  Current: $(git branch --show-current)
  Tag: v${NEW_VERSION}

Next Steps:
  1. Review the commit: git show HEAD
  2. Review the tag: git show v${NEW_VERSION}
  3. Push to remote:
     git push origin $(git branch --show-current)
     git push origin v${NEW_VERSION}

========================================
EOF
```

## Safety Checklist

This skill MUST follow these safety rules:

1. **Two-Phase Confirmation**
   - Phase 1: Display all planned changes and ask for user confirmation
   - Phase 2: Wait for explicit "yes" before making any changes

2. **Branch Protection**
   - Never directly modify `main` without user confirmation
   - Always check for uncommitted changes before proceeding
   - For feature branches, always offer merge-to-main option first

3. **Version Validation**
   - Validate new version follows semver format
   - Ensure new version is greater than current version
   - Suggest appropriate bump type (major/minor/patch) based on changes

4. **Reversibility**
   - Create commits that can be easily reverted
   - Never force-push to remote branches
   - Always allow user to review before pushing
