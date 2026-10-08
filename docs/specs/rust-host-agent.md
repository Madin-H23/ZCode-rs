# Rust Host：agent 子系统补齐

日期：2026-10-08。上级规格：[rust-host.md](rust-host.md)；核心聊天的设计见 [rust-host-r2.md](rust-host-r2.md)。分支：`feat/rust-host-p-agent`。

## 1. 目标与范围

R2 只实现了 `zcode-agent` 的 V4 核心成员。本规格补齐 agent 子系统其余部分，使桌面端在 Rust Host 与 Node Host 上行为一致：

- `zcode-agent` 除 automation 以外的全部成员（automation 成员、automation 与闲时任务的反向请求归 automation 规格）；
- `zcode-session` 的全部成员；
- `plugin-management` channel（插件管理 lane 的门面）；
- 三个 CLI lane：chat、plugin-management、mcp-status；
- CLI 通知的转发（遥测、TTFT、CUA 观察、资源样本、MCP 遥测、插件进度）与 connection scope 规则；
- 浏览器控制反向请求经 Main 的 CDP 执行器；
- 启动期的会话库准备（`--prepare-storage`），上报 `preparing_session_storage`；
- CLI 进程生命周期与 stderr 进程诊断上报 Main。

不在本规格：automation 成员与反向请求（automation 规格）、`attachmentPreviewSourceV4` 本地路径（workspace 规格）、Account Config 同步与 provider headers（account 规格）、远端 workspace 与手机 relay（R6）。

## 2. 产品规则

### 2.1 三个 lane

| lane               | 进程 key                         | 用途                                                                         | 超时与回收                                                                              |
| ------------------ | -------------------------------- | ---------------------------------------------------------------------------- | --------------------------------------------------------------------------------------- |
| chat               | `workspaceIdentity \|\| path`    | 会话、V4 订阅与命令、技能与插件引用目录、已保存工作流、用量、生成文本等      | 请求 180 s，超时回收；不做空闲回收                                                      |
| plugin-management  | `<dataBaseDir>/.zcode/plugin-workspace` | 插件列表、市场、安装卸载、配置、启停、推荐插件解析；全局工作流的兜底载体 | 变更类请求 5 min，`plugins/list` 180 s（超时回收后重试一次），`cancelOperation` 5 s   |
| mcp-status         | 同上                             | 仅 `mcp/list`（MCP 状态探测）                                                | 请求 180 s；无在途请求且存储已就绪后空闲 5 min 回收整棵进程树                           |

1. 三个 lane 共用命令解析、spawn 环境、扩展点（反向请求、环境贡献者）与通知分发；各自独立的进程表、进程代、请求队列与 watchdog。
2. plugin-management 与 mcp-status 的工作目录在首次使用时 `mkdir -p`；真实 workspace 只作为协议参数传给 CLI。
3. 只有 chat lane 发布 `onAgentRuntimeLifecycle` / `onAgentRuntimeRestarted`，也只有 chat lane 回放 app 运行时偏好与功能门禁策略。
4. runtime identity：chat 与 plugin-management 为 `<key>:<gen>:<pid>`；mcp-status 为 `<key>:<gen>:<pid>:mcp-status`，并带 `lane` 字段（同 Node：plugin-management 的进程管理器不设 lane）。
5. 资源样本的 lane 标签按所属 lane 打：`chat`、`plugin`、`mcp-status`。

### 2.2 `zcode-agent` 成员

“只读 client”指 `get_client(start-if-needed)`（不过 provider 门禁）；“写 client”指 `get_ready_client`（provider/model 门禁）。

