# Rust Host R3：工作区基础（workspace）

日期：2026-10-09。上级规格：[rust-host.md](rust-host.md) 第 5 节 R3。分支：`feat/rust-host-p-workspace`。

## 1. 目标与范围

把 R3 剩余部分补齐，让 Desktop 在 Rust Host 上的工作区能力与 Node Host 一致：

| channel                      | 成员                                                                                     | 本阶段                                             |
| ---------------------------- | ---------------------------------------------------------------------------------------- | -------------------------------------------------- |
| `git`                        | `generateCommitMessage`                                                                  | 经 AgentRuntime 调 CLI `workspace/generateText`    |
| `git-checkpoint`             | `createCheckpoint`、`diffCheckpoints`、`restoreBetweenCheckpoints`、`deleteCheckpoint`   | 全部实现                                           |
| `media-preview`              | `prepare`、`refreshPlaybackUrl`、`release`                                               | 本地实现；后两者只服务远端，本地按 Node 行为应答   |
| `prompt-attachment-transfer` | `stage`、`adopt`、`cancel`、`cleanup`、`onDynamicProgress`                               | 本地零拷贝实现                                     |
| `system`                     | `probeIntranet`                                                                          | TCP 与 HTTP(S) 探测                                |
| `zcode-agent`                | `attachmentPreviewSourceV4`                                                              | `local_path` 经 Main 授权                          |
| `file`、`file-watcher`、`terminal`、`git` | 已有成员                                                                    | 逐成员对照 Node 审计并修正                         |

不在本阶段：远端 workspace（SSH/WSL/Docker）的媒体代理与附件暂存（R6）。

## 2. 产品规则

### 2.1 提交信息生成（`git.generateCommitMessage`）

- 与 `gitService.ts` 相同：先跑一次 `git status`；`includeUnstaged` 缺省为 `true`；按 `currentSessionFilePaths` 过滤（`commitMessageFileScope.ts` 的路径规则逐条移植）；没有文件时报 `There are no changes available to commit.`；前 8 个文件取 diff，失败的 diff 丢弃。
- 模型：读 `model-selection` 视图的 `preferredSelection`（与 Node `node.ts` 的 `readCurrentModel` 相同），缺失时报 `未读取到当前模型。`。
- prompt、裁剪上限、Conventional Commit 校验与去装饰规则逐字移植 `gitCommitMessageGenerator.ts`；locale 缺省时读 Host 环境的 `LC_ALL` / `LC_MESSAGES` / `LANG`（Node 读 `Intl` 默认 locale），只有 `zh*` 视为中文。
- 调用链：`AgentRuntime.get_client(target)` → 使用前钩子（`UseReason::WorkspaceGenerateText`，对应 Node `ensureAccountProviderConfigSynced(reason: "workspace_generate_text")`）→ `workspace/generateText`（`querySource: "git_commit_message"`，带 `operationId`）。
- 取消：Renderer 取消调用或连接断开时，Host 丢弃该调用的 future；守卫在 drop 时向同一 CLI 发 `workspace/cancelGenerateText { operationId }`（尽力而为，失败只记 debug）。
- 错误信息与 Node 一致：模型请求失败 `模型请求失败。`，输出不合规 `模型没有返回可用的 Conventional Commit 提交消息。`。UI 只显示通用失败文案，不按 message 分支。

### 2.2 Git checkpoint（`git-checkpoint`）

- 移植 `gitCheckpointService.ts` + `gitCheckpointRepo.ts` + `gitCheckpointStore.ts` + `gitCheckpointHelpers.ts`，git 参数、错误文案（`Checkpoint does not exist: <id>`、`Checkpoint workspace mismatch: <id>`、`Checkpoint repoRoot mismatch.` 等）与 Node 相同。
- manifest：`<dataBase>/.zcode/v2/checkpoints/<sha256(workspacePath)[0..12]>/<checkpointId>.json`，内容 `JSON.stringify(meta, null, 2) + "\n"`，临时文件 + rename 原子写；同一文件的写入串行；读取时先等该文件的待完成写入。字段顺序与 Node 相同，Node 与 Rust Host 可互读。
- hidden ref：`refs/zcode/checkpoints/<hash12>/<checkpointId>`；临时 index 在 `<dataBase>/.zcode/git-checkpoint-index/index-XXXXXX/`，结束必删；先复制用户 index（`rev-parse --git-path index`），失败退到 `read-tree HEAD`，再退到空 index。
- `restoreBetweenCheckpoints`：三方比较（from→to diff、校验受影响路径仍等于 from、`force` 才覆盖、restore worktree、删除 deleted/rename 旧路径、再按 to 校验）。冲突时返回 `{ success: false, conflicts }`，校验失败报 `Checkpoint restore verification failed.`。

### 2.3 媒体预览（`media-preview`）

- `prepare({ path, expectedKind })`：扩展名表与 `getMediaPreviewFormat` 相同；格式不符报 `Unsupported media preview format: <path>`；不是文件报 `Path is not a media file: <path>`。
- 本地 desktop：Host 向 Main 发 `local-media-preview-path-authorize-request { requestId, path }`，Main（`localMediaPreviewProtocol`）规范化并登记路径后回 `local-media-preview-path-authorize-result`；Host 返回 `{ kind: "local-url", mediaType, path: <canonical>, size, url: zcode-media://local/preview?path=<form-urlencoded> }`。URL 编码与 `buildLocalMediaPreviewUrl`（WHATWG `URLSearchParams`）一致。
- Main 拒绝时把错误原样抛给 Renderer。Main 不回复时 Host 等待 30 s 后报 `Local media preview path authorization timed out.`（Node 无超时，见第 5 节）。
- `refreshPlaybackUrl` / `release`：Node 本地实现不提供这两个可选成员（Renderer 只在 `host-range-url` 时调用）。Rust 按 Node 远端代理以外的行为应答：`refreshPlaybackUrl` 报 `Media preview is not a host range preview: <previewId>`，`release` 空操作。
- `zcode-agent.attachmentPreviewSourceV4`：只有 `desktop-continuous` 且无 `remoteSessionId` 才问 CLI `v4/attachment/previewSource`；CLI 返回 `local_path` 时同样经 Main 授权后把 `path` 换成规范路径；授权器缺失报 `fault.attachment.previewPathAuthorizationUnavailable`。授权器通过 `Extensions::set_media_path_authorizer` 注入，agent 模块不依赖 media-preview。

### 2.4 Prompt 附件传输（`prompt-attachment-transfer`，本地）

- 与 `createLocalPromptAttachmentTransferService` 相同：`stage` 直接返回 `{ operationId, ref: localPath, bytes, staged: false }`，`bytes` 优先取正的 `sizeBytes`，否则 `stat`，失败为 0；`adopt` / `cancel` / `cleanup` 空操作；`onDynamicProgress(operationId)` 合法但本地永不触发。

### 2.5 内网探测（`system.probeIntranet`）

- 逐条移植 `systemService.ts`：目标归一化（端口缺省 22、超时夹到 [100, 10000] ms、缺省 800 ms、重试次数 [1, 3]、缺省 2）、`requiredSuccessCount` 夹取、策略 `tcp-connect` / `service-http` / `mixed`、错误文案（`timeout(<n>ms)`、`HTTP <status>`、`service returned ok=false`、`marker mismatch(expected=…, actual=…)`）。
- HTTP 探测用 reqwest（rustls + ring，不读系统代理，与 Node `fetch` 一致不走 HTTP(S)_PROXY）；token 放 `x-zcode-intranet-token`。
