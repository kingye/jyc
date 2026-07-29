---
name: jyc-deploy-bare
description: Build and deploy jyc on bare metal using deploy.sh with nohup. Use when instructed to build, deploy, or build-and-deploy jyc.
---

IMPORTANT: The JYC source repository is at ./jyc/ relative to your working directory.
All commands below MUST be run from the jyc/ directory (use `cd jyc` first).

## Critical Safety: Build/Deploy Operations
CRITICAL: AI MUST use TWO-PHASE CONFIRMATION for ALL build/deploy operations:

Phase 1 - Present Plan:
- List ALL commands that will be executed
- Explain what each command does
- State potential risks (e.g., "build will take 2-3 min", "deploy will restart your session")

Phase 2 - Wait for Approval:
- Ask user to type "yes" or "proceed" to continue
- If user responds with anything else, ABORT operation
- Only execute commands after receiving explicit "yes" or "proceed"

CRITICAL: Never execute build/deploy commands without explicit user approval.

## Build (when instructed)
MUST follow TWO-PHASE CONFIRMATION above before executing.
MUST execute EXACTLY these commands in order:

1. cd jyc
2. cargo test
3. cargo build --release
4. ./target/release/jyc --version
5. Report: version, binary size, test results

CRITICAL: Do NOT improvise or use alternative build commands.

## Deploy (when instructed)
MUST follow TWO-PHASE CONFIRMATION above before executing.

WARNING: Deploy causes JYC to restart. Your session will be interrupted.
WARNING: Process will be killed and restarted automatically by deploy.sh.

MUST execute EXACTLY these steps in order:

Step 1: Verify jyc/target/release/jyc exists
Step 2: Execute the deploy command FIRST (it returns immediately, deployment runs in background):
   ```
   systemd-run --user --unit=jyc-deploy-$(date +%s) --working-directory=$(pwd)/jyc bash ./deploy.sh
   ```
   This runs deploy.sh as a separate systemd transient unit, outside the jyc service cgroup,
   so it survives the jyc service stop/restart cycle.
   The timestamp in the unit name prevents conflicts with previous deploy units.
Step 3: IMMEDIATELY after the deploy command, send reply using reply_message tool confirming deployment started.
Step 4: STOP immediately. Do NOT run any more commands.

CRITICAL Warnings:
- Do NOT send reply AFTER the deploy command — process will be dead and reply will fail
- Do NOT run the deploy command multiple times — one restart is sufficient
- Do NOT improvise or use alternative commands
- Do NOT run deploy.sh directly with `bash ./deploy.sh` — it MUST use systemd-run to survive the service restart
- Do NOT use "systemctl --user stop jyc" or "systemctl --user start jyc" or "systemctl --user restart jyc" directly — always use deploy.sh

## Build and Deploy (when instructed)
MUST follow TWO-PHASE CONFIRMATION above before executing.
MUST execute EXACTLY these steps in order:

1. Run Build steps above (cd jyc, test + compile)
2. Run Deploy steps above (reply first, then deploy.sh, then STOP)

## Service Management

You can query the jyc service with these observation commands:

```bash
# Check service status
systemctl --user status jyc

# View service logs
journalctl --user -u jyc -f
```

CRITICAL: These are observation-only commands. For deployment, ALWAYS use deploy.sh as specified above.
