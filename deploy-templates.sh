#!/bin/bash
set -e

# Deploy templates by composing AGENTS.md + referenced skills from .opencode/skills/
#
# Usage:
#   ./deploy-templates.sh <target_dir>
#
# Example:
#   ./deploy-templates.sh /home/jiny/projects/jyc-data/templates
#
# This script:
# 1. Reads templates/ directory for AGENTS.md files
# 2. Reads the skill mapping below
# 3. Copies AGENTS.md + skills into target/<template_name>/

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TEMPLATES_DIR="${SCRIPT_DIR}/templates"
SKILLS_DIR="${SCRIPT_DIR}/.opencode/skills"

if [ -z "$1" ]; then
    echo "Usage: $0 <target_dir>"
    echo "Example: $0 /home/jiny/projects/jyc-data/templates"
    exit 1
fi

TARGET_DIR="$1"

# Return the skills for a given template name
get_skills() {
    case "$1" in
        invoice-processing)   echo "invoice-processing" ;;
        jyc-dev)              echo "plan-solution dev-workflow incremental-dev jyc-deploy-bare" ;;
        jyc-review)           echo "pr-review" ;;
        github-planner)       echo "dev-workflow" ;;
        github-developer)     echo "incremental-dev dev-workflow" ;;
        github-reviewer)      echo "pr-review" ;;
        *)                    echo "" ;;
    esac
}

echo "=== Template Deployment ==="
echo "Source templates: ${TEMPLATES_DIR}"
echo "Source skills:    ${SKILLS_DIR}"
echo "Target:           ${TARGET_DIR}"
echo ""

# Deploy each template
for template_dir in "${TEMPLATES_DIR}"/*/; do
    template_name=$(basename "$template_dir")
    target="${TARGET_DIR}/${template_name}"

    echo "--- ${template_name} ---"

    # Create target directory
    mkdir -p "${target}"

    # Copy AGENTS.md
    if [ -f "${template_dir}/AGENTS.md" ]; then
        cp "${template_dir}/AGENTS.md" "${target}/AGENTS.md"
        echo "  AGENTS.md copied"
    fi

    # Copy .jyc directory (model-override, etc.)
    if [ -d "${template_dir}/.jyc" ]; then
        rm -rf "${target}/.jyc"
        cp -r "${template_dir}/.jyc" "${target}/.jyc"
        echo "  .jyc copied"
    fi

    # Copy skills
    skills=$(get_skills "$template_name")
    if [ -n "$skills" ]; then
        mkdir -p "${target}/.opencode/skills"
        for skill in $skills; do
            skill_src="${SKILLS_DIR}/${skill}"
            if [ -d "$skill_src" ]; then
                rm -rf "${target}/.opencode/skills/${skill}"
                cp -r "$skill_src" "${target}/.opencode/skills/${skill}"
                echo "  skill: ${skill}"
            else
                echo "  WARNING: skill '${skill}' not found at ${skill_src}"
            fi
        done
    else
        echo "  (no skills)"
    fi
done

echo ""
echo "=== Deployment complete ==="
echo "Templates deployed to: ${TARGET_DIR}"
ls -d "${TARGET_DIR}"/*/
