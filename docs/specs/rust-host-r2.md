# Rust Host R2：核心聊天（agent 子系统）

日期：2026-10-08。上级规格：[rust-host.md](rust-host.md) 第 5 节 R2。分支：`feat/rust-host`。

## 1. 目标与范围

在 Rust Host 上跑通核心聊天：打开本地 workspace → 侧栏任务列表 → 新建会话 → 发送 → 流式 → 权限与提问 → Stop → 切换模型 → 查看文件改动。

本阶段只覆盖本地 desktop（`desktop-continuous`）。`web-remote-replayable`、远端 workspace、Bot 远程运行时、task realtime bridge（lease）在 R6 随远程能力一起做。

## 2. 产品规则

### 2.1 CLI 进程

1. **每个 workspace 一个 CLI 进程**（chat lane），以 `workspaceIdentity?.trim() || workspacePath` 为 key。启动是 single-flight：同一 key 的并发请求只拉起一个进程。
2. **命令解析**，命中第一个即用：
   1. `ZCODE_AGENT_SERVER_COMMAND`。附带参数来自 `ZCODE_AGENT_SERVER_ARGS_JSON`，缺省为 `app-server --stdio`。`ZCODE_AGENT_SERVER_RUNTIME` 只允许 `zcode-cli-rust`；这时 Host 追加 `--cwd <workspacePath>`，并且参数里出现 `--cwd` 就报错。
   2. `ZCODE_HOST_NODE_EXEC_PATH` 加上开发态 `zcode.cjs`（从进程 cwd 和 Host 可执行文件目录向上查找，或者取 `ZCODE_AGENT_BUNDLE_PATH`），以 `ELECTRON_RUN_AS_NODE=1` 运行。Rust Host 不是 Node，所以由 Main 把 `process.execPath` 作为 `ZCODE_HOST_NODE_EXEC_PATH` 传给它。
   3. `GLM_BINARY_PATH`。

   最后一律去掉已有的 `--surface` 参数，再追加 `--surface desktop`。

3. **spawn 环境**，后写的覆盖先写的：
   1. 清洗后的 Host 环境，加上 Main 给的登录 shell 补丁（`runtimeProcessEnvPatch`）。Rust Host 不改自己的进程环境，而是算出这份有效环境，显式交给子进程。
   2. `ZCODE_RUNTIME_ENV`：取上一步里的值，不是 development、production、test 之一时用 `production`。
   3. 设置页的代理、No Proxy、CA（规则同 `agentProxyEnv.ts`）。
   4. `ZCODE_BUILTIN_PROVIDER_CONFIG_FILE`、`ZCODE_PERSONAL_PROVIDER_CONFIG_FILE`。
   5. 命令自带的环境。
   6. 有 identity 时设置 `ZCODE_WORKSPACE_IDENTITY`。
4. **cwd**：命令自带的 cwd 优先，否则用 workspace 路径。workspace 路径不是目录时，回退到 `InitLocal.agentSpawnFallbackCwd`。
5. **存储门禁**：命令声明 `supportsStorageStartup` 时，请求要等 `startup/storageState` 进入 `ready` 才发出。
   - 首帧 30 s 内没到，或者进入 `failed`，请求就以 `SQLite startup failed: <code>` 失败。
   - 只接受同一次尝试里 `sequence` 严格递增、且尚未到达终态的帧。
6. **只做懒重启**：进程退出、协议行非法、请求超时（180 s）、dispose 都会关闭连接并回收进程树。之后不会自动拉起，下一次请求才启动新一代。
7. **进程树回收**：关闭 stdin → 等 1.8 s → 对整个进程组发 SIGTERM → 等 2 s → SIGKILL。主进程退出后，同组还活着的进程一并结束。Windows 用 `taskkill /T /F`。
8. **stderr**：只用于诊断，逐行进 debug 日志，并保留脱敏后的最后 20 行。非预期退出时，把这 20 行附在 warn 日志里。

### 2.2 反向请求（CLI → Host）

| 方法                                                            | 应答                                                                                                 |
| --------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| `session/requestRuntimePreferences`                             | 按设置回答（规则同 `node.ts` 的 `resolveSessionRuntimePreferences`）                                 |
| `interaction/requestPermission`、`interaction/requestUserInput` | **永不应答**。CLI 会让它和 V4 `resolveInteraction` 赛跑，回错误会让权限或提问在用户作答前就失败      |
| `interaction/requestProviderRuntimeHeaders`                     | `{headersApplied:false, errorMessage:"Provider request auth is unavailable"}`。账号登录随 OAuth 迁移 |
| `interaction/requestOfficialMcpAuthHeaders`                     | `{ok:false, reason:"official_auth_unavailable"}`                                                     |
| `interaction/browserList` / `browserExecute`                    | `{browsers:[]}` / `backend_unavailable`                                                              |
| 其他                                                            | `-32601 Unsupported ZCode Protocol request: <method>`                                                |

