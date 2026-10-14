# JYC Project

JYC is a channel-agnostic AI agent framework written in Rust.
It monitors inbound channels (email via IMAP), routes messages to topics,
and uses an in-process AI agent to generate replies.

## Tech Stack
- Rust, tokio async runtime
- IMAP/SMTP for email channels
- In-process AI agent as the AI backend
- Docker for containerized deployment

## Code Conventions
- Use `tracing` for all logging (never `println!`)
- Error handling: propagate with `?`, use `.context()` for meaningful errors
- All public functions must have doc comments

## 路径命名（统一术语）

- 术语以 `DESIGN.md` 的 **Terminology** 为准，代码、文档、日志、提交信息一律使用统一名称
- 禁止使用的旧名：`workdir`/`--workdir`、`data_home`、`config_home`、`jyc_dir`、`state_root`、`agents_root`、`agents_workspace_root`、`staging_dir`
- 数据目录参数固定为 `--data-dir`（无短选项）
- topic 的工作目录一律称 `topic_path`；`.jyc` 一律称 state dir，且恒在 agents dir 下
- topic 永不落在 channel 下；`<data dir>/channels/<channel>/` 是该 channel 的运行状态（游标），不是 topic

## Git Rules
- NEVER run `git config user.name` or `git config user.email` (local or global)
- NEVER run `git config --global` for any setting

## 测试要求

### 测试隔离
- 使用 `tempfile::TempDir` 创建临时目录，测试后自动清理
- 禁止使用 `unsafe { std::env::set_var() }` 修改环境变量，避免污染全局状态
- 测试用例不得依赖外部服务（网络、文件系统固定路径），使用 mock 或 test fixture

### 并行安全
- `cargo test --workspace` 默认并行模式必须稳定通过
- 测试间不得共享可变状态；如必须串行，使用 `#[serial]` 标记并在 CI 中串行执行
- 资源泄漏（如端口占用）的测试必须实现 `Drop` 或使用 `TempDir` 自动清理

## 工作流约定

### 分支命名
- 功能分支：`feat/issue-{N}-<简短描述>`（如 `feat/issue-220-add-imap-idle`）
- 修复分支：`fix/issue-{N}-<简短描述>`（如 `fix/issue-42-fix-timeout-panic`）
- 使用连字符（`-`）分隔单词，禁止大写字母

### PR 数量与串行
- 开 PR 的代价高（每次 push 触发完整 CI：fmt、clippy、llvm-cov）。**尽量少开 PR**：同一轮工作的改动合入同一个 PR，即使关注点不同
- **禁止并行开 PR**：必须等前一个 PR 合并后再开下一个
- 已有 PR 尚未合并时，后续改动推送到该 PR 的分支，不新开 PR
- 只有彼此独立且需要独立回滚的改动，才拆成多个 PR

### PR 前检查清单
- 本地只跑 `cargo check -p <改动的 crate>`（改了该 crate 的测试才加 `--tests`）和 `cargo fmt -- --check`；**禁止** `--workspace` 全量 check，以及本地运行 `cargo build` / `cargo test` / `cargo clippy` / `cargo llvm-cov`（开发机资源受限：全量 check 会 OOM 并把 `target/` 撑满，完整验证以 CI 为准）
- `cargo check --tests` **只编译不执行**：绿灯只证明测试能编译，禁止据此声称测试通过；测试结论只能来自 CI 的测试运行
- 本地 gate 每个变更集只跑一次：批量改完再跑，测试文件放最后改（`#[cfg(test)]` / `tests/*.rs` 一旦改动，test cfg 单元必然整包重编）
- 本地 gate 是编译级检查，热缓存下只需数秒；若突然变慢，通常是 `check` 缓存被打脏（`cargo build` / `cargo test` 的产物与 `check` 不同一类，本地跑一次就会让下一次 gate 重付整个依赖图，`CARGO_BUILD_JOBS=1` 时逐 crate 串行）。**禁止**以「太慢所以换更重的命令」为理由跑 `cargo test` / `cargo build`——那条命令正是把缓存打脏的元凶
- 跨 crate 的类型/接口变更：先用 grep 找全调用点，只 check 受影响的 crate，其余交给 CI；不为「更保险」重跑已通过的检查
- CI（`.github/workflows/ci.yml`）自动执行：fmt、clippy -D warnings、llvm-cov（60% 阈值）
- 改依赖时提交 `cargo check -p <crate>` 顺带刷新的 `Cargo.lock`
- 按「文档约定」检查是否需更新相关文档
- 非平凡改动（跨文件、改逻辑，尤其本地跑不了测试时）：push / 开 PR 前或同时派一次评审——`jyc_spawn_subagent(name="review-<what>", type="review")`，再用 `jyc_send_to_topic(..., require_reply=true)` 派任务（任务模板见 `forked-review` 技能），范围给 `git diff <base>..HEAD`（`<base>` = `git merge-base origin/main HEAD`，含刚提交的改动）；它异步运行、不阻塞 push，报告作为同一个 PR 的 follow-up commit

