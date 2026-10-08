# Rust Host：跨区域收尾（integration）

日期：2026-10-09。上级规格：[rust-host.md](rust-host.md)。分支：`feat/rust-host-p-integration`（基于 `feat/rust-host` @ `aaf6f9d`）。

各区域（[agent](rust-host-agent.md)、[tasks](rust-host-tasks.md)、[automation](rust-host-automation.md)、[account](rust-host-account.md)、[extensions](rust-host-extensions.md)、[workspace](rust-host-workspace.md)、[resilience](rust-host-resilience.md)）各自完成后，留下了几处需要跨区域接线的缺口。本文只处理这些缺口；每一项都以 Node 实现为准，并在对应区域规格的偏差表里删除或改写原来的偏差行。

## 1. 范围

| #   | 项                           | Node 依据                                                                                                                | 涉及区域                             |
| --- | ---------------------------- | ------------------------------------------------------------------------------------------------------------------------ | ------------------------------------ |
| 1   | 闲时任务的真实凭据           | `offPeakRuntimeModel.ts`（`resolveOffPeakCredentials`）、`offPeakServerClient.ts`、`node.ts` 的 `resolveAccountProvider` | automation（off-peak）、account      |
| 2   | settings-sync 手动导入       | `settingsSyncService.ts`（`detect` 带 categories、`importSelected`）                                                     | account（settings-sync）、extensions |
| 3   | 遥测                         | `@zcode/rpc` 的 `NetworkTelemetryChannelServer`、`hostNetworkTelemetry.ts`、`hostSessionCreateTelemetry.ts`              | host-core、automation                |
| 4   | 动态工作流门禁的所有者       | `zcodeAgentService.ts` 的 `resolveDynamicWorkflowGate`、`node.ts` 的 `resolveDynamicWorkflowClientConfig`                | agent、account、tasks、automation    |
| 5   | 反向请求错误带 `data`        | `zcodeAgentService.ts` 的 `client.respondError({code, message, data})`                                                   | agent、automation                    |
| 6   | 统一 `generateWorkspaceText` | `zcodeAgentService.generateWorkspaceText`（git 提交信息经它调用）                                                        | agent、workspace                     |

不在本文：bots、远端 workspace（`services/src/remote`）、Rust CLI。

## 2. 产品规则

### 2.1 闲时任务的真实凭据

1. **唯一解析入口**：`off_peak::credentials::CredentialSource::resolve()`（`resolveOffPeakCredentials`）。取号客户端、`getCodingPlanSupport`、模型选择、`off-peak-run` 派发的逐请求鉴权都调它；不缓存，每次按当前账号状态重新解析。
2. **mock**：`ZCODE_OFFPEAK_MOCK=1` 时与 Node 相同，返回确定性的 mock 凭据（`ZCODE_OFFPEAK_MOCK_NO_PLAN=1` 时 `connection_unavailable`）；mock 网关只接收这份 placeholder。
3. **真实链**（最多两轮，两轮都不稳定时 `selection_changed`）：
   1. **选中的账号 Provider**：provider runtime 当前快照里**在 Registry 中**、`access.type = "zhipu-account"`、`mode ∈ {individual-coding-plan, team-coding-plan}` 的 provider 恰好一个，否则视为没有；`access` 经 account 区域的 `AccountRequestAuth::resolve_access_current` 换成当前账号的动态访问事实（`{type, family, planKind, …}`），解析不到也视为没有。`baseURL` 取 `effective_config.api.baseUrl`。
   2. 没有 → `connection_unavailable`；`planKind = start-plan` → `start_plan_not_supported`；个人套餐 → `zai-personal` / `bigmodel-personal`；Team → `zai-team` / `bigmodel-team`（带 organizationId / projectId）。
   3. `oauth:active_provider` 必须等于 family，否则 `provider_identity_mismatch`（不能把一家的 JWT 和另一家的 key 拼在一起）。
   4. `zcodejwttoken`（trim）为空 → 凭据缺失 `jwt`（support 原因 `jwt_missing`）。
   5. 再解析一次 Provider 与 active provider，与第一次的指纹（`JSON.stringify(provider)`）不一致则重来。
   6. Coding Plan key：`AccountRequestAuth::resolve_current(providerId, access)`（个人 key 走 CodingPlanKeys 缓存，Team 走 org/project 运行时 key）。拿不到 → 凭据缺失 `codingPlanApiKey`（support 原因 `connection_unavailable`）。
4. **错误分类**（读凭据库失败等非分类错误：`getCodingPlanSupport` 原样报错，派发按 transient，同 Node）：`OffPeakCodingPlanUnavailableError(reason)` 的消息 `off-peak selected coding plan unavailable: <reason>`；凭据缺失的消息 `off-peak requires zcode login (jwt missing)` / `off-peak requires a coding plan provider api key`。两类在派发里都是 `failureKind:"permanent"`。
5. **请求形状**（`offPeakServerClient.ts`）：`<origin>/api/v1/off-peak<path>`，头依次为 ZCode 来源头（account 区域的 `SourceHeaders`，`HTTP-Referer` 为缺省 origin，同 `buildZCodeSourceHeaders`）、`content-type`（有 body 时）、`authorization: Bearer <jwt>`、`x-coding-plan-api-key`、BigModel Team 且 org 与 project 都有时的 `bigmodel-organization` / `bigmodel-project`，最后补 `x-request-id`。错误响应的日志只记 `bizCode`、`credentialKind`、`httpStatus`、`method`、`path`、`requestId`（优先响应头的 `x-request-id`），不记凭据。
6. **逐请求鉴权**（`buildOffPeakRequestAuth`）：`{apiKey: jwt, headers: {Authorization, X-Coding-Plan-Api-Key, X-Off-Peak-Ticket-ID, 以及 Team 身份头}}`。
7. **依赖注入**：automation 区域的 `Area` 增加 `with_account(AccountLink{auth, source_headers})`，install 时由 account 栈提供；没有注入（单元测试）时真实链按“没有账号 Provider”处理。

### 2.2 settings-sync 手动导入

1. **入口**：`detect({workspacePath?, intent:"manualImport", categories})` 只看 categories 里第一个命中的类别，顺序 skills → commands → plugins → mcpServers（同 Node 的 if 链）；其余请求（含首启）返回 `{agents: []}`。`importSelected({workspacePath?, selections})` 逐条 selection 顺序执行，汇总 `successCount / skippedCount / failedCount / taskResults`；未知类别记一条 `skipped`（skippedCount 1）。
2. **来源表**（与 Node 常量逐项相同）：skills 16 个 agent、commands 15 个、plugins 15 个、MCP 12 个（含 `.claude/settings.json` 与项目 `.mcp.json`、Codex `config.toml`、OpenCode `opencode.json` 的 `mcp` 段）。用户级根在 `HOME`（`USERPROFILE` 次之，都空时 `os.homedir()`），项目级根只在带 `workspacePath` 时加入。
3. **扫描**：
   - skills：每个根用有界遍历（extensions 区域的 `walk::walk`，与 Node `walkSkillMarkdownPaths` 同规则）找 `SKILL.md`，按 `localeCompare` 排序，跨根按路径去重；名称、版本读 frontmatter（YAML 解析失败时按 `name:` / `version:` 行宽松读取，缺省用目录名）。
   - commands：栈式遍历 `*.md`（跳过点名），名称为 `/` + 相对路径去掉 `.md`；描述与参数提示用 commands 区域的 `parse_command_file`。
   - plugins：根下一级的目录或软链（跳过点名），有 `.zcode-plugin` / `.claude-plugin` / `.codex-plugin` 下的 `plugin.json` 且 `name` 非空才算；id 为 `<name>@inline`。
   - MCP：读来源文件（`mcpServers` / OpenCode `mcp` 规整为 stdio/http / TOML `mcp_servers`），条目 `sourcePath = <file>#<name>`，去掉 `timeout`、`startup_timeout_sec`。
4. **冲突判定**（`skipReason`）：
   - skills / commands：目标路径已存在 → `targetExists`；目标根里已有同名（名称 trim + 小写）→ `sameNameExists`。目标根缓存一次后随导入追加。
   - plugins：目标目录已存在，或目标 scope 无配置路径 → `targetExists`；目标配置的 `plugins.dirs` 已有同 id → `sameNameExists`。
   - MCP：目标配置的 `mcp.servers` 已有同名 → `sameNameExists`；目标 scope 无配置路径时记 `sameNameExists`（同 Node）。