| 成员                                                                                   | 规则                                                                                                                                                                                                 |
| -------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `prepareStorage`                                                                       | 启动 chat 进程（不过门禁、不回放偏好），等存储门禁进入 ready；failed 时以 `SQLite startup failed: <code>` 失败                                                                                       |
| `getStorageStartupState`                                                               | 无进程时 `null`，否则 `{generation, state}`（state 为最近一帧 `startup/storageState` 或 `null`）                                                                                                     |
| `onDynamicStorageStartupState(target)`                                                 | 该 workspace 每次新进程启动发一次 `{generation, state:null}`，之后每个被接受的存储帧发一次                                                                                                         |
| `getWorkspaceRuntimeIdentity`                                                          | 只观察已有进程；没有进程时回 `ZCODE_AGENT_RUNTIME_UNAVAILABLE`                                                                                                                                       |
| `createSession` / `resumeSession`                                                      | 写 client；先执行使用前钩子（`CreateSession` / `SessionResume`）；参数构造与 Node 相同，`-32602` 且只含已知可选键的 `unrecognized_keys` 时省略这些键重试一次；create 因 thoughtLevel 被省略时补发 `session/setThoughtLevel` |
| `listSessions` / `readSession`                                                         | 按 `runtimePolicy` 取 client；`readSession` 非 existing-only 时执行 `SessionRead` 钩子                                                                                                               |
| `readSessionMessages` / `readSessionEvents` / `listSessionSubagents`                   | 只读 client；返回 CLI 结果的 `messages` / `events` / 整个结果；`endedLimit` 缺省 20                                                                                                                 |
| `readSessionDebug`                                                                     | existing-only                                                                                                                                                                                        |
| `readWorkspacePresentation`                                                            | 与 `zcode-session` 同一实现                                                                                                                                                                          |
| `grantWorkspaceHookTrust`                                                              | 只读 client，`workspace/hooks/trustGrant`                                                                                                                                                           |
| `sendPrompt`                                                                           | 写 client；未带 `browserAmbientContext` 时收集浏览器环境（2.5）；兼容重试同 create                                                                                                                  |
| `compactSession`（ACK 超时 5 min）/ `goalSession` / `closeSession` / `setModel` / `setThoughtLevel` / `setMode` | 写 client；`closeSession` 返回 `closed ?? true`                                                                                                                             |
| `respondSessionRuntimePreferences` / `onDynamicSessionRuntimePreferencesRequest`       | 本地 Host 的偏好权威是 local（2.4 节反向请求直接作答），永远没有待转发请求：事件不触发，respond 回 `ZCode session runtime preferences request not found: <id>`                                       |
| `onDynamicSessionEvent`                                                                | legacy 订阅面：见 2.6                                                                                                                                                                                |
| `listMcpServerStatuses`                                                                | mcp-status lane；`mode:"status"` 被旧 CLI 拒绝时回 `ZCODE_AGENT_MCP_STATUS_MODE_UNSUPPORTED`；其余未识别键省略重试；`mcp/list` 超时后重试一次                                                         |
| 插件管理成员（18 个，与 `plugin-management` channel 相同）                              | plugin-management lane；参数逐字段同 Node（`undefined` 不发）；`getPluginReferenceCatalog` 例外：走 chat lane，先 `plugins/referenceCatalogWithCategory`，`-32601` 时退回 `plugins/referenceCatalog` |
| `onDynamicPluginOperationProgress(operationId)`                                        | 来自任意 lane 的 `plugins/operationProgress` 通知按 operationId 分发                                                                                                                                |
| `getSkillReferenceCatalog`                                                             | chat lane，`skills/referenceCatalog`                                                                                                                                                                 |
| 已保存工作流（list/get/updateMeta/delete/runs）                                        | 带 workspacePath 时走该 workspace 的只读 client；只给 `scope:"global"` 时复用任一本地 chat 进程，没有则用 plugin-management lane；`scope` 只在有值时下推                                            |
| `moveSavedWorkflow`                                                                    | 只读 client，`workflows/move`                                                                                                                                                                        |
| `collectLocalRuntimeChildProcesses`                                                    | 枚举三个 lane 的存活、存储已就绪进程，各发 `process/childProcesses`（800 ms，观察型：超时不回收），失败时该进程 children 为空                                                                        |
| `getAppUsageStats`                                                                     | 任一已运行的 chat 进程上 `v4/usage/stats`；没有时回 `no_active_workspace`                                                                                                                           |
| `getTaskTokenUsage`                                                                    | 只读 client，`v4/conversation/usage`                                                                                                                                                                 |
| `generateWorkspaceText`                                                                | 写 client + `GenerateText` 钩子；`requestTimeoutMs` 生效；调用被取消（RPC promise cancel）时发 `workspace/cancelGenerateText {operationId}`（5 s，超时不回收）                                      |
| `testModelConnectivity`                                                                | 写 client + `TestModelConnectivity` 钩子；`provider/testModelConnectivity`                                                                                                                          |
| `conversationWorkflowRun{Events,Artifacts,ArtifactData,ArtifactRead,Workspace,NodeResult}V4` | 只读 client 透传，可选字段缺省不发                                                                                                                                                             |
| `backgroundBashOutputV4`                                                               | 握手后；existing-only；`-32601` → `{kind:"unsupported"}`，runtime 不可用 → `{kind:"unavailable"}`                                                                                                   |
| `conversationAttachmentReadV4` / `conversationAttachmentStatV4`                        | 握手后；只读 client 透传                                                                                                                                                                             |
| workspace-config topic                                                                 | 与 sessions-index 同构：`subscribeWorkspaceConfigV4` 按 `runtimePolicy` 取 client，topic `workspace-config/<key>`；unsubscribe / resync / `onDynamicWorkspaceConfigFrame` 走同一套所有权规则         |
| `hasActiveCuaOperationTurn`                                                            | `false`（CUA 操作追踪不在本规格，见第 5 节）                                                                                                                                                         |
| `disposeWorkspace` / `disposeAll`                                                      | 回收该 workspace 的 chat 进程 / 回收全部 lane                                                                                                                                                        |

### 2.3 V4 命令信封

`sendConversationCommandV4` 在 R2 的门禁之后、发往 CLI 之前：

1. `createSession`：功能门禁（2.7）打开时在 payload 写入 `offPeakToolEnabled: true` / `dynamicWorkflowEnabled: true`，关闭时不写字段。
2. `sendText`：payload 带 `automationId` 时把 `CronCreate`、`CronUpdate`、`CronDelete` 并入 `toolDisallowlist`；带 `offPeakTaskId` 时并入 `OffPeakCreate`。不覆盖已有值、去重、保持首次出现的顺序。
3. 远端 workspace（有 identity 或 remoteSessionId）去掉信封顶层的 `ttft`。
4. `sendText` 带 `sessionId` 时收集浏览器环境，有结果则写入 `payload.browserAmbientContext`。

### 2.4 反向请求

在 R2 表格基础上：

| 方法                          | 应答                                                                                                                                                      |
| ----------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `interaction/browserList`     | 参数非对象 → `-32602`；否则 `{browsers:[IAB 描述]}`（2.5）                                                                                                |
| `interaction/browserExecute`  | 参数缺 `sessionId` / `command` → `-32602`；否则经 Main 执行，结果原样回给 CLI；内部失败回 `{ok:false,error:{code:"execution_error",message},elapsedMs:0}` |

### 2.5 浏览器控制（Host ↔ Main）

Main 已有 CDP 执行器（`desktopHostProcess.ts` 处理 `browser-execute-request`，回 `browser-execute-result`）；Rust Host 走同一组控制消息，Main 无需改动。

1. **后端描述**：每个 Host 进程一个 IAB 描述，`id = iab:<uuid>`，`generation` = Host 启动时的毫秒时间戳，其余字段与 `browserControlMainBridge.ts` 逐字相同。
2. **执行**：
   - `browserId` 不等于本 Host 的 id，或 `browserGeneration` 不等于本代 → `backend_unavailable`（不发给 Main）；
   - `requestId` 缺省时生成；同一 requestId 已在途 → `duplicate_request_id`，`sideEffect:"none"`；
   - 超时 30 s；`playwrightWaitForTimeout` 为 `command.timeoutMs + 2 s`，`playwright` 为 `action.timeoutMs`（缺省 30 s）+ 2 s；
   - 超时：向 Main 发同 scope 的 `{method:"cancelRequest", requestId}`，并回 `timeout`，`sideEffect` 按命令是否可能有副作用取 `uncertain` / `none`；
   - 结果按 requestId 关联，迟到结果丢弃；
   - 录制完成且原命令带 `outputPath`（`recordingStatus`）：把 Main 的临时录制文件复制到 workspace 下的 `outputPath`（见第 5 节）；未带 `outputPath` 时去掉结果里的 `recording.artifact`。
3. **环境收集**（`collectBrowserAmbientContext`）：list → 找 `type:"iab"` 的后端 → 并发执行 `list`、`listUserTabs` → 合并 tab id；没有 tab 时不写；`currentUrl` 只接受 about/http/https，去掉用户名密码，长度 ≤ 4096。任何失败都只返回空，不阻断发送。
4. Host 退出时在途请求以 `backend_unavailable`、`sideEffect:"uncertain"` 结束。

### 2.6 legacy 会话事件（`onDynamicSessionEvent`）

V4 界面不再使用，但契约仍暴露；语义同 Node：

1. 每个 `(workspaceKey, sessionId)` 一个共享事件源。来源：任意 chat 进程的 `session/event`（按 `zcodeSessionEventSchema` 的必需字段校验）、`state.updated`（有 sessionId 发给该会话，否则发给该 workspace 的全部会话），以及 `interaction/requestPermission` / `interaction/requestUserInput` 反向请求（同一业务 requestId 只发一次 `permission.request` / `userInput.request`；Host 仍不应答，见 R2）。
2. `seq <= 0` 的旧事件按 eventId 补号（单调递增，最多记 10000 个）；同一 eventId 的 live 事件只投递一次。
3. 每个监听者：先暂存 live 事件，`session/subscribe {sessionId, deliveryKind, afterSeq, includeSnapshot}` 成功后，先投递 replay（按 seq、timestamp 排序，只给自己），再投递 snapshot，再放行暂存事件；按 eventId 去重。
4. 订阅失败按 500 ms × 2ⁿ（上限 5 s）重试，共 8 次；放弃时 warn 并放行暂存事件。
5. `eventCoalescing.mode = "background-summary"`：text/reasoning/tool-input delta 按 key 合并 delta，tool progress 与 streamRecovery 保留最新；其他事件先冲刷再立即投递；间隔缺省 1.5 s，最多 96 项；监听者取消时先冲刷。

### 2.7 功能门禁与 workspace 策略

Node 的两个门禁在 Rust Host 由扩展点提供（`Extensions::set_off_peak_tool_gate`、`set_dynamic_workflow_gate`），由拥有对应能力的 service 登记；未登记等于关闭（与 Node 未注入 options 相同）：

1. **闲时工具**：门禁存在且 workspace 是本地（无 remoteSessionId，identity 不是远端 identity）时开启。
2. **动态工作流**：门禁在本 Host 进程内只判定一次并缓存；判定失败按关闭处理且不重试。
3. 开启时：chat 进程交给调用方之前推送 `workspace/updateOffPeakToolPolicy` / `workspace/updateDynamicWorkflowPolicy {workspace, enabled:true}`（`-32601` 静默，其余失败 warn，不阻断就绪）；legacy `session/create`、`session/resume` 带 `offPeakToolEnabled` / `dynamicWorkflowEnabled: true`；V4 `createSession` 见 2.3。关闭时什么都不发。

### 2.8 app 运行时偏好（`syncAppRuntimePreferences`）

1. 归一化：`modelIoFullRetentionEnabled === true`，其余字段原样。记住最新值并带版本号。
2. 对每个已运行的 chat 进程：在该 workspace 的偏好锁内，推送**当时最新**的偏好：`workspace/updateInteractionPreferences {askUserQuestionAutoResolutionEnabled}`，再 `workspace/updateModelIoPreferences {fullRetentionEnabled}`（`-32601` 降级忽略）。全部完成后返回；任一失败则调用失败。
3. 不为同步拉起进程。新进程（以及之前同步失败的进程）在交给调用方之前补推最新偏好，直到已推版本等于最新版本；补推失败则本次取 client 失败，进程保留，下次再试。
4. 同一 workspace 的推送串行，后推的永远是最新值，因此并发调用不会让旧值覆盖新值。

### 2.9 CLI 通知

| 通知                             | 处理                                                                                                                                                                        |
| -------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `v4/conversation/frame`          | R2 路由                                                                                                                                                                     |
| `process/resourceSample`         | 补 lane 标签后发 Main `agent-resource-sample {runtimeSurface:"local", sample}`                                                                                              |
| `process/mcpTelemetry`           | Main `mcp-telemetry {runtimeSurface:"local", event}`                                                                                                                        |
| `process/mcpResourceSamples`     | Main `mcp-resource-samples {runtimeSurface:"local", samples}`                                                                                                               |
| `process/toolExecResource`       | Main `tool-exec-resource {runtimeSurface:"local", sample}`                                                                                                                  |
| `plugins/operationProgress`      | `onDynamicPluginOperationProgress(operationId)`                                                                                                                              |
| `v4/telemetry/local-ttft`        | 仅本地 workspace（无 identity、无 remoteSessionId）→ `onDynamicLocalTtftFacts(target)`                                                                                      |
| `v4/telemetry/event`             | `onDynamicConversationTelemetryFact(target)`                                                                                                                                |
| `v4/cua/permission-observation`  | 仅本地 workspace → `onDynamicCuaPermissionObservation`，附 `workspacePath` / `workspaceIdentity`                                                                           |
| `session/event`、`state.updated` | 2.6                                                                                                                                                                         |
| 其他                             | debug 日志后丢弃                                                                                                                                                            |

Main 侧负责这些消息的 schema 校验（`hostResponseMessageSchema`，不合法则丢弃）；发给 Renderer 的三类事实由 Host 校验（见下）。

**connection scope 规则**（`zcodeAgentConnectionScope.ts`）：Rust Host 的连接都是 terminal-client，没有 trusted-host-relay。

- `onDynamicLocalTtftFacts`：连接是 desktop-continuous 且目标是本地 workspace 才订阅，否则是永不触发的空流；
- `onDynamicConversationTelemetryFact`：desktop-continuous 才订阅；**握手前也允许**（不发协议请求）；
- `onDynamicCuaPermissionObservation`：desktop-continuous 才订阅；
- `onDynamicProcessResourceSample`、`onDynamicMcpTelemetry`、`onDynamicMcpResourceSamples`、`onDynamicToolExecResource`：只给 trusted relay，Renderer 订阅得到空流；
- 以上在连接关闭后订阅回 `fault.connection.closed`（与 Node 的 `assertOpen` 相同；后两个 resource 事件同 Node 回空流）。

**严格校验**：Node 对发给 Renderer 的 TTFT、telemetry fact、CUA 观察用 strict zod schema，未知字段整条丢弃并 warn。Rust 用同样的键白名单与类型检查（见 `agent/notifications.rs`），不合法整条丢弃。

### 2.10 CLI 进程生命周期与诊断

chat 与 mcp-status lane（Node 只给这两个 process manager 注入了 reporter）向 Main 报告：

| 时机                                  | 消息                                                                                                                         |
| ------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| spawn 成功                            | `agent-process-spawned {pid, provider:"glm", workspacePath, command, args, startedAt, runtimeGeneration, runtimeInstanceId, lane?}` |
| 首次通过 provider 门禁（每进程一次）  | `agent-process-ready {…, readyAt, startupDurationMs}`                                                                        |
| spawn 失败                            | `agent-process-error {pid:null, errorName, errorCode?, errorMessage, occurredAt, …}`                                         |
| 进程退出                              | `agent-process-exited {exitCode, signal, endedAt, terminationKind, terminationReason?, runtimeReady, uptimeMs, stderrLineCount, stderrTail?}` |
| stderr 出现进程诊断行                 | `agent-process-exception {…, diagnostic}`                                                                                    |

1. `terminationKind`：Host 主动回收（dispose、空闲回收、Host 退出）为 `expected`，请求超时为 `watchdog_recycle`，其余 `unexpected`；`terminationReason` 用 Node 的原因名（`workspace-dispose`、`idle-timeout`、`manager-dispose`、`request-timeout`、`protocol-close`）。只有 `unexpected` 才带 `stderrTail`。
2. **进程诊断**：stderr 行以 `[zcode-process-exception] ` 开头、总长 ≤ 128 KiB，其后 JSON 满足 `zcodeProcessDiagnosticSchema`（strict：version 1、errorId 为 UUID、kind/origin 为 `uncaughtException`/`unhandledRejection`、name 1–128、message ≤ 4000、stack ≤ 16000、occurredAt 非负整数）。这类行不进 stderr tail；name/message/stack 脱敏后再截到上限，发 `agent-process-exception`。Node 也只发给 Main（崩溃遥测），不发给 UI。
3. stderr tail：每行脱敏、超过 1000 字符截断并加 `…[truncated]`，保留最后 20 行。

