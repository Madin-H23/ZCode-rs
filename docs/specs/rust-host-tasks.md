# Rust Host：tasks（zcode-task 全量、tasks-index 其余仓库、window-controller、replayable、realtime bridge）

日期：2026-10-08。上级规格：[rust-host.md](rust-host.md)；侧栏与 tasks-index 基础见 [rust-host-r2.md](rust-host-r2.md) 第 5、7 节。分支：`feat/rust-host-p-tasks`。

## 1. 目标与范围

R2 只移植了侧栏子集。本规格把 `zcode-task` channel 的其余成员、tasks-index 的其余仓库、window-controller 的归档删除与全文搜索、`web-remote-replayable` attachment 与 task realtime bridge 补齐到与 Node Host 一致：

| 部分                                 | Node 来源                                                              | Rust 模块                                                                                    |
| ------------------------------------ | ---------------------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| 分组、grouped 视图、搜索、归档、删除 | `taskIndexRepo.ts`                                                     | `tasks_index/groups.rs`、`grouped_view.rs`、`task_list.rs`、`archive.rs`                     |
| zcode-task 其余成员                  | `zcodeTaskServiceAdapter.ts`、`claude-native/*`、`modelTrajectory*.ts` | `tasks/index_members.rs`、`lifecycle.rs`、`queries.rs`、`claude_import.rs`、`task_events.rs` |
| window-controller                    | `windowHostControllerService.ts`                                       | `tasks/controller.rs`                                                                        |
| replayable attachment                | `zcodeAgentConnectionScope.ts`、`desktopRemoteSessions.ts`             | `host-core`、`agent/service.rs`、`rustDesktopHost.ts`                                        |
| task realtime bridge（U4 删除）      | `host/taskRealtimeBridge.ts`、`main/taskRealtimeBus.ts`                | 会话消息应答在 `tasks/session_messages.rs`                                                   |

automation 与闲时任务的表、调度和派发属于另一部分，不在本规格。

## 2. 产品规则

### 2.1 tasks-index：分组与 grouped 视图

与 Node 共用同一个 `tasks-index.sqlite`，SQL、事务边界、排序与错误文案逐条照搬 `taskIndexRepo.ts`。

1. **创建分组**：id 为 `task-group-<uuid>`，标题 trim 后为空时用 `New Group`，颜色缺省 `gray`（不校验）。写 `task_groups`，再把分组放到 grouped 顶层最前（`MIN(sort_order) ?? 2000` 减 1000）。不广播。
2. **改名、改色**：改名标题为空时同样用 `New Group`；颜色必须是 gray、red、orange、yellow、green、blue、purple 之一，否则报 `Task group 颜色无效`。行不存在时分别报 `Task group 不存在，无法重命名` / `Task group 不存在，无法更新颜色`。成功后对参数里的每个 `workspaceScopes` 广播一次不带 taskId 的 `task_meta_changed`。
3. **删除分组**：`BEGIN IMMEDIATE` 内删 `task_groups` 行（成员随外键级联删除）和该分组的顶层顺序；行不存在报 `Task group 不存在，无法删除`。广播同上。
4. **`listGroupedTaskViewStructure`**：先按设置自动归档（`taskAutoArchiveEnabled`，天数 `taskAutoArchiveOlderThanDays ?? 7`，每个 workspace 一次，逐个归档的任务各广播一次带 meta 的 `task_meta_changed`），再读结构：
   - groups：bootstrap 绑定到不可见 workspace 的分组不返回；
   - members、topLevelOrders：不过滤、不排序（SQLite 扫描序，与 TS 相同的 SQL）；task 顺序键只认 `JSON.stringify([workspaceKey, taskId])`，旧格式静默跳过。
5. **`applyGroupedTaskViewOrder`**：
   - 校验在事务外：scope 外的 task、不可见（缺失、已删除、已归档、已置顶）的 task、不存在的分组、同一 task 进多个分组，各自报 TS 的中文错误；provider 不符的 task 静默跳过。
   - `BEGIN IMMEDIATE`：写 bootstrap 标记 → 顶层 task 退出分组 → 按顺序 upsert 分组成员（`(i+1)*1000`）→ 删除全部分组顶层顺序和 scope 内所有 task 顶层顺序 → 按输入顺序插入顶层顺序（重复节点触发主键错误并回滚）。
   - 返回值是 legacy 的完整 grouped 视图（含成员与顶层顺序的补齐写入）。UI 忽略返回值，但写入要一致。
   - 对每个 `workspaceScopes` 广播一次 `task_meta_changed`。
