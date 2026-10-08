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

| 成员                                                                                                             | 行为                                                                                                                        |
| ---------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------- |
| `listAutomations(target)` / `listAllAutomations()`                                                               | 按 `created_at` 倒序；前者限定 workspace                                                                                    |
| `createAutomation` / `updateAutomation`                                                                          | 经 service 校验与重算；update 限定调用方 workspace，找不到时返回 `null`                                                     |
| `deleteAutomation` / `setAutomationEnabled` / `restartAutomation` / `deleteAutomationRun` / `listAutomationRuns` | 限定调用方 workspace；restart 只复活 `failed`                                                                               |
| `runAutomationNow`                                                                                               | 抢到锁返回 `{status:"queued"}` 并在当前 Host 直接派发；锁被占用返回 `{status:"duplicate"}`；automation 不存在也是 duplicate |

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
   6. 不带 `targetTaskId`（新建会话）时上报 `session-create-telemetry`（`create_source:"automation_scheduled"`，见 [rust-host-integration.md](rust-host-integration.md) §2.3）；
   7. 回 `cron-run-result {ok:true, taskId, sessionId}`；任一步失败记 run `outcome=failed` 并回 `ok:false, failureKind:"transient"`。
2. **终态**：会话从非终态转为终态时（sessions-index 观察者），按本轮 inputId 结算：写 run outcome（`succeeded` / `failed`），把会话置为未读；manual run 同时释放 single-flight 锁。manual run 在执行期间每 60 s 续租一次锁。
3. **立即运行**：`runAutomationNow` 抢锁成功后直接派发（不经 scheduler），派发成功后 `markManualRunDispatched`（回写失败只记 warn，锁仍由终态释放）；派发失败时把 run 记为 `failed_to_dispatch` 并释放锁，再把错误抛给 UI。

### 2.6 Main 的 scheduler

scheduler 是 Main 起的 Node utilityProcess，只读写 tasks-index，经 Main 把 `cron-dispatch-request` 转成 `cron-run` 发给某个本地 Host，再把 `cron-run-result` 转回去结算。它与 Rust Host 无关，保持不变；Rust Host 只需消费 `cron-run`、回 `cron-run-result`。控制消息类型与 Node 相同，不新增。

## 3. 状态所有者

| 状态                                                  | 唯一所有者                                                                   |
| ----------------------------------------------------- | ---------------------------------------------------------------------------- |
| automation 定义、调度状态、single-flight 锁、run 台账 | tasks-index.sqlite（Rust 经 `TasksIndex` 线程，scheduler 与 Node Host 同库） |
| 到期判定、认领、重试退避、错过窗口                    | Main 的 scheduler（Node）                                                    |
| 本 Host 在途运行的终态订阅、manual 续租定时器         | `automation::dispatch::Dispatcher`（内存，Host 退出即丢；靠锁超时回收）      |
| 会话、轮次、模型配置                                  | CLI                                                                          |

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

| Node 行为                                                             | Rust Host                                                     | 理由                                                                                     |
| --------------------------------------------------------------------- | ------------------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| 远端 workspace 的 automation 派发到远端 Host                          | 远端 identity 一律报 `Automation 目标 Remote Host 当前不可用` | 远端 workspace 不在本阶段；与 Node 在远端不可用时的行为相同                              |
| 会话终态取 task adapter 的 `activePromptInputIds`                     | Dispatcher 自己记录本轮 inputId，只结算自己派发的轮次         | Rust Host 的 zcode-task 还没有 sendPrompt facade；结果相同                               |
| 终态 `stopped` 从未产生（adapter 只映射 completed / failed）          | 同 Node                                                       | 保真                                                                                     |
| `session/create` 旧协议字段兼容重试                                   | 不做                                                          | Rust Host 只配当前 CLI                                                                   |
| `sendConversationCommandV4` 为所有调用方合并 automation 工具 denylist | 只在 Host 自己派发的 sendText 上合并                          | Renderer 的命令走 `zcode-agent`（agent 区域）；CLI 侧还有 `resolveTurnAutomationId` 兜底 |

## 6. 闲时任务

模块：`off_peak/`（`client.rs` 服务端客户端、`gateway.rs` mock 网关、`service.rs` 编排、`dispatch.rs` 派发、`reverse.rs` 反向请求、`credentials.rs` 双凭证、`models.rs` 支持矩阵与模型）、`tasks_index/off_peak_repo.rs`。

### 6.1 规则

1. **仓库**（`offPeakTaskRepo.ts`）：与 Node Host、Main 的 scheduler 共用 `off_peak_tasks`，SQL 原样移植。行映射同 `rowToTask`（新列 `model_selection` 损坏或缺失时给 `modelSelectionIssue:{code:"repair-required"}`；`list` 联查 tasks 表的绑定会话标题）。终态（completed / failed / cancelled）不可逆出；绑定会话的唯一索引冲突识别为 `session_bound`。只移植 Host 用到的方法，`claimDue`、`markRunning`、`releaseClaim`、`recoverInterrupted` 由 scheduler 调用。
2. **`off-peak-task` channel 的 11 个成员**：语义同 `OffPeakTaskService`。`createTask` 先校验（标题、prompt、workspace 非空，权限是六档之一）→ 用当前 Registry 解析固定的 Off-Peak Provider 与模型 → 绑定会话预检 → `POST /ticket` → 成功才落库；失败只回稳定分类 `{ok:false, failureStage, errorCategory, errorCode, providerName}`（3101 / 3103 / network / invalid_response / local_persist / client_validation）。`cancelTask` 先落终态再停 loop（V4 `stop`），再后台核销；`continueTask` 票活着只恢复，票已废才重取号；`updateTask` 只允许 queued / paused；`list` / `get` 只派生模型诊断，不写库。
3. **轮询同步**：tokio 定时器驱动。每个周期先补报未核销终态（4xx 当作幂等 ack），无票的 queued 补取号，再 `POST /ticket/status` 写回快照（ready → schedulable、位次、废票重取号）；翻为可派发时发 `off-peak-scheduler-wake-request`。间隔取服务端 `next_poll_after`，钳制在 5 s – 5 min，失败按 10 s 起指数退避。没有任务时也按 5 min 重排（TS 的 `finally` 如此，注释所说“不重排”与代码不符，以代码为准）。
4. **凭据与灰度**：`ZCODE_OFFPEAK_MOCK=1` 时与 Node 相同，用确定性的 mock 凭据（bigmodel-personal），服务端 origin 是进程内 mock 网关；非 mock 时走账号栈的真实凭据链（zcode JWT + 选中 Coding Plan 的 key，`off_peak::credentials`，见 [rust-host-integration.md](rust-host-integration.md) §2.1），服务端请求带 ZCode 来源头；`coding-plan-subscription.getOffPeakClientConfig` 由本区域包一层，在 mock 模式下按 `resolveOffPeakClientConfig` 的 mock 分支返回（有隐藏的 Off-Peak Provider 模型即 enabled），其余情况原样交给 coding-plan 的实现。非 mock 时 origin 是 `ZCODE_BASE_URL` / `ZCODE_ENDPOINT_ORIGIN`（缺省 `https://zcode.z.ai`）。
5. **mock 网关**：`127.0.0.1`，端口 `ZCODE_OFFPEAK_MOCK_PORT` 或固定 45197，被占用时复用已有实例；`ZCODE_OFFPEAK_MOCK_*` 的延迟、额度、429 注入、`capture` / `invalid-ticket` 场景与 Node 相同；状态机为 take → queued → ready → active → expired / settled，在轮询与 messages 请求时懒惰推进。
6. **`off-peak-run` 派发**：无票 → transient；模型不可解析或凭据不可用 → `failureKind:"permanent"`。三个分支：resume（续跑同一会话，发固定续跑提示词）、bound-first-run（恢复绑定会话，先检查已删除（permanent）/ 正忙（transient），再设权限模式）、init（新建会话，`persistence:"deferred"`，盖 `offPeakTaskId`）。V4 sendText 携带闲时 Selection、`modelExecution`（`memoryExtraction:"skip"`、`selectionScope:"execution"`、逐请求鉴权头、`subagents`）、`offPeakTaskId`、`offPeakRunType`，denylist 含 `CronCreate`、`OffPeakCreate`。init 分支首发后上报 `session-create-telemetry`（`create_source:"automation_idle"`）。终态：succeeded → completed，其余 → failed；回写失败（已终态）只记日志；会话置未读。
7. **反向请求** `offPeak/create|list`：参数按 strict schema 校验（-32602 + 固定文案 + zod flatten 的 `data`，见 integration §2.5）；灰度关闭或模型不在白名单时回 `{ok:false, …, errorCode:"offpeak_disabled" | "model_not_allowed"}`；省略模型取白名单末位，档位取模型最高档；兜底异常只回 `-32603 Internal off-peak service error`（`data {errorCode:"offpeak_internal_error"}`）。`list` 只给本 workspace 最近 20 条的协议最小面。
8. **工具面**：本地 workspace（非远端 identity、无 remoteSessionId）在会话工作前推送一次 `workspace/updateOffPeakToolPolicy {enabled:true}`（每代进程一次，经 `add_use_hook`；-32601 静默），Host 自己发的 `session/create`、`session/resume` 带 `offPeakToolEnabled:true`。

### 6.2 事件顺序

```text
UI createTask ─► OffPeakService：校验 / 模型 / 绑定预检 ─► POST /ticket（mock 网关或服务端）
                 └─► 落库 queued（ready 则 schedulable）─► 唤醒 scheduler
sync 周期 ─► 补报核销 ─► POST /ticket/status ─► 写回快照 ─► ready 翻转时唤醒 scheduler
scheduler(Node) ─claimDue─► Main ─off-peak-run─► OffPeakDispatcher
   ├─ resume / bound-first-run：session/resume + 权限模式（V4）
   └─ init：session/create（deferred，盖 offPeakTaskId）
   ─► 登记终态订阅 ─► sendText（闲时 Selection + requestAuth）─► off-peak-run-result
sessions-index：会话 running → 终态 ─► markTerminal ─► 会话置未读；下个 sync 周期核销
```

### 6.3 有意与 Node 不同之处

| Node 行为                                                    | Rust Host                                              | 理由                                                                                     |
| ------------------------------------------------------------ | ------------------------------------------------------ | ---------------------------------------------------------------------------------------- |
| mock 网关准入后代理到用户 Coding Plan 的真实上游             | 回固定的 Anthropic 响应（Node 拿不到上游时的同一路径） | 仅开发演示；不能连真实服务（见 [rust-host-integration.md](rust-host-integration.md) §5） |
| mock 网关的 `foreground-subagents` 脚本场景                  | 未移植                                                 | 只服务 Node 的一个 E2E 脚本                                                              |
| 终态回填 `files_changed`（task diff 汇总）                   | 不回填                                                 | Rust Host 的 zcode-task 还没有 `getTaskSnapshot`                                         |
| Renderer 的 V4 `createSession` 信封注入 `offPeakToolEnabled` | 未注入                                                 | 信封处理属于 agent 区域的 `zcode-agent` 命令路径；工作区策略推送已覆盖冷恢复             |
| bound-first-run 的“正忙”取 `listTasks` 活跃分区的 status     | 取 tasks-index 行的 status（含置顶 / 归档行）          | 结果只在置顶或归档的绑定会话上不同                                                       |

## 7. bots

模块：`bots/`（`model.rs` 数据规则与 schema、`store.rs` 配置/状态文件、`credentials.rs` 凭据文件、`feishu/` 开放平台 HTTP、长连接帧与客户端、`runtime.rs` 长连接生命周期、`runtime_lock.rs` 跨进程运行锁、`inbound.rs` 入站命令、`delivery.rs` 投递与终态回推、`service.rs` 管理面与 channel）。本阶段只实现飞书 / Lark 通道。

### 7.1 规则

1. **文件**（与 Node 共用，`~/.zcode/v2/`）：`bot-config.v3.json`、`bot-state.v3.json`。读写都在跨进程文件锁内，写入为私密原子写（0600），内容为 `JSON.stringify(value, null, 2) + "\n"`。当前文件缺失时从 `bot-config.json` / `bot-state.v2.json` / `bot-state.json` 单向导入（旧 `model` / `thoughtLevel` 迁移成 `modelSelection`，`builtin:*` provider 按冻结表改名，官方 GLM 模型 id 规范大小写）并立即落盘。
2. **schema**：与 zod 等价——`botConfigSchema` 等 strict 对象拒绝未知键，状态条目这类非 strict 对象丢弃未知键，输出按 schema 键序重建。`zcodePermissionResponseSchema` 的 `modifiedInput` / `permissionUpdates` 与问答 `origin` 只校验形状、原样保留。
3. **归一化**：`saveBot` / `saveConfig` / `/bind` 走 `normalizeBotConfig`：`allowedWorkspaces` 去空去重、空或含 `*` 时为 `["*"]`；命令策略补默认值并丢弃旧 `cli`；`currentOptions` 只保留合法的 `modelSelection` 与非空 `mode` / `sandboxMode` / `approvalPolicy`；回复颗粒度飞书只允许 `streaming_card`，其余通道不允许它；微信去掉 `webhookUrl`。`saveBot` 把更新后的 Bot 移到列表末尾（同 Node）。
4. **凭据**：`credentials.json` 与 Node 逐字节兼容：值为 `enc:v1:<iv>.<tag>.<cipher>`（base64url，AES-256-GCM），密钥 = sha256(`ZCODE_CREDENTIAL_SECRET`，否则 `zcode-credential-fallback:<platform>:<homedir>:<username>`)；非前缀值按旧明文读。文件损坏时按内容备份为 `.corrupt-<sha>.bak` 并拒绝覆盖。Bot 凭据键为 `bot:<id>:credential`、`bot:<id>:webhook-secret`。
5. **校验**（`validateBotConfig`）：id 必填；同通道已启用的 Bot 不能绑定同一用户；Telegram / 飞书 / Lark 已启用的 Bot 不能共用同一凭据。
6. **名称**：保存时带新凭据或名称为空，从开放平台读应用名（先 `applications/<appId>`，失败或为空再读 `me`），按 0 / 800 / 1800 ms 重试。
7. **注册**：`beginFeishuRegistration` 向 accounts 域的 `/oauth/v1/app/registration` 依次发 `init`、`begin`（轮询域固定 feishu，二维码 URL 补 `from=sdk&source=node-sdk/zcode&tp=sdk`）；`pollFeishuRegistration` 按 `authorization_pending` / `slow_down` / `access_denied` / `expired_token` 映射状态，租户品牌为 lark 时切到 lark 轮询域。
8. **长连接**（@larksuiteoapi/node-sdk `WSClient` 协议）：`POST <open>/callback/ws/endpoint {AppID, AppSecret}` 取 wss 地址与 ClientConfig；帧为 `pbbp2.Frame` protobuf（手写编解码）；按 `PingInterval` 发 control/ping，pong 更新 ClientConfig；`type=event` 的 data 帧按 `message_id/sum/seq` 拼包，事件按 EventDispatcher 规则拍平（v2 schema 把 header、event 摊到顶层）后处理，回同一帧作为 ack（追加 `biz_rt`，payload `{"code":200[,"data":base64(JSON)]}`，处理失败为 500 让飞书重投）。首次连接 20 s 超时；断线按 `ReconnectNonce` 随机延迟、`ReconnectInterval` 间隔重连，`ReconnectCount` 次后终止，5 s 后由运行循环整体重试。
9. **运行锁**：连接前在 `~/.zcode/v2/bots-runtime-locks/<provider>-websocket/<sha256(appId)>.lock/` 拿锁（pending 目录 + rename、`owner.json`、每 10 s touch 的租约文件；持有者存活且租约 30 s 内更新时让出，状态为“handled by another ZCode window”，10 s 后重试）。Node Host 与 Rust Host 共用这套锁，同一应用只会有一条长连接。
10. **对齐**：配置变更后 `scheduleRefresh` 串行对齐（只有最新一次走完）：启用且配齐 App ID / App Secret 的飞书 Bot 才连接；凭据指纹（sha256(JSON[provider, appId, credentialRef, secret])）变化时重连；禁用的显示 `disabled`。运行状态（`getStatus.botRuntime`）与 Node 的 `messageId` / 文案相同；投递错误单独记在 `deliveryError`。
11. **入站**：解析飞书文本 / 富文本（post）/ 卡片按钮回调，去掉 @；2 分钟内同一消息 id 只处理一次；同一聊天上下文串行处理。授权（`withAuthorizedContext`）：Bot 启用且绑定的是这个用户、私聊、命令未被禁用、有可用 workspace（`settings.lastWorkspaceSession`）、上下文 workspace 仍在授权范围；带 `workspaceIdentity` 的上下文视为远端未连接，只放行不需要运行时的命令。
12. **命令**：`/bind <code>`（30 s 绑定码，私聊，覆盖旧绑定，回绑定成功 + 帮助）、`/help`、`/status`（工作区、模型、任务、状态）、`/new`（回到草稿，Bot 硬锁 yolo）、`/stop`、普通消息。普通消息：草稿态用目标 Host 的首选（或草稿已选并经有效解析的）模型新建 v4 草稿会话，写入上下文后发送；任务态续用当前会话（`session/read` 取原选择并有效解析）；已软删除的任务回到草稿新建并提示；任务运行中回 `taskRunning`。输入在后台以 `botDeliveryTarget` = 当前聊天发送，先订阅终态再发。
13. **回复**：飞书普通回复用 Card JSON 2.0 的 markdown 元素（单换行转硬换行、`---` 补空行），超过 1900 个 UTF-16 码元分卡；`receive_id_type` 按 `oc_` 前缀选 chat_id / open_id。会话终态后回推最后一段助手文本（摘要模式的 latestPart），失败回 `taskFailed`。
14. **automation 回推**：`watchAutomationRun`（channel 成员与 automation 区域的 `BotDelivery`）只对存在、启用、通道一致的 Bot 订阅该会话终态并回推摘要；跳过原因 5 分钟内只告警一次。
15. 未实现的成员（`beginWeixinRegistration`、`pollWeixinRegistration`）留在 stub，回 `NotImplemented`；`handleProviderCallback(Response)` 与 `testBot` 对 Telegram / Webhook / 微信回 `NotImplemented`，discord / wecom 回“reserved for a future version”（同 Node）。

### 7.2 状态所有者

| 状态                                       | 唯一所有者                                                           |
| ------------------------------------------ | -------------------------------------------------------------------- |
| Bot 配置、聊天上下文（草稿 / 当前任务）    | `bot-config.v3.json`、`bot-state.v3.json`（Node 与 Rust 同文件同锁） |
| 应用密钥                                   | `credentials.json`（与 Node 共用的加密文件）                         |
| 某个飞书应用的长连接归属                   | `bots-runtime-locks` 目录锁（跨进程）                                |
| 绑定码、入站去重、运行中任务、运行状态展示 | `bots::service::Bots`（内存，Host 退出即丢，同 Node）                |
| 会话与轮次、终态                           | CLI（经 TaskDriver 的 sessions-index 终态事件）                      |

### 7.3 事件顺序

```text
飞书                       Rust Host（runtime / inbound / delivery）                        CLI
│◄── POST callback/ws/endpoint ── 拿运行锁 → 取接入点
│◄══ wss 连接 ═══════════════════ running
│── event 帧(im.message.receive_v1) ─►│ 拼包 → 拍平 → 去重 → 串行队列
│                                    │ 授权 / 读上下文
│                                    │ 草稿：model_view → session/create（v4 草稿）────────────►│
│                                    │ 写上下文（task）→ 订阅终态（inputId）
│◄── ack 帧 {code:200} ───────────────│ 后台 sendText(botDeliveryTarget) ─────────────────────►│ v4/command
│                                    │◄──────────────── sessions-index：running → completed
│                                    │ rowsRange 取最后一段助手文本
│◄── POST im/v1/messages（卡片）───────│
```

### 7.4 有意与 Node 不同之处