除 `v4/conversation/frame` 之外的通知（遥测、CUA、legacy session 事件）都丢弃。

### 2.3 `zcode-agent` channel：连接 scope

1. **每条 RPC 连接一个 scope**，以连接序号为键，而不是以 attachmentId 为键。同一个 attachmentId 重连时，旧连接的清理不会误伤新连接。
2. **连接身份**：scope 的 `connectionId` 为 `host-rpc-<uuid>`，`clientMode` 取签发 capability 时登记的值。UI 自报的这两个字段一律忽略。
3. **握手**：先 `helloConversationV4`，再 `initializeConversationV4`（严格校验 clientHello）。握手完成之前，订阅、resync、命令、查询、行分页和附件调用都回 `fault.connection.handshakeRequired`。命令的 `envelope.clientId` 必须等于握手时绑定的 clientId，否则回 `fault.command.clientMismatch`。
4. **写门禁**：命令和附件上传要求至少有一个可选模型，否则回 `ZCODE_AGENT_PROVIDER_NOT_READY`（`data` 同 Node）。只读路径没有这个门禁。
5. **sessions-index** 订阅只用已有进程（`existing-only`），没有进程时回 `ZCODE_AGENT_RUNTIME_UNAVAILABLE`。
6. **订阅所有权**是 `(kind, workspaceKey, topic, subscriptionId, 进程代)`。帧只发给拥有它的连接。
   - ACK 之前到达的同 topic 帧按 pending 暂存（上限 1024 帧或 32 MiB，超出时订阅失败并反向退订），ACK 后按到达顺序放行与 ACK 同一 subscriptionId 的帧。
   - 同一 topic 的新订阅取代旧订阅。
7. **连接关闭**：先对发过请求的 workspace 发 `v4/connection/flow {state:"closed"}`，再尽力退订本连接拥有的订阅。订阅在 ACK 前遇到连接关闭时，收到 ACK 后立即反向退订。

### 2.4 有意与 Node 不同之处

| Node 行为                                                                    | Rust Host                                     | 理由                                                                         |
| ---------------------------------------------------------------------------- | --------------------------------------------- | ---------------------------------------------------------------------------- |
| scope 监听 lifecycle `unavailable`，作废该 workspace 的所有权                | 所有权带进程代；帧、退订、resync 都只认同一代 | 结果相同：新进程即便复用 subscriptionId 也不会串帧；而且没有跨事件的竞态窗口 |
| 退订时按 `runtimePolicy` 取 client，可能为退订拉起新进程，随后发现路由已失效 | 退订只发给仍在运行的同代进程                  | 不为退订启动 CLI                                                             |
| 订阅、建会话前同步 Account Config（`provider/updateAccountConfig`）          | 不发送                                        | 未登录时没有账号 provider；随 OAuth 迁移                                     |
| spawn 时注入 `ZCODE_BASE_URL`                                                | 不注入                                        | production 下注入值等于 CLI 从环境推导的值；设置页自定义端点随账号迁移       |
| `attachmentPreviewSourceV4` 授权本地媒体路径                                 | 固定回 `{kind:"chunked"}`                     | UI 回退到分块读取；media-preview 在 R3                                       |
| 订阅 ACK 合并 `openTiming` 诊断                                              | 原样转发 CLI ACK                              | 只是诊断字段                                                                 |
| 冷订阅从 tasks-index 带 `resumeThoughtLevel`                                 | 随 tasks-index repo 实现                      | 见第 6 节                                                                    |
| plugin-management、mcp-status 两个 lane                                      | 未实现，相关成员回 stub                       | 不在核心链路                                                                 |
| 流控 `saturated` / `drained`                                                 | 只在断开时发 `closed`                         | WebSocket 没有 MessagePort 的流控边带；背压另行评估                          |

## 3. 状态所有者

| 状态                                                   | 唯一所有者                                        |
| ------------------------------------------------------ | ------------------------------------------------- |
| CLI 进程、进程代、lifecycle 事件                       | `AgentRuntime`（`services/src/agent/runtime.rs`） |
| 请求 id、挂起请求、存储门禁                            | 每个进程的 `ProtocolClient`（`agent/client.rs`）  |
| 握手状态、订阅所有权、pending 暂存、路由               | 每条连接的 `Scope`（`agent/service.rs`）          |
| 会话、命令、帧内容                                     | CLI（Host 只转发，不解析 `frame`）                |
| app 运行时偏好（最新一次 `syncAppRuntimePreferences`） | `AgentRuntime`；新进程启动时回放                  |

