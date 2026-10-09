# U4：一个 App 一个 Host

日期：2026-10-09。上级规划：[rust-unified-host.md](rust-unified-host.md) 的 U4、8.1、8.5、9.1 与 D-U7。前置：[rust-u3-single-process.md](rust-u3-single-process.md) A 期（runtime 在 Host 进程内）。

## 1. 目标与范围

Rust Host 从“每个窗口一个进程”改为“每个 App 一个进程”，并收回今天放在 Main 里的业务状态。完成后，Rust 开关下的桌面只有三类进程：Main、Renderer、一个 Host。

- **只改 Rust 开关下的路径**（`ZCODE_DESKTOP_HOST=rust` 且非打包构建）。Node Host 的路径、Node scheduler、BroadcastHub、`taskRealtimeBus` 的代码保持不变，到 U8 一并删除（D-U8）。
- 产品今天只有一个主窗口。本期让 Host 正确支持多个窗口接入，但不新增“打开第二个主窗口”的产品入口；多窗口只通过开发开关验证（2.9）。

## 2. 规则

### 2.1 Host 的生命周期

| 事件                        | 行为                                                                                     |
| --------------------------- | ---------------------------------------------------------------------------------------- |
| App ready                   | Main 采集登录 shell 环境（等待上限与今天相同），随后拉起唯一的 Host，发一次 `init-local` |
| 窗口 dom-ready（含 reload） | Main 为该窗口签发一个 attachment endpoint；Host 尚未就绪时，等就绪后签发                 |
| 窗口关闭或销毁              | 只关闭这个窗口的连接；Host 继续运行                                                      |
| App 退出                    | Main 发 `dispose`，最多等 3.5 s 后结束进程（不变）                                       |
| Host 意外退出               | 见 2.8                                                                                   |

- `init-local` 是 App 级的：`runtimeProcessEnvPatch` 是 App 启动时采集的环境；`agentWarmupTargets` 是启动 workspace；不再携带 `hostId`（2.5）。`databaseStartupId` 是 App Host 的固定标识。
- 没有窗口时 Host 照常工作（调度、Bot、远端连接都不依赖窗口）。

### 2.2 窗口身份

- `issue-attachment-endpoint` 增加 `windowId`（主窗口的窗口 id，与今天 `sourceWindowId` 用的是同一个值）。
- Host 把 `windowId` 记在 attachment 上，连接建立后进入该连接的调用上下文（`CallContext.window_id`）。
- 一个窗口可以有多条连接（reload 前后的新旧连接、同一窗口的多个 attachment），它们的窗口身份相同。
- 需要回到某个窗口的 Host→Main 请求（`browser-execute-request`、Bot 重连通知）带上发起调用的 `windowId`；Main 按它路由，窗口已不存在时回失败。

### 2.3 广播与 claim

所有者从 Main 的 BroadcastHub 改为 Host 的 `broadcast` 服务；Main 不再中继，也不再收到广播。

| 发送方                      | 同一窗口的连接                | 其他窗口的连接                        |
| --------------------------- | ----------------------------- | ------------------------------------- |
| 某个窗口的连接              | 原样（没有 `sourceWindowId`） | 带 `sourceWindowId` = 发送方的窗口 id |
| Host 内部（如账号失效通知） | 所有连接都收到原样的消息      | 同左                                  |

- UI 用“消息没有 `sourceWindowId`”判断是不是自己窗口发的，这条语义保持不变。
- 没有窗口身份的连接（U7 的手机远控）按“另一个窗口”对待：收到的窗口广播带 `sourceWindowId`；它发出的广播到达窗口时 `sourceWindowId` 为 `0`（窗口 id 从 1 开始，`0` 表示来源不是窗口）。
- claim 表在 Host 内，规则与 BroadcastHub 相同：先到先得；`reserved` 5 s 过期；只有持有 token 的申请方可以 commit 或 release；`committed` 永久保留；上限 1024 条，超出时淘汰最早的。申请方是一条连接，连接关闭时回收它尚未 commit 的 reservation。
- `acquireClaim` 不再需要 2 s 的等待上限：结果在 Host 内同步得出。

### 2.4 调度

所有者从 Node scheduler 进程改为 Host 内 automation 域的 `Scheduler`（一个任务）。状态机与 `packages/desktop/src/scheduler` 相同：