6. **workspace 分组 bootstrap**：只在 grouped 视图查询里、且 bootstrap 表为空时运行。`applyGroupedTaskViewOrder` 先写标记再查询，所以在 zcode-task 现有调用路径上永远不会执行；Rust 照搬标记语义，不移植 bootstrap 本体（见第 5 节）。

### 2.2 tasks-index：搜索、归档、删除

1. **`listTaskList`（`queryTaskList`）**：
   - workspace key 去重、去空、排序；为空时直接返回空结果。
   - 条件顺序：`deleted = 0` → `workspace_key IN` → provider（zcode-task 固定 `glm`）→ kind（pinned：`pinned=1 AND archived=0`；archived：`archived=1`；其他：`pinned=0 AND archived=0`）→ 搜索时 `(LOWER(title) LIKE ? OR LOWER(searchable_text) LIKE ?)`，模式为 `%<trim 后整串小写>%`，不分词、不转义。
   - 先 `COUNT(1)`，再按 `sortBy`（created：`created_at, updated_at, task_id` 倒序；否则 `updated_at, created_at, task_id` 倒序）取 `limit`（有限正数取整）。`hasMore = total > 返回条数`。
   - 搜索时每行生成摘要：在小写正文里逐个找命中，窗口前 20、后 72 个 UTF-16 单元，前后加 `...`，空白折叠后截 140，与已有窗口重叠的跳过，最多 4 条；一条都没有时用整段正文兜底。非空时加 `searchSnippet`（第一条）与 `searchSnippets`。scope 带 `workspacePurpose` 时，行上补 `workspacePurpose`。
2. **`archiveStaleTasks`**：天数 `max(1, floor(n))`，截止时间 `now - days*86400000`。条件：未删除、未归档、未置顶、无未读、`updated_at < cutoff`、`task_status = 'completed'`；不带 provider 过滤。`BEGIN IMMEDIATE` 内只改 `archived` 列，返回更新前读出的 meta。每个归档的任务广播一次带 meta 的 `task_meta_changed`。
3. **`archiveWorkspaceTasks`**：列出该 workspace 的 glm 活跃与置顶行，逐个设为归档（每个一个事务），然后广播一次不带 taskId 的 `task_meta_changed`，最后返回归档分区。
4. **`deleteArchivedTask`**：`BEGIN IMMEDIATE` 内检查行存在、未删除、已归档，否则返回 false 且不广播；写删除墓碑并清理分组引用后提交，再广播带 meta 的 `task_deleted`。不清理 CLI 会话。
5. **`deleteArchivedTasks`**：taskIds 去重保序；为空直接返回三个空数组。逐个执行第 4 条（各自一个事务）：返回 null 记入 skipped，出错记入 failed，其余记入 deleted。只有 deleted 非空时，整批结束后广播一次不带 taskId 的 `task_deleted`。

### 2.3 window-controller

1. **搜索**：`listTaskList` 带 `search` 时，对每个 source 调用 zcode-task 的 `listTaskList`（只含该 source 的 scope，不带 limit），结果与投影行按 taskId、path、identity 对齐：有投影行时用投影 meta 与 liveStatus、activity，否则用索引 meta 并由 meta 推导 liveStatus；带回 `searchSnippets`。排序与 limit 同无搜索时。
2. **`deleteArchivedTask(s)`**：经 TaskSource 调用 zcode-task 的同名实现，再强制刷新该 source；刷新失败不改写已提交结果。批量删除 taskIds 为空时直接返回三个空数组。
3. 排序的 taskId 比较按 `localeCompare` 的常见结果（ASCII 下即字典序）。

### 2.4 zcode-task：生命周期、配置与交互

Node 经 IZCodeAgentService 的 legacy 方法转发，Rust 直接请求 CLI（`tasks/agent_calls.rs`）。写路径（Node `getClient`）走 `get_ready_client` 的 provider 门禁，只读路径（`getReadOnlyClient`）用 `get_client`。

