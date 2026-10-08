# Rust Host：账号、网络与账号增值服务

日期：2026-10-08。上级规格：[rust-host.md](rust-host.md) 第 5 节 R1（账号部分）与 R4。分支：`feat/rust-host-p-account`。状态：已实施（偏差见第 5 节）。

## 1. 目标与范围

把 Node Host 的账号栈搬到 Rust Host，使登录、账号 provider、额度与套餐、反馈在两种 Host 上表现一致。覆盖的 channel：

| channel                        | 成员数 | Node 来源                                                       |
| ------------------------------ | ------ | --------------------------------------------------------------- |
| `credential`                   | 3      | `credential/credentialService.ts`                               |
| `oauth`                        | 13     | `oauth/**`                                                      |
| `onboarding-record`            | 9      | `onboarding/onboardingRecordService.ts`                         |
| `provider-settings`            | 15     | `model-provider/providerFacadeServices.ts`、`packages/provider` |
| `provider-provisioning-target` | 1      | `model-provider/providerProvisioningTarget.ts`                  |
| `client-config`                | 1      | `client-config/clientConfigService.ts`                          |
| `client-scenes`                | 1      | `client-scenes/clientScenesService.ts`                          |
| `settings-sync`                | 6      | `settings-sync/settingsSyncService.ts`                          |
| `usage-stats`                  | 8      | `usage-stats/**`                                                |
| `coding-plan-subscription`     | 29     | `coding-plan-subscription/**`、`bigmodel/**`                    |
| `feedback`                     | 15     | `feedback/**`                                                   |

另含不属于 channel 的部分：Host 的网络客户端（ApiClient）、账号 provider 叠加层、CLI 集成（Account Config 同步、反向请求、spawn 环境）、官方 MCP 凭据源、遥测环境。

## 2. 产品规则

### 2.1 凭据（`credential`）

1. 文件 `<appConfigDir>/credentials.json`（`appConfigDir = <ZCODE_DATA_BASE_DIR 或 HOME>/.zcode/v2`），与 Node Host、TS CLI 共用。格式：`JSON.stringify(record, null, 2) + "\n"`，值为字符串。
2. 加密：`enc:v1:<iv>.<tag>.<ciphertext>`，三段都是无填充 base64url；AES-256-GCM，IV 12 字节，tag 16 字节；key = `sha256(secret)`，secret 取 `ZCODE_CREDENTIAL_SECRET`，否则 `zcode-credential-fallback:<platform>:<homedir>:<username>`（`platform` 为 Node 的 `darwin` / `linux` / `win32`；`homedir` 同 `os.homedir()`；`username` 在 Unix 取 `getpwuid(geteuid())`，Windows 取 `USERNAME`）。不带前缀的值按明文返回。
3. 解密失败：错误 `code = ZCODE_CREDENTIAL_DECRYPT_FAILED`，消息 `凭据解密失败：<原因>`，原因与 Node 相同（`密文格式非法`、`IV 长度非法`、`AuthTag 长度非法`、`密钥不匹配或密文已损坏`）。
4. 读改写整个过程持 `<file>.lock/` 目录锁（与 Node `withFileLock` 同一协议），锁内私有原子写（0600）。
5. 文件损坏（JSON 非法或值不是字符串）：按内容哈希备份为 `<file>.corrupt-<sha256 前 24 位>.bak`（排他创建、0600），报错 `ZCode credentials are corrupt: <path>`，拒绝覆盖。
6. key 为空串时报参数错误（zod `nonEmptyString`）。

### 2.2 网络客户端（ApiClient）

1. **端点 origin**：`setting.zcodeEndpointOrigin`（非空时）优先，其次环境变量 `ZCODE_BASE_URL`、`ZCODE_ENDPOINT_ORIGIN`，都没有时为 `https://zcode.z.ai`。每次请求重新读取设置。Main 已把 `ZCODE_ENV`、`ZCODE_BASE_URL`、`ZAI_OAUTH_ORIGIN`、`ZAI_BUSINESS_BASE_URL`、`ZAI_OAUTH_CLIENT_ID`、`BIGMODEL_API_BASE_URL` 解析后放进 Host 环境（`desktopRuntimeEnv.ts` `applySelectedZCodeEnvLinks`），Rust Host 只读环境，不依赖 Node 的编译期注入。
2. **改写**：URL 的 origin 等于 `https://zcode.z.ai` 且当前 origin 不同时，替换 origin，保留 path、query、fragment。
3. **来源头**：请求 origin 等于当前 origin 时，先放 ZCode 来源头（`User-Agent: ZCode/<ver>`、`HTTP-Referer`、`X-Title: Z Code@electron`、`X-ZCode-App-Version`、`X-Platform`、`X-Release-Channel`、`X-Client-Language`、`X-Client-Timezone`、`X-Os-Category`、`X-Os-Version`、已存在时的 `X-Device-Mid`），再叠加调用方的头；`HTTP-Referer` 为默认 origin 时改为当前 origin。
4. 每个请求补 `x-request-id`（调用方已给时不覆盖）。
5. **代理**：只用设置页的 `httpProxy`、`httpProxyNoProxy`、`httpProxyCaCertPath`，不读 shell 的 `HTTP(S)_PROXY`。No Proxy 规则同 `nodeApiNetwork.ts` `matchesNoProxy`（逗号或空白分隔、`*`、前导 `*.` / `.`、可选端口）。代理 URL 无 scheme 时补 `http://`，非 http/https 或无 host 时请求失败 `Configured Host proxy URL is invalid`，不回退直连。
6. **CA**：配置了 CA 文件时，信任集合为“内置根证书 + 该文件”（Node 的 `[...rootCertificates, customCa]`），代理与目标都用它。
7. **超时**：调用方给 `timeoutMs` 时，超时报 `Request timed out after <n>ms`。
8. **401**：响应 401 且请求带的是当前登录凭据（同 `isCurrentOAuthCredentialRequest`）时，触发与 Node 相同的“当前会话失效”退出（在会话变更队列里复核后才清理）。
9. `readApiJson`：非 2xx 时错误消息取响应体 `error` / `message` / `msg` / `detail`，否则原文或 `HTTP <status>`，并带 `status`。

### 2.3 OAuth 与会话（`oauth`）

行为逐条对应 `oauthService.ts`：

1. **provider**：`bigmodel`（order 0）、`zai`（order 1），`BIGMODEL_OAUTH_ENABLED` / `ZAI_OAUTH_ENABLED` 为 `0` / `false` 时禁用。URL 与 client id 的环境变量覆盖规则同 `*ProviderConfig.ts`。
2. **凭据键**：`oauth:active_provider`、`oauth:<provider>:access_token`、`oauth:<provider>:refresh_token`、`oauth:<provider>:user_info`、`zcodejwttoken`、`oauth:login_attribution`，格式同 `OAuthCredentialRepo`（zai 的 user_info 存后端原始 `user` 对象）。任一 OAuth 键解密失败等价于强制退出全部 provider，并清理派生的 Coding Plan key。
3. **恢复**：`restoreCachedSessionState` 只看本地：无 active provider 或无 user_info → `signed-out`；`zcodejwttoken` 已过期 → 清理并回 `reauthentication-required`；zai 没有 JWT → `signed-out`。bigmodel 在后台迁移旧 profile。
4. **登录**：UI 调 `startOAuthWithPolling`（Host 向 `<origin>/api/v1/oauth/cli/init` 申请 flow），UI 自己用 `platform.openExternal` 打开授权页（Main 负责，Host 不打开浏览器），然后周期性调 `pollPendingOAuth`；deep link 回调由 Main → Renderer → `handleCallback`。轮询与回调哪个先完成都只落盘一次，另一个回 `duplicate`。
5. **退出**：`logout` / `logoutAll` / 401 失效 / JWT 过期都会清理凭据、取消 pending flow、删除派生的个人 Coding Plan key，并刷新账号 provider。
6. **串行化**：所有改写 OAuth 凭据的路径在一个会话变更队列里执行；会话代际（generation）防止迟到的结果写回。

### 2.4 账号 provider（模型选择里的账号模型）

1. 内置配置中 `access.type = "zhipu-account"` 的 provider 由账号层决定是否可用（第三层 overlay，`packages/provider` `AccountProviderService`）。
2. 解析规则同 `accountProviderConnectionResolver.ts`：按 family（zai、bigmodel）读登录身份、套餐 key、权益；Start Plan 返回模型白名单；off-peak 跟随所选付费套餐；结果带 `availability`、`entitled`、`unavailableReason`、`current`、`connectionKey`。
3. 刷新触发：启动、登录 / 退出 / 回调（`oauth-callback`、`oauth-logout:<provider>`）、`providerFamilyDomain` 与 `providerFamilyConnectionSelections` 设置变化、内置配置变化、`provider-settings.refresh`。刷新是 single-flight，后到的原因并入下一轮；查询期间身份或设置变化时丢弃本轮结果。
4. 首次查询失败时发布 fail-closed（全部 `entitled:false`），之后失败保留上一份成功快照。
5. **内置配置的 Active / CDN 刷新**（`provider-node` `EndpointScopedZCodeBuiltinSource`）：
   - Active 路径 `<appConfigDir>/runtime/provider/<platform>/<appVersion>/endpoint-<sha256(origin) 前 32 位>/zcode-builtin.json`，`platform` 同 `resolveClientConfigPlatform`（如 `darwin-aarch64`，支持 `ZCODE_TEST_*` 覆盖），origin 为 2.2 的当前端点。Endpoint 切换即换一份 Active，不读上一 Endpoint 的缓存。
   - 读取：Bundled 与 Active 中 revision 较新者胜出；同 revision 内容不同时 Bundled 胜出；必要时在 `<active>.lock/` 锁内原子替换 Active（0600）。Active 物化失败时退回 Bundled。registry revision 为 `zcode-builtin:<revision>:<sha256(Active 绝对路径)>`，CLI 的 `ZCODE_BUILTIN_PROVIDER_CONFIG_FILE` 指向同一 Active 路径（spawn 前等首轮物化）。
   - 刷新：启动时一次、之后每 60 秒检查；`provider-settings.refresh` 强制刷新。经同目录 `zcode-builtin-refresh.json` 控制文件（锁内读写）合并多进程：租约 30 秒，成功后 1 小时、失败指数退避（1 分钟起，上限 1 小时）才再下载；网络期间不持锁。下载先读 `<origin>/api/v1/client/configs?app_version&platform` 的 `builtin_provider_config_json`（只接受无凭据 https URL），再取 Release；不跟随重定向、正文上限 10MB、总预算 20 秒。revision 更高才写入（`updated`），相同内容 `unchanged`，更低 `stale`，同 revision 不同内容报错并退避。

### 2.5 CLI 集成

1. **Account Config 同步**：会话订阅、新建、读取 presentation、`session/read` 之前，按 workspace 进程把当前账号 provider 配置发给 CLI（`provider/updateAccountConfig`，同 `zcodeAgentService.ts` 的 `ensureAccountProviderConfigSynced`）。CLI 声明 `runtime/capabilities.accountProviderConfig === false` 时跳过。账号层变化后推送给已运行的 CLI。
2. **反向请求**：`interaction/requestProviderRuntimeHeaders` 用账号鉴权（`accountRequestAuthService`）给出请求头；`interaction/requestOfficialMcpAuthHeaders` 用官方 MCP 凭据源。`interaction/providerRuntimeHeadersCancelled` 通知取消对应的进行中请求。
3. **spawn 环境**：`ZCODE_BASE_URL` = 当前 origin（含设置覆盖）；配置了 OTLP 端点时注入遥测环境（`ZCODE_TELEMETRY_DEVICE_MID`、身份状态、`sha256(userId)` subject、`ZCODE_TELEMETRY_RUNTIME_SURFACE=desktop_local_host`）。

### 2.6 账号增值（R4）

所有远端地址来自 2.2 的端点解析，经同一个 ApiClient（来源头、代理、401）。

