# GitHub Issue 通道实现计划

## 概述

为 JYC 添加 GitHub Issue 通道支持，使 AI Agent 可以通过 GitHub Issue 与用户交互。

## 需求

- **模式**: 轮询模式（不需要公网 IP/Webhook）
- **功能**: 
  - 监听 Issue 评论
  - 监听 Issue 打开/关闭事件
  - 回复评论
  - 不支持创建 Issue
- **事件类型**: 
  - `issue_comment`: Issue 评论
  - `issues`: Issue 打开/关闭

## 设计

### 配置模型

```toml
[channels.my_repo]
type = "github"

[channels.my_repo.github]
owner = "myorg"
repo = "myrepo"
token = "${GITHUB_TOKEN}"
poll_interval_secs = 30
events = ["issue_comment", "issues"]

[[channels.my_repo.patterns]]
name = "urgent"
enabled = true

[channels.my_repo.patterns.rules]
labels = ["urgent", "bug"]
```

### 数据模型映射

| GitHub 事件 | InboundMessage 字段 |
|-------------|---------------------|
| Issue 评论 body | `content.markdown` |
| Issue 标题 | `topic` |
| Issue 编号 | `channel_uid` |
| 评论者 login | `sender` |
| 评论者 ID | `sender_address` |
| Issue 编号 | `reply_to_id` |
| 仓库 full_name | `metadata["repo"]` |
| Issue labels | `metadata["labels"]` |
| 事件 action | `metadata["action"]` |
| 评论/Issue ID | `external_id` |

### Thread 命名

- 格式: `github-{issue_number}`，如 `github-42`
- 同一 Issue 的所有评论在同一线程中

### 模块结构

```
src/channels/github/
├── mod.rs           # 模块导出
├── config.rs        # GitHubConfig 结构
├── types.rs         # GitHub 特有类型
├── client.rs        # GitHub API 客户端 (reqwest)
├── inbound.rs       # GitHubInboundAdapter + GitHubMatcher
└── outbound.rs      # GitHubOutboundAdapter
```

## 实现步骤

### 1. 添加依赖

```toml
# Cargo.toml
reqwest = { version = "0.12", features = ["json"] }
serde_json = "1.0"
```

### 2. 创建目录结构

```
src/channels/github/
```

### 3. 实现配置

- 在 `src/config/types.rs` 添加 `GitHubConfig`
- 创建 `src/channels/github/config.rs`

### 4. 实现 GitHubClient

- 轮询获取新评论: `GET /repos/{owner}/{repo}/issues/comments?since={timestamp}`
- 获取 Issue 变更: `GET /repos/{owner}/{repo}/issues?since={timestamp}`
- 发送回复: `POST /repos/{owner}/{repo}/issues/{issue_number}/comments`

### 5. 实现 InboundAdapter

- 实现 `ChannelMatcher` trait
- 实现 `InboundAdapter` trait
- 轮询逻辑参考 `ImapMonitor`

### 6. 实现 OutboundAdapter

- 实现 `OutboundAdapter` trait
- 回复到 Issue 评论

### 7. 注册通道

- 在 `src/channels/mod.rs` 添加 `pub mod github`
- 在 `src/cli/monitor.rs` 注册适配器

## 参考实现

- 现有通道: `src/channels/email/`, `src/channels/feishu/`
- 轮询逻辑: `src/services/imap/monitor.rs`
- 类型定义: `src/channels/types.rs`

## 注意事项

1. GitHub API 速率限制（轮询间隔不宜过短）
2. 需要 `repo` 权限的 GitHub PAT
3. Pattern 匹配支持 labels、sender、keywords
4. 状态管理：记录 `last_poll_timestamp`