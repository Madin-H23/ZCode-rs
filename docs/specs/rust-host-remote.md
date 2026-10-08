# Rust Host：远端 workspace（SSH / WSL / Docker）

日期：2026-10-09。上级规格：[rust-host.md](rust-host.md) 第 5 节 R6（远端 workspace collection、远端媒体预览代理）。R0 的“远端 scope 一律拒绝”（[rust-host-r0.md](rust-host-r0.md) §2.6）由本规格取代。分支：`feat/rust-host-p-remote`。

## 1. 目标与范围

窗口可以打开远端 workspace：Host 连到远端的 Node `zcode-server.cjs`（由 App 部署，部署资产不变），把远端服务经 scoped attachment 暴露给 Renderer。行为与 Node Host 一致：

| 部分                             | Node 来源                                                                    | Rust / TS 模块                                                       |
| -------------------------------- | ---------------------------------------------------------------------------- | -------------------------------------------------------------------- |
| `@zcode/rpc` 客户端              | `packages/rpc/src/channelClient.ts`                                          | `crates/rpc/src/client.rs`                                           |
| 远端传输：探测、部署、启动、握手 | `packages/server/src/remote/*`（ssh2、docker/wsl CLI、deploy）               | 远端连接器 `packages/desktop/src/host/remoteConnector.ts`（见 §2.1） |
| 窗口级连接注册表                 | `host/windowRemoteConnectionRegistry.ts`                                     | `services/src/remote/registry.rs`                                    |
| Main ↔ Host 远端控制消息         | `host/index.ts`（Connect/Cancel/Bind/Dispose、ProviderProvisioningExecute）  | `services/src/remote/control.rs`                                     |
| scoped 服务集合                  | `host/remoteWorkspaceServiceCollection.ts`、`exposeServicesOnMessagePort`    | `services/src/remote/scope.rs`、`agent_scope.rs`、`tasks.rs`         |
| attachment 的远端 scope          | `host/windowHostAttachmentRegistry.ts`                                       | `host-core`（签发、attach、detach）                                  |
| window-controller 远端 source    | `host/windowHostControllerService.ts`、`windowHostControllerProjection.ts`   | `tasks/controller.rs`（增量）+ `remote/sources.rs`                   |
| 远端媒体预览代理                 | `host/remoteMediaPreviewProxy.ts`                                            | `services/src/remote/media.rs`                                       |
| Provider Provisioning 首次同步   | `host/remoteProviderProvisioningService.ts`、`providerProvisioningSource.ts` | `services/src/remote/provisioning.rs`                                |
| Renderer 的远端 endpoint         | `main/desktopRemoteSessions.ts`、`preload`、`renderer/main.tsx`              | 同文件增量：`ScopedServiceEndpoint`                                  |

不在本规格：远端 server 本身（仍是 Node）、Bot 远程运行时与手机 relay 的 MessagePort 桥（由 tasks 部分的 `rustHostPortBridge.ts` 承担，见 §6）、cua。

## 2. 产品规则

### 2.1 远端传输（连接器）

1. **传输层不重写**：Node Host 用 `@zcode/server/remote` 完成“探测平台 → 部署/升级（CDN 清单、SHA 去重、远端部署锁、刷新标记、node-pty、agent bundle）→ 启动 `~/.zcode/server/node ~/.zcode/server/zcode-server.cjs` → hello 握手”。这条链依赖 ssh2（密码、keyboard-interactive、私钥口令、agent、keepalive、sftp）且与其它 App 版本共享远端安装目录与锁协议。Rust Host 不移植它，而是每个连接拉起一个**远端连接器**进程：`ZCODE_HOST_NODE_EXEC_PATH`（Electron）以 `ELECTRON_RUN_AS_NODE=1` 运行 `out/host/remoteConnector.js`，路径由 Main 经 `ZCODE_REMOTE_CONNECTOR_PATH` 传入。连接器直接调用原有的 `createRemoteBackend`、`deployServer`、`performHandshake` 与远端启动命令，部署资产与远端协议逐字节不变。
2. **连接器协议**（stdin/stdout，二进制帧 `stream u8 | len u32BE | payload`）：
   - stream 0：控制 JSON。Host → 连接器：`connect{target, remoteAssets, remoteRuntimeNetwork?, network}`、`dispose`。连接器 → Host：`log{level, message}`、`connected{hello}`、`failed{error}`、`closed{exitCode, error?}`。
   - stream 1：握手之后远端 `zcode-server` 的 stdio 字节，双向原样转发（远端 `SocketProtocol` 字节流）。
   - stream 2：连接器自身的 `@zcode/rpc` 服务（每帧一条完整消息），channel `remote-backend`：`materializePromptAttachments`、`prompt-attachment-transfer` 的全部成员。它们需要 backend 的 upload / exec，仍用原 TS 实现。
3. **取消与释放**：Host 取消连接时发 `dispose` 并立即返回取消。与 Node Host 相同，只有 SSH 的建连可中断（AbortSignal 只传给 SSH）：SSH 立即释放 backend 后退出；WSL / Docker 等建连自然结束后释放再退出。Host 先等 1.5 s，之后在后台最多再等 120 s（一个远端部署锁周期）才结束进程树。连接器 stdin EOF 等同 `dispose`。已连接时 `dispose` 先结束远端 stdin，等其关闭最多 5 s，再 `disposeAndWait` backend；Host 等连接器退出最多 6.5 s。
4. **日志**：连接器把 deploy/connect 过程的 console 输出（含远端 stderr，前缀 `[remote]`）作为 `log` 帧发回；Node 自身的 warning（`shouldReportHostConsoleError` 过滤的那类）不上报。连接进行中由 Host 转成带 requestId 的 `remote-workspace-connection-log`（日志归发起建连的 requestId，连接 settle 后停止上报，同 `remoteConnectionProgressContext`），同时进结构化日志。日志不含密码与口令（连接器从不打印 target 原文）。
5. **网络**：CDN 资源下载使用设置页代理（`httpProxy`、`httpProxyNoProxy`、`httpProxyCaCertPath`），由 Host 读设置后放进 `connect.network`，连接器据此构造 `createHostApiNetworkTransport`。WSL 另带 `remoteRuntimeNetwork{authoritative:true, httpProxy, noProxy}`，读设置失败时不带（同 Node）。
6. **测试传输**：`ProcessTransport` 直接拉起一个说 hello 协议的命令（例如本地构建的 `zcode-server.cjs`），Host 自己完成握手（逐行跳过 banner，同 `handshake.ts`，10 s 超时，错误带退出码与 stdout / stderr 诊断）。`scripts/rust-host/remote-connector-local.ts` 用产品的 `runRemoteConnector` 加一个本机打开器（直接拉起 `zcode-server.cjs`、本机 exec / upload），验证 Rust `ConnectorTransport` 的完整帧协议。两者都只用于测试注入，产品路径不读任何开关。
7. **缺少配置**：`ZCODE_HOST_NODE_EXEC_PATH` 或 `ZCODE_REMOTE_CONNECTOR_PATH` 缺失时，建连明确失败（`远程连接器不可用：…`），不静默回退。

### 2.2 `@zcode/rpc` 客户端

与 `ChannelClient` 一致：

1. 收到 `[200]` 前的请求排队，Initialize 后按序发出；请求 id 从 0 递增。
2. `call` 返回远端结果的原始 `RpcValue`；`[202]` 还原为 `RpcError`（`message`、`name` 与 9 个透传键）；`[203]` 的载荷作为 `RpcError{name:"Error"}` 的 `data`，`message` 取载荷的 `message` 或 JSON 文本。
3. 调用方放弃等待（future drop）时发 `[101]`；事件流 drop 时发 `[103]`。
4. 连接关闭：所有挂起调用以 `RpcError("Error", "Connection closed")` 失败，事件流结束。

转发时参数按 `RpcValue` 原样重编码（`VSBuffer` 标签改写为 `Buffer`，TS 侧同为 `Uint8Array`）；结果与事件回写给 Renderer 时，顶层 `undefined`、二进制保持原标签，其余按 JSON（数组内的 `undefined` 变为 `null`，见 §5）。

### 2.3 连接注册表（窗口级）

注册表是远端连接与 logical session 的唯一所有者，移植自 `windowRemoteConnectionRegistry.ts`，规则逐条相同：

1. **连接复用键**：SSH `ssh:<buildSshRemoteHostKey>`（凭据不进键）；WSL `wsl:<distro|<default>>\0<user|<default>>`；Docker 不复用（`docker:dedicated:<remoteSessionId>`）。同键且处于 connecting / online 的 entry 直接复用。
2. **logical session**：每次 `connect-remote-workspace` 生成一个 `remoteSessionId`（UUID），带 requestId、target、workspacePath、workspaceIdentity（缺省时用 `buildRemoteWorkspaceIdentity(path, target)` 构造）、generation（从 1 起）、state（connecting / online / closing / failed / disconnected）、sourceAvailability。requestId 重复报 `远程连接 requestId 重复，requestId=<id>`；registry 已释放报 `窗口 Host 的远程连接 registry 已释放`。
3. **连接失败**：entry 置 failed 并移出复用表，挂在它上面的所有 session 删除；Docker entry 在最后一个 session 失败后释放。
4. **取消**（`cancel-remote-workspace-connect`）：只作用于仍在等待的 session；最后一个等待者取消时，SSH connecting entry 先按对象身份退休再中止底层连接；WSL online entry 进入空闲回收；SSH online entry 保留为窗口缓存；其它 entry 直接释放。迟到的连接成功结果在没有 session 时立即释放，不复活旧连接。
5. **断线**：底层关闭（stdio 结束或 backend 断连，退出码 -1）时 entry 置 disconnected 并移出复用表；其上每个 session 置 disconnected / offline，并触发 §2.6 的关闭流程。
6. **释放 session**（`dispose-remote-workspace-session`）：最后一个 session 离开后，WSL online entry 等 60 s 空闲再释放（期间有 session 复用或有运行中任务则取消）；SSH online entry 保留到窗口 Host 退出；其它释放。
7. **WSL workspace 运行时释放**：同一 WSL entry 上，某 workspaceKey 的最后一个 owner 离开且没有运行中任务时，调用远端 `zcode-task.releaseWorkspacePreparation({workspacePath, workspaceIdentity?, provider:"glm"})`；有运行中任务时挂起，任务数归零后再释放。新一代 owner 进入时，必须等上一代释放完成（释放失败则新一代 fail-closed）。运行中任务数来自远端 `onDynamicTaskReady` 与 sendPrompt 的跟踪（§2.5）。
8. **bind**（`bind-remote-workspace-context`）：改写 session 的 workspacePath / identity，generation 加一，重新准备 workspace 运行时；workspaceKey 变化时释放旧 key。旧 generation 的 attachment 立即断开（§2.4）。
9. **scope 解析**：attachment 的 scope 必须与 session 的 path、identity 完全相同，否则报 `远程 attachment scope 与 logical session 不匹配，remoteSessionId=<id>`；session 不在线、workspace 运行时未就绪或没有连接时报 `远程连接当前不可用，remoteSessionId=<id>`。**绝不按路径单独匹配，也绝不回落到本地服务。**
10. **按 workspace 找 session**（controller、provisioning 用）：按 (path, identity) 精确匹配；多个在线匹配或无在线时多个匹配报错。
11. **Host 退出**：取消所有等待中的连接，释放所有 entry。

### 2.4 attachment 的远端 scope

1. **签发**：`issue-attachment-endpoint` 接受 `scope:{kind:"remote", remoteSessionId, workspacePath, workspaceIdentity}`（严格校验，字段非空），两种 clientMode 都可以。签发时 session 必须存在且 scope 匹配，否则回 `ok:false`。
2. **连接**：WS 握手后先等该 session 的 workspace 运行时就绪（WSL 释放屏障），再按 §2.3.9 解析；失败时关闭连接，不发 `[200]`。成功后为这条连接组装 scoped channel 表（§2.5），记录 `(attachmentId, remoteSessionId, generation)`。
3. **断开**：
   - Main 的 `detach-service-port{attachmentId}`：关闭该 attachmentId 的所有连接（本地 scope 同样适用）。
   - session 断线或释放：关闭该 remoteSessionId 的全部 attachment（所有 clientMode）。
   - bind 换代：关闭 generation 小于新值的 attachment。
     关闭时先停止接收请求，再走正常的连接关闭清理（connection scope 发 `closed`、反向退订）。

### 2.5 scoped channel 表

与 `remoteWorkspaceServiceCollection.ts` + `exposeServicesOnMessagePort` 的组合相同：

| channel                                                                                                                                                                                                               | 来源                                                                                                            |
| --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `file`、`git`、`git-checkpoint`、`system`、`terminal`、`file-watcher`、`model-selection`、`provider-settings`、`skills`、`skill-sync`、`mcp-sync`、`plugin-sync`、`plugins`、`plugin-management`、`commands`、`hooks` | 原样转发到远端                                                                                                  |
| `zcode-agent`                                                                                                                                                                                                         | 每条连接一个 connection scope（terminal-client 角色），base 是远端 channel（下条）                              |
| `zcode-task`                                                                                                                                                                                                          | 远端转发 + 附件物化（`sendPrompt`、`enqueueTaskCommand`）+ 运行中任务跟踪 + 列表写操作经 window-controller 路由 |
| `zcode-session`                                                                                                                                                                                                       | 远端转发 + 附件物化（`sendPrompt`，taskId = sessionId，traceId = inputId ?? sessionId）                         |
| `prompt-attachment-transfer`                                                                                                                                                                                          | 连接器（stream 2），远端 backend 上传                                                                           |
| `media-preview`                                                                                                                                                                                                       | 只给 `desktop-continuous`：远端媒体预览代理（§2.7）；replayable 不注册                                          |
| `window-controller`、`setting`、`credential`、`broadcast`、`bots`、`oauth`、`usage-stats`、`coding-plan`、`client-config`、`client-scenes`、`subagents`、`memory`、`settings-sync`、`conversation-share`              | 本地 Host 的同名 channel（本机设置、账号与窗口级 controller）                                                   |
| 其他                                                                                                                                                                                                                  | 不注册，立即回 `Unknown channel`                                                                                |

**zcode-agent connection scope**（`zcodeAgentConnectionScope.ts`，terminal-client）：

1. hello / initialize 在 Host 本地完成（与本地 scope 同一 hello 载荷与 clientHello 校验）；握手前的命令、查询、订阅、附件调用回 `fault.connection.handshakeRequired`；命令的 `envelope.clientId` 必须等于握手 clientId（`fault.command.clientMismatch`）；`queryConversationCommandsV4`（非 clock）绑定第一个 workspace，换 workspace 回 `fault.command.queryForeignWorkspace`。
2. 转发前删除参数里的 `__zcodeTrustedV4Connection`、`connectionId`、`clientMode`、`deliveryProfile`、`subscriberScope`、`workflowRunDeltas`，写入 `__zcodeTrustedV4Connection:{connectionId:"host-rpc-<uuid>", clientMode, workflowRunDeltas?}`。远端 stdio server 以 trusted-host-relay 角色读取它。
3. 订阅（conversation / sessions-index / workspace-config）：ACK 前同 ownership 的帧暂存（≤1024 帧、32 MiB，超出则反向退订并回 `fault.subscription.initialFrameStagingOverflow`），ACK 后按到达顺序放行同 subscriptionId 的帧。退订、resync 带 `__zcodeTrustedV4UnsubscribeRoute:{topic, connectionId}`；不属于本连接的 resync 回 `fault.subscription.notOwned`。
4. 远端 `onAgentRuntimeLifecycle` 的 `unavailable` 使该 workspace 的所有权与暂存失效（迟到 ACK 回 `fault.subscription.runtimeRestarted`）。
5. 连接关闭：对触达过的每个 (workspace, connection) 路由发 `setConnectionFlowStateV4{state:"closed"}`，再退订本连接拥有的订阅。
6. 资源、MCP 遥测类事件（`onDynamicProcessResourceSample` 等）与 `onDynamicCuaPermissionObservation` 对 replayable 返回空流；`onDynamicLocalTtftFacts` 对远端 identity 返回空流；`onDynamicConversationTelemetryFact` 只对 desktop-continuous 转发（带可信 carrier）。

**远端 runtime 偏好**：每个远端连接监听远端 `onDynamicSessionRuntimePreferencesRequest`，用本机设置回答 `respondSessionRuntimePreferences`（字段与 Node 相同；设置读取失败回 `{status:"failed", message}`）。

**zcode-task 的列表写操作**：`setTaskPinned`、`archiveTask`、`unarchiveTask`、`deleteTask`、`setTaskUnread`、`deleteArchivedTask(s)` 先经 controller 用 attachment scope 解析唯一地址（scope 的 path / identity 必须与参数一致，否则 `列表 mutation 与 remote attachment scope 不匹配`），再写远端。

### 2.6 断线、换代与 Main 通知

```text
Main                        Host（registry / controller / attachments）           连接器 / 远端
connect-remote-workspace ─► session(connecting) ─► 复用或新建 entry ────────────► connect（探测/部署/启动/握手）
                            ◄──── log 帧 → remote-workspace-connection-log{requestId}
                            entry online → session online → workspace 运行时就绪
                            替换同 path+identity 的 disconnected 旧 session：
                              controller.replaceDisconnectedSource(旧→新)
                              关闭旧 session attachment → disposeSession(旧)
◄─ remote-workspace-closed{旧, reason:"disposed"}
◄─ remote-workspace-connected{requestId, descriptor}
provider-provisioning-execute{environment-online} ─► 读本机 envelope → 远端 provider-provisioning-target.apply
◄─ provider-provisioning-execution-result{status}
issue-attachment-endpoint{scope:remote} ─► 校验 session ─► url + capability
（Renderer 经 WS 连接）                     等运行时就绪 → 组装 scoped channel 表 → [200]
                                                                          ◄──── closed{exitCode}（断线）
                            entry/sessions disconnected → 关闭 attachment
                            controller.disconnectSource（行标记 offline）
◄─ remote-workspace-closed{reason:"connection-closed", exitCode, signal:null, error?}
bind-remote-workspace-context ─► generation+1 → 关闭旧代 attachment → controller.removeSource(旧 scope)
dispose-remote-workspace-session ─► 关闭 attachment → disposeSession → controller.removeSource
◄─ remote-workspace-closed{reason:"disposed"}
```

- 连接失败回 `remote-workspace-connect-failed{requestId, error}`（error 为原始错误文本）。
- provisioning：session 不存在、没有 identity 或 environmentKey 不匹配时回 `failed`，`error:"Remote Environment registration 已失效"`；远端没有 `provider-provisioning-target` 时回 `unsupported`。

### 2.7 window-controller 的远端 source

1. **source 键**：本地 `local\0<workspaceKey>`；远端 `remote\0<remoteSessionId>\0<workspaceKey>`。行地址、`task.removed`、workspace 事实都带 `remoteSessionId`。
2. **解析**：查询的 workspace scope 先找远端 session（§2.3.10）。找到 → 远端 source（在线时读远端 tasks-index 三个分区，离线时只登记）；scope 的 identity 是远端 identity 但没有 session → 跳过（绝不落回本地 tasks-index）；否则本地 source。
3. **实时摘要**：在线的远端 source 用 Host 自己的 connectionId 订阅远端 sessions-index（`runtimePolicy:"existing-only"`，不拉起远端 CLI），叠加规则与本地相同；远端 workspace 事件（`onDynamicWorkspaceEvent`）触发强制重读。
4. **disconnectSource**：行全部改为 `sourceAvailability:"offline"`（一个 deltas 帧），workspace 事实为 `offline / disconnected`；停止实时摘要。
5. **replaceDisconnectedSource(旧, 新)**：新 source 首次读取成功时，在一个 task deltas 帧里同时移除旧行、加入新行，并发 `workspace.removed`（旧）+ `workspace.upserted`（新）；读取失败时继续展示旧的离线行（查询带上旧 source），等下次成功再原子替换。
6. **removeSource**：发全部行的 `task.removed` 与 `workspace.removed`；连同仍在借用的旧代离线 source 一起移除。
7. **写操作**：远端离线时回 `远程 source 当前离线，禁止列表写操作`。

### 2.8 远端媒体预览代理

只给 desktop-continuous 的远端 attachment（`ZCODE_REMOTE_MEDIA_RANGE_PREVIEW_ENABLED=0` 时关闭，同 Node）：

1. `prepare{path, expectedKind}`：远端 `file.resolvePath` → 必须在 workspace 内（`Media preview path is outside remote workspace`）→ 格式按扩展名（视频 mp4/mov/webm/m4v、音频 mp3/wav/m4a/ogg/opus/flac/weba，与 `MEDIA_PREVIEW_FORMATS` 相同）且 kind 一致 → 远端 `file.stat` 为文件且 ≤ 512 MiB → 登记租约（32 字节随机 token，TTL 2 h，空闲 10 min）→ 返回 `{kind:"host-range-url", previewId, mediaType, path, size, url:"http://127.0.0.1:<port>/__zcode_media/<token>", urlExpiresAt}`。
2. `refreshPlaybackUrl{previewId}`：租约有效时换 token、延长 TTL；否则 `Remote media preview lease expired`。`release` 删除租约。
3. HTTP：只接受 GET / HEAD；Range 规则同 `parseRange`（单段、后缀、越界 416 并带 `Content-Range: bytes */size`）；每租约 2 个、整个 Host 4 个并发 GET，超出回 429 + `Retry-After: 1`；先 `stat` 校验大小未变（变了回 409 并作废租约），按 1 MiB 分块 `readFileRange` 读远端，块长度不符即作废并断开；读完再 `stat` 一次。响应头 `Accept-Ranges`、`Cache-Control: no-store`、`Content-Disposition: inline`、`Content-Length`、`Content-Type`、206 时 `Content-Range`。
4. 代理随 attachment 关闭而关闭，监听在 `127.0.0.1:0`，首次 prepare 时才监听。

## 3. 状态所有者

| 状态                                                        | 唯一所有者                                                          |
| ----------------------------------------------------------- | ------------------------------------------------------------------- |
| 远端连接 entry、logical session、复用表、WSL 空闲与释放屏障 | `RemoteRegistry`（`remote/registry.rs`）                            |
| SSH 会话、部署、远端进程                                    | 连接器进程（每个 entry 一个），Host 只持有它的进程句柄与 rpc 客户端 |
| rpc 请求 id、挂起调用、事件订阅                             | 每个远端连接的 `RpcClient`                                          |
| 握手、订阅所有权、暂存帧                                    | 每条 attachment 连接的 `RemoteAgentScope`                           |
| attachment 连接表（attachmentId → 连接、scope、generation） | `host-core`                                                         |
| 列表投影（含远端行、offline 标记、待替换链）                | window-controller（`tasks/controller.rs`）                          |
| Main 侧端口路由、渲染端 ready ACK                           | Main（`desktopRemoteSessions.ts`），不保存业务状态                  |
| 会话、任务、帧内容                                          | 远端 server / 远端 CLI                                              |

## 4. 接口

### 4.1 控制消息（全部沿用 `validation.ts` 现有 schema）

Main → Host：`connect-remote-workspace`、`cancel-remote-workspace-connect`、`bind-remote-workspace-context`、`dispose-remote-workspace-session`、`provider-provisioning-execute`、`detach-service-port`；`issue-attachment-endpoint` 的 `scope` 改为 `windowHostAttachmentScopeSchema`、`clientMode` 两值。Host → Main：`remote-workspace-connected`、`remote-workspace-connect-failed`、`remote-workspace-closed`、`remote-workspace-connection-log`、`provider-provisioning-execution-result`。

### 4.2 Renderer

新增 `InternalChannels.ScopedServiceEndpoint`（`zcode:scoped-service-endpoint`），载荷 `{attachmentId, sessionId, target, url, capability}`（strict，url 必须是 `ws://127.0.0.1:<port>/rpc`）。preload 校验后转发；Renderer 用 WebSocket 建立远端 scoped 连接、注册远端 session，再回 `ScopedServicePortReady`（与 MessagePort 路径同一 ACK）。Main 在窗口 Host 是 Rust Host 时走这条路径，Node Host 不变。

## 5. 有意与 Node 不同之处

| Node 行为                                                                                                      | Rust Host                                                                | 理由                                                                                                                                    |
| -------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------- |
| Host 进程内用 ssh2 / docker / wsl 与 deploy 模块                                                               | 每个连接一个 TS 连接器进程（Electron 以 Node 模式运行）                  | 部署链与 ssh2 认证语义逐字节不变，并与其它 App 版本共享远端安装与锁；无法在本地验证的重写风险过高。已有先例：Host 以同样方式运行 TS CLI |
| 附件上传、`prompt-attachment-transfer` 在 Host 进程内                                                          | 由连接器执行（stream 2）                                                 | 需要 backend 的 upload / exec                                                                                                           |
| Renderer 远端 scoped 端口是 MessagePort                                                                        | Rust Host 下是 WebSocket endpoint（`ScopedServiceEndpoint`）             | D7：Renderer 走本机 WebSocket                                                                                                           |
| `[203]`（非 Error 拒绝）原样转给 Renderer                                                                      | 转成 `[202]`，原载荷放在 `data`                                          | Rust 服务端只产出 Error 载荷；Renderer 只读 message / code                                                                              |
| 转发结果按 TS 值原样序列化                                                                                     | 数组内的 `undefined` 变为 `null`；`VSBuffer` 参数按 `Uint8Array` 转发    | Rust 结果模型是 JSON；契约内没有依赖这两点的成员                                                                                        |
| Bot 远程运行时的 realtime lease 与 stream mirror（reporting wrapper）                                          | 不做；只跟踪运行中任务与 session route announce                          | 依赖 task realtime bridge（tasks 部分）；合并后在 `remote/tasks.rs` 接入                                                                |
| 远端 workspace 的本地 `usage-stats`、`conversation-share`、`bots` 绑定远端 agent / task                        | 使用本地 Host 的同名 channel                                             | 这三个服务的 Rust 实现在其它部分；绑定远端的版本随它们接入（§7）                                                                        |
| 远端资源遥测转发给 Main                                                                                        | 不做                                                                     | 资源采样在 R7                                                                                                                           |
| 本机 Provider 配置、账号设置、凭据变化时发 `provider-provisioning-source-changed`，Main 再同步所有 Environment | 只做 `environment-online` 的首次同步；Main 发来的任何 trigger 都照常执行 | 变化源（provider、settings、credential 的写入）在其它部分；它们在写入成功后发这条消息即可接上                                           |
| sessions-index 观察者在 ACK 前暂存帧（有上限）、组装超时后重订阅                                               | ACK 后按到达顺序处理同一队列里的帧；seq 缺口或 resync 失败时重订阅       | rpc 事件流本身是有序队列，不会丢 ACK 前的帧；没有超时定时器                                                                             |
| controller 解析 source 歧义时整个列表查询失败                                                                  | 远端 identity 的 scope 跳过并记 warn                                     | 不让一个歧义 scope 清空其它 source 的列表                                                                                               |
| SSH 失败时 Host 自己归一化错误文案                                                                             | 文案来自连接器（同一 TS 归一化函数）                                     | 同源                                                                                                                                    |

## 6. 与其它部分的接口

- **tasks**：`rustHostPortBridge.ts` 已为 Main 发起的 MessagePort attachment 签发 endpoint；把它对 `scope.kind === "local"` 的限制放宽为任意 scope 后，Bot 远程运行时（`createBotRemoteWorkspaceRuntimePort`）与手机 shared-host attachment 即可经本规格的远端 scope 工作。
- **account / bots**：provisioning 的凭据解密（`enc:v1` AES-256-GCM，`ring`，与 bots 部分相同的依赖）在 `remote/provisioning.rs` 自带一份；合并后改用共享的 credential store。
- **providers / settings**：写入成功后发 `provider-provisioning-source-changed{trigger}`（见第 5 节），远端 Environment 就会随本机配置同步。
- **tasks**：window-controller 新增 `ExternalSources` 钩子（`set_external`、`publish_external`，State 里缓存远端行与事实），`list_task_list` 把远端 scope 交给它认领、`mutate_task` 把带 `remoteSessionId` 的地址交给它；行构建用的 5 个辅助函数改为 `pub(crate)`。远端行与本地行共用一个 `{logEpoch, seq}`。

## 7. 验收

| 范围                            | 测试                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     | 结果                                                     |
| ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------- |
| rpc 客户端                      | `cargo test -p zcode-host-rpc --test client`：请求编码与 TS 夹具逐字节相同、Initialize 前排队、取消、退订、`[202]` 透传键、`[203]`、连接关闭                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             | 通过                                                     |
| 身份工具                        | `remote::target`：`buildRemoteWorkspaceIdentity` / `buildRemoteEnvironmentKey` / `buildSshRemoteHostKey` 与 TS 输出逐字节相同（期望值由 TS 实现生成），凭据不进复用键                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    | 通过                                                     |
| 注册表                          | `remote::registry_tests`：SSH 复用（凭据不进键、释放后保留为窗口缓存）、Docker 独占、建连失败、取消最后一个 SSH 等待者（迟到成功被释放）、断线、scope 不匹配与 bind 换代、WSL 空闲回收、WSL 释放屏障（运行中任务、失败 fail-closed）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     | 通过                                                     |
| 传输                            | `remote::transport`：帧、hello 校验、`ProcessTransport` 握手与失败诊断                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   | 通过                                                     |
| connection scope                | `remote::remote_tests::agent_scope_*`：握手、carrier 改写（伪造字段被删）、ACK 前帧暂存与放行、resync 归属与路由、lifecycle 失效、关闭时 `closed` 流控与反向退订、其余成员透传                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           | 通过                                                     |
| controller 远端 source          | `remote::remote_tests::controller_*`：远端行与地址、workspace 事实、写操作经投影、断线 offline 与拒写、重连原子替换、释放后跳过且不读本地 tasks-index                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    | 通过                                                     |
| 媒体代理、provisioning、wrapper | `remote::media`（Range、格式、workspace 边界）、`remote::provisioning`（Node 密文解密、allowlist、personal config）、`remote::tasks`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     | 通过                                                     |
| host-core                       | `remote_scope_tests`：未初始化时拒签远端 scope、detach 关闭全部连接、关闭信号结束入站转发                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                | 通过                                                     |
| 真实远端 server                 | `cargo test -p zcode-host-services --test remote_server`：本仓库构建的 `zcode-server.cjs`（临时 HOME，出站 HTTP 指向 127.0.0.1 的关闭端口）。① 经控制消息连接、连接日志、`environment-online` provisioning 返回 applied、环境键不符回 failed；scoped attachment 转发 `system` / `file`（含二进制 `readFileRange`）、未注册 channel 不可达、zcode-agent 握手与 existing-only 订阅错误透传、resync 归属；媒体代理的 206 / 200（跨 1 MiB 分块逐字节一致）/ 416 / 越界路径 / 格式不符 / release 后 404，replayable 不注册 media-preview；释放后 attachment 关闭。② `ConnectorTransport` 经 TS 连接器（`remote-connector-local.ts`）连到同一 server，backend channel 把本机附件上传到远端私有目录并改写路径，释放后连接器退出 | 通过（构建：`pnpm --filter @zcode/server build:remote`） |
| 真实 App                        | `node scripts/rust-host/e2e-desktop.mjs --scenario remote-ssh-connect`：隔离 Electron 上的 Rust Host，经远程向导连接 `127.0.0.1` 上已关闭的 SSH 端口——向导 → Main → Host → 注册表 → 连接器（Electron 以 Node 模式运行 `out/host/remoteConnector.js`）→ ssh2 → 失败；连接日志（含 `detecting remote env...`）与 `ECONNREFUSED` 错误显示在向导里，可重试，Node warning 不进连接日志                                                                                                                                                                                                                                                                                                                                        | 通过                                                     |
| TS                              | `pnpm typecheck`、`pnpm lint`（0 error）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 | 通过                                                     |

未在本机执行：

- 成功建连的真实 App 路径：需要部署资产（CDN 或 `packages/desktop/mock-cdn`）与可登录的远端主机，本机离线无法验证；建连成功之后的 Host 侧链路由上面两个真实 server 测试覆盖。
- 真实 SSH 主机、WSL（Windows 专属）、Docker（本机 daemon 未运行）。三种 backend 的命令构造与认证仍是原 TS 实现，由它们各自的单测覆盖；连接器只是调用它们。