5. **目标**：`targetScope` 缺省取来源 scope（skills / commands 取候选自身的目标根）；`global` → `~/.zcode/{skills,commands,plugins}`、`~/.zcode/cli/config.json`；`project` → `<workspace>/.zcode/{…}`、`<workspace>/.zcode/config.json`；没有 workspace 的 `project` 记 skipped。
6. **导入方式**：`importMode` 缺省 `symlink`。目录（skill、plugin）软链指向绝对路径（Node 在 Windows 用 junction，见第 5 节），文件（command）软链（Windows 用硬链接，失败退回复制）；`copy` 为递归复制且目标存在即失败。plugin 导入后把目标目录的绝对路径追加到配置的 `plugins.dirs`（已有则不写）；MCP 把配置合并进 `mcp.servers`。
7. **写配置**：`~/.zcode/cli/config.json` 与 `<workspace>/.zcode/config.json` 都经 extensions 区域的 `cli_config::update`（跨进程锁 + 原子写），读取宽松（坏 JSON 视为 `{}`），写出 `JSON.stringify(value, null, 2) + "\n"`。这是两份文件在 Rust Host 内唯一的写路径。
8. **结果**：每类结果项字段与键序同 Node（`name, path, sourceScope, status, [skipReason], [version]` 等）；任务状态 `failed`（有失败）> `success`（有导入）> `skipped`。

### 2.3 遥测

1. **RPC 网络遥测**：host-core 的分发层在每次 `call` 完成时记一条 `{transport:"rpc", interface:"<channel>.<method>", durationMs, ok, errorKind?, attempt:1}`（`durationMs` 四舍五入、不小于 0）。`errorKind` 按错误消息小写匹配：含 `timeout` / `timed out` → `timeout`；`dns` / `getaddrinfo` / `enotfound` → `dns_failure`；`econnreset` / `connection reset` / `econnrefused` → `connection_reset`；其余 `other`。未知 channel 不记（同 Node，错误在包装之前产生）。契约桩（`fault.host.notImplemented`）不记。
2. **批量上报**：`RpcTelemetry` 是观测缓冲的唯一所有者。每 30 s 或累计 200 条时发一批 `network-telemetry-batch {observations}`（一批最多 200 条）；Host 退出前冲刷一次。上报到哪里由 Main 决定（Node 与 Rust Host 相同的 `ingestHostNetworkObservations`），Host 不出网。
3. **会话创建遥测**（`reportHostSessionCreate`）：只在 Host 自己**新建**会话并完成首发后上报 `session-create-telemetry`：automation 派发不带 `targetTaskId` 时 `create_source:"automation_scheduled"`；闲时任务 `init` 分支 `create_source:"automation_idle"`。字段：`elementName:"session_create"`、`eventRegion:"app"`、`eventType:"result"`、`talkId`（会话 id）、`messageId`（首条输入的 id）、`context {clientTimezone, clientLanguage, screenResolution:""}`、`eventExtraDetail {create_source, client_kind:"desktop", workspace_kind, remote_kind}`（`resolveWorkspaceTelemetryDetail`）。上报失败只丢弃，不影响派发结果。
4. 测试与 E2E 的遥测端点为空或指向 127.0.0.1；Rust Host 本身不直连任何遥测端点。

### 2.4 动态工作流门禁

1. **唯一写入方**：`coding-plan-subscription` 的 `CodingPlanSubscription::dynamic_workflow_config(false)`（Node：`codingPlanSubscriptionService.getDynamicWorkflowClientConfig()`）。install 时由 integration 段把它登记为 `Extensions::set_dynamic_workflow_gate`，`enabled === true` 即开启；之后 runtime 在 Host 进程内只判定一次（失败按关闭、不重试，扩展点已有语义）。
2. **本地覆盖**：`ZCODE_DYNAMIC_WORKFLOW_MODE`（Main 按打包类型决定后写进 Host 环境）优先于远端配置；不进入 CLI spawn 环境（CLI 不读它，Node 也只在 Host 判定）。
3. **请求面**：门禁开启时，所有 Host 发出的会话创建 / 恢复都带 `dynamicWorkflowEnabled: true`（与 Node 经 `zcodeAgentService.createSession/resumeSession` 的统一注入相同）：agent 区域的 legacy create/resume、V4 `createSession` 信封、tasks 区域的 `createTask`（legacy 与 V4）/ `resumeTask`、automation 驱动的 create/resume（Bot 建会话与普通恢复也走这条路径）、Bot 直接发出的 `session/resume`（`/new` 继承配置、任务态 `/model` / `/think`，见 [rust-host-bots.md](rust-host-bots.md) §2.9）；进程就绪前推送 `workspace/updateDynamicWorkflowPolicy`。导入会话（Claude 导入、对话分享导入）不带工具面参数（Rust CLI 拒绝）。Host 自己发起的 create / resume（tasks、automation、bots）从 `Extensions::session_tool_flags(target)` 取这组参数；agent 区域的 `gates` 读同一组扩展点。

### 2.5 反向请求错误带 `data`

1. `ReverseReply` 增加 `ErrorWithData {code, message, data}`；runtime 写出 `{"id", "error": {code, message, data}}`。原 `Error {code, message}` 不变（不写 `data`）。
2. 使用点（与 Node 逐条对应）：
   - `offPeak/create`、`offPeak/list` 参数校验失败：`-32602` + 固定文案 + `data = zod error.flatten()`（`{formErrors, fieldErrors}`，消息为 zod 4 的英文默认文案）。
   - 闲时反向请求兜底异常：`-32603 Internal off-peak service error` + `data {errorCode: "offpeak_internal_error"}`。
   - `automation/create|update|list|checkTaskBinding|delete` 参数校验失败：`-32602` + 原文案 + flatten `data`。
3. flatten 由 `support::zod` 的严格对象校验器生成：字段按 schema 顺序校验，未知键在最后报 `Unrecognized key(s)`，`refine` 只在基础校验全部通过后执行；与真实 zod 的输出由 fixture（`scripts/rust-host/zod-flatten-fixtures.ts`）对拍。

### 2.6 统一 `generateWorkspaceText`

1. 只保留 agent 区域的 `AgentApi::generate_workspace_text`（写 client、`GenerateText` 钩子、`requestTimeoutMs`、取消守卫）。
2. workspace 区域的提交信息生成器（`AgentTextGenerator`）改为持有 `AgentApi` 并调用它；删除 `workspace::generate_workspace_text` 与其重复的取消守卫。

## 3. 状态所有者

| 状态 / 能力                                 | 唯一所有者                                                                  |
| ------------------------------------------- | --------------------------------------------------------------------------- |
| 闲时凭据快照                                | 无缓存；`off_peak::credentials::CredentialSource` 每次按账号状态解析        |
| OAuth 凭据、Coding Plan key、账号访问事实   | account 区域（`CredentialStore`、`CodingPlanKeys`、`AccountRequestAuth`）   |
| `config.json` 的 plugins.dirs / mcp.servers | 文件本身；Rust Host 内经 `extensions::cli_config::update` 持锁读改写        |
| 导入的 skills / commands / plugins 目录     | `~/.zcode/*`、`<workspace>/.zcode/*` 文件系统（Host 不缓存）                |
| RPC 观测缓冲                                | host-core `RpcTelemetry`（每 30 s / 200 条冲刷，退出前冲刷）                |
| 动态工作流开关                              | `CodingPlanSubscription::dynamic_workflow_config` → `Extensions` 进程内缓存 |
| 会话工具面参数                              | `Extensions::session_tool_flags`                                            |
| 生成文本请求与取消                          | `AgentApi::generate_workspace_text`                                         |

## 4. 事件顺序

### 4.1 闲时凭据解析

```text
caller（取号 / support / 派发）
  └► CredentialSource::resolve
       ├─ mock? ──► placeholder
       └─ attempt 0..2:
            providers.snapshot ─► 唯一 zhipu-account coding-plan provider ─► auth.resolve_access_current
            credentials.load(oauth:active_provider) ≟ family ─✗► provider_identity_mismatch
            credentials.load(zcodejwttoken) ─空► jwt 缺失
            再取 provider + active provider ─指纹不同► 下一轮
            auth.resolve_current(providerId, access) ─空► key 缺失
            ─► {jwt, key, kind, family, providerId, org?, project?, baseURL?}
       两轮都不稳定 ─► selection_changed
```

### 4.2 RPC 遥测

```text
Renderer ─call─► host-core serve ─► channel.call ─► reply
                       │                        └► RpcTelemetry.record（Instant 计时）
                       │                               ├─ 满 200 条 ─► flush
                       │                               └─ 首条记录时启动 30 s 定时 flush
Host 退出 ─► HostCore.shutdown ─► RpcTelemetry.flush ─► control: network-telemetry-batch ─► Main ingest
```

### 4.3 settings-sync 手动导入

```text
UI detect(categories:[c]) ─► 扫描来源根 ─► 候选（按路径去重）─► 读目标根 / 配置判定 skipReason ─► agents[]
UI importSelected(selections) ─► 每条 selection：重新扫描该类候选 ─► 过滤 agent / sourceScope / 路径
   └► 逐候选：目标根 ─► skipReason? ─► mkdir ─► symlink / copy ─► （plugin）cli_config::update 追加 dirs
                                                                └► （MCP）cli_config::update 合并 servers
```

## 5. 偏差表（本文新增）

| Node 行为                                                       | Rust Host                                                   | 理由                                                                    |
| --------------------------------------------------------------- | ----------------------------------------------------------- | ----------------------------------------------------------------------- |
| 闲时 mock 网关准入后代理到用户 Coding Plan 的真实上游（开发用） | 回固定的 Anthropic 响应                                     | 仅开发演示；测试与 E2E 不能连真实上游                                   |
| 选中 Provider 的静态 access 不合 strict schema 时抛 ZodError    | 视为没有选中的 Provider（`connection_unavailable`）         | 只会出现在损坏的内置配置上；结果同样不可派发                            |
| settings-sync 写 `config.json` 用无锁 `writeFile`（0644）       | 持跨进程锁、原子写、0600                                    | 与 extensions 区域的其他写入方共用一条写路径，避免并发丢更新            |
| RPC 调用被取消（连接关闭、PromiseCancel）后仍记一条观测         | 被取消的调用不记                                            | Rust 直接中止调用 future，没有完成时间                                  |
| 契约桩（未实现成员）不存在                                      | 桩的 `NotImplemented` 应答不记遥测                          | 桩是 Rust Host 的过渡物，不是业务调用失败                               |
| `session_create` 的 `messageId` 是派发时新生成的 traceId        | 取首条输入的 inputId（automation 为 runId，闲时为 traceId） | Rust 驱动以 inputId 作首条消息的标识；两者都唯一标识本次首发            |
| settings-sync 在 Windows 上用 junction 导入目录                 | 目录软链（`symlink_dir`）                                   | 标准库不提供 junction；没有开发者模式时导入记 failed，copy 模式不受影响 |
| Codex `config.toml` 的日期时间值经 `TomlDate` 写成 ISO 字符串   | 写成 TOML 原文的日期时间字符串                              | 只影响 MCP 配置里极少见的日期字段                                       |

## 6. 验收

- `cargo test -p zcode-host-services --lib off_peak::credentials`：mock 与无账号栈、支持矩阵（个人 / Team / start plan / 无选中）、strict access、错误文案与 support 原因、Team 身份头只在组织与项目都有时发。
- `cargo test -p zcode-host-services --test off_peak_account`：临时 HOME 的已登录 Z.ai 账号（Node 加密格式）+ 127.0.0.1 mock 后端：`getCodingPlanSupport` 为 `zai-personal`，逐请求鉴权与取号请求带 JWT、Coding Plan key、来源头与 `x-request-id`；active provider 换成另一家时为 `provider_identity_mismatch`；JWT 删除后为 `jwt_missing`，取号在发请求前失败。
- `pnpm exec tsx scripts/rust-host/settings-sync-fixtures.ts --check` + `cargo test -p zcode-host-services --lib settings_sync`：同一棵来源树上真实 TS 服务与 Rust 的 `detect`（四类）、`importSelected`（symlink / copy、global / project、冲突、未知类别）结果和落盘文件逐项相同。
- `pnpm exec tsx scripts/rust-host/zod-flatten-fixtures.ts --check` + `cargo test -p zcode-host-services --lib support::zod`：闲时与 automation 反向请求参数的 flatten 与 zod 4 相同。
- `cargo test -p zcode-host-core`：RPC 观测的字段、错误分类、桩不记、200 条立即冲刷、shutdown 冲刷。
- `cargo test -p zcode-host-services --lib integration`：`session-create-telemetry` 的消息形状（本地、远端 identity、非远端格式的 identity）；coding-plan 门禁（`ZCODE_DYNAMIC_WORKFLOW_MODE=alwaysOn` / `disabled`，假 CLI）开启时进程就绪前推 `workspace/updateDynamicWorkflowPolicy`、tasks 的 `session/resume` 带 `dynamicWorkflowEnabled`，关闭时都不发；`session_tool_flags` 对远端 identity 只给动态工作流。派发点（automation 只在不带 `targetTaskId` 时、闲时只在 `init` 时上报）由代码审查确认，E2E 走过这两条派发路径但不观测 Main 侧遥测。
- 桌面 E2E：`core`、`automation-offpeak`、`workspace-commit-message`、`extensions-*`、`account-logged-in`，以及新增的 `integration-settings-sync`（设置页 → 技能 → 从其他 Agent 导入 → 选中 Claude Code 的技能 → 导入 → 技能列表出现该技能，`~/.zcode/skills` 下是指向来源的软链）。
