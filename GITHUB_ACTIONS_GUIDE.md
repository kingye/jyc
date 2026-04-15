# GitHub Actions Guide for JYC

This guide provides comprehensive instructions for setting up and using GitHub Actions with the JYC project (Rust-based AI agent framework).

## Table of Contents
1. [Introduction](#introduction)
2. [Workflow Structure](#workflow-structure)
3. [Core Workflows](#core-workflows)
4. [Configuration](#configuration)
5. [Best Practices](#best-practices)
6. [Troubleshooting](#troubleshooting)

## Introduction

GitHub Actions is a CI/CD platform that automates your build, test, and deployment pipeline. For Rust projects like JYC, it provides:

- **Automated Testing**: Run tests on every push and pull request
- **Continuous Integration**: Build and validate code changes
- **Release Automation**: Automate version tagging and releases
- **Docker Publishing**: Build and publish container images
- **Security Scanning**: Detect vulnerabilities in dependencies and code

## Workflow Structure

A GitHub Actions workflow consists of:

```yaml
name: Workflow Name
on: [push, pull_request]  # Triggers
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
    - uses: actions/checkout@v4
    - run: echo "Hello, world!"
```

Key components:
- **Triggers**: Events that start the workflow (push, PR, schedule, etc.)
- **Jobs**: Independent units of work that run in parallel
- **Steps**: Sequential tasks within a job
- **Actions**: Reusable units of code

## Core Workflows

### 1. CI Pipeline (`ci.yml`)

The CI pipeline validates code quality on every push and pull request:

**Features:**
- Build verification on multiple platforms
- Unit and integration tests
- Code formatting with `cargo fmt`
- Linting with `cargo clippy`
- Rust-specific caching for faster builds

**Triggers:**
- `push` to main branch
- `pull_request` to main branch

### 2. Release Automation (`release.yml`)

Automates the release process when version tags are pushed:

**Features:**
- Automatic changelog generation from commits
- GitHub Release creation
- Release artifact attachment
- Docker image tagging

**Triggers:**
- `push` with tags matching `v*` (e.g., `v0.1.0`)

### 3. Docker Build (`docker.yml`)

Builds and publishes Docker images to GitHub Container Registry:

**Features:**
- Multi-architecture builds (linux/amd64, linux/arm64)
- Version tagging (`v0.1.0`, `latest`)
- Automated push to GHCR
- Image scanning for vulnerabilities

**Triggers:**
- `push` to main branch
- `push` with version tags

### 4. Security Scanning (`security.yml`)

Performs security analysis on code and dependencies:

**Features:**
- Dependency vulnerability scanning with `cargo audit`
- Code scanning with GitHub CodeQL
- Secret detection
- License compliance checking

**Triggers:**
- `push` to main branch
- `pull_request` to main branch
- Weekly schedule

## Configuration

### Environment Variables

Set these in your repository settings (Settings → Secrets and variables → Actions):

```bash
# Required for Docker publishing
DOCKER_USERNAME=${{ github.actor }}
DOCKER_PASSWORD=${{ secrets.GITHUB_TOKEN }}

# Optional: External service credentials
SMTP_PASSWORD=${{ secrets.SMTP_PASSWORD }}
FEISHU_APP_SECRET=${{ secrets.FEISHU_APP_SECRET }}
```

### Permissions

Workflows require specific permissions to interact with GitHub:

```yaml
permissions:
  contents: write  # For creating releases
  packages: write  # For publishing Docker images
  security-events: write  # For security scanning
  pull-requests: write  # For PR comments
```

### Caching

Rust-specific caching dramatically improves build times:

```yaml
- uses: actions/cache@v3
  with:
    path: |
      ~/.cargo/bin/
      ~/.cargo/registry/index/
      ~/.cargo/registry/cache/
      ~/.cargo/git/db/
      target/
    key: ${{ runner.os }}-cargo-${{ hashFiles('**/Cargo.lock') }}
```

## Best Practices

### 1. Efficient Caching
- Cache Rust toolchain and dependencies
- Use hash-based cache keys for invalidation
- Separate cache for different platforms

### 2. Matrix Testing
```yaml
strategy:
  matrix:
    os: [ubuntu-latest, macos-latest]
    rust: [stable, nightly]
```

### 3. Error Handling
- Use `continue-on-error: true` for non-critical steps
- Implement proper failure notifications
- Include debugging information in error messages

### 4. Security
- Never hardcode secrets in workflow files
- Use minimal required permissions
- Regularly update GitHub Actions versions
- Scan for vulnerabilities in actions

### 5. Performance
- Run jobs in parallel when possible
- Use self-hosted runners for large builds
- Implement artifact retention policies

## Troubleshooting

### Common Issues

**1. Cache Not Working**
```bash
# Clear cache manually
rm -rf ~/.cargo/registry
rm -rf target/
```

**2. Build Times Too Long**
- Check cache hit rate
- Consider splitting workflows
- Use self-hosted runners

**3. Permission Errors**
- Verify repository secrets are set
- Check workflow permissions
- Ensure GitHub token has correct scopes

**4. Docker Build Failures**
```bash
# Test locally first
docker build -t jyc:test .
docker run --rm jyc:test --help
```

### Debugging Workflows

1. **Enable Step Debugging**: Add `ACTIONS_STEP_DEBUG: true` secret
2. **Check Logs**: View workflow run logs in GitHub UI
3. **Test Locally**: Use `act` CLI to run workflows locally
4. **Simplify**: Create minimal reproduction workflow

### Getting Help

- [GitHub Actions Documentation](https://docs.github.com/en/actions)
- [Rust GitHub Actions](https://github.com/actions-rs)
- [GitHub Community Discussions](https://github.com/community/community)
- [Project Issues](https://github.com/kingye/jyc/issues)

## Next Steps

1. Review and customize the provided workflow templates
2. Set up repository secrets for your environment
3. Test workflows with a test PR
4. Monitor initial runs and adjust as needed
5. Extend workflows with additional features as required

Remember: Start simple and add complexity gradually. The provided workflows are templates that should be adapted to your specific needs.