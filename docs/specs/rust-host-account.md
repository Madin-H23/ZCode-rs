# Rust Host：账号、网络与账号增值服务

日期：2026-10-08。上级规格：[rust-host.md](rust-host.md) 第 5 节 R1（账号部分）与 R4。分支：`feat/rust-host-p-account`。状态：实施中。

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

### 2.5 CLI 集成

1. **Account Config 同步**：会话订阅、新建、读取 presentation、`session/read` 之前，按 workspace 进程把当前账号 provider 配置发给 CLI（`provider/updateAccountConfig`，同 `zcodeAgentService.ts` 的 `ensureAccountProviderConfigSynced`）。CLI 声明 `runtime/capabilities.accountProviderConfig === false` 时跳过。账号层变化后推送给已运行的 CLI。
2. **反向请求**：`interaction/requestProviderRuntimeHeaders` 用账号鉴权（`accountRequestAuthService`）给出请求头；`interaction/requestOfficialMcpAuthHeaders` 用官方 MCP 凭据源。`interaction/providerRuntimeHeadersCancelled` 通知取消对应的进行中请求。
3. **spawn 环境**：`ZCODE_BASE_URL` = 当前 origin（含设置覆盖）；配置了 OTLP 端点时注入遥测环境（`ZCODE_TELEMETRY_DEVICE_MID`、身份状态、`sha256(userId)` subject、`ZCODE_TELEMETRY_RUNTIME_SURFACE=desktop_local_host`）。

### 2.6 账号增值（R4）

`usage-stats`、`coding-plan-subscription`、`feedback`、官方 MCP 凭据源按 Node 实现移植，所有远端地址来自 2.2 的端点解析；反馈的完整日志归档经 Main（`FeedbackLogArchive*` 控制消息）生成。

## 3. 状态所有者

| 状态                                         | 唯一所有者                                                 |
| -------------------------------------------- | ---------------------------------------------------------- |
| `credentials.json` 内容                      | 文件本身；进程内无缓存，每次读写都经目录锁                 |
| pending OAuth flow、会话代际、会话变更队列   | `OAuthService`（每个 Host 一个）                           |
| 账号 provider 快照（providers + states）     | `AccountProviderSource`；provider runtime 只消费它         |
| 内置 / 个人配置、合并结果、revision          | `ProviderRuntime`                                          |
| 每个 CLI 进程已同步的 Account Config 版本    | `AccountConfigSync`（按 workspace key + 进程代）           |
| 设置（端点覆盖、代理、账号连接选择）         | `setting.json`（`SettingStore`）                           |

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

## 5. 有意与 Node 不同之处

（实施过程中补全。）

## 6. 验收

- `cargo test -p zcode-host-services account`：凭据格式与 TS 交叉读写、锁、损坏处理；ApiClient 的端点、来源头、代理 / No Proxy / CA（127.0.0.1 mock）；OAuth 轮询、回调、去重、退出、JWT 过期；账号 provider 解析。
- TS 夹具：`scripts/rust-host/account-fixtures.ts` 用真实 TS 实现生成凭据密文与格式夹具。
- 桌面 E2E：`account-*` 场景，用 127.0.0.1 mock 的 ZCode / BigModel / Z.ai 后端，不访问真实服务。
