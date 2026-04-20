#!/bin/bash
set -e

# Deploy templates by composing AGENTS.md + referenced skills from .opencode/skills/
#
# Usage:
#   ./deploy-templates.sh <target_dir> [template_name] [--model <model-id>]
#
# Examples:
#   ./deploy-templates.sh /path/to/templates
#   ./deploy-templates.sh /path/to/templates github-planner
#   ./deploy-templates.sh /path/to/templates --model tencent/glm-5.1
#   ./deploy-templates.sh /path/to/templates github-planner --model ark/minimax-m2.5

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TEMPLATES_DIR="${SCRIPT_DIR}/templates"
SKILLS_DIR="${SCRIPT_DIR}/.opencode/skills"

TARGET_DIR=""
TEMPLATE_NAME=""
MODEL_OVERRIDE=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --model)
            MODEL_OVERRIDE="$2"
            shift 2
            ;;
        -*)
            echo "Unknown option: $1" >&2
            exit 1
            ;;
        *)
            if [ -z "$TARGET_DIR" ]; then
                TARGET_DIR="$1"
            elif [ -z "$TEMPLATE_NAME" ]; then
                TEMPLATE_NAME="$1"
            else
                echo "Unexpected argument: $1" >&2
                exit 1
            fi
            shift
            ;;
    esac
done

if [ -z "$TARGET_DIR" ]; then
    echo "Usage: $0 <target_dir> [template_name] [--model <model-id>]"
    echo ""
    echo "Examples:"
    echo "  $0 /path/to/templates"
    echo "  $0 /path/to/templates github-planner"
    echo "  $0 /path/to/templates --model tencent/glm-5.1"
    echo "  $0 /path/to/templates github-planner --model ark/minimax-m2.5"
    exit 1
fi

if [ -n "$TEMPLATE_NAME" ]; then
    template_path="${TEMPLATES_DIR}/${TEMPLATE_NAME}"
    if [ ! -d "$template_path" ]; then
        echo "Error: template '${TEMPLATE_NAME}' not found at ${template_path}" >&2
        exit 1
    fi
fi

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
if [ -n "$TEMPLATE_NAME" ]; then
    echo "Template filter: ${TEMPLATE_NAME}"
fi
if [ -n "$MODEL_OVERRIDE" ]; then
    echo "Model override:  ${MODEL_OVERRIDE}"
fi
echo ""

for template_dir in "${TEMPLATES_DIR}"/*/; do
    template_name=$(basename "$template_dir")

    if [ -n "$TEMPLATE_NAME" ] && [ "$template_name" != "$TEMPLATE_NAME" ]; then
        continue
    fi

    target="${TARGET_DIR}/${template_name}"

    echo "--- ${template_name} ---"

    mkdir -p "${target}"

    if [ -f "${template_dir}/AGENTS.md" ]; then
        cp "${template_dir}/AGENTS.md" "${target}/AGENTS.md"
        echo "  AGENTS.md copied"
    fi

    if [ -d "${template_dir}/.jyc" ]; then
        rm -rf "${target}/.jyc"
        cp -r "${template_dir}/.jyc" "${target}/.jyc"
        echo "  .jyc copied"
    fi

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

    if [ -n "$MODEL_OVERRIDE" ]; then
        mkdir -p "${target}/.jyc"
        echo -n "${MODEL_OVERRIDE}" > "${target}/.jyc/model-override"
        echo "  model-override: ${MODEL_OVERRIDE}"
    fi
done

echo ""
echo "=== Deployment complete ==="
echo "Templates deployed to: ${TARGET_DIR}"
ls -d "${TARGET_DIR}"/*/