## 核心原则

- 新增或修改行为前，先更新对应 spec；目录不存在时按需创建。先明确产品规则、状态所有者、接口和验收场景，再实现代码。
- 以当前检出的源码、`package.json` 和架构策略为准。说明中只保留当前仓库提供的功能、命令和文件；删除功能时同步清理指令和技能中的引用。
- 定位问题时，未明确要求修改代码就先调查原因。结合源码、日志和运行时证据，区分已确认原因与待验证假设。
- 保留与任务无关的本地改动，不自行恢复已移除的模块或内部依赖。
- 任务完成提交个 commit

## 命令与仓库结构

开工前运行 `node scripts/check-workspace-freshness.mjs` 检查基线。Node 版本以 `mise.toml` 为准。

以下命令从仓库根目录执行：

| 用途                 | 命令                                      |
| -------------------- | ----------------------------------------- |
| 类型检查             | `pnpm typecheck`                          |
| Lint                 | `pnpm lint` / `pnpm lint:fix`             |
| 格式检查             | `pnpm fmt:check`                          |
| 桌面开发             | `pnpm dev:desktop`                        |
| Web 开发             | `pnpm dev:web`                            |
| 提交前检查           | `pnpm verify:pre-push`（Lint 与架构检查） |
| 架构检查             | `pnpm architecture:check --changed`       |
| 模块阅读包           | `pnpm architecture:context <module-id>`   |
| 未使用依赖与导出     | `pnpm knip`                               |
| 导出引用查询         | `pnpm dep:refs --list-exports <file>`     |
| Rust 静态检查        | `pnpm check:rust`                         |
| Rust 基础 crate 测试 | `pnpm test:rust`                          |

测试入口以目标包当前的 `package.json` 和实际测试文件为准，不假定存在统一的单测或 E2E 命令。

- `packages/desktop`：Electron main、host、renderer。
- `packages/web`、`packages/server`：Web 客户端与服务端。
- `packages/ui`：共享 React 组件、hooks 与 Zustand store。
- `packages/services`：业务服务；`packages/rpc`：RPC 框架。
- `packages/shared`：共享协议与类型；`packages/client`：Agent 客户端 SDK。
- `apps/zcode-cli`：Agent CLI 与运行时。
- `apps/zcode-cli-rust`：Rust agent runtime；`apps/zcode-host`：Rust Host（连接、调用顺序与出站队列在 `crates/gateway`）；`rust/`：两者共用的 Rust 基础 crate（`zcode-kit`、`zcode-async`、`zcode-proc`），通过路径依赖引用。
- `CONTEXT.md`：插件商店领域词汇；修改相关 UI 前阅读。
- `DESIGN.md`：UI 设计规范；修改 UI 前阅读。

## 实现与验证

- 代码改动使用 `.agents/skills/architecture-governance/SKILL.md`，先运行架构检查，再读取目标模块的受控上下文。
- 避免重复状态和多条写入路径。明确唯一所有者、接口、依赖方向、事件顺序与幂等边界，不能用超时掩盖同步问题。
- 有行为改动时先补充对应测试；交互改动需要 E2E 场景。检查测试与实现是否一致，并实际执行可用的验证。未执行或环境受限时如实说明。
- 修复 bug 时用中文注释说明原因和修复依据。发现设计缺陷时先与用户对齐，不不断增加兜底分支。
- 涉及状态、时序、远端或异步同步的方案，用图展示所有者及事件顺序。
- 必须执行 `pnpm typecheck` 和 `pnpm lint`，报告真实结果，不将已有失败写成通过。
- 使用异步文件和网络 IO；跨包导入使用公开入口，遵守现有路径别名。
- 禁止 UI 直接调用 Repo、Service 引用 Runtime 具体实现、跨域导入实现细节及循环依赖。

## Rust

- 架构与迁移路线见 `docs/specs/rust-unified-host.md`，工程实践见 `docs/specs/rust-engineering-practices.md`，基础 crate 与护栏见 `docs/specs/rust-u0-foundation.md`。改动遵守其中的分层、依赖方向和唯一入口规则。
- 工具链由仓库根 `rust-toolchain.toml` 固定；`clippy.toml`、`deny.toml`、`.cargo/config.toml` 也在仓库根，三个 workspace 共用。`pnpm check:rust` 与测试脚本依赖 `cargo-nextest`、`cargo-deny`、`cargo-shear`，缺失时脚本会给出安装命令。
- 测试用 `cargo nextest run --workspace`：`apps/zcode-cli-rust` 的根 `Cargo.toml` 同时是一个 package，不加 `--workspace` 时 cargo 只处理根包。
- 唯一入口（由 `clippy.toml` 的 `disallowed-methods` 或评审保证）：
  - 子进程只经 `zcode_proc::command` / `std_command` 创建；结束进程树用 `zcode_proc::tree` 与 Windows `zcode_proc::job`。
  - Node 风格的“临时文件 + rename”原子写用 `zcode_kit::fs::atomic_write`；数据目录的 home 由二进制入口经 `zcode_kit::paths::home_dir` 解析一次，库内用 runtime 给定的 home（`Egress::home`），不得回落到空路径或 cwd。
  - 进程环境与进程 cwd 只在二进制入口读取（`clippy.toml` 禁用 `std::env::{var, var_os, vars, vars_os, current_dir}`）；库内用 `RuntimeEnv` 快照、workspace 根或显式基准目录，允许的位置用 `#[expect]` 标注理由（`docs/specs/rust-u1-runtime-library.md`）。
  - 秘密值用 `zcode_kit::secret::RedactedString`，日志中的值经 `zcode_kit::redact::redact_secrets`。
  - 任务只经 `zcode_async` 启动并有唯一所有者：作用域任务用 `Spawner::spawn` / `spawn_blocking`，函数内启动并等待的用 `owned` / `blocking`；runtime 非测试代码由 `scripts/check-zcode-cli-rust-boundaries.mjs` 禁止直接 `tokio::spawn` / `spawn_blocking`（例外只有 `src/embed.rs`），Host 已迁移的 crate（`gateway`、`host-core`、`zcode-host`）由 `scripts/rust/check-host-spawns.mjs` 检查，Host 全部迁移后改由 clippy 强制（`docs/specs/rust-u2-task-scope.md`）。
  - spawn 出去、负责发出终态或回复的任务用 `zcode_async::catch_panic` 兜住 panic。
- 不新增 `#[expect(clippy::await_holding_invalid_type)]`：现有标注是待 U2 清理的存量。
- 编码约定：
  - 调用处要能看懂参数含义：不用 `bool` 或含义不明的 `Option` 作位置参数，改用枚举、newtype 或具名方法。
  - `match` 尽量穷尽，不写 `_ =>` 通配分支。
  - 新 trait 写文档注释，说明职责以及实现方应遵守的约定。
  - 模块默认私有，crate 的公开 API 显式导出并保持最小；不为测试扩大公开面，不写只被调用一次的小 helper。
  - 异步追踪在函数定义上加 `#[tracing::instrument]`，不在调用处给 future 套 `.instrument()`。
  - 单元测试放在同级 `*_tests.rs`，用 `#[path]` 引入；断言比较整个对象，不为静态定义的值写测试。
- 并发：不持锁跨 `.await`；spawn 出去、负责发送终态或回复的任务必须处理 panic；新通道默认有界，并写明满了以后的策略。
- 评审规则：
  - 非机械性改动单次不超过 800 行，复杂逻辑不超过 500 行；超出时先合最小的可合入步骤。
  - 逐项检查破坏性变更：Renderer `@zcode/rpc` 契约、V4 wire、Main ↔ Host 控制消息、CLI 参数、配置加载、从已有会话库恢复会话。
  - 模型可见的上下文只追加，不改写历史；注入的每一项都有大小上限，单项不超过 10K tokens；可能超过 1K tokens 的新增项单独审查。
  - runtime 核心（Engine）只做编排，新概念放进新的模块或 crate。

## UI 与平台边界

- 遵守 `DESIGN.md`，复用已有组件，兼顾桌面与手机 Web 的布局、交互、主题和国际化。
- 组件通过 `packages/ui/src/hooks/` 访问服务；平台操作通过 `IPlatformService`（`packages/shared/src/platform.ts`），不直接调用 `window.zcode`。
- 通过依赖注入处理 Desktop、Web、本地和远程环境的差异，并兼顾 Windows、macOS 和 Linux。
- Zustand 状态位于 `packages/ui/src/store/`。广播同步的主题、语言等字段需要防止回环；UI 局部状态不应被误当作服务端事实。
- hooks 中含 JSX 的文件使用 `.tsx`。

## 进程、协议与远程控制

- Desktop app 通过 stdio 与 Agent 通信。协议改动同步更新 `packages/shared/src/zcode-protocol/index.ts`，提供严格类型与运行时校验。
- Main 负责窗口、原生操作、进程调度和消息转发，不承载 task/session 业务状态。
- 每个窗口使用一个 window-scoped Local Host；本地 workspace 共享该 Host。远程 workspace 由窗口内的连接注册表管理，不另建 Desktop Remote Host。
- 手机远控连接桌面已有 Host attachment，复用会话运行时；不为手机另起 Agent、Local Host 或远程会话。
- Desktop 的 `desktop-continuous` 实时链路与手机的 `web-remote-replayable` 恢复链路必须明确区分。修改 stream、snapshot、queue 或重连时，同时验证两种语义。
- 外部 relay 与 Main 只做鉴权、配对、心跳、转发及 attachment 调度，不保存任务队列、快照等业务状态。
- 已接受的 busy/running 输入由 CLI/runtime `CommandInbox` 串行 admission；Renderer 只保留未提交草稿与 pending optimistic overlay，Host owner/lease 负责路由。
- 保留 owner/lease、跨 Host 路由和 stale run 防护，不能仅根据单一路径删除边界判断。

## Workspace Identity

- `workspaceIdentity` 用于身份隔离，`workspacePath` 用于文件操作、命令 cwd、Git 和路径展示。
- 身份 key 统一为 `workspaceIdentity?.trim() || workspacePath`，适用于去重、绑定、缓存、队列、持久化和请求关联。
- 远程链路贯穿传递 `workspaceIdentity` 与 `remoteSessionId`，不得仅按路径匹配。
- 新接口保留本地路径 fallback；远程 identity 复用现有构造和解析工具，不在业务代码中手写格式。

## 日志

- UI 使用 `packages/ui/src/logger.ts`，不直接使用 `console.log` 或 `window.zcode?.log`。
- Agent/session/runtime 相关服务日志使用 `createServiceLogger(scope)`（`packages/services/src/logger/serviceLogger.ts`）。
- `debug` 用于协议原始数据、流式 chunk 和逐条工具更新等高频诊断，生产环境不落盘。
- `info` 用于进程和会话生命周期、权限结果、一次性初始化等生产可用事件。
- `warn` 用于可恢复异常；`error` 用于崩溃、握手失败、鉴权丢失等不可恢复错误。
- 不在日志、示例或提交中写入凭据、真实用户数据和内部服务地址。
