# Rust Host：bots（全部通道、流式卡片、选择命令、会话读取）

日期：2026-10-09。上级规格：[rust-host.md](rust-host.md)；R6 首版见 [rust-host-automation.md](rust-host-automation.md) 第 7 节（文件、凭据、飞书长连接、运行锁、绑定与基础命令）。分支：`feat/rust-host-p-bots`。

## 1. 目标与范围

R6 首版只做了飞书 / Lark 长连接、20 个成员和终态摘要回推。本规格把 `bots` channel 补齐到与 Node Host（`packages/services/src/bots/**`）一致：

| 部分                     | Node 来源                                                                        | Rust 模块（`apps/zcode-host/crates/services/src/bots/`） |
| ------------------------ | -------------------------------------------------------------------------------- | -------------------------------------------------------- |
| 微信扫码注册（2 个成员） | `providers/weixinRegistration.ts`                                                | `weixin.rs`                                              |
| Telegram                 | `providers/telegramProvider.ts`、`telegramChannelRuntime.ts`                     | `telegram.rs`、`channel.rs`                              |
| Webhook                  | `providers/webhookProvider.ts`                                                   | `webhook.rs`                                             |
| 微信 iLink               | `providers/weixinProvider.ts`、`weixinChannelRuntime.ts`                         | `weixin.rs`、`channel.rs`                                |
| 通道适配与重试           | `providers/types.ts`、`providerRequest.ts`、`channelRuntime.ts`                  | `providers.rs`、`channel.rs`、`runtime_lock.rs`          |
| 飞书卡片                 | `feishuProvider.ts`（交互卡片、流式卡片、分段、typing reaction、card/update）    | `feishu/cards.rs`、`feishu/provider.rs`、`feishu/api.rs` |
| 回复流水线               | `botsService.ts` `watchTaskStream`、`replyFormatter.ts`、`statusFormatting.ts`   | `watch.rs`、`reply.rs`、`status.rs`                      |
| 会话读取                 | `getTaskSnapshot`、`onDynamicTaskEvent`（legacy）                                | `stream.rs`（V4 帧适配，见第 4 节）                      |
| 选择命令                 | `botsService.ts` `handleInboundMessage` 的 workspace/model/mode/think/task/reply | `selection.rs`                                           |
| 问答与权限               | `botsService.ts` elicitation / permission 分支、飞书问答卡片                     | `interaction.rs`、`feishu/cards.rs`                      |
| 入站、授权、上下文、附件 | `botsService.ts` `processProviderCallback`、`withAuthorizedContext` 等           | `inbound.rs`、`aes.rs`（微信 CDN 解密）                  |

不在本规格：远端 workspace 的 Bot 运行时与 `/reconnect` 实连（见第 7 节，留接缝）。

## 2. 产品规则

### 2.1 成员

22 个成员全部实现，参数、结果形状与错误文案同 Node：

1. `beginWeixinRegistration()`：`GET <iLink>/ilink/bot/get_bot_qrcode?bot_type=3`，头 `iLink-App-ClientVersion: 1`，30 s 超时。响应先把 `data` 摊平到顶层（`{...payload, ...payload.data}`），`ret` / `errcode` 非 0 时报 `errmsg` 或 `ret=<r> errcode=<e>`；HTTP 非 2xx 报 `Weixin login /get_bot_qrcode?bot_type=3 failed: HTTP <status>`。返回 `{qrCode, qrUrl, interval: 3, expiresAt: now + (expires_in ?? 120) s}`；`qrcode`/`qr_code` 缺失报 `Weixin login did not return a QR code.`。
2. `pollWeixinRegistration({qrCode})`：`GET .../get_qrcode_status?qrcode=<encodeURIComponent>`。`status`（或 `qrcode_status`、`qr_status`）按 Node `normalizeQrStatus` 归一（数字 0/1/2/3|4 与字符串别名）。成功时取 `bot_token`/`token`，缺失回 `{status:"error", message:"Weixin login succeeded but did not return bot_token."}`；`botId` 取 `ilink_bot_id`/`bot_id`。请求超时回 `{status:"pending", interval:3}`。
3. `testBot`：按通道分派（第 2.3 节），discord / wecom 仍回 `reserved for a future version`。
4. `handleProviderCallback(Response)`：所有已实现通道（含 Webhook、Telegram、微信）走同一条 `processProviderCallback`；未知通道（discord / wecom）回 `{ok:false, replies:[], status:400}`。payload 必须自带 `botId`（Webhook 的 HTTP 入口按路由补上，同 Node）；R6 首版在缺 botId 时补“该通道第一个启用的 Bot”，已去掉。
5. `saveBot` 的名称回填：飞书 0/800/1800 ms 重试，其余通道只试一次（Telegram 读 `getMe` 的 `first_name || username`）。`saveConfig` / `saveBot` / `removeBotSecret` / `deleteBot` 之后三个运行时一起对齐；删除或摘密钥时，Telegram 先 `deleteMyCommands`，飞书与微信先停连接。

### 2.2 出站消息与重试

1. **统一请求**（`providerRequest.ts`）：每个出站 HTTP 请求有 15 s 截止，覆盖到读完响应体；JSON 响应体非法时 2xx 报错、非 2xx 视为无载荷。
2. **出站消息**（`BotOutboundMessage`）：`{botId, provider, providerUserId, text, locale?, selection?, elicitation?, providerContextToken?}`，`providerUserId` 群聊取 chatId。各通道：
   - **Telegram**：按 3900 个 UTF-16 码元切分，`sendMessage {chat_id, text, parse_mode:"Markdown"}`；非 2xx 时去掉 `parse_mode` 重发一次。选择项只挂在最后一段：每个选项一行 inline 按钮，`callback_data` 只放序号（`zc:<action>:<n>`；权限 `zc:permission:<n>`；问答 `zc:e:<token>:<n>`；模型 `zc:cmd:/model provider|model <n>`），`showCancel !== false` 时追加 `zc:cancel`。回调解码规则同 `decodeTelegramCallbackData`。
   - **Webhook**：`POST webhookUrl`，体 `{type:"zcode.bot.message"|"zcode.bot.elicitation_request", botId, provider:"webhook", userId, text, selection, elicitation, sentAt}`，带选择时正文追加 `n. label - description\n<命令>`；secret 放 `webhookAuthHeaderName || x-zcode-bot-secret`。按 0 / 500 / 1500 ms 重试，2xx 与 4xx 立即返回，最后仍非 2xx 报 `Webhook outbound endpoint returned HTTP <status>`。没有 webhookUrl 时静默跳过。
   - **微信**：`POST <iLink>/ilink/bot/sendmessage`，头 `AuthorizationType: ilink_bot_token`、`Authorization: Bearer <token>`、`X-WECHAT-UIN: base64(随机 u32 十进制)`，体 `{base_info:{channel_version:"2.0.0"}, msg:{from_user_id, to_user_id, client_id:"zcode-weixin-<uuid>", message_type:2, message_state:2, context_token?, item_list:[{type:1, text_item:{text}}]}}`，换行统一成 CRLF；`ret`/`errcode` 非 0 报 `Weixin iLink /sendmessage failed: <errmsg>`。微信没有原生选项卡：选择用纯文本编号（第 2.5 节）。
   - **飞书 / Lark**：交互卡片 Card JSON 2.0；带选择时每个选项一个 `primary` 按钮（`behaviors[0].value = {command, zcodeCardText}`），再加取消按钮；不带选择时按 1900 个 UTF-16 码元分卡。投递结果记入 `deliveryError`（只飞书）。
3. **typing**：Telegram `sendChatAction typing`，长任务每 4 s 重发；微信先 `getconfig {ilink_user_id, context_token?}` 取 `typing_ticket` 再 `sendtyping {status:1}`；飞书给入站消息加 `Typing` reaction（记下 reaction_id），短命令回复发送后、长任务终态或进入交互时删除。全部失败静默。
4. **回调确认**：Telegram `answerCallbackQuery`（toast 取首条回复文本，折叠空白后截 180）并清掉原消息的按钮；飞书异步卡片回调用 `card/update`（token 按 Node 的十二个候选字段读取，带 `open_ids`）把原卡改为去掉按钮的原文。确认有 3 s 截止，超时只记 warn。飞书长连接里的卡片点击走同步回包，不再 `card/update`。

### 2.3 通道运行时

1. **对齐**（`createLatestRuntimeRefreshQueue`）：每次配置变更后三个运行时（Telegram 轮询、微信轮询、飞书长连接）各自串行对齐，只有最新一次走完；凭据指纹 `sha256(JSON[provider, credentialRef, secret])`（飞书多一个 appId）变化时先停旧连接再起新连接；禁用的 Bot 状态为 `disabled`（`bots.runtime.botDisabled`）。
2. **Telegram 轮询**：缺 token 报 `Telegram bot token is missing.` 并退出。循环：拿 `telegram-polling/<sha256(token)>` 运行锁（他人持有：idle + `bots.runtime.telegramLongPollingHandledElsewhere`，10 s 后重试；锁 IO 失败 error，5 s 后重试）→ `deleteWebhook {drop_pending_updates:false}` → `getUpdates {timeout:25, offset?, allowed_updates:["message","callback_query"]}`（40 s 截止）。409 报 `Telegram token is already used by another polling client.` 等 10 s，其他非 2xx 报 `Telegram getUpdates failed: HTTP <s>` 等 5 s；`ok !== true` 报 description 等 5 s。每条 update 交给 `processProviderCallback("telegram", {botId, update})`，`ok:false` 视为失败；**成功后才**把 offset 写成 `update_id + 1`（`bot-state.v3.json`；没有上下文时按首个允许的 workspace 建一条草稿上下文）。异常时 `bots.runtime.telegramPollingFailedRetrying`，释放锁后 5 s 重试。启用的 Bot 对齐时先 `setMyCommands`（`help/status/new/project/model/mode/think/reply/bind` 按命令策略过滤，英文描述同 Node）。
3. **微信轮询**：锁 `weixin-polling/<sha256(token)>`；`POST /ilink/bot/getupdates {get_updates_buf}`（90 s 截止）。消息逐条交给 `processProviderCallback("weixin", {botId, messages:[{id, text, from, chatId, displayName, context_token, attachments}], buf?})`，失败即中断本批；整批成功后才写 `weixinGetUpdatesBuf`。异常时 `Weixin polling failed: <msg>`，5 s 后重试。
4. **飞书长连接**：同 R6（`rust-host-automation.md` 7.1 第 8–10 条）。
5. **停止**：`stopPolling` / `stopWebSocket` 通知停止并等待请求退出、锁释放，再把状态改为 idle（`...Stopped`）。Host 退出时 `dispose` 等三个运行时全部收尾。desktop-attached 远端 Host（`background=false`）不起任何运行时，也不同步 Telegram 命令。

### 2.4 入站

1. **解析**：Telegram `message`（text 或 caption、私聊判断 `chat.type === "private"`、附件元数据）与 `callback_query`；Webhook `{botId, text, userId, displayName?, chatType?, chatId?, messageId|id, attachments?, type:"zcode.bot.elicitation_response"...}`；微信 `msgs|messages|updates|items|list`（`message_type === 2` 是 Bot 自己的消息，丢弃）、`item_list` 文本、发送者与 context_token；飞书同 R6。
2. **校验**：Webhook Bot 有 `webhookSecretRef` 时 payload 的 `webhookSecret` 必须相等，飞书比对 `token` / `header.token`；不符回 `webhookSecretInvalid`。
3. **授权**（`findAuthorizedBot`）：微信按 botId 找启用的微信 Bot（扫码登录没有用户 id），其余通道按 `providerUserId` 找。微信首条普通消息只做激活（写 `weixinActivatedAt`，回 `weixinActivatedWelcome` + 帮助），不当作 prompt。
4. 发送者昵称：飞书用通讯录接口 `contact/v3/users/<id>?user_id_type=` 补 `displayName`（缓存 10 min），失败只记 debug。
5. 去重、串行、失败释放去重、回复投递失败 → `ok:false status:503`（同 R6）。

### 2.5 选择命令

完全移植 `handleInboundMessage` 的分支与文案：

1. **待选状态**：每个聊天上下文（`botId::provider::chatId||userId`）一份 `SelectionPrompt` 与 task / workspace 候选缓存（只在内存，Host 退出即丢）。`/new`、`/workspace` 切换会清掉该 Bot 的全部待选状态。
2. **展示**：`markCurrentSelection` 给当前项加 ` · 当前`（en-US ` · current`），`cancelLabel` 用 `selectionCancelOption`。非微信通道发结构化选择（标题即正文）；微信发纯文本：`标题\n0. 取消\n1. label description\n…\n\n回复数字选择，0 取消。`，模型供应商一级不展示描述，`showCancel:false` 时不显示 0。
3. **数字回复**：只有微信把纯数字普通消息解析成待选项（越界或非数字清空待选并按普通消息处理）；其他通道普通文本一律清空待选。非微信通道里单独的 `0` 是普通消息。
4. **命令**：
   - `/workspace`：运行中回 `taskRunning`；列出允许的 workspace（远端加 ` [远端]` / ` [Remote]`），选中后写草稿上下文（Bot 硬锁 yolo）并回 `/status`。
   - `/model`：草稿态列出目标 Host 的模型供应商（`modelProviderSelectTitle`，当前项按 `custom:` / `provider/model` 解析），选供应商后列模型（`modelModelSelectTitle`），选模型后经 `completeNewModelSelection` 补成完整选择写入草稿；任务态同样两级，最后 `session/setModel` 并以 resume 快照写 tasks-index。
   - `/mode`：一律回 `modeLocked`。
   - `/think`：草稿态从模型规格的 `reasoningLevel` 取候选，选中写入草稿 `modelSelection.options.reasoningLevel`；任务态读会话 configOptions，选中后 V4 `switchModelConfig`。没有候选回 `thoughtLevelMissing`。
   - `/task`：运行中回 `taskRunning`；列出当前 workspace 最近 10 个任务（标题 + 状态），选中后写任务上下文并回 `/status`；按值找不到时按 taskId 读会话（`session/read`）。
   - `/reply`：列出该通道支持的回复颗粒度（飞书只有流式卡片），选中经 `saveBot` 保存。
   - `/cancel` 与 `0`：有待选项时清空并回 `/status`；没有时回 `unknownCommand 0`（有待回答的问答时改为取消问答）。
5. **`/new`**：继承当前任务的模型与思考档位（会话 configOptions），模式硬锁 yolo；`/status` 增加 `已工作` 与 `进展`（运行中优先用流式缓存的最新进展，其次 `session/read` 的最后一条助手消息）。

### 2.6 回复颗粒度与流式卡片

`watchTaskStream` 移植（`watch.rs`）。每个 (workspaceKey, taskId) 只有一个订阅；Bot 发出的每次输入都先订阅、再发送。

1. **assistant_changes**（Telegram / Webhook / 微信默认）：正文 chunk 累积在缓冲区，遇到工具调用或终态时整段发出（超过 3500 字符按换行 / 空格切分）。
2. **assistant_toolcalls_changes**：同上，另外每个工具进入终态时发一条 `工具调用：\n- <状态> · <工具名> · \`<命令或路径>\` · 🟢 \`+n\` 🔴 \`-m\``。
3. **summary_changes**：终态时只发最后一段正文（latestPart）。automation 回推把颗粒度设为它；飞书 / Lark 只支持流式卡片，归一化后 automation 回推仍是流式卡片（Node 同样如此）。
4. **streaming_card**（飞书 / Lark 唯一模式）：一张卡片随流更新。
   - 时间线块：连续正文合并成一个 message 块；工具按出现顺序归入 tools 块（同一工具只出现一次），渲染为可折叠面板 `🛠️ 工具摘要 (n)`，只有运行中的最后一个面板展开；没有任何块时显示 `正在处理...`；非 sealed 状态末尾加 `_⏳ 运行中_` / `_✅ 已完成_` / `_失败_`。
   - 节流：首次创建立即发送；之后正文 chunk 至少间隔 1 s 才 PATCH，工具出现与终态、工具进入终态时强制更新。
   - 失败退避：连续失败 1、2 次分别等 1 s、2 s 再试，第 3 次熔断，本轮不再更新卡片；创建接口返回 code 0 但没有 message_id 也算失败。退避与熔断先于强制更新判断。
   - 分段：Card JSON 2.0 带 tag 的元素超过 180 个时，已满的卡片封存（sealed，不带状态行），后续块写入新卡片。
   - 封存：权限请求或问答出现前，当前卡片以 sealed 状态更新一次后释放，交互之后的输出写入新卡片。
   - 终态：完成时状态改为已完成并强制更新；失败时没有正文就先追加 `任务失败：<msg>`。
5. **完成兜底**：非流式卡片模式下，终态时如果什么都没发过，发 `任务已完成。` / `Task completed.`；失败时发 `任务失败：<msg>`。
6. **变更摘要**：Node 的任务快照 `fileChanges` 固定为空，完成消息实际不带变更摘要；Rust 同样不发。

### 2.7 问答与权限

1. **问答**（AskUserQuestion / 计划审批）：会话出现 `userInput` 类待处理交互时，先封存流式卡片，写 `pendingElicitation`（taskId、requestId=interactionId、runId、origin、actorKey、题目、答案草稿、计划正文），再发问答选择：每题一个选择，多选题 `[x]`/`[ ]` 前缀加“完成”，单选题加“跳过”，计划审批补“自定义回答”。选择 token = `sha256(taskId::runId::requestId::题号)[0..12]`，非微信通道的按钮必须带 token，否则回 `elicitationExpired`。
2. 回答推进与 Node 相同：单选直接下一题，多选切换草稿，`__custom__` 展开自定义输入，`__form__:` 表单值与按钮草稿合并，最后一题提交 V4 `resolveInteraction {action:"accept", content}`（`answers` 按题目文本、`answer_<i>`、单题时 `answer`）。`/cancel` 提交 `cancel`。提交成功后清掉待答状态，回 `elicitationSubmitted` / `elicitationCancelled`。
3. 飞书问答卡片：标题 `#### 提问`，选项按钮带 ○/●（单选）或 ☐/☑（多选），自定义回答按钮，必要时附表单（输入框 + 完成）与取消按钮；已回答的题目逐题列在上方；完成后更新为只读卡（计划审批保留计划正文，取消显示 `✅ 问答已取消`）。飞书流式卡片模式下问答用同一张临时卡片原地更新，回调后由同步回包或 card/update 刷新。
4. **权限**：Bot 会话硬锁 yolo，正常不会出现权限请求；出现时按 Node 发选择（允许 / 始终允许 / 拒绝 / 始终拒绝，顺序与说明文案同 Node，`showCancel:false`），`pendingPermissionOptions` 记入上下文，回答经 `resolveInteraction {optionId}` 提交，回 `permissionSubmitted` / `permissionDenied`。
5. 交互从会话的待处理列表消失时（桌面端已回答），清掉对应的待答状态。

### 2.8 附件

Telegram（`getFile` + 文件下载）、飞书（`im/v1/messages/<id>/resources/<key>`）、微信（CDN 下载 + AES-128-ECB 解密）、Webhook（`dataBase64` / `localPath` / `downloadUrl`）。每条消息最多 4 个，超过 5 MB 回 `attachmentTooLarge`，下载失败回 `attachmentDownloadUnavailable`，30 s 截止。缓存到 `<appConfig>/bot-attachments/<botId>/<messageId>/<sha256前16>-<文件名>`；图片和音频作为 prompt 附件（`session/send`），其余只在 prompt 里写本地路径（文案同 Node）。

## 3. 状态所有者

| 状态                                                                                                 | 唯一所有者                                                      |
| ---------------------------------------------------------------------------------------------------- | --------------------------------------------------------------- | -------------- | ---------------- | ------------------------ |
| Bot 配置、聊天上下文（草稿 / 任务 / 待答问答 / 待答权限 / 游标）                                     | `bot-config.v3.json`、`bot-state.v3.json`（与 Node 同文件同锁） |
| 通道凭据                                                                                             | account 的 `CredentialStore`（`credentials.json`）              |
| 某个 token / 应用的连接归属                                                                          | `bots-runtime-locks/<telegram-polling                           | weixin-polling | feishu-websocket | lark-websocket>/` 目录锁 |
| 待选项、task / workspace 候选、绑定码、入站去重、typing 句柄、流式卡片句柄、运行中任务、/status 进展 | `Bots`（内存，Host 退出即丢，同 Node）                          |
| 会话行、交互、终态                                                                                   | CLI；Bot 只读 V4 帧与 `session/read`                            |

## 4. 会话读取（有意的偏差）

Node Bot 通过 legacy 任务快照与事件流（`getTaskSnapshot`、`onDynamicTaskEvent`、`onDynamicStreamEvent`）读会话。Rust Host 有意不提供这三个 legacy 成员，Bot 直接读 agent runtime：

- **快照**：CLI `session/read`（模型选择、思考档位、/status 的已工作时长与进展）与 `session/resume`（configOptions）。
- **实时**：Bot 用自己的连接（`host-bots-<uuid>`，`desktop-continuous`）对 `conversation/<taskId>` 发 `v4/conversation/subscribe`，从 runtime 的 V4 帧总线取帧（与 sessions-index 观察者同一套 `Assembler` 重组），`stream.rs` 的投影器把帧翻译成 Node `watchTaskStream` 消费的同一组事件。订阅建立并收到初始快照后才发送输入（10 s 内没收到快照则退回只用终态事件）。

### 4.1 映射表（V4 帧 → Bot 事件 → Bot 消息）

基线：初始快照里已有的行与交互只记录、不产生事件；之后到达的快照（resync）按行 id 与已知状态求差。

| V4 帧                                                                 | Bot 事件（Node 名）                   | Bot 消息                                               |
| --------------------------------------------------------------------- | ------------------------------------- | ------------------------------------------------------ |
| `row.appended` / `row.upserted` `assistantText`（新行，text 非空）    | `agent_message_chunk(text)`           | 流式卡片 message 块 / 正文缓冲                         |
| `row.delta {path:"text", append}` 落在 assistantText 行               | `agent_message_chunk(append)`         | 同上（卡片节流 1 s）                                   |
| 已知 assistantText 行被 upsert 且 text 变长                           | `agent_message_chunk(新增后缀)`       | 同上                                                   |
| `reasoning` 行新增 / `row.delta` 落在 reasoning 行                    | `agent_thought_chunk`                 | 只进 /status 进展与 latestPart 边界，不发送            |
| `toolCall` 行首次出现                                                 | `tool_call(toolName, input)`          | 正文缓冲先发出；卡片 tools 块；强制更新卡片            |
| `toolCall` 行 status / input / output 变化                            | `tool_call_update(status, …)`         | 卡片工具摘要；完整颗粒度在工具终态时发工具卡           |
| `state.updated` / 快照 `pendingInteractions` 新增 `userInput`         | `elicitation_request`                 | 封存卡片 → 问答选择 / 飞书问答卡                       |
| 新增 `permission`                                                     | `permission_request`                  | 封存卡片 → 权限选择                                    |
| 交互从列表消失                                                        | `elicitation_response`                | 清掉待答状态                                           |
| `control.phase`：非终态 → `completedSuccess` / `completedInterrupted` | `task_complete`                       | 卡片完成 / 缓冲发出 / 摘要 / `任务已完成。`            |
| `control.phase`：非终态 → `error`                                     | `task_error(lastError.message)`       | 卡片失败 / `任务失败：<msg>`                           |
| 订阅失败或进程换代（帧流结束）                                        | sessions-index 终态（TaskDriver）兜底 | 读 `v4/conversation/rowsRange` 最后一段正文（R6 行为） |

工具状态：`inputStreaming`/`pendingApproval` → pending，`running` → in_progress，`success` → completed，`error` → failed（带 `error.message`），`cancelled` → stopped。工具标题与 kind 都取 `toolName`（同 Node legacy 映射）。

- 工具行在 `inputStreaming`（输入还在流式生成）时不投影；离开该状态时带完整输入发 `tool_call`（legacy 的 tool_call 在工具调度时带 input）。
- 工具输入优先取行上的 `input`，没有时解析 `inputText`（Rust CLI 只下发 JSON 文本）。
- Rust CLI 在 desktop-continuous 下常用 `row.upserted` 携带完整文本而不是 `row.delta`；投影器对已知行按“新文本以旧文本开头”求差，两种写法得到相同的 chunk。
- 轮次收口以基线之后新出现的 `turnHeader` 进入终态为准（同一帧里先处理完全部增量，再取 `control.lastError`）；没有新轮次行时，以观察到运行（`prewarming`/`running`）后 phase 进入终态为准。

## 5. 事件顺序

```text
通道（Telegram 轮询 / 微信轮询 / 飞书长连接 / Webhook 回调）
  │ update ─► processProviderCallback：校验 → 昵称 → 去重 → 按聊天串行
  │                  └► handleInboundMessage：授权 → 命令 / 选择 / 问答 / 普通消息
  │ 普通消息：
  │   草稿 → model_view 解析 → session/create(yolo, deferred) → 写上下文(task)
  │   任务 → session/resume + session/read 原选择 → model_view 有效选择
  │   watch：v4/conversation/subscribe(conversation/<id>) ─► 等初始快照（基线）
  │   后台 sendText / session/send(附件)  ─────────────────────────────► CLI
  │ ◄── ACK（Telegram offset / 微信 buf 此时才提交）
  │                                  CLI ─► V4 帧（row.delta / toolCall / state.updated）
  │   stream 投影器 ─► 事件 ─► 串行队列 ─► 卡片 create/update（节流、退避、熔断、分段）
  │                                                    或正文缓冲 / 工具卡
  │                                  CLI ─► state.updated control.phase=completedSuccess
  │   task_complete ─► 终态卡片 / 剩余正文 / 兜底文案 ─► 退订、停 typing、清运行中
```

## 6. 有意与 Node 不同之处

| Node 行为                                                                                            | Rust Host                                                                                           | 理由                                                                       |
| ---------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------- |
| 读 legacy 任务快照与事件流                                                                           | `session/read` + V4 帧适配（第 4 节）                                                               | Rust Host 不提供 legacy 快照成员；V4 是 CLI 的权威投影                     |
| `bots:task`、`bots:task-stream` 广播                                                                 | 不发                                                                                                | Renderer 直接订阅 V4 会话，侧栏由 tasks syncer 的任务事件驱动              |
| 权限请求摘要用 `getPermissionRequestPreview`                                                         | 用 V4 交互的 `summary` 与 detail 里的 command / path                                                | V4 交互已带 CLI 生成的摘要；Bot 会话硬锁 yolo，正常不出现权限请求          |
| `/status` 的已工作时长读任务快照消息                                                                 | 读 `session/read` 消息（不做可见性过滤与连续助手消息合并）                                          | 只影响少数边界上的时长与进展取值                                           |
| 首发配置失败时删除临时任务（`applyDraftConfigOptions`）                                              | 建会话时直接带 `mode: yolo`，没有单独的配置步骤                                                     | 结果相同（任务硬锁 yolo），没有中间态                                      |
| 远端 workspace 的 Bot 运行时、`/reconnect`                                                           | 远端上下文视为未连接（第 7 节）                                                                     | 远端在另一个区域                                                           |
| 开放平台 / Telegram / iLink 基址固定                                                                 | `ZCODE_FEISHU_OPEN_BASE_URL`、`ZCODE_TELEGRAM_API_BASE_URL`、`ZCODE_WEIXIN_ILINK_BASE_URL` 等可覆盖 | 只供测试与 E2E 指向 127.0.0.1 mock                                         |
| `/new` 继承模型时用 `parseBotModelOptionValue` 解析 `provider/model$level`，`$level` 留在 modelId 里 | 先去掉 `$level` 后缀（档位另由思考档位带上）                                                        | Node 的写法让 `/new` 之后的下一条消息必然解析模型失败（Node 缺陷，已报告） |
| `listWorkspaceRefs` 5 s 缓存                                                                         | 每次读设置                                                                                          | 设置读取本身已缓存                                                         |
| 微信 CDN 附件用 Node `crypto` 的 AES-128-ECB 解密                                                    | `bots/aes.rs` 按 FIPS-197 自实现解密                                                                | 依赖树里没有可用的分组密码实现；用标准测试向量校验                         |
| 入站去重、待选项、typing 等内存状态在 Host 退出前 `disposeAllAndWait`                                | Host 退出时进程结束，运行锁靠 30 s 租约与进程存活检查回收（同 R6）                                  | Host 关闭路径没有 Bot 的异步收尾钩子；锁的跨进程语义不变                   |

## 7. 远端 workspace 接缝

`RemoteBots` trait（`bots/mod.rs`）对应 Node `BotRemoteWorkspaceService`：`is_connected`、`ensure_connected`、`task_port(target)`。当前安装的是 `NoRemote`：`is_connected` 恒为 false、`ensure_connected` 回 `remote reconnect service unavailable`。因此：

- 带 `workspaceIdentity` 的上下文：`help`、`status`、`workspace`、`reconnect`、`reply` 照常处理，其余回 `remoteDisconnected`；`/status` 显示远端未连接；`/reconnect` 回 `remoteReconnectUnavailable`。
- `syncAppRuntimePreferences` 转给 `RemoteBots::sync_app_runtime_preferences`（缺省空操作）。
- 远端区域合并后只需提供实现并在 `bots::install` 注入，入站、选择与回复流水线不用改。Node 对 `/reconnect` 的两层幂等（同一 provider 消息 2 分钟内只执行一次、同一聊天与 workspace 3 秒内只重连一次）只在远端可用时才有意义，随远端实现一起补。

## 8. 验收

- `cargo test -p zcode-host-services --lib bots`（`bots/tests.rs`、`tests_channels.rs`、`tests_commands.rs` 与各模块单测）：
  - 对拍：命令解析、归一化、旧版导入、文案表（`bots-fixtures.ts`）；
  - 127.0.0.1 mock：Telegram（getMe 回填名称、setMyCommands、deleteWebhook、getUpdates、sendMessage 与 Markdown 回退、inline 按钮与 answerCallbackQuery、409 退避、业务失败不提交 offset）、Webhook（0/500/1500 重试、secret 校验、选择正文、结构化问答回执）、微信（扫码注册两成员、激活、纯文本编号菜单与数字回复、typing ticket、buf 提交）、飞书（长连接绑定、通讯录昵称、Typing reaction、流式卡片创建与 PATCH、模型选择卡与同步回包、流式卡片的退避与熔断、按元素预算分段）；
  - V4 投影器：快照基线、row.delta 与 upsert 求差、工具状态与 inputStreaming、交互出现与消失、终态与失败、resync；
  - 选择命令：workspace / model 两级 / think / mode / task / reply / cancel / stop / `/new` 继承；问答按钮推进与提交、过期 token；automation 回推与订阅失败时的终态兜底。
- `cargo test -p zcode-host-services --test bots_cli`（`ZCODE_TEST_CLI_RUST_BINARY`）：真实 Rust CLI + mock 模型，Bot 的订阅与投影器从真实帧得到正文 chunk、Bash 工具调用与完成、轮次收口，第二轮不重放上一轮。
- E2E `automation-bots`：隔离 App 预置一个已绑定的 Telegram Bot（Bot API mock 在 127.0.0.1），Host 启动即同步菜单并长轮询；mock 投递一条消息，Host 建会话（yolo）、Rust CLI 跑 mock 模型，断言 mock 的 sendMessage 收到回复、typing 已发、offset 提交为 2，侧栏出现 Bot 创建的任务并能打开看到同一段回复。
