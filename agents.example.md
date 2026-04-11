# JYC Self-Bootstrapping Thread

Copy this file to your thread workspace directory as `AGENTS.md` and
uncomment the environment section that matches your deployment.

## Setup

1. Clone the jyc repository into your thread workspace (if not already present):
   ```bash
   git clone https://github.com/kingye/jyc.git jyc
   ```

2. Symlink the deploy skills so OpenCode can discover them:
   ```bash
   mkdir -p .opencode
   ln -s ../jyc/.opencode/skills .opencode/skills
   ```

3. Copy this file as AGENTS.md:
   ```bash
   cp jyc/agents.example.md AGENTS.md
   ```

4. Edit `AGENTS.md` and uncomment the environment section below that
   matches your deployment.

## Instructions

You are developing the JYC project itself (self-bootstrapping).

### Repository
The JYC git repository is at: ./jyc/

### Environment
<!-- Uncomment ONE of the following sections: -->

<!-- Bare metal deployment:
This is a bare metal deployment using systemd for process supervision.
Use the `jyc-deploy-bare` skill for build and deploy operations.
-->

<!-- Docker deployment:
This is a Docker container deployment using s6 for process supervision.
Use the `jyc-deploy-docker` skill for build and deploy operations.
-->