1. **V4 命令**：信封键序 `commandId, clientId, sessionId, baseRevision?, type, payload, issuedAt`；clientId 是进程内固定的 `host-services-<uuidv7>`。ACK 为 accepted、duplicate、noop 视为成功，否则报 `ZCODE_V4_COMMAND_REJECTED`，文案 `v4 command <type> <status>[ (<reasonCode>)][: <message>] — <context>`。CAS 命令从 baseRevision 0 起，stale 时取 `revisionAtDecision` 重试，最多 4 次。
2. **createTask**：三条路径按顺序判定。
   - 有 `draftSessionId` 且没有 MCP：读 draft 会话，模型或思考档位不同就先 `session/setModel` / `session/setThoughtLevel`；会话已失效（`Session not found|is not active:`）时降级为新建。
   - `v4Create`（Bot）：V4 `createSession`，从 ACK 的 `result.sessionId` 读快照；缺失时报 `v4 createSession accepted without sessionId result`。
   - 其余（automation、闲时）：`session/create`；automation 或 `deferPersistenceUntilFirstPrompt` 时 `persistence: "deferred"`，automation 关闭标题生成。
   - 之后：写索引（带 cronAutomationId / offPeakTaskId）→ grouped 顶层最前 → `workspace_config_options_update` → 带 meta 的 `task_created`。返回 meta 加 `initialSlashCommands`。
3. **sendPrompt**：target 取登记表（见第 3 节），记下 active input = traceId。有附件走 `session/send`，否则 V4 `sendText`（`heldQueueDisposition: keepQueueAndSend`，`toolDenylist` 改名为 `toolDisallowlist`）。automation 轮追加 Cron 写工具黑名单，闲时轮追加 `OffPeakCreate`。失败时清除 active input。
4. **host command 队列**：enqueue / promote / cancel 只改内存队列；带 `ownerRunId` 时校验 active input（`NO_ACTIVE_TASK_OWNER`、`STALE_TASK_OWNER_COMMAND`），promote 找不到命令报 `OWNER_COMMAND_FAILED`。drain 每个 taskKey 串行：有 active input 或 running 命令时不发，否则取第一条 accepted 改为 running 并发送；发送失败记 failed。cancel 移除命令时在 `onDynamicTaskEvent` / `onDynamicStreamEvent` 上发 `task_snapshot_updated`。
5. **终态与 ready**：syncer 观察到非终态 → 终态时先发会话信号，再写库。lifecycle 收到信号后：发 `onDynamicTaskTerminalOutcome {taskId, inputId?, outcome}`（inputId 取此刻的 active input）→ 收口 running 命令、清 active input → 发 `onDynamicTaskReady {taskId, reason}` → drain 下一条。
6. **resumeTask**：缺少显式 thoughtLevel 时从索引行取 hint；`session/resume`（模型 hint 不上线，同 Node）→ `workspace_config_options_update` → 写索引（automation / 闲时再补写归属）。不广播列表事件。
7. **closeTask**：`session/close` → 写删除墓碑（行缺失时先从 CLI 回源）→ 不带 meta 的 `task_meta_changed`。
8. **配置**：getTaskConfigOptions 经 `session/resume` 读 settings 投影成旧 configOptions。setModel 走 `session/setModel`；思考档位与 automation 配置走 V4 `switchModelConfig`（CAS）；模式走 V4 `switchCollaborationMode`，`auto` 例外走 `session/setMode`。写入后都以 resume 快照收口并写索引。未绑定模型时设置思考档位报 `请先选择模型，再设置思考档位`。
9. **交互**：stop 发 V4 `stop`；respondPermission / respondElicitation 发 V4 `resolveInteraction`；replayable 的 elicitation 应答在任务事件流上补发 `elicitation_response`。compact、goal 走 `session/compact`、`session/goal`，结束后写索引并广播 `task_status_changed`。
10. **不支持的成员**：`branchTaskFromPrompt`、`deliverSessionMessage`、`sendSessionMessageDeliveryResult` 与 Node 一样报 `ZCODE_AGENT_UNSUPPORTED_LEGACY_TASK_METHOD`。`getTaskSnapshotBody / Ref / ToolCallsSlice` 与 Node 一样固定回 null。

### 2.5 文件与查询类成员

- **getModelTrajectory**：读 `~/.zcode/cli/{debug,rollout}` 与 `<dataBaseDir>/.zcode/cli/{debug,rollout}` 下的 `model-io-<sanitized taskId>.jsonl`，每个文件最多读末尾 32 MiB（从行中间开始时丢弃残行）。只取 `type: model_io` 且 sessionId 相等的记录，按开始时间与 requestId 排序，展开 delta 消息，映射为轨迹记录，最多保留最近 `limit`（缺省 200）条。
- **getTaskNativeSessionLogFile**：`$ZCODE_LOG_DIR` 或 `~/.zcode/cli/log` 下的 `zcode-<本地日期>.jsonl`，附 `exists`。
- **getTaskSessionFilePath**：`<workspacePath>/<taskId>.zcode-session`，`exists: false`（同 Node）。
- **getTaskTokenUsage**：`v4/conversation/usage`。**getTaskMeta**：索引行。**getTaskModelSelection**：`session/read` 的当前模型或 null。

### 2.6 Claude Code 会话导入

1. **扫描**：在 `~/.claude/projects` 与 `<dataBaseDir>/.claude/projects` 下递归收集 `*.jsonl`，跳过 subagents、worktree、worktrees 目录。每个文件读前 16 条记录：侧链会话跳过；workspace 取第一条可见文本的 cwd，worktree 路径跳过；按 workspacePath 过滤时比较规范化路径。返回按修改时间倒序，`limit` 截断。
2. **导入**：sessionId trim 去重。逐个：找不到或 workspace 不符记 skipped（`session_not_found_or_workspace_mismatch`），workspace 目录不存在记 skipped（`workspace_path_missing`）。原子复制 jsonl 到 `<dataBaseDir>/.zcode/v2/agent-config/claude/<wsHash>/projects/…` → 解析全部记录（assistant 按轮次合并，去掉 synthetic 占位）→ CLI `session/create`（id `claude-import-<sha256 前 24 位>`，immediate，importedHistory）→ 写索引（migrationSource）→ 写 legacy 快照 `<dataBaseDir>/.zcode/v2/sessions/<wsHash>/<taskId>.json` → 带正文写索引并解除归档、删除 → 广播带 meta 的 `task_meta_changed`。任何一步失败记入 failed，删除本次新建的副本。

### 2.7 web-remote-replayable attachment

1. **签发**：host-core 对本地 scope 的 `desktop-continuous` 与 `web-remote-replayable` 都签发 capability；远端 scope 仍拒绝，随远端 workspace 接入。
2. **语义**：连接 scope 按签发时的 clientMode 回 hello（replayable 时 `deliveryProfile: "replayable"`，`nativeDialogs`、`localTerminal` 为 false），订阅把 `clientMode` 带给 CLI，由 CLI 做 replayable 投递（合并窗口、去掉文本增量）。命令在 replayable、带 identity 或远端目标时去掉 `ttft`。
3. **角色**：Node Host 的 attachment 全部是 `terminal-client`；`trusted-host-relay` 只出现在远端 `zcode-server`，Rust Host 不承担这个角色，因此 relay 规则（flow 控制、relay 身份、路由精确命中）不适用，`setConnectionFlowStateV4` 保持拒绝。
4. **Main 侧**：Rust Host 不能接收 MessagePort。Main 收到发往 Rust Host 的本地 `AttachServicePort` 时，签发同 clientMode 的 endpoint，再把端口与 WebSocket 逐条消息互转（`rustHostPortBridge.ts`）；任一侧关闭都会关闭另一侧，`DetachServicePort` 关闭桥，同一 attachmentId 重新 attach 时替换旧桥。

### 2.8 task realtime bridge（U4 删除）

窗口级 Host 时，InitLocal 带 `hostId` 才与 Main 的 `taskRealtimeBus` 建立实时桥（lease、stream op、owner 命令、会话消息中继），Node Host 里它的唯一使用方是远端 workspace 的 sendPrompt 镜像，送达事件与 owner 命令没有订阅者。U4 起一个 App 只有一个 Host（[rust-u4-app-host.md](rust-u4-app-host.md) §2.5），Rust Host 不再建立这座桥：

1. `init-local` 的 `hostId` / `deliveryKind` 仍可解析（Main 旧路径落地前可能携带），不再使用；Host 不发也不处理 `task-realtime-*`、`task-run-lease-*`、`task-stream-op-publish`、`task-owner-command-*`、`session-message-*`、`session-route-announce`。
2. run 的 owner 判断、stale run 防护与 owner 错误码一直在 `TaskLifecycle`（`queries.rs`），与这座桥无关，行为不变。
3. **会话消息**：远端 workspace 的 `workspace_session_message_send_requested` 在 Host 内应答（`tasks::session_messages::answer_send_request`）：结果 `{status: "failed"}`，错误文案同 Node 的 `deliverSessionMessage`（适配器不支持）；发起方同样无法转交结果（`sendSessionMessageDeliveryResult` 不支持），记一条 warn，与经 Main 绕回时一致。

## 3. 状态所有者

| 状态                                                 | 唯一所有者                                            |
| ---------------------------------------------------- | ----------------------------------------------------- |
| 分组、成员、grouped 顶层顺序、bootstrap 标记、任务行 | tasks-index.sqlite（`TasksIndex` 线程；跨进程靠写锁） |
| 会话、命令幂等、V4 revision、模型与模式              | CLI                                                   |
| task target 登记（taskId → workspace）               | `TaskContext`（内存；未登记时按 taskId 查索引兜底）   |
| active input、host command 队列、drain 串行门        | `TaskLifecycle`（内存，Host 重启清空，同 Node）       |
| 终态信号                                             | syncer（sessions-index 的 phase 迁移）                |
| 跨窗口 owner / lease、owner 命令路由、会话消息路由   | Main `taskRealtimeBus`（不变）                        |
| attachment 的 clientMode / scope                     | host-core 签发 capability 时登记；UI 自报的值一律忽略 |
| Main 发起的 MessagePort attachment 桥                | `RustHostProcess.portBridges`（按 attachmentId）      |

## 4. 事件顺序

```text
sendPrompt / drain                syncer（sessions-index）              TaskLifecycle
 │ active input = traceId          │
 │ V4 sendText ─────► CLI          │
 │                                 │ phase running → completedSuccess
 │                                 │ SessionSignal ─────────────────────► terminal {taskId, inputId, outcome}
 │                                 │ patch 状态 → task_status_changed     收口 running 命令、清 active input
 │                                 │ 回源 session/read → 写索引           ready {taskId, reason}
 │                                 │                                      drain：取下一条 accepted → running → 发送

UI（mutation）              zcode-task                     tasks-index              window-controller
 │ deleteArchivedTasks ─────► 去重，逐项 IMMEDIATE ──────► 墓碑 + 清分组引用
 │                          │ 整批结束、deleted 非空 ─────────────────────────────► task_deleted（无 taskId）
 │                          │                                                       强制重读该 source → 增量帧

Main                          RustHostProcess（Main）                Rust Host
 │ AttachServicePort + port ─► issue-attachment-endpoint(clientMode) ─► 登记 capability
 │                            │◄──────────────── url + capability
 │                            │ WebSocket(capability) ─────────────────► attach：CallContext.client_mode
 │ port ⇄ MessagePortProtocol ⇄ SocketProtocol ⇄ WebSocket ⇄ scope（hello: replayable）
 │ DetachServicePort ─────────► 关闭桥（端口与 WebSocket 一起关）
```

## 5. 有意与 Node 不同之处