1. **`usage-stats`**：`getEntitlementSnapshot`（额度、套餐、MCP 额度；Start Plan 走余额）、`getAppUsageSnapshot`（经任一运行中 CLI 的 `v4/usage/stats`，没有时 `no_active_workspace`）、Coding Plan 重置四个成员、`getSnapshot`（monitor `model-usage` / `tool-usage`）、`getCodingPlanUsageSnapshot`（quota 必须成功；`credit-usage/activity`、`usage-detail`（MODEL / MCP）、`model-performance-day` 传输失败只清空该区域，业务信封失败仍报错）。Coding Plan 页面只接受预置 Coding Plan provider，否则 `no_bigmodel_api_key`；缺 key 时 `zai_coding_plan_api_key_required` / `bigmodel_coding_plan_api_key_required`。Team Plan 带 `bigmodel-organization` / `bigmodel-project`，quota / monitor 用 `type=2`，credit-usage 用 `type=3`。
2. **`coding-plan-subscription`**：29 个成员按 `coding-plan-subscription/**` 与 `bigmodel/**` 移植（订阅、支付、发票、团队、闲时视图等），信封与错误归一同 Node。
3. **`feedback`**：
   - 基址：`ZCODE_FEEDBACK_API_BASE`，否则 `<当前 origin>/api/v1`；请求带 `X-Device-Mid`（宿主 deviceMid）与已登录时的 `Authorization: Bearer <zcodejwttoken>`；建连失败重试 3 次（间隔 300ms × 次数）；信封 `code != 0` 报 `msg`。
   - `create` 30 秒超时，`cancelCreate(operationId)` 中止；标题、描述、环境字段先脱敏（`feedbackPrivacy.ts` 逐条移植）。未登录时工单写入本地索引 `<dataRoot>/feedback/tickets.json`（每设备最多 200 条），`list` 未登录时读本地、登录后读服务端。
   - 附件：先 `upload-credential`，再向 OSS 直传 multipart 表单（不经代理，与 Node `https.request` 相同）；`uploadAttachmentWithProgress` 按 `progressId` 发 `preparing` / `uploading` / `complete` / `canceled`，`cancelUpload` 中止为 `Feedback upload canceled`。普通附件上限 100MB，日志 1GB。
   - 日志：`prepareCompactLogArchive` 默认打当天修改的诊断日志（`<appConfigDir>/logs`，`.log` / `.jsonl` / `.ndjson`，单文件 8MB、总量 2MB，诊断模式脱敏，zip 0600，附 `about.txt`）；`full` 时发 `feedback-log-archive-request{requestId, sourceDir}` 给 Main，等 `feedback-log-archive-result`（与“导出日志”同源）。`cleanupPreparedLogArchive` 删除归档所在目录。
4. **官方 MCP 凭据源**：`interaction/requestOfficialMcpAuthHeaders` 先校验目标 origin 可信，再用当前个人套餐凭据给出身份头。
5. **Host 资源样本**：每 60 秒发 `host-resource-sample{cpuPercent, rssKb, heapUsedKb}` 给 Main；上报端点由 Main 决定。
6. **`settings-sync`**：首启提示状态、标记已处理、`~/.claude/CLAUDE.md` → `~/.zcode/AGENTS.md` 迁移状态与复制、首启 `detect`（返回空结果）。

### 2.7 个人 Provider 配置写入（`provider-settings` 写成员）

1. 文件 `<appConfigDir>/provider_config.json`，与 Node 共用：读改写在 `<file>.lock/` 锁内，私有原子写 0600，内容为 `JSON.stringify(encoded, null, 2)`（无结尾换行）。
2. 编码与 `encodeProviderConfigFile` 逐字节一致：`ProviderConfig.toJSON` 字段顺序；provider 规则 `providerId, templateId, providerName, enabled, config`；model 规则 `modelId, config, providerId`；手动模型配置按 `manualModelConfigSchema` 字段顺序。
3. 输入严格校验（与 zod schema 相同的拒绝条件），失败不写文件。
4. 成员关系校验：写入携带的 personal revision（文件原始字节的 sha256）在锁内与当前文件比对，不一致报冲突；同一 provider 的写入串行。
5. 写入后 `ProviderRuntime` 重新读取并通知 `onDidChange`；`refresh` 先强制刷新内置配置与账号层，再刷新 registry，来源刷新失败在 registry 刷新之后抛给调用方。

## 3. 状态所有者