### CI 等待规则
- **禁止**阻塞等待 CI：不得轮询 `gh pr checks` / `gh run watch`，不得用 `sleep` 重试，不得因为「CI 还没跑完」而推迟提交或推迟回复
- push / 开 PR 之后**立即结束本轮**并把链接交给用户；CI 由远端异步执行，失败时由用户或下一次消息再驱动修复
- 如确需状态：只查一次（`gh pr checks <branch>`），拿到输出立刻返回，不重复查询

### 提交信息格式
遵循 [Conventional Commits](https://www.conventionalcommits.org/)：`feat:` / `fix:` / `refactor:` / `docs:` / `test:` / `chore:`（示例：`feat: add IMAP idle support for real-time email monitoring`）

## 文档约定

### 文件用途映射
| 文件 | 定位 |
|------|------|
| `DESIGN.md` | 系统架构设计文档，记录设计决策和 trade-off |
| `CHANGELOG.md` | 面向用户的版本变更记录 |
| `docs/` | 专题文档目录（API 文档、配置指南等） |
| `AGENTS.md` | AI agent 行为约束规则，使用精简、断言式语言编写 |

> **AGENTS.md 编写规则**：使用断言式语言（"必须……" / "禁止……"），避免冗长描述，每条规则可直接作为判断依据。

### 文档更新触发规则
| 变更类型 | 需更新文档 |
|----------|------------|
| 架构变更、新 crate、模块拆分/合并 | `DESIGN.md` |
| 新增配置项或环境变量 | `config.example.toml` 及 `README.md` |
| 新增 channel 类型 | `docs/channels/` 对应文档 |
| 功能变更（新增/修改/移除） | `CHANGELOG.md` |
| Agent 行为规则变更 | `AGENTS.md` |

### CHANGELOG 格式约束
遵循 [Keep a Changelog](https://keepachangelog.com/) 规范，按 **Added / Changed / Fixed / Removed** 顺序组织，每项格式：`- {简短描述} (#{issue/PR 编号})`

## Agent Behavior Rules

### Subagents
- 术语以 `DESIGN.md` 的 **Terminology** 为准：`topic_path` = 工作目录（agent 在哪跑、父被授权读写哪），state dir = 该 topic 的 `.jyc`（恒在 agents dir 下、按名字定位）
- 委派前必须先看系统提示里的子 agent 列表；已有能用的子 agent 就用 `jyc_send_to_topic`（`require_reply=true`）追加任务，**禁止**重复创建
- 子 agent 的角色用 `type` 声明，不要用散文描述：`type` 指向一个 subagent type（`[subagents.<type>]` 或 `<config dir>/subagents/<type>/config.toml`），spawn 时把该角色的 model、技能白名单、`disabled_tools`、mode、instructions 写入子的 `.jyc/config.toml` → 评审这类"不能改文件、只看某几个技能"的角色因此是**结构性**约束（技能被限定、列出的工具被禁），不是靠提示词约束
- `type` 由两个入口用**运行中**的 config 解析（`/spawn` 的 `CommandContext`、spawn 工具的共享 `ArcSwap`），不读盘：`--config` 指定的文件里定义的角色同样可见；type 目录只扫 L1 配置目录，不分层
- `disabled_tools` 只作用于它列出的工具：`bash` 能绕开（本项目自带评审角色 `subagents/review/config.toml` 就保留 `bash` 以便 `git diff`），要留就靠 mode 与 instructions 兜底，别把"只读"说成结构性保证
- 本项目自带评审角色 `subagents/review/config.toml`：用前 `cp -r subagents/review <config dir>/subagents/`；评审改动用 `type="review"` 即可（该角色在自己的 `[spawn]` 里声明了 `share_topic_path = true`，所以它直接在你的目录里读未提交的改动；未提交的、刚提交的都算，范围给 `git diff <base>..HEAD`）；它异步运行，不占 turn、不阻塞 push
- type 的**安装文件**（`<config dir>/subagents/<type>/config.toml`）除 topic 那些键外还认两个**只有角色才有**的键：`description`（一句话「什么时候该用我」，会被列进 spawn 工具 `type` 参数的描述里，供父 agent 自主选择）与 `[spawn]`（该角色的两个起始开关默认值：`inherit_context`、`share_topic_path`，显式传参优先——**注意入口不对称**：两个开关的双向覆盖只有 spawn 工具能做，`/spawn` 只有 `--inherit` 一个开关、且只能打开）。内联 `[subagents.<type>]` 是 topic config，放不下这两个键
- 仅当已有子 agent 都不合适时才用 `jyc_spawn_subagent` 新建；它与 `/spawn` 是同一实现：子 agent 继承本 topic 的**设置**（mode、pattern、各类 override——传 `type` 时只有该类型**未声明**的键继承：类型声明了 `mode`/`[ai]` 模型/`skills`/`mcps`，对应的 override 文件才被丢弃；起始模型另按父**在该模式下的**模型做快照（父没为这个模式设过，才用父当前正在跑的那个）——spawn/fork 时解析并写进子的 `model-override`，角色自己声明了模型则听角色的；任务列表从不继承，子 agent 从空列表开始、自己做 `task_create`），**对话与 transcript 默认不继承**——要延续同一思路才传 `inherit_context=true`（`/spawn --inherit`；带对话也不带任务列表）；它默认有自己的 `topic_path`，传 `share_topic_path=true` 才借用父的目录（或由角色的 `[spawn]` 声明，评审改动专用，未提交的与刚提交的都算；其 state 仍在自己名下）
- 子 agent 的输入由父用 `write` 放进它的 `topic_path`（父对该目录有读写权），再用 `jyc_send_to_topic` 派任务；给已存在的子 agent 追加任务同样用 `jyc_send_to_topic`
- 改子 agent 的可配置项（模式、模型等）：把命令体（`/build`、`/plan`、`/model`）作为消息发给它；正文为空即不进模型，不消耗子 agent 的 turn
- type 决定子的**起始**状态，不是枷锁：之后给子发的 `/plan`、`/model`、`/skill` 照旧优先于它；要改角色就按新 type 重 spawn（type 在 spawn 时展开，改 type 定义不影响已存在的子）
- **禁止**读写子 agent 的 state dir（`.jyc`——那是它自己的状态）；只操作它的 `topic_path` 里的普通文件
- 子 agent 可以再派子 agent，层级不限；一个 topic 能建多少子 agent 也不设上限。任务为异步：发完即结束本轮，**禁止**轮询等待结果，结果会作为新消息回到本 topic
- 父 topic 关闭（含上游 auto-close）会连同整棵子树一起关闭（子、孙……各自的 `<agents dir>/<child>` 一并清理）：禁止假设子 agent 能活过父；共享目录的子 agent 关闭时只删自己的 state，父的目录由父决定

### Reply vs. SendMessage
- The agent's final message at the end of a turn IS the in-topic reply; no reply tool exists.
- `jyc_send_message` is only for out-of-topic proactive messages; never use it to spam users — limit to alerts and notifications.

### Task List
- Multi-step work (a plan, an implementation, a cross-file fix) must start with `task_create`; the list is the topic's single plan of record.
- Mark an item `in_progress` when picking it up and `completed` with `task_update` as soon as it is genuinely done — never ahead of the work, never with validation still failing.
- Re-plan with a new `task_create` (it replaces the list and renumbers ids) rather than patching a stale one item by item.
- After a context reset, call `task_list` for the ids and current progress; never guess ids from memory.

## References
- See DESIGN.md for architecture
- See CHANGELOG.md for version history

- jin AGENTS.md (约束来源参考): https://github.com/kingye/jin/blob/main/AGENTS.md