| Node 行为                                                                  | Rust Host                                                      | 理由                                                     |
| -------------------------------------------------------------------------- | -------------------------------------------------------------- | -------------------------------------------------------- |
| Telegram 轮询、Webhook、微信轮询与扫码注册                                 | 未实现（`NotImplemented`）                                     | 本阶段只做飞书长连接；其余通道沿用同一管理面后续补传输层 |
| 飞书流式卡片（`streaming_card`）逐步更新、工具面板、变更摘要               | 终态后发一张卡片，内容为最后一段助手文本                       | 卡片流式更新与 Markdown 工具面板体量大，后续单独移植     |
| `/workspace`、`/model`、`/mode`、`/think`、`/task`、`/reply` 选择卡与按钮  | 回 `unknownCommand`                                            | 依赖选择卡、配置项读写，随流式卡片一起移植               |
| 权限请求、问答（elicitation）在聊天里应答                                  | 不转发（在 ZCode 界面处理）                                    | 同上                                                     |
| `/new` 继承当前任务的模型与思考档位                                        | 新草稿只带 yolo，模型取目标 Host 首选                          | 继承需要读任务配置项；派发仍经有效选择解析               |
| `/status` 含已工作时长与实时进展                                           | 不含                                                           | 依赖流式事件的进展跟踪                                   |
| 附件下载、缓存后作为 prompt 附件                                           | 回 `attachmentRejected`（下载不可用）                          | 附件管线后续移植                                         |
| 入站时查通讯录补发送者昵称；输入中 reaction；卡片点击后 `card/update` 原卡 | 不做                                                           | 只影响展示；绑定时 `displayName` 为空                    |
| `bots:task` 广播（侧栏增量）                                               | 不发；新建会话由 TaskDriver 写 tasks-index 并广播 task_created | 侧栏走已有的任务列表事件                                 |
| 远端 workspace 的 Bot 运行时与 `/reconnect`                                | 远端上下文一律视为未连接                                       | 远端 workspace 不在本阶段                                |
| `listWorkspaceRefs` 5 s 缓存                                               | 每次读设置                                                     | 设置读取本身已缓存                                       |
| 开放平台基址固定                                                           | 可由 `ZCODE_FEISHU_OPEN_BASE_URL` 等变量覆盖                   | 只供测试指向 127.0.0.1 mock                              |

## 8. conversation-share

模块：`conversation_share/`（`client.rs` 服务端客户端、`integrity.rs` 规范化摘要、`import.rs` 导入、`errors.rs` 错误契约）。

### 8.1 规则

1. **连接门禁**（conversationShareAttachmentService.ts）：desktop-continuous 全部可用；web-remote-replayable 只能能力发现与预览，`preflight`、`publish`、`importShare`、`getContinuation` 回 `{kind:"feature_disabled"}`（“Conversation sharing is only available from Desktop”），导入副本读回 `null`，进度事件不触发。
2. **错误契约**：`ConversationShareClientError` 与 `ConversationShareServiceError` 经 RPC 透传键（`kind`、`status`、`code`、`retryAfterMs`、`details`）到 Renderer；ServiceError 的 `details` 只含脱敏字段（requestId、reasonCode、诊断白名单、最多 5 条 issue）。为此 `RpcError` 增加了透传键（`rpc` crate，增量改动）。
3. **HTTP**：base 为 `<ZCode origin>/api/v1`。`required` 端点（capabilities、preparation、upload、confirm）没有 token 时在发请求前以 `authentication_required`（status 401）失败；`optional` 端点（preview、continuation）照常请求。错误按业务码映射 kind（3001–3215），非 JSON 的 5xx 归 network，空 401 归 authentication_required，未知业务码保留 msg 归 unknown。成功响应必须是 `{code:0, msg, data}`。
4. **完整性**：continuation 对服务端原样发来的 rows 与去掉签名 URL 字段的 artifacts 计算规范化 JSON（键按 UTF-16 排序、数字按 JS 序列化）的 sha256，与 `integrity` 比对；不一致报 `invalid_contract`。版本高于本端时报 `unsupported_schema_version`。rows 按已知 kind 保留，未知 kind 计入 `unsupportedRowCount`。
5. **导入**：按 (shareCode, workspace) 去重（已完成的回 `reused:true`，在途的合并）；在目标 workspace 的 `.zcode-share/<shareId>/` 下写 marker → staging 下载（120 s 超时、Content-Length 预检、size + sha256 校验）→ rename → 写原样 rows 的只读副本 → 格式化 shared context Markdown → CLI `session/create`（`importedHistory.source:"sharedContext"`，标题加“来自分享：”/“From Share: ”前缀）→ 写入 tasks-index 顶部并广播 → 删 marker。会话提交之前失败只删本次 import-owned 目录；完成索引写在默认 conversation workspace 的 `.zcode-share-imports.json`（串行、原子写）。启动时清理默认 workspace 里没有对应会话的残留导入。远端或缺省目标回落到默认 conversation workspace，并带 `fallbackReason`。