| 状态                                       | 唯一所有者                                                   |
| ------------------------------------------ | ------------------------------------------------------------ |
| `credentials.json` 内容                    | 文件本身；进程内无缓存，每次读写都经目录锁                   |
| pending OAuth flow、会话代际、会话变更队列 | `OAuthService`（每个 Host 一个）                             |
| 账号 provider 快照（providers + states）   | `AccountProviderSource`；provider runtime 只消费它           |
| 内置 / 个人配置、合并结果、revision        | `ProviderRuntime`                                            |
| 每个 CLI 进程已同步的 Account Config 版本  | `AccountConfigSync`（按 workspace key + 进程代）             |
| 设置（端点覆盖、代理、账号连接选择）       | `setting.json`（`SettingStore`）                             |
| 内置配置 Active 文件与刷新控制文件         | 文件本身（与 Node 共用，锁内读写）；`BuiltinSource` 只做单飞 |
| 匿名反馈工单索引                           | `<dataRoot>/feedback/tickets.json`                           |
| 进行中的建单 / 上传取消句柄、上传进度订阅  | `Feedback`（按 operationId / progressId）                    |
| 等待 Main 的完整日志归档请求               | `MainBridge`（按 requestId）                                 |

## 4. 事件顺序

```text
UI                         oauth（Rust Host）                 ZCode 后端（mock）        账号层 / CLI
│ startOAuthWithPolling(p)
│───────────────────────►│ POST /api/v1/oauth/cli/init ──────►│
│◄───────────────────────│◄── {flow_id, authorize_url, ...} ──│
│ platform.openExternal(authorize_url)（Main）
│ pollPendingOAuth()（周期）
│───────────────────────►│ GET /api/v1/oauth/cli/poll/<flow> ►│
│                        │◄── pending / ready{token, user} ───│
│                        │ 会话队列：清另一 provider → 写 token → 写 user_info → 写 active
│◄── {kind:"session"} ───│
│ 刷新账号（provider-settings.refresh("oauth-callback")）
│───────────────────────────────────────────────────────────────────────────────────►│ 账号层重新解析
│                                                                                     │ ProviderRuntime 合并 → onDidChange
│                                                                                     │ 对每个运行中的 CLI 发 provider/updateAccountConfig
```

```text
CLI ── interaction/requestProviderRuntimeHeaders{requestId, access} ──► Host
Host：读当前账号选择 → 取 token / 套餐 key（必要时远端换取）→ 回 {headersApplied, headers}
CLI ── interaction/providerRuntimeHeadersCancelled{requestId} ──► Host：中止同一 requestId 的进行中解析
```

```text
BuiltinSource（Rust Host）        控制文件 / Active（与 Node 共用）      ZCode 控制面 / CDN（mock）
│ refresh(force?)（启动、每 60s、provider-settings.refresh）
│── 锁内读控制文件：租约未过期 → skipped；未到期且非 force → skipped
│── 锁内写租约 {endpointKey, leaseId, leaseUntil=now+30s}
│── GET /api/v1/client/configs ─────────────────────────────────────────►│
│◄── builtin_provider_config_json（https）───────────────────────────────│
│── GET <cdn> ───────────────────────────────────────────────────────────►│
│── 复核 endpoint 未变 → Active 锁内比较 revision → 写 Active（updated）
│── 锁内结束租约：成功 nextEligibleAt=now+1h；失败按 failureCount 退避
│── ProviderRuntime.reload → builtin_changed → 账号层重新解析 → onDidChange
```

```text
UI ── feedback.prepareCompactLogArchive({full:true, progressId}) ──► Feedback
Feedback ── progress{preparing 0/0} ──► UI
Feedback ── feedback-log-archive-request{requestId, sourceDir} ──► Main（导出日志同源打包）
Main ── feedback-log-archive-result{requestId, ok, path, size} ──► ControlRouter → MainBridge.resolve
Feedback ──► UI {path, size}；上传后 cleanupPreparedLogArchive 删除目录
```

## 5. 有意与 Node 不同之处