- **轮次**：启动后先回收中断的闲时任务（`recoverInterrupted`），随后立即一轮，之后每 20 s 一轮。同一时刻只有一轮在执行；执行期间收到的唤醒合并为“结束后立即再来一轮”。
- **唤醒来源**：手动运行入列、闲时任务变为可调度、一次派发结算完成。
- **一轮的内容**：`claimDue`（事务内把到期且未认领的任务标为认领）→ 逐个处理；`claimManualRuns` → 逐个派发；闲时任务 `claimDue` → 逐个派发；上报执行中的闲时任务数。
- **misfire**：非重试、且计划时间早于现在超过 5 分钟的任务，记为 skipped（原因 `computer_asleep_or_app_not_running`）并排到下一个周期；一次性任务直接终结。
- **派发**：直接调用 Host 内已有的派发器（今天处理 `cron-run` / `off-peak-run` 的那段代码），不经 Main。
- **结算**：成功则记 dispatched 并计算下一次时间；失败按 `transient` / `permanent` 退避或终结；手动运行只累计次数，不推进周期。闲时任务的失败退避表在内存中。
- **退出**：`dispose` 时释放仍在途的认领。
- 认领的 SQL 语义与 Node 的 repo 相同（`BEGIN IMMEDIATE`，认领超时后的回收规则不变）。因此同一份 tasks-index 被另一个 App 实例的 Node scheduler 同时使用时，一个任务仍只会被认领一次。
- 轮询用单调时钟的定时器，到期判断用墙上时钟。休眠唤醒后最迟 20 s 进入下一轮，由 misfire 规则处理错过的窗口。
- 执行中的闲时任务数变化时，Host 发 `off-peak-active-count` 给 Main，Main 据此维持防休眠（与今天 scheduler 上报的用途相同）。

Rust 开关下 Main 不再启动 scheduler 进程，也不再转发 `cron-run` / `off-peak-run`。

### 2.5 任务实时总线

只有一个 Host 时，`taskRealtimeBus` 协调的对象只剩它自己。Rust 开关下：

- Main 不把 Host 注册到 `taskRealtimeBus`，`init-local` 不带 `hostId`，Host 不建立到 Main 的实时桥。
- run 的 owner 判断、stale run 防护、owner 相关错误码留在 Host 内，行为不变（AGENTS.md 的 owner / lease 规则）。
- 远端 workspace 的 `sendPrompt` 镜像（今天经 Main 绕回同一个 Host 并固定回失败）改为在 Host 内直接应答，错误码不变。

### 2.6 设置

`setting.json` 的唯一写入方是 Host 的 `SettingStore`。

| 方向        | 消息                                          | 说明                                                                                     |
| ----------- | --------------------------------------------- | ---------------------------------------------------------------------------------------- |
| Main → Host | `setting-write{requestId, patch}`             | Main 原来直接写文件的位置（窗口尺寸、缩放、更新相关字段、endpoint origin）都改为这条请求 |
| Host → Main | `setting-write-result{requestId, ok, error?}` |                                                                                          |
| Host → Main | `setting-changed{setting}`                    | 任何一次成功写入之后推送完整快照；Main 的设置缓存只由它更新                              |

- Host 启动之前的读取（数据目录、硬件加速、启动 workspace）仍然直接读文件：这些值决定 Host 怎么启动。
- Host 不可用时（启动中、重启中）Main 的写入请求等待 Host 就绪；超过 10 s 记 warn 并放弃这次写入，不回落到直接写文件。
- App 退出与安装更新前的写入，先等结果再 `dispose` Host。

### 2.7 Bot 远端运行时

- 连接状态查询改为 Host 直接读自己的远端注册表，不经 Main。
- 重连仍由 Main 的远端会话管理器执行（它持有 SSH 连接器，U5 替换）。请求带 `windowId`；通知发给发起连接的那个窗口。
- Main 的 `ensureHostListener` 不再把第一个窗口固定为 Host 的归属窗口，连接结果按请求里的窗口路由。

### 2.8 Host 意外退出

- 重启策略是 App 级的：60 s 内最多 3 次，退避 500 ms / 2 s / 5 s（数值不变）。
- 顺序：所有主窗口收到连接状态（断开）→ Main 重启 Host → Host 就绪后 Main reload 所有主窗口 → 每个窗口在 dom-ready 时重新接入。
- 放弃重启后，所有主窗口显示启动失败页；任意一个窗口的 Retry 触发一次 App 级重启，Exit 退出 App。
- 数据库启动状态（`database-startup-state`）向所有主窗口扇出；任意窗口的 retry / exit 控制都被接受。

沿用 reload 而不是原地重连：桌面是 `desktop-continuous` 语义，Renderer 没有“断线后补齐”的恢复路径，reload 是今天已验证的做法。

### 2.9 多窗口的开发入口

`ZCODE_DEV_EXTRA_MAIN_WINDOW=1` 且非打包构建时，Main 在第一个主窗口就绪后再创建一个主窗口。只用于 E2E。

## 3. 状态所有者