### 2.11 启动：会话库准备

`InitLocal` 之后，tasks-index 迁移完成、`starting_services` 之前（Node `hostDatabaseStartup.ts`）：

1. 候选目录：`agentWarmupTargets` 的 workspacePath（没有时用 `InitLocal.workspacePath`）；为空时用 `agentSpawnFallbackCwd`（再没有用 Host 进程 cwd）。每个候选按普通 Agent 的 cwd 规则解析（不是目录时回退到 fallback），去重后按顺序处理。
2. 每个目录先报 `preparing_session_storage` + `databasePhase:"checking"`（tasks-index 之后的第一帧），再启动 `<chat 命令> --prepare-storage`（stdin/stdout/stderr 管道，stderr 排空丢弃；环境 = 子进程基础环境 + 命令环境）。
3. 存储准备协议（stdout 每行 ≤ 64 KiB）：
   - `startup/storagePath {path}`：只允许一次；realpath（不存在时取绝对路径）；同一次启动已准备过的同一真实路径 → 回 `{"method":"startup/storagePathReady","reuse":true}`，否则 `reuse:false`；
   - `startup/storageState`：`failed` → 记录失败（`errorCode`，缺省 `sql_failed`，带 sqliteCode / systemCode / migrationId / migration），其余阶段转成 `preparing_session_storage` 的 `databasePhase`，并带 `currentMigration`、聚合的 `migration`、`migrationBaselines`、`finalDatabase`（最后一个目录为 true）；
   - `startup/storagePrepared`：已收到 path 时标记成功并关闭 stdin；
   - 任何非法帧、首帧 30 s 未到 → 失败（`transport_closed` / `startup_status_timeout`），回收进程树。
4. 进程以 0 退出且已 prepared、无失败 → 成功，记住该真实路径；否则启动失败：`phase:"failed"`、`failedPhase:"preparing_session_storage"`、`errorCode` 按 `classifyDatabaseStartupError` 规则分类。
5. 命令声明不支持存储启动（`GLM_BINARY_PATH`）→ `unsupported_runtime`。

### 2.12 `zcode-session`

在 `zcode-agent` 之上的 desktop-continuous 门面（`zcodeSessionService.ts`）：

1. `createSession`：`sessionTraceId` 缺省时生成 UUID；filesystem MCP（`name:"filesystem"` 且参数含 `@modelcontextprotocol/server-filesystem`）在本机 workspace 存在且尚未包含时追加 workspace 路径（只改本次请求）。`persistence:"deferred"` 的草稿记入草稿表后直接返回，不写 tasks-index；否则写入 tasks-index 顶部并广播 `task_meta_changed`（首次写入 grouped 顺序时为 `task_created`，没有用户内容时只写库）。
2. `resumeSession`：参数 `broadcastSnapshot`（缺省 true）不发给 agent。thoughtLevel 与快照不同、且快照的可用档位包含它（或没有档位信息）时，对同一会话补发 `setThoughtLevel`；不支持的档位跳过并 warn。之后按 `broadcastSnapshot` 写库广播（`task_status_changed`，带 model / thoughtLevel 覆盖），或只更新行的 model。
3. `setModel`：非草稿时写库并广播 `task_model_changed`（model 覆盖为 `provider/model[$level]`）；草稿只透传。
4. `closeSession`：忘记草稿后关闭；`closeDeferredDraftSession`：`expectedPersistence:"deferred"` 条件关闭，关闭成功才忘记草稿；失败返回 `false`（旧 Agent 拒绝该字段时保留旧会话）。`promoteDeferredDraftSession`：忘记草稿。
5. 所有返回快照的成员套用 API 重试运行态：快照自带 `runtime.apiRetry` 时记住；`completed` / `error` 时置 `null`；否则补回记住的值。
6. `listSessions`、`readSession`、`readSessionMessages`、`readSessionEvents`、`setThoughtLevel`、`setMode`、`getWorkspaceRuntimeIdentity` 透传。

### 2.13 `plugin-management` channel

18 个成员逐一委托给 2.2 的同名 `zcode-agent` 实现（`plugins/pluginManagementService.ts`），包括 `onDynamicPluginOperationProgress`。

