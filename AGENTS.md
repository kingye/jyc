# JYC Project

JYC is a channel-agnostic AI agent framework written in Rust.
It monitors inbound channels (email via IMAP), routes messages to topics,
and uses an in-process AI agent to generate replies.

## 诚实与证据

- 任何外部状态结论（分支、commit、PR、CI、文件是否存在、命令是否执行）只能来自**本轮真实的工具返回**：报 commit 前 `git rev-parse HEAD`，报 PR 前 `gh pr view <n> --json state,headRefOid`，报推送看 push 输出的 `<old>..<new>` 行
- 禁止把「预期会打印什么」写进回复：没有工具结果块支撑的输出一律算没发生。被指出编造时立刻补一次可验证的调用并贴真实返回，不许辩解、不许让用户查日志自证
- 报「完成 / 仅文档改动 / 新增 N 个测试」之前用外部事实核对：`git show --stat HEAD`、逐个 test binary 的计数、`gh pr view --json state,mergedAt`；「没有 X」与「X 是 Y」同样需要证据（先确认搜索路径，必要时 `rg --hidden --no-ignore`）

## Tech Stack
- Rust, tokio async runtime
- IMAP/SMTP for email channels
- In-process AI agent as the AI backend
- Docker for containerized deployment

## Code Conventions
- Use `tracing` for all logging (never `println!`)
- Error handling: propagate with `?`, use `.context()` for meaningful errors
- All public functions must have doc comments
- 禁止用 `.ok()` / `unwrap_or_default()` 抹掉错误路径：它把「读取失败」变成「没有」，在下游表现成同形的 401/空值——遇到「凭据没错却被拒」先查错误是否被吞
- 禁止造第二条真相：工具/技能的可用性从活注册表与活对象读（`source()` 返回提供方），不要另存构造时的 name→组快照，也不要让同一份过滤列表同时决定「提示里列什么」与「能调用什么」
- 废弃一个语义时必须同轮改名或删除：文档、渲染路径、提示词三者中任一在描述已经不可能发生的行为 ⇒ 该字段删掉或改名，不能只改语义
- 工具注册：主体排除须在 `register_extended_tools()` **之前**（deferred group 要反映排除结果）；仅 `load_extended_tools` / `load_mcp_schema` 两个 loader 需在注册后单独再套一遍（主体排除跑时它们还不存在）；MCP 暴露保留名（内置 loader 名）时在加载循环里跳过并 warn

## 路径命名（统一术语）

- 术语以 `DESIGN.md` 的 **Terminology** 为准，代码、文档、日志、提交信息一律使用统一名称
- 禁止使用的旧名：`workdir`/`--workdir`、`data_home`、`config_home`、`jyc_dir`、`state_root`、`agents_root`、`agents_workspace_root`、`staging_dir`
- 数据目录参数固定为 `--data-dir`（无短选项）
- topic 的工作目录一律称 `topic_path`；`.jyc` 一律称 state dir，且恒在 agents dir 下。例外：`topic_path` 下自己的 `.jyc/` 不是 state——它是 L3 的**用户层**（`config.toml` 无 `mcps` + `skills/`），写「工作目录里的 .jyc」时必须点明这一区分
- topic config 读取经 `load_topic_config`（state dir config 与 `<topic_path>/.jyc/config.toml` 逐键合并、state 优先；用户层剥离 `mcps`/`mcps_replace`）；skills 发现的最低优先级来源是 `<topic_path>/.jyc/skills`
- topic 的 **home** 是它在 agents dir 下的归属目录（pipe topic 为 `<agents dir>/<agent>/<topic>/`，agent-keyed 的 1:1 与 `/spawn` 为 `<agents dir>/<name>/`）：home 下 `.jyc` 是 state dir、`.data` 是默认工作目录（即 `topic_path`），两者**并列**——state 永不落在工作目录内
- 工作目录一经选定即不迁移：老 topic 保持它创建时的目录（state 也跟着不动），只有全新 topic 才得到 `<home>/.data`
- 框架自有目录一律点开头（`.jyc`、`.data`、`.attachments`、`.inbound`）；工作目录里非点开头的内容属于用户/agent
- 入站附件落地目录写作 `topic_path/.attachments/`（`ATTACHMENT_DIR_NAME`），`save_path` 显式配置时以配置为准
- 不把 topic 的工作目录称作 "data dir"：`<data dir>` 是实例根目录（`--data-dir`），topic 的默认工作目录写作 `<home>/.data`
- topic 永不落在 channel 下；`<data dir>/channels/<channel>/` 是该 channel 的运行状态（游标），不是 topic
- 查找 topic 一律经 `jyc_types::find_topic`/`topic_index`/`has_topic`（身份 = topic name，全进程唯一；`TopicInfo` 与 `TopicSummary` 都实现 `TopicRef`；调用方知道 channel 时一并传入做校验，channel 缺省时只按名字匹配）；禁止按 `topic_path` 查找 topic——同一路径可被多个 topic 共享（`share_topic_path` 的子 agent 与父同 path），path 不是身份

## Git Rules
- NEVER run `git config user.name` or `git config user.email` (local or global)
- NEVER run `git config --global` for any setting
- 禁止 `git add -A` / `git add .`：工作区里有未跟踪产物（根目录的旧 `jyc_back` 二进制、框架落盘的 jsonl），一律 `git add <显式路径>`；推送前用 `git diff --name-only <base>..HEAD | grep -vE '\.(rs|md|toml)$'` 查杂物
- 提交信息与多行文本先写文件再 `-F <文件>`，禁止内联进 bash：反引号会被命令替换（信息静默残缺），含 `/` 开头的片段会被沙箱判为越界路径
- 发布只推 `git push origin main vX.Y.Z`，禁止 `--tags`：它连带推 `nightly`，non-fast-forward 会让整条 push 报错退出（main 与 tag 其实已推成功，退出码骗人）

## 测试要求

### 测试隔离
- 使用 `tempfile::TempDir` 创建临时目录，测试后自动清理
- 禁止使用 `unsafe { std::env::set_var() }` 修改环境变量，避免污染全局状态
- 测试用例不得依赖外部服务（网络、文件系统固定路径），使用 mock 或 test fixture

### 并行安全
- `cargo test --workspace` 默认并行模式必须稳定通过
- 测试间不得共享可变状态；如必须串行，使用 `#[serial]` 标记并在 CI 中串行执行
- 资源泄漏（如端口占用）的测试必须实现 `Drop` 或使用 `TempDir` 自动清理

### 断言

- 期望值必须由**该用例自己的** fixture 推出：字面量、宽度、列宽都不要从别的用例复制；批量替换断言行前先数命中并确认该值是显式传入还是默认回落
- 断言「payload 含/不含 X」「两个区域不同」之前先确认 fixture 真能区分：不同区域用不同标记，需要两区域不同就放进会被压缩的内容（如 `tool_calls`）；比较序列化 payload 用消息级前缀（值相等 ⇒ 字节前缀相等），禁止对 JSON 数组用 `starts_with`
- TUI 断言要 scope 到目标 pane：`Borders::LEFT` 让每行以 `│` 开头（用 `trim_matches` / `starts_with`；`str::find` 返回字节偏移，`│` 占 3 字节），宽度用 `UnicodeWidthStr::width()`，图例列宽取当前 composition 的最大标签长；能由 widget flag 关掉的 chrome 就在 fixture 里关掉
- 改 prompt/schema 的「合成形状」后，用旧形状的特征串全仓 `rg`，连同断言它的用例一起改——旧断言照常编译，红只能由 CI 发现
- 涉及 state dir 的测试先 `register_if_absent(name, path)`、结尾 `unregister` + `clear_closed`：`resolve_state_dir` 对未注册的 topic 是 panic
- 验证「某能力仍保留」要沿 producer→consumer 走完整链路：转发层还在不等于端到端还通（如 `send_reply` 在 `pending_delivery` 里传真实附件，但 agent 最终回复路径 `deliver_reply` 恒传 `None`）

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
- 本地不跑 clippy，所以提交前按 clippy 默认 lint 自查：`collapsible_if` 写成 `if cond && let Some(x) = …`（edition 2024）、`len() == 0` → `is_empty()`、`iter().any(|x| x == y)` → `contains(&y)`、函数参数别超 7 个、只借用的 `vec!` 改数组、`&[x.clone()]` 改 `std::slice::from_ref(&x)`、format 用内联 `{name}`
- 非平凡改动（跨文件、改逻辑，尤其本地跑不了测试时）：push / 开 PR 前或同时派一次评审——`jyc_spawn_subagent(name="review-<what>", type="review")`，再用 `jyc_send_to_topic(..., require_reply=true)` 派任务（任务模板见 `dev-workflow` 技能），范围给 `git diff <base>..HEAD`（`<base>` = `git merge-base origin/main HEAD`，含刚提交的改动）；它异步运行、不阻塞 push，报告作为同一个 PR 的 follow-up commit

### CI 等待规则
- CI 结果**必须**回收：每次 push / 开 PR 都安排一次异步等待，把结果带回本 topic——**默认**派 `ci-watch` 子 agent（`jyc_spawn_subagent(name="ci-watch-<pr>", type="ci-watch")` 建出角色，再用 `jyc_send_to_topic(..., require_reply=true)` 派任务；只发消息不 spawn 会直接报错（那个名字没有 topic 拥有，send 从不创建 topic）；步骤见 `dev-workflow`）
- 等待**只安排一次**，针对本轮结束时留下的那个 head：中途还会再 push 时，不为将被取代的 head 派等待者。同一 PR 出了新 head，优先复用同一个 watcher（再派一次任务＝它的一轮新 turn），它已回报过或确实没了才新派
- **禁止**由自己等待：不得在轮内轮询 `gh pr checks` / `gh run watch`，不得用 `sleep` 重试，不得因为「CI 还没跑完」而推迟提交或推迟回复——安排完**立即结束本轮**并把链接交给用户；结果作为新消息回到本 topic，红 CI 在那一轮里修
- 唯一例外：结论**已经出来**且马上要用它做决定（如合并前）——允许单查一次（`gh pr checks <branch>`），拿到输出立刻返回，不重复查询；结论还没出时回到第 1 条
- 判读 CI：`gh run list` 必须带 `--workflow CI`（main 上还有 Gitee / Bloat / Docker 工作流）；`gh run view` 要 run id 而非 job id，无 checkout 的目录要加 `--repo`；修 CI 前先按 `headRefOid` 对齐当前 head 再读 `--log-failed`；force-push 后必须告知正在等待的 watcher 旧 head 作废

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

### Plan Mode
- plan mode 只是提示词约束（没有工具级禁用）：获批也不等于可以动手，只有 mode 切走才能写文件/提交/推送；它的 MAY ONLY 是穷举清单，但显式把 task tools（`task_create`/`task_update`/`task_list`）与 `lesson_add` 列为例外——它们是自己的记录，不是对项目的改动

### Subagents
- 术语以 `DESIGN.md` 的 **Terminology** 为准：`topic_path` = 工作目录（agent 在哪跑、父被授权读写哪），state dir = 该 topic 的 `.jyc`（恒在 agents dir 下、按名字定位）
- 委派前必须先看系统提示里的子 agent 列表；已有能用的子 agent 就用 `jyc_send_to_topic`（`require_reply=true`）追加任务，**禁止**重复创建
- 子 agent 的角色用 `type` 声明，不要用散文描述：`type` 指向一个 subagent type（`[subagents.<type>]` 或 `<config dir>/subagents/<type>/config.toml`），spawn 时把该角色的 model、技能白名单、`disabled_tools`、mode、instructions 写入子的 `.jyc/config.toml` → 评审这类"不能改文件、只看某几个技能"的角色因此是**结构性**约束（技能被限定、列出的工具被禁），不是靠提示词约束
- `type` 由两个入口用**运行中**的 config 解析（`/spawn` 的 `CommandContext`、spawn 工具的共享 `ArcSwap`），不读盘：`--config` 指定的文件里定义的角色同样可见；type 目录只扫 L1 配置目录，不分层
- `disabled_tools` 只作用于它列出的工具：`bash` 能绕开（本项目自带评审角色 `subagents/review/config.toml` 就保留 `bash` 以便 `git diff`），要留就靠 mode 与 instructions 兜底，别把"只读"说成结构性保证
- 本项目自带评审角色 `subagents/review/config.toml`：用前 `cp -r subagents/review <config dir>/subagents/`；评审改动用 `type="review"` 即可（该角色在自己的 `[spawn]` 里声明了 `share_topic_path = true`，所以它直接在你的目录里读未提交的改动；未提交的、刚提交的都算，范围给 `git diff <base>..HEAD`）；它异步运行，不占 turn、不阻塞 push
- type 的**安装文件**（`<config dir>/subagents/<type>/config.toml`）除 topic 那些键外还认两个**只有角色才有**的键：`description`（一句话「什么时候该用我」，会被列进 spawn 工具 `type` 参数的描述里、并作为 `/spawn` 弹窗里该 type 那一行，供父 agent 与用户选择）与 `[spawn]`（该角色的两个起始开关默认值：`inherit_context`、`share_topic_path`）。**注意入口不对称**：工具能双向覆盖（显式 `false` 也算数），`/spawn` 的 `-i/--inherit` 与 `-s/--share` 都只能打开——命令行不写就是「没说」，此时角色的默认生效、再不然才是 `false`。内联 `[subagents.<type>]` 是 topic config，放不下这两个键
- 仅当已有子 agent 都不合适时才用 `jyc_spawn_subagent` 新建；它与 `/spawn` 是同一实现：子 agent 继承本 topic 的**设置**（mode、pattern、各类 override——传 `type` 时只有该类型**未声明**的键继承：类型声明了 `mode`/`[ai]` 模型/`skills`/`mcps`，对应的 override 文件才被丢弃；起始模型另按父**在该模式下的**模型做快照（父没为这个模式设过，才用父当前正在跑的那个）——spawn 时解析并写进子的 `model-override`，角色自己声明了模型则听角色的（声明的值指不到槽位时除外：子 agent 保留父的模型，不掉到全局默认）；任务列表从不继承，子 agent 从空列表开始、自己做 `task_create`），**对话与 transcript 默认不继承**——要延续同一思路才传 `inherit_context=true`（`/spawn -i/--inherit`；带对话也不带任务列表）；它默认有自己的 `topic_path`，传 `share_topic_path=true`（或 `/spawn -s/--share`）才借用父的目录（或由角色的 `[spawn]` 声明，评审改动专用，未提交的与刚提交的都算；其 state 仍在自己名下）
- **派新的子 agent 只能用 `jyc_spawn_subagent`**：`jyc_send_to_topic` 只投递给**已经存在**的 topic，给一个**没人拥有的名字**会直接报错（不再顺手把它变成一个新 agent）——它也不继承角色的 mode/技能/禁用的工具，评审也就只是提示词里的请求而非结构约束。派评审或 CI 等待一律 `jyc_spawn_subagent(name="review-<what>", type="review")` / `type="ci-watch"`
- 子 agent 的输入由父用 `write` 放进它的 `topic_path`（父对该目录有读写权），再用 `jyc_send_to_topic` 派任务；给已存在的子 agent 追加任务同样用 `jyc_send_to_topic`
- 改子 agent 的可配置项（模式、模型等）：把命令体（`/build`、`/plan`、`/model`）作为消息发给它；正文为空即不进模型，不消耗子 agent 的 turn
- 子 agent 用完即关：给它发消息体 `/close --force`（裸 `/close` 只是确认提示，不会真关）；对已关闭的子再发消息会**报错**（它的 home 已被删，这个名字不再属于任何 topic），不会另起一个新 topic
- type 决定子的**起始**状态，不是枷锁：之后给子发的 `/plan`、`/model`、`/skill` 照旧优先于它；要改角色就按新 type 重 spawn（type 在 spawn 时展开，改 type 定义不影响已存在的子）
- **禁止**读写子 agent 的 state dir（`.jyc`——那是它自己的状态）；只操作它的 `topic_path` 里的普通文件
- 子 agent 可以再派子 agent，层级不限；一个 topic 能建多少子 agent 也不设上限。任务为异步：发完即结束本轮，**禁止**轮询等待结果，结果会作为新消息回到本 topic
- 父 topic 关闭（含上游 auto-close）会连同整棵子树一起关闭（子、孙……各自的 `<agents dir>/<child>` 一并清理）：禁止假设子 agent 能活过父；共享目录的子 agent 关闭时只删自己的 state，父的目录由父决定
- 关闭子 agent 后必须确认 `<agents dir>/<name>` 真的消失：在它自己正在跑的那一轮里执行 `/close --force`，然后确认目录已删；close 路径先用 `mark_closed` 冻结 state（`close_one_topic_under`），后续记账写入会被丢弃而非重建 `.jyc`——若仍见幽灵 topic，查 `mark_closed` 是否先于删除调用
- 改 `<config dir>/subagents/<type>/config.toml` 只动被要求的字段：整份重写会顺手丢 `[ai]` model、`disabled_tools`、`[context_strategy]`，仓库的契约测试会当场抓住
- 「不用任何技能」写 `skills = []`，但它不是绝对关闭：topic 状态里遗留的 `/skill on X` 会把空列表重新撑开，要绝对关闭得先 `/skill reset`

### Reply vs. SendMessage
- The agent's final message at the end of a turn carries an ordinary text reply; no reply tool exists.
- Use `jyc_send_message` for proactive alerts, cross-channel messages, or a reply that carries attachments; never use it to spam users.
- Use `jyc_send_to_topic` to send a message to another topic for agent processing.
- **禁止**借 `jyc_send_to_topic` / `/send` / 回复转发注入 `/grant`、`/ungrant`：它们是授权命令，服务端在入队前与 dispatch 两处都拒绝来自 agent 的消息（`CommandSpec::authorization`）。agent 扩大边界只有一条路——调 `request_access` 问用户，由用户在服务端执行命令
- 调 `request_access` 后本轮结束，授权在用户回复后**下一轮**生效：禁止在同一轮内声称「已获授权」或重试越界工具；也不要替用户执行 `/grant`（做不到，且那是绕过）
- 边界拒绝消息里的 `resolved: /abs/path` 才是可授权的形式：`request_access` 只接受绝对路径，相对路径不构成 grant

### Task List
- The task tools are in the extended group: in a fresh registry, call `load_extended_tools` before the first `task_create`; the loader description points multi-step work at them.
- Multi-step work (a plan, an implementation, a cross-file fix) must start with `task_create`; the list is the topic's single plan of record.
- Mark an item `in_progress` when picking it up and `completed` with `task_update` as soon as it is genuinely done — never ahead of the work, never with validation still failing.
- Re-plan with a new `task_create` (it replaces the list and renumbers ids) rather than patching a stale one item by item.
- After a context reset, call `task_list` for the ids and current progress; never guess ids from memory.
- 报告一轮完成前用 `task_list` 对账外部事实（`gh pr view <n> --json state,mergedAt`、main 上的合并提交），再标 completed；列表状态不能凭记忆推断

## References
- See DESIGN.md for architecture
- See CHANGELOG.md for version history

- jin AGENTS.md (约束来源参考): https://github.com/kingye/jin/blob/main/AGENTS.md