| 状态                              | 今天（Rust 开关下）                | U4 之后                         |
| --------------------------------- | ---------------------------------- | ------------------------------- |
| Host 进程句柄、重启计数           | Main，按窗口                       | Main，App 级                    |
| 窗口 → attachment                 | Main `windowHostProcessMap`        | Main（窗口表）与 Host（连接表） |
| 广播扇出、claim 表                | Main BroadcastHub                  | Host `broadcast`                |
| 调度状态机（认领、misfire、退避） | Node scheduler 进程                | Host automation `Scheduler`     |
| run lease、owner 路由             | Main `taskRealtimeBus` 加 Host     | Host                            |
| `setting.json` 写入               | Main 与 Host（跨进程文件锁）       | Host                            |
| Main 的设置缓存                   | Renderer 的 `SyncAppSettings` 通知 | Host 的 `setting-changed` 推送  |
| 远端连接注册表                    | Host（窗口级）                     | Host（App 级）                  |
| SSH 连接器、远端 route 表         | Main，按窗口                       | 不变（U5 迁走）                 |

## 4. 控制消息

| 变化                               | 消息                                                                                                                                                                                                                                                                                          |
| ---------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 新增                               | `issue-attachment-endpoint.windowId`；`setting-write` / `setting-write-result` / `setting-changed`；`off-peak-active-count`；`browser-execute-request.windowId`；Bot 重连请求的 `windowId`                                                                                                    |
| 不再使用（Rust Host 删除处理代码） | `broadcast`（双向）、`broadcast-claim-*`、`task-realtime-*`、`task-stream-op-publish`、`task-run-lease-*`、`task-owner-command-*`、`session-message-*`、`session-route-announce`、`cron-run` / `off-peak-run` 及其结果、两条 `*-scheduler-wake-request`、`bot-remote-workspace-runtime-port*` |

协议定义（`packages/shared`）中这些消息保留到 U8，因为 Node Host 仍在使用。

## 5. 事件顺序

```mermaid
sequenceDiagram
    participant M as Main
    participant H as Host（App 级）
    participant W1 as 窗口 1
    participant W2 as 窗口 2
    M->>H: spawn，init-local（一次）
    H-->>M: host-endpoint-ready，database-startup-state(ready)
    H->>H: Scheduler 启动：回收中断任务 → 第一轮
    W1-->>M: dom-ready
    M->>H: issue-attachment-endpoint{windowId: 1}
    H-->>M: {url, capability}
    M->>W1: 交付
    W1->>H: WebSocket attach
    W2-->>M: dom-ready
    M->>H: issue-attachment-endpoint{windowId: 2}
    H-->>M: {url, capability}
    M->>W2: 交付
    W2->>H: WebSocket attach
    W1->>H: broadcast.send(message)
    H-->>W1: message（没有 sourceWindowId）
    H-->>W2: message（sourceWindowId = 1）
    W2-xH: 窗口 2 关闭，连接断开
    H->>H: 回收窗口 2 未 commit 的 claim；其余不变
    H->>H: 到期的 automation：认领 → 派发 → 结算（不经 Main）
```

## 6. 验收

- Rust 测试：
  - 广播按窗口身份扇出（同窗口原样、其他窗口带 `sourceWindowId`）；claim 的先到先得、过期、连接关闭回收。
  - 调度状态机：到期认领、misfire 跳过、一次性任务终结、失败退避与上限、手动运行、闲时任务的回收与退避；两个调度器同时认领时一个任务只派发一次。
  - 没有任何连接时，到期的 automation 被派发并结算。
- 桌面 E2E（Rust 开关）：
  - 进程表中恰好一个 Host，没有 scheduler 进程，没有 CLI 子进程。
  - 两个窗口（2.9）接入同一个 Host；一个窗口切换语言，另一个窗口跟随；关闭第二个窗口后第一个窗口继续正常对话。
  - reload 前后 Host 的 pid 不变；杀掉 Host 后自动重启，窗口恢复；连续失败后出现失败页，Retry 可恢复。
  - 定时的 automation 到点运行。
  - Main 发起的设置写入（缩放）落盘，Renderer 读到新值。
- 现有 E2E 场景与 `pnpm test:rust-host` 全部通过；owner 相关错误码的现有测试不变。

## 7. 本期不做

| 项                                 | 去向                                                                 |
| ---------------------------------- | -------------------------------------------------------------------- |
| 每个窗口各自的 workspace 列表      | 产品出现第二个主窗口时再定：今天的列表是 Host 级的，多窗口会互相可见 |
| Host 自己采集登录 shell 环境       | U5（远端 Host 也需要）                                               |
| SSH 连接器与远端 route 表迁入 Host | U5                                                                   |
| 手机远控连接的广播与窗口路由       | U7                                                                   |
| Host 重启后不 reload、原地重连     | 需要 Renderer 的恢复路径，另行立项                                   |