## 4. 事件顺序

```text
UI                      Scope                         AgentRuntime / CLI
│ listen onDynamicConversationFrame(target)
│──────────────────────►│ 建 route（每个 (kind, key) 订阅一次 runtime.frames）
│ sendConversationCommandV4(createSession)
│──────────────────────►│ 握手 + clientId 检查 ──────► get_ready_client：provider 门禁
│                       │                             ├─ spawn（第 N 代）
│                       │                             ├─ N>1：onAgentRuntimeRestarted
│                       │                             ├─ onAgentRuntimeLifecycle available
│                       │                             └─ 存储门禁 ready 后发 v4/command
│◄──────────────────────│◄──────────────── CommandAck{result.sessionId}
│ subscribeConversationV4(sessionId)
│──────────────────────►│ begin_pending ─────────────► v4/conversation/subscribe
│                       │◄──────────────── ACK 响应行（CLI 先写响应）
│                       │ remember：登记所有权，放行暂存帧
│                       │◄──────────────── frame(initial) → route：已拥有 → 发给 listen
│◄──────────────────────│ EventFire(原始 params)
│                       │◄──────────────── frame(online)…
│                                              进程退出 / 超时 / 协议错误
│◄─────────────────────────────────────────── onAgentRuntimeLifecycle unavailable
│ （UI 退避重连，subscribe 懒启动第 N+1 代）
```

## 5. 验收

- `cargo test -p zcode-host-services --test agent_cli`：用 Rust CLI 的 release 构建和本地 mock 的 OpenAI 兼容端点，走完以下流程：握手、`existing-only` 的 dormant 错误、createSession（带首条输入）、订阅、收到流式回复、订阅 sessions-index、拒绝不属于本连接的 resync、断开后回收进程。整个过程不需要真实凭据，也不读写开发者的 `~/.zcode`。
- 单元测试覆盖：NDJSON 分行（含 U+2028）、strict 信封、存储门禁、错误码透传、进程树升级回收、stderr 脱敏、帧路由、clientHello 严格校验、代理环境、运行时偏好。
- 真实 App：`ZCODE_DESKTOP_HOST=rust pnpm dev:desktop`，在临时 HOME 下配置 BYOK provider，手工走一遍核心流程（见第 6 节，侧栏依赖 tasks-index）。

## 6. 剩余工作（本阶段内）

1. **tasks-index.sqlite**：同一文件、同一 schema 与迁移账本（checksum 必须一致），由 Host 在 `InitLocal` 时迁移并报告 `preparing_host_storage`。repo 已实现（第 7 节），尚未接入 `install()`。
2. **index syncer**：Host 自己订阅每个 workspace 的 sessions-index，重组分片，把会话写进 tasks-index，并发出 `workspace_task_list_changed`。
3. **zcode-task**：`listTasks`、`listPinnedTaskIds`、`listPinnedTasks`、`listArchivedTasks`、`listDeletedTaskIds`、`setTaskUnread`、`onDynamicWorkspaceEvent`、`releaseWorkspacePreparation`、`restartWorkspaceProcess`。
4. **window-controller**：基于同一份行数据的投影。
5. **zcode-session**：`readWorkspacePresentation`（经 `runtime/capabilities` 和 `workspace/readPresentation`）、`initializeWorkspace`。
6. **启动预热**：对 `agentWarmupTargets` 调用 `initialize`，让 `existing-only` 的 sessions-index 一启动就能连上。

## 7. tasks-index repo

模块：`apps/zcode-host/crates/services/src/tasks_index/`（`TasksIndex`）。移植自 `packages/services/src/session/taskIndexRepo.ts` 与 `tasksDatabase/`，只覆盖侧栏需要的部分：启动迁移与修复、列表读取、snapshot 合并、agent 补丁、unread、右键菜单操作。分组管理、搜索、归档清理、automation 与闲时任务的业务读写不在本节。

### 7.1 兼容规则

文件 `{dataBaseDir}/.zcode/v2/tasks-index.sqlite` 与 Node Host、其他窗口的 Host 共用，Rust 必须是完全兼容的写者：