| Node 行为                                                                                               | Rust Host                                               | 理由                                                                                                                                                                |
| ------------------------------------------------------------------------------------------------------- | ------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| task target 只认本进程登记过的 taskId                                                                   | 未登记时按 taskId 查 tasks-index 兜底                   | Rust 的 listTasks 等侧栏成员不登记；兜底只让更多合法调用成功，错误码不变                                                                                            |
| 内存 overlay 先于写库隐藏 / 归档                                                                        | 不维护 overlay，直接读写库                              | Rust 每次读都来自索引；结果相同                                                                                                                                     |
| 旧 CLI 的 `session/create` / `resume` / `send` 兼容重试                                                 | 不重试                                                  | Rust Host 只配 Rust CLI 或当前 TS CLI，协议是最新的                                                                                                                 |
| 发送前同步 Account Config、注入浏览器环境上下文                                                         | 不做                                                    | 账号、浏览器在其他里程碑；`createTask` / `resumeTask` 的 off-peak 与动态工作流工具面参数已与 Node 一致（[rust-host-integration.md](rust-host-integration.md) §2.4） |
| `session/compact` 单独 5 分钟超时                                                                       | 用统一的 180 s 请求超时                                 | 超时后懒重启进程；长 compact 场景随 agent 超时策略一并评估                                                                                                          |
| `getTaskSnapshot(WithEtag)`                                                                             | 仍是 stub                                               | 依赖约 2.5k 行的 legacy 消息投影（`snapshotToZCode`）；UI 不调用，调用方是 Bot（另一部分）                                                                          |
| `setAssistantMessageFeedback`（读上一行的快照，改内存副本的 feedback 后返回，不落盘）                   | 仍是 stub                                               | 只是包了一层上一行有意不实现的 legacy 快照；UI 与其他调用方都不调用（[rust-host-setting.md](rust-host-setting.md)）                                                 |
| `onDynamicTaskEvent` / `onDynamicStreamEvent` 订阅 legacy `session/subscribe` 并投影为 ZCodeStreamEvent | 只转发 Host 本地产生的事件（命令取消、replayable 应答） | 同上；UI 不订阅，Bot 与远端镜像属于其他部分                                                                                                                         |
| Claude 导入的会话在 CLI 丢失时由 resume 回填历史（importedClaudeHistoryRepair）                         | 不回填                                                  | 导入时 `persistence: immediate`，会话由 CLI 持久化；回填只服务旧版本遗留数据                                                                                        |
| legacy 快照 JSON 的键序取 zod schema 顺序                                                               | 键序取写入顺序                                          | 读方（Node / Rust）都按字段解析，不比较字节                                                                                                                         |
| `localeCompare` 排序                                                                                    | `support::collate` 近似（ASCII 内一致）                 | 只影响并列项                                                                                                                                                        |
| 本地日期按系统时区                                                                                      | unix 用 `localtime_r`；其他平台按 UTC                   | 只影响 `getTaskNativeSessionLogFile` 的文件名                                                                                                                       |
| `applyGroupedTaskViewOrder` 之后的 workspace 分组 bootstrap                                             | 不移植                                                  | 同一次调用先写了标记，这条路径上它不会执行                                                                                                                          |

## 6. 验收

- `pnpm exec tsx scripts/rust-host/tasks-index-groups-fixtures.ts [--check]`：用真实 TS `TaskIndexRepo` 在固定时钟下生成 `tasks_index/fixtures/parity-groups.json`（分组 CRUD、grouped 结构与排序、搜索与摘要、自动归档、归档删除；每步返回值或错误文案，以及最终表内容）。`cargo test -p zcode-host-services tasks_index::groups_tests` 逐项复现。`scripts/rust-host/test.mjs` 先跑 `--check`。
- `cargo test -p zcode-host-services --lib tasks::`：搜索 / 归档删除 / 分组事件与 controller 全文搜索、自动归档、V4 信封与 ACK、configOptions 与模型选择字符串、轨迹解析、Claude 解析规则、会话消息在 Host 内应答（`tasks::session_messages`）。
- `ZCODE_TEST_CLI_RUST_BINARY=… cargo test -p zcode-host-services --test tasks_cli`：真实 Rust CLI + 本地 mock 模型：
  - initialize → createTask（legacy）→ sendPrompt → terminal 带 inputId、ready → configOptions、切换权限模式（V4 CAS）、读模型、token 用量 → enqueue 在 ready 边界 drain → resume → close；
  - Claude 扫描（侧链、subagents 目录被排除）→ 导入（去重、缺失记 skipped）→ 广播 → 全文搜索命中 → resume。
- `cargo test -p zcode-host --test end_to_end`：replayable 本地 attachment 可签发，hello 报告 replayable 与受限能力。
- `ZCODE_RUST_HOST_BINARY=… node --import tsx --test packages/desktop/test/rustHostPortBridge.test.ts`：MessagePort 经 Main 的桥连到真实 Rust Host 的 replayable attachment，关闭端口即关闭连接。
- 桌面 E2E（`node scripts/rust-host/e2e-desktop.mjs --scenario <name>`，隔离 Electron + Rust Host，runtime 缺省在 Host 进程内）：
  - `tasks-search`：命令中心搜索只在回复正文里出现的词，结果带正文摘要，可点击；
  - `tasks-archive`：右键归档 → 归档列表 → 删除全部已归档任务，tasks-index 写入墓碑，列表刷新。