## 3. 状态所有者

| 状态                                                       | 唯一所有者                                                      |
| ---------------------------------------------------------- | --------------------------------------------------------------- |
| 每个 lane 的进程表、进程代、空闲计时                       | 各 lane 的 `AgentRuntime`（`agent/runtime.rs`）                 |
| 反向请求处理器、环境贡献者、使用前钩子、功能门禁、通知总线、进程事件总线 | `Extensions`（三个 lane 共享）                       |
| app 运行时偏好（最新值与版本）、每个进程已推版本           | chat lane 的 `AgentRuntime`                                     |
| 浏览器请求的在途表、IAB 描述                               | `BrowserBridge`（`agent/browser.rs`）                           |
| Host ↔ Main 请求关联                                       | `MainBridge`                                                    |
| 插件进度、TTFT、telemetry、CUA 观察的分发                  | `AgentEvents`（`agent/notifications.rs`）                       |
| legacy 会话事件源与 seq 补号                               | `SessionEvents`（`agent/session_events.rs`）                    |
| 草稿表、API 重试运行态                                     | `zcode-session` 门面（`agent/session.rs`）                      |
| 本次启动已准备的会话库真实路径                             | `prepare_session_storage` 调用（host-core 启动流程内的局部状态）|
| 会话、插件、MCP、工作流的事实                              | CLI                                                             |

## 4. 事件顺序

### 4.1 浏览器命令

```text
CLI ── interaction/browserExecute ──► Host Handler（reverse 扩展）
                                       │ 校验 browserId/generation、requestId 去重、登记在途 + 计时器
                                       ├──► Main: browser-execute-request {requestId, …, command}
                                       │                 Main: WebContentsView + CDP 执行
                                       │◄── Main: browser-execute-result {requestId, result}（ControlRouter）
                                       │ 摘除在途；recordingStatus 完成且有 outputPath → 复制到 workspace
CLI ◄── result ────────────────────────┘
超时：摘除在途 → Main: browser-execute-request {command:{method:"cancelRequest", requestId}} → CLI ◄ timeout
迟到的 result：在途表里没有 → 丢弃
```

### 4.2 lane 与通知

```text
plugin 成员 ─► plugin lane.get_client（不过门禁，不回放偏好）─► 请求（5 min）
mcp 成员    ─► mcp-status lane ─► mcp/list ─► 在途归零 ─► 空闲计时 5 min ─► close("idle_timeout") ─► 回收
任一 lane 的 CLI 通知 ─► Extensions.notifications ─┬► 资源/MCP/工具 → Main 控制消息
                                                    ├► plugins/operationProgress → AgentEvents(operationId)
                                                    ├► TTFT / telemetry / CUA → 校验 → AgentEvents → scope 过滤 → Renderer
                                                    └► session/event / state.updated → SessionEvents
```

### 4.3 启动

```text
init-local ─► starting ─► preparing_host_storage（tasks-index）
          ─► 对每个候选目录：preparing_session_storage/checking
                spawn <cli> --prepare-storage ─► storagePath ─► realpath ─► storagePathReady{reuse}
                storageState(checking…ready) ─► preparing_session_storage/<phase>（finalDatabase）
                storagePrepared ─► 关 stdin ─► exit 0 ─► 记住真实路径
          ─► starting_services ─► install() ─► ready
任一失败 ─► failed{failedPhase:"preparing_session_storage", errorCode}；attachment 继续挂起
```

### 4.4 偏好同步

```text
syncAppRuntimePreferences(p) ─► latest = (v+1, p)
   └► 每个运行中的 chat 进程：lock(prefs) ─► 推 latest ─► applied = v ─► unlock ──► 全部完成后返回
get_client(target)：进程 applied < latest.v ─► lock(prefs) ─► 推 latest ─► applied ─► 交给调用方
```

## 5. 有意与 Node 不同之处