1. **账本**：`tasks_schema_migration(id, checksum, time_applied)`，迁移 `0001_adopt_task_schema`、`0002_provider_selection`、`0003_official_glm_selection`。checksum 是 `sha256(JSON.stringify(checksumInput))`，Rust 不另算，取自 TS 生成的 fixture。已应用的 checksum 不一致时以 `Task database migration checksum mismatch: <id>` 失败（`checksum_mismatch`），不修改账本。
2. **schema**：Rust 执行的 DDL 与 TS 逐字节相同，新库的 `sqlite_master`（type、name、tbl_name、sql）与 TS 准备的库一致；旧库补列、重复闲时绑定时不建唯一索引的规则相同。
3. **启动**：mkdir -p → `busy_timeout=25`、`foreign_keys=ON` → `journal_mode=WAL` → `synchronous=NORMAL` → 只读预检 → `BEGIN IMMEDIATE` → 迁移 → COMMIT。WAL、预检、BEGIN 遇 `SQLITE_BUSY` 时先报 `waiting_for_lock`，每 100 ms 重试，最长 60 min（`lock_timeout`）。之后执行 TS 启动 Worker 里的一次性修复：闲时标记回填、闲时系统分组归属、已删除 task 的分组引用清理、`awaiting_approval` 回到 `running`。业务连接 `busy_timeout=5000`、`foreign_keys=ON`、WAL、`synchronous=NORMAL`。
4. **行语义**：`writeRecord` 的 `ON CONFLICT` 语句（含 `unread_at` 的 CASE 与 `last_unread_at` 的 MAX）原样移植；读出的 meta 与 TS `rowToMeta` 相同，包括 zod 的剥离、trim、默认值与键序，meta_json 非法时由列兜底；写入的 meta_json 按 TS 对象展开的键序序列化。

### 7.2 状态所有者与事件顺序

每个 `TasksIndex`（及其 clone）独占一个专用线程和一条连接，请求在该线程上按到达顺序串行执行，覆盖 TS `enqueueWrite` 的同行串行。跨句柄、跨进程靠 SQLite 写锁：每个读改写都在 `BEGIN IMMEDIATE` 内完成。

```text
async 调用方 ──job──► mpsc ──► tasks-index 线程（rusqlite）
     ▲                              │ BEGIN IMMEDIATE：读行 → 合并 → 写回 →（分组顺序）→ COMMIT
     └────────── oneshot 结果 ◄──────┘
其他窗口 Host / Node Host ─────► 同一文件（WAL，写锁串行）

open：checking →（waiting_for_lock）→ checking(facts) →（migrating n/total）→ committing
      → maintaining（启动修复）→ ready；任一步失败 → failed，错误带分类码、SQLite 码、迁移 id、迁移事实
```

### 7.3 有意与 TS 不同之处

| TS 行为                                                                        | Rust                                                         | 理由                                                       |
| ------------------------------------------------------------------------------ | ------------------------------------------------------------ | ---------------------------------------------------------- |
| seed、`syncTaskMeta`、`applyAgentPatch`、pin/归档/重命名在自动提交模式下读后写 | 全部在 `BEGIN IMMEDIATE` 内                                  | 多个 Host 交错写同一行时不丢更新；结果与串行执行的 TS 相同 |
| 行缺失或已删除时 `updateTaskState` / `clearTaskUnreadIfMatches` 抛错           | 返回 `None`                                                  | 由调用方决定是否 seed 后重试（同 TS adapter）              |
| 入参 meta 可以带 `undefined`                                                   | JSON 顶层 `null`（`target` 除外）按 undefined 处理           | JSON 没有 undefined；避免写出 TS 读取时整体判非法的 meta   |
| `applyAgentPatch` 的 `status` 用 `??`，传 undefined 等于保留                   | `status: null` 清除该字段；`title`、`updatedAt` 传 null 报错 | 补丁统一为“键缺失保留、null 清除”                          |
| 启动修复多数在自动提交模式下逐条执行                                           | 每项一个 IMMEDIATE 事务                                      | 结果相同，且原子                                           |

### 7.4 验收

- `pnpm exec tsx scripts/rust-host/tasks-index-fixtures.ts [--check]`：用真实 TS 实现（迁移定义、`prepareTasksIndexStorage`、`TaskIndexRepo`）在固定时钟下执行 `tasks-index-scenarios.ts`，生成 `fixtures/migrations.json`（运行时嵌入）与 `fixtures/parity.json`（测试嵌入）。`scripts/rust-host/test.mjs` 先跑 `--check`。
- `cargo test -p zcode-host-services tasks_index`：
  - Rust DDL 与 TS checksum 输入逐字节相同，sha256 与 fixture 一致；
  - 5 个启动场景（新库、旧库升级、0002、0003、启动修复）的进度序列与表内容与 TS 相同；
  - 5 组操作场景（rowToMeta、合并、unread、右键菜单、分组顺序）每一步的返回值与最终表内容与 TS 相同；
  - 打开 TS 准备的库不重新迁移；篡改 checksum 失败；锁等待与超时；两个句柄并发打开只迁移一次，并发 unread 不撞号，并发首发的分组顺序互不相同。
