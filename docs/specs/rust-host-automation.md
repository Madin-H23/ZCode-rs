# Rust Host R6：自动化与外部集成（不含远端 workspace）

日期：2026-10-08。上级规格：[rust-host.md](rust-host.md) 第 5 节 R6。分支：`feat/rust-host-p-automation`。

## 1. 目标与范围

在 Rust Host 上实现与 Node Host 等价的：

1. **automation（定时任务）**：tasks-index 里 `automations`、`automation_runs` 的仓库；automation 服务（校验、下次触发时间、生命周期）；`zcode-agent` 的 10 个 automation 成员；CLI 反向请求 `automation/*`；与 Main 的 scheduler 配合的 cron 派发（`CronRun`）与立即运行。
2. **闲时任务（off-peak）**：`off_peak_tasks` 仓库、`off-peak-task` channel、mock 网关、`OffPeakRun` 派发、反向请求 `offPeak/create|list`（第 6 节）。
3. **bots**（飞书长连接等，第 7 节）、**conversation-share**（第 8 节）、**cua-permission**（第 9 节）。

远端 workspace（Bot 远程运行时、远端 collection）不在本阶段。

模块：`apps/zcode-host/crates/services/src/automation/`（区域入口与 automation 本体）、`tasks_index/automation_repo.rs`、`off_peak/`、`tasks_index/off_peak_repo.rs`、`bots/`、`conversation_share/`、`cua_permission.rs`。`automation::install` 是本区域在 `install()` 里唯一的一段代码（`// ── automation ──`）。

## 2. automation 产品规则

### 2.1 仓库（`automationRepo.ts`）

与 Node Host、Main 的 scheduler 进程共用 `tasks-index.sqlite`，Rust 是完全兼容的写者：

1. **行映射**（`rowToAutomation` / `rowToRun`）：键序与 TS 对象字面量相同；可空列为 NULL 时省略字段；`mode` 不在 `yolo|plan|edit|auto|autoEdit|build` 内（含历史空串）按未设置；`modelSelection` 只读 `model_selection` 新列，JSON 损坏或不符合 schema 时省略；`scheduleRule` 原样解析 JSON；`scheduleEditedByUser` 只在为 1 时出现；`locationKind` 非 `remote` 一律 `local`。
2. **写入**：
   - `create`：`BEGIN IMMEDIATE` 内先数总行数，达到 20 条时报 `[AUTOMATION_CREATE_LIMIT_REACHED] At most 20 automations may be retained. Delete an existing automation before creating another.`；id 为 `automation-<uuid>`；`model_selection` 缺省写字符串 `"null"`（表示“跟随 workspace”，与未迁移的 SQL NULL 区分）；`lifecycleStatus=completed` 时 `enabled=0`。
   - `update`：读出整行后按“未提供保留、null 清除”合并，`writeRow` 整行写回；只有显式改生命周期时才改 `enabled`；`resetRetry` 清重试态。
   - `runNow`：`BEGIN IMMEDIATE` 内回收超时 10 分钟的僵尸认领，再以 `running 0→1` 抢 single-flight 锁；成功则插入 `<automationId>:manual:<uuid>` 的 manual run（`dispatch_status=claimed`、`attempts=1`、`model_selection` 为 NULL）。
   - `markManualRunDispatched`、`fixRunModelSelection`、`markRunOutcome`、`ensureRunClaimed`、`markRunDispatch`、`releaseManualClaim`、`touchManualClaim`、`deleteRun`、`setEnabled`、`restart`、`delete`：SQL 原样移植。
   - 模型选择序列化：trim 后按 `providerId, modelId, options?` 写 JSON，`options` 为空对象时省略；非法时报错。
3. 只移植 Host 用到的方法。`claimDue`、`claimManualRuns`、`markDispatched`、`markDispatchFailed`、`skipAndReschedule`、`upsertRunClaimed`、`recordSkippedRun`、`pruneRuns` 只由 scheduler 调用，scheduler 仍是 Node（第 2.6 节）。
4. **workspace 归属**：带 scope 的写和单查都加 `workspace_key` 过滤，key = `workspaceIdentity?.trim() || workspacePath`。

### 2.2 下次触发时间（`automationCron.ts`）

1. cron 只支持 croner 10 的语义（5 段，本地时区，秒固定为 0）：`*`、`?`、列表、区间、步长、英文月份/星期名、`L`、`#`、`W`、`@yearly` 等别名；日与星期同时受限时按 OR（croner `legacyMode`）。非法表达式报 `非法的 cron 表达式：<expr>`。
2. `scheduleRule`（minute / hourly / daily / weekly / monthly / yearly）的计算逐行移植，所有日历运算按 JS `new Date(y, m, d, h, mi)` 的本地时间语义（越界进位、夏令时空档按转换前的偏移解释）。
3. 新建：相对延迟（`relativeDelayMinutes`）由服务端时钟生成一次性规则；`intervalUnit + interval` carrier 归一化为权威 `scheduleRule`；`*/N * * * *` 推断为以创建时刻为锚点的分钟规则；一次性固定月日 cron 刚错过 1 分钟内立即补执行，30 分钟内的陈旧目标拒绝。
4. 校验与错误文案（`InvalidCronExprError`、调度规则、carrier、相对延迟、maxRuns）与 TS 逐字相同。

### 2.3 `zcode-agent` 成员

| 成员                                                                                                              | 行为                                                                                                                         |
| ----------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `listAutomations(target)` / `listAllAutomations()`                                                                | 按 `created_at` 倒序；前者限定 workspace                                                                                     |
| `createAutomation` / `updateAutomation`                                                                           | 经 service 校验与重算；update 限定调用方 workspace，找不到时返回 `null`                                                      |
| `deleteAutomation` / `setAutomationEnabled` / `restartAutomation` / `deleteAutomationRun` / `listAutomationRuns` | 限定调用方 workspace；restart 只复活 `failed`                                                                               |
| `runAutomationNow`                                                                                                | 抢到锁返回 `{status:"queued"}` 并在当前 Host 直接派发；锁被占用返回 `{status:"duplicate"}`；automation 不存在也是 duplicate |

这些成员不依赖连接 scope（Node 也不读 connectionId），由 `automation::members` 实现，`agent/service.rs` 的 dispatch 里只有一段委托。

### 2.4 CLI 反向请求

`automation/create|update|list|delete|checkTaskBinding`，workspace 一律取发请求的 CLI 进程所属 workspace，不进协议参数。参数按 `zcode-protocol` 的 strict schema 校验，失败回 `-32602` 加固定文案（`Invalid automation create params` 等）；业务错误回 `-32603` 加原始错误文案。create 时若带 `targetTaskId` 且 tasks-index 有该行，给该行盖 `cronAutomationId` 标记。update 找不到时报 `Scheduled task not found in the current workspace.`。结果里的 automation 按 `toProtocolAutomation` 的字段与顺序输出。

### 2.5 派发

1. **定时运行**：Main 的 scheduler 认领到期项后经 Main 发 `cron-run`。Rust Host：
   1. 数据库未就绪时立即回 `cron-run-result {ok:false, failureKind:"transient", error:"Local database startup is not ready"}`（Rust Host 只有在存储就绪后才安装服务，所以这一分支不会出现）；
   2. 固定本次运行的模型选择：run 已固定的直接复用；否则读 automation 的新列（损坏时报 `Automation 模型选择不可用，请重新选择模型与思考档位`），经 model-selection 视图校验得到 `effectiveSelection`；跟随 workspace 时用 `preferredSelection`（缺推理档位时报 `Automation 无法从目标 Host 解析首选模型`）；用 `COALESCE` 原子写入 run；
   3. 有 `targetTaskId` 时 `session/resume` 该会话并盖 `cronAutomationId`，再用 V4 `switchModelConfig`（CAS，stale 时按 `revisionAtDecision` 重试，最多 4 次）与 `switchCollaborationMode` 应用模型、档位、模式；否则 `session/create`（`persistence:"deferred"`、`titleGenerationEnabled:false`，带模型、档位、模式），把会话写入 tasks-index 顶部并盖 `cronAutomationId`；
   4. 若 automation 配置了 Bot 回推目标，先登记 `watchAutomationRun`（失败只记 warn）；
   5. 记 run `outcome=running`，订阅该会话的终态，再用 V4 `sendText`（`commandId = runId`，`heldQueueDisposition:"keepQueueAndSend"`，`automationId`，`toolDisallowlist` 合并 `CronCreate/CronUpdate/CronDelete`）提交 prompt；
   6. 回 `cron-run-result {ok:true, taskId, sessionId}`；任一步失败记 run `outcome=failed` 并回 `ok:false, failureKind:"transient"`。
2. **终态**：会话从非终态转为终态时（sessions-index 观察者），按本轮 inputId 结算：写 run outcome（`succeeded` / `failed`），把会话置为未读；manual run 同时释放 single-flight 锁。manual run 在执行期间每 60 s 续租一次锁。
3. **立即运行**：`runAutomationNow` 抢锁成功后直接派发（不经 scheduler），派发成功后 `markManualRunDispatched`（回写失败只记 warn，锁仍由终态释放）；派发失败时把 run 记为 `failed_to_dispatch` 并释放锁，再把错误抛给 UI。

### 2.6 Main 的 scheduler

scheduler 是 Main 起的 Node utilityProcess，只读写 tasks-index，经 Main 把 `cron-dispatch-request` 转成 `cron-run` 发给某个本地 Host，再把 `cron-run-result` 转回去结算。它与 Rust Host 无关，保持不变；Rust Host 只需消费 `cron-run`、回 `cron-run-result`。控制消息类型与 Node 相同，不新增。