### 8.2 有意与 Node 不同之处

| Node 行为                                                                               | Rust Host                                     | 理由                                                        |
| --------------------------------------------------------------------------------------- | --------------------------------------------- | ----------------------------------------------------------- |
| token 来自 OAuth 凭据库（zcode JWT）                                                    | token provider 恒为空                         | Rust Host 还没有账号（R4）；接入后替换 `TokenProvider`      |
| 登录后 preflight 做行选择、结构检查、结果物发现；publish 做公开投影、上传、confirm 轮询 | 能力发现（需要登录）之后报 `feature_disabled` | 没有 token 时这些步骤不可达，也无法验证；随 R4 一起移植     |
| preflight / publish 经可信连接的 zcode-agent scope 读行（需要 V4 握手）                 | 尚未使用（同上）                              | 同上                                                        |
| rows 用完整的 `conversationRowSchema` 解码（zod 剥离未知字段）                          | 按已知 kind 保留原样行                        | 渲染与格式化只读已知字段；完整性校验本来就基于原样值        |
| 导入会话的 `session/create` 带 `offPeakToolEnabled`                                     | 不带                                          | Rust CLI 拒绝导入会话携带工具面参数；工作区级策略推送已覆盖 |
| HTTP-date 形式的 `Retry-After`                                                          | 只解析整数秒                                  | 3215 的退避提示；日期形式罕见                               |
| 文件名 `normalize("NFKC")`                                                              | 不做 NFKC                                     | 只影响兼容字符的落盘文件名                                  |

## 9. cua-permission

本构建里 Computer Use（`@zcode/zcode-cua`）是占位包，Helper 无法安装或启动。`getStatus` / `restartHelper` fail closed，返回 Node Host 在同样条件下的结果：非 macOS 为 `CUA permissions are only available on macOS.`，macOS 为 `ZCode Computer Use is not enabled (plugin off or not product mode).`（`{available:false, reason}` / `{ok:false, reason}`），从不拉起 Helper。`cua-pip-session` 不在 Renderer 契约里，保持不实现。

## 10. 验收

- `pnpm exec tsx scripts/rust-host/automation-fixtures.ts --check`：用真实 TS 实现（`AutomationRepo`、`automationCron.ts`、`AutomationService`）在固定时钟、固定时区下生成对拍 fixture。
- `cargo test -p zcode-host-services automation`：仓库每一步的返回值与最终表内容与 TS 相同；cron 与 scheduleRule 的下次触发时间与 croner / TS 相同（含夏令时）；service 的校验文案相同；反向请求的参数校验与结果形状。
- 真实 App（`scripts/rust-host/e2e/scenarios/automation-run-now.mjs`）：在设置页创建定时任务，点“立即运行”，mock 模型收到 prompt，侧栏出现带定时标记的新会话，运行历史出现成功的 manual run。
- `pnpm exec tsx scripts/rust-host/bots-fixtures.ts --check`：Bot 文案表、命令解析、配置归一化、旧版配置/状态导入后的 schema 输出、Node 加密的凭据向量，均由真实 TS 实现生成。
- `cargo test -p zcode-host-services bots`：上述对拍；`pbbp2.Frame` 编码与 protobufjs 逐字节相同；凭据文件与 Node 互通、损坏时备份并拒绝覆盖；运行锁互斥；127.0.0.1 上的飞书 mock（开放平台 HTTP + 长连接 WS）走完扫码注册轮询、保存 Bot 时回填应用名、`/help`（未绑定）→ `/bind` → `/status` → 普通消息（新建会话、后台发送、终态回推卡片）→ 重投去重 → 未移植命令，以及 automation 回推到绑定的群聊。