| Node 行为                                                                  | Rust Host                                                     | 理由                                                                                         |
| -------------------------------------------------------------------------- | ------------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| 解析不到 CLI 命令时会话库准备以 `unsupported_runtime` 失败                 | 解析不到命令时跳过会话库准备（warn），首次使用 Agent 时报 `ZCODE_AGENT_RUNTIME_UNAVAILABLE` | Rust Host 运行开发态 `zcode.cjs` 需要 Main 传 `ZCODE_HOST_NODE_EXEC_PATH`；没有 CLI 的环境（单测、端到端 Host 测试）不应让 Host 启动失败。命令可解析但不支持存储启动时仍按 Node 失败 |
| TS CLI 的会话库准备在 Node Worker 里运行 `storagePreparationEntry`         | 以子进程运行同一入口：`<node> zcode.cjs app-server --stdio --prepare-storage --cwd <dir>` | Rust 进程没有 Worker；stdin/stdout 协议相同                                                  |
| 启动期采样磁盘可用空间（`StartupDiskSampler`）                             | `disk` 始终为空数组                                           | 只用于诊断；随启动诊断另行移植                                                               |
| `onDynamicSessionEvent` 的 replay 与 live 事件经 strict zod 解析           | 只校验 `eventId`、`sessionId`、`type`、`seq`、`timestamp` 的类型 | legacy 订阅面，V4 界面不使用                                                                 |
| Claude Code 导入会话的空历史修复（`repairImportedClaudeSessionSnapshot`）  | 不修复，原样返回快照                                          | 依赖 Claude 导入仓库与解析器，随导入功能迁移                                                 |
| CUA product MCP（broker socket / token）注入 session MCP                   | 不注入                                                        | CUA 能力不在本规格                                                                           |
| CUA 操作追踪（Windows 顶部提示、macOS PiP 生命周期）、`hasActiveCuaOperationTurn` | 返回 `false`；`computer-use/operation-event` 留在通知总线上 | CUA 能力不在本规格；后续 CUA service 可直接订阅通知总线                                     |
| 远端 workspace 的浏览器录制上传                                             | 回 `remote Browser recording materialization is unavailable for this session` | 远端随 R6                                                                                    |
| `syncAppRuntimePreferences` 推送调用时的快照，靠 Promise 链保序            | 在 workspace 锁内推送当时最新值                               | 结果相同（最终值是最新值），且不依赖调用到达顺序                                             |
| 新进程偏好补推失败后，下一次 getReadOnlyClient 重新登记并补推              | 进程记住已推版本，下次取 client 时补推                        | 结果相同                                                                                     |
| `listMcpServerStatuses` 在 `mcp/list` 超时后由 process manager 回收再重试   | 同；超时由 `AgentProcess::request` 关闭连接，下一次请求懒启动 | —                                                                                            |
| 发给 Renderer 的 telemetry/TTFT/CUA 事实用 zod 校验                         | 用等价的键白名单与类型检查                                     | 规则逐字段对应 TS schema，测试用 TS 生成的样本对照                                           |

## 6. 验收

- `cargo test -p zcode-host-services agent`：
  - 浏览器桥：描述字段、跨代 / 跨 id 拒绝、requestId 去重、超时发 cancel 与 `sideEffect`、迟到结果丢弃、录制复制到 workspace 且拒绝越界路径、Host 退出结束在途请求；
  - lane：plugin-management 工作目录、mcp-status 空闲回收、请求超时参数；
  - 通知分发：lane 标签、本地 / 远端过滤、telemetry / TTFT / CUA 严格校验（对照 TS 生成的样本）、插件进度按 operationId；
  - stderr 进程诊断解析与脱敏、退出事件的 terminationKind；
  - 偏好：最新值语义、`-32601` 降级、新进程补推；
  - 信封：denylist 合并、ttft 剥离、功能门禁写入；
  - legacy 会话事件：seq 补号、去重、replay 先于 live、合并规则；
  - 会话库准备：协议、reuse、失败分类、首帧超时；
  - zcode-session：草稿不写库、API 重试运行态、filesystem MCP 追加。
- `cargo test -p zcode-host-services --test agent_cli`（真实 Rust CLI + 本地 mock 模型）：legacy createSession / readSession、workspace-config 订阅、插件列表（plugin lane）、MCP 状态（mcp-status lane）、技能目录、`--prepare-storage`。
- 桌面 E2E：`agent-plugins`（设置页插件列表经 plugin lane 加载）、`agent-startup`（启动页经过 `preparing_session_storage` 后进入主界面）。