## 3. 状态所有者

| 状态                                                          | 唯一所有者                                                                 |
| ------------------------------------------------------------- | -------------------------------------------------------------------------- |
| automation 定义、调度状态、single-flight 锁、run 台账         | tasks-index.sqlite（Rust 经 `TasksIndex` 线程，scheduler 与 Node Host 同库） |
| 到期判定、认领、重试退避、错过窗口                            | Main 的 scheduler（Node）                                                  |
| 本 Host 在途运行的终态订阅、manual 续租定时器                 | `automation::dispatch::Dispatcher`（内存，Host 退出即丢；靠锁超时回收）   |
| 会话、轮次、模型配置                                          | CLI                                                                        |

## 4. 事件顺序

```text
scheduler(Node)          Main                 Rust Host（Dispatcher / TaskDriver）              CLI
│ claimDue → run claimed
│── cron-dispatch-request ─►│── cron-run ─────►│ 固定 run 模型（COALESCE）
│                          │                  │ create / resume 会话 ───────────────────────────►│ session/create|resume
│                          │                  │ （resume：switchModelConfig / Mode，CAS 重试） ──►│ v4/command
│                          │                  │ watchAutomationRun（可选，Bot）
│                          │                  │ run outcome=running；订阅终态（inputId=runId）
│                          │                  │ sendText(commandId=runId, automationId) ─────────►│ v4/command → ACK
│                          │◄─ cron-run-result {ok, taskId, sessionId}
│◄─ cron-dispatch-result ──│                  │
│ markDispatched（计数、下次时间）              │◄──── sessions-index：会话 running → completed/error
│                                             │ run outcome=succeeded|failed；会话置未读；manual 释放锁

UI runAutomationNow ─► runNow（BEGIN IMMEDIATE 抢锁 + 插 manual run）
   ├─ 未抢到 → {status:"duplicate"}
   └─ 抢到 → 同上派发（不经 scheduler）→ markManualRunDispatched → {status:"queued"}
             派发失败 → run failed_to_dispatch + 释放锁 → 错误抛给 UI
```

## 5. 有意与 Node 不同之处（automation）

| Node 行为                                                                | Rust Host                                                    | 理由                                                                                 |
| ------------------------------------------------------------------------ | ------------------------------------------------------------ | ------------------------------------------------------------------------------------ |
| 远端 workspace 的 automation 派发到远端 Host                             | 远端 identity 一律报 `Automation 目标 Remote Host 当前不可用` | 远端 workspace 不在本阶段；与 Node 在远端不可用时的行为相同                           |
| 会话终态取 task adapter 的 `activePromptInputIds`                        | Dispatcher 自己记录本轮 inputId，只结算自己派发的轮次         | Rust Host 的 zcode-task 还没有 sendPrompt facade；结果相同                            |
| 终态 `stopped` 从未产生（adapter 只映射 completed / failed）              | 同 Node                                                      | 保真                                                                                 |
| 派发后上报 `session_create` 遥测                                         | 不上报                                                       | 遥测不在 Rust Host 范围                                                              |
| `session/create` 旧协议字段兼容重试                                       | 不做                                                         | Rust Host 只配当前 CLI                                                               |
| `sendConversationCommandV4` 为所有调用方合并 automation 工具 denylist     | 只在 Host 自己派发的 sendText 上合并                         | Renderer 的命令走 `zcode-agent`（agent 区域）；CLI 侧还有 `resolveTurnAutomationId` 兜底 |

## 6. 闲时任务

见 [rust-host-automation-offpeak.md](rust-host-automation-offpeak.md)（实现时补充）。

## 7. bots

见 [rust-host-automation-bots.md](rust-host-automation-bots.md)（实现时补充）。

## 8. conversation-share

见 [rust-host-automation-share.md](rust-host-automation-share.md)（实现时补充）。

## 9. cua-permission

（实现时补充。）

## 10. 验收

- `pnpm exec tsx scripts/rust-host/automation-fixtures.ts --check`：用真实 TS 实现（`AutomationRepo`、`automationCron.ts`、`AutomationService`）在固定时钟、固定时区下生成对拍 fixture。
- `cargo test -p zcode-host-services automation`：仓库每一步的返回值与最终表内容与 TS 相同；cron 与 scheduleRule 的下次触发时间与 croner / TS 相同（含夏令时）；service 的校验文案相同；反向请求的参数校验与结果形状。
- 真实 App（`scripts/rust-host/e2e/scenarios/automation-run-now.mjs`）：在设置页创建定时任务，点“立即运行”，mock 模型收到 prompt，侧栏出现带定时标记的新会话，运行历史出现成功的 manual run。