| 项                                                                                    | Node Host                                                             | Rust Host                                                                                                        | 原因                                                                              |
| ------------------------------------------------------------------------------------- | --------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------- |
| `provider-provisioning-target.apply`                                                  | 只在 desktop-attached remote / server 目标注册                        | 不注册（保留 contract 桩）                                                                                       | Rust Host 只承担 desktop-local 角色，与 Node desktop-local 一致                   |
| `settings-sync` 手动导入（带 categories 的 `detect`、`importSelected`）               | 扫描并导入 Claude / Codex 等的 skills、commands、plugins、MCP         | 报 `fault.host.notImplemented`                                                                                   | 写入的是扩展域（R5）的存储，需与扩展域合并后接入；首启路径与 AGENTS.md 迁移已实现 |
| 内置 Release 校验                                                                     | `decodeZCodeBuiltinRelease` 深度解析规则                              | 只做结构校验（严格的顶层与 `config` 键、revision 范围、拒绝 `builtin:zapi`），规则内部按解析层的容错处理         | 规则解析在 resolver 内；校验失败的 Release 同样不会被采用                         |
| Active 文件的写入格式                                                                 | `encodeZCodeBuiltinRelease` 规范化后 2 空格缩进                       | 选中 Release 的原 JSON 2 空格缩进                                                                                | 两边按语义内容比较（键序无关、数值按值），不会互相反复改写                        |
| 端点不可解析时读取内置配置                                                            | `read()` 抛错                                                         | 退回随包配置并记 warn                                                                                            | 模型列表不因设置里的非法端点而整体不可用                                          |
| 反馈基址                                                                              | `HostInitMessage.feedbackApiBase` → env → 端点                        | env → 端点                                                                                                       | Main 从不下发 `feedbackApiBase`                                                   |
| 反馈设备信息                                                                          | 含 `nodeVersion`、编译期 commit 与构建时间                            | 无 `nodeVersion`；commit 取 `ZCODE_BUILD_COMMIT_ID`，构建时间 `unknown`；`about.txt` 的 `node:` 为 `(rust host)` | Rust Host 没有 Node 运行时与 TS 编译期常量                                        |
| 完整日志归档等待                                                                      | 无超时                                                                | 30 分钟上限后报 `Feedback log archive timed out`                                                                 | 避免 Main 丢消息时请求永久挂起                                                    |
| Host 资源样本                                                                         | `heapUsedKb` 为 V8 堆；同一读数写 `[memory] role=utility_host` 日志   | `heapUsedKb` 固定 0，不写该日志行；Windows 不发样本                                                              | Rust Host 没有 JS 堆；Windows 的常驻内存读取未实现                                |
| RPC 网络遥测（`network-telemetry-batch`）、会话创建遥测（`session-create-telemetry`） | Host 的 RPC 中间件与自动化派发上报                                    | 未实现                                                                                                           | 需要 host-core 分发层的调用钩子与会话域的派发点，由协调者在合并时接入             |
| 网络错误消息                                                                          | undici 的 `fetch failed`（cause 带原因）                              | reqwest 的错误描述                                                                                               | 运行时不同；状态码、超时与 401 语义一致                                           |
| 无效时区                                                                              | `Intl.DateTimeFormat` 抛 `RangeError: Invalid time zone specified: X` | 同名同消息的 RPC 错误；IANA 名称大小写不敏感                                                                     | 与 Intl 行为对齐                                                                  |

## 6. 验收

- `cargo test -p zcode-host-services --lib -- account:: providers:: settings_sync`：凭据格式、锁、损坏处理；ApiClient 的端点、来源头、代理 / No Proxy / CA（127.0.0.1 mock）；OAuth 轮询、回调、去重、退出、JWT 过期；账号 provider 解析；内置配置的物化、刷新租约与退避；反馈映射、上传、本地工单、Main 归档；monitor 映射与时间范围；资源样本公式。
- `tests/account_interop.rs`：经 `scripts/rust-host/credential-interop.ts` 调真实 TS 实现——Rust 写 TS 读、TS 写 Rust 读、两边并发写不丢更新、固定 IV 密文逐字节相同、反馈脱敏两种模式逐字一致。
- `tests/provider_interop.rs`：经 `scripts/rust-host/provider-config-interop.ts` 核对 Rust 写出的 `provider_config.json` 与 TS codec 逐字节相同。
- `tests/account_cli.rs`：临时 HOME 里的已登录 Z.ai 账号 + mock 后端 + 真实 Rust CLI：账号 provider 进入模型选择、Coding Plan key 缓存、权益与用量面板、反向请求、Account Config 同步。
- 桌面 E2E：`scripts/rust-host/e2e/scenarios/account-logged-in.mjs`，隔离 HOME 里的 Node 格式加密凭据 + 127.0.0.1 mock 的 ZCode / Z.ai 后端：模型菜单出现账号 provider，余额与订阅按正确凭据查询，套餐 key 写回凭据库。不点登录，不访问真实服务。
