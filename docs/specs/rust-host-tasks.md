# Rust Host：tasks（zcode-task 全量、tasks-index 其余仓库、window-controller、replayable、realtime bridge）

日期：2026-10-08。上级规格：[rust-host.md](rust-host.md)；侧栏与 tasks-index 基础见 [rust-host-r2.md](rust-host-r2.md) 第 5、7 节。分支：`feat/rust-host-p-tasks`。

## 1. 目标与范围

R2 只移植了侧栏子集。本规格把 `zcode-task` channel 的其余成员、tasks-index 的其余仓库、window-controller 的归档删除与全文搜索、`web-remote-replayable` attachment 与 task realtime bridge 补齐到与 Node Host 一致：

| 部分                       | Node 来源                                                                 | Rust 模块                                               |
| -------------------------- | ------------------------------------------------------------------------- | ------------------------------------------------------- |
| 分组、grouped 视图、搜索、归档、删除 | `taskIndexRepo.ts`                                                  | `tasks_index/groups.rs`、`grouped_view.rs`、`task_list.rs`、`archive.rs` |
| zcode-task 其余成员        | `zcodeTaskServiceAdapter.ts`、`claude-native/*`、`modelTrajectory*.ts` | `tasks/index_members.rs`、`lifecycle.rs`、`queries.rs`、`claude_import.rs`、`task_events.rs` |
| window-controller          | `windowHostControllerService.ts`                                          | `tasks/controller.rs`                                   |
| replayable attachment      | `zcodeAgentConnectionScope.ts`、`desktopRemoteSessions.ts`                | `host-core`、`agent/service.rs`、`rustDesktopHost.ts`   |
| task realtime bridge       | `host/taskRealtimeBridge.ts`、`main/taskRealtimeBus.ts`                   | `tasks/realtime.rs`                                     |

automation 与闲时任务的表、调度和派发属于另一部分，不在本规格。

## 2. 产品规则

### 2.1 tasks-index：分组与 grouped 视图

与 Node 共用同一个 `tasks-index.sqlite`，SQL、事务边界、排序与错误文案逐条照搬 `taskIndexRepo.ts`。

1. **创建分组**：id 为 `task-group-<uuid>`，标题 trim 后为空时用 `New Group`，颜色缺省 `gray`（不校验）。写 `task_groups`，再把分组放到 grouped 顶层最前（`MIN(sort_order) ?? 2000` 减 1000）。不广播。
2. **改名、改色**：改名标题为空时同样用 `New Group`；颜色必须是 gray、red、orange、yellow、green、blue、purple 之一，否则报 `Task group 颜色无效`。行不存在时分别报 `Task group 不存在，无法重命名` / `Task group 不存在，无法更新颜色`。成功后对参数里的每个 `workspaceScopes` 广播一次不带 taskId 的 `task_meta_changed`。
3. **删除分组**：`BEGIN IMMEDIATE` 内删 `task_groups` 行（成员随外键级联删除）和该分组的顶层顺序；行不存在报 `Task group 不存在，无法删除`。广播同上。
4. **`listGroupedTaskViewStructure`**：先按设置自动归档（`taskAutoArchiveEnabled`，天数 `taskAutoArchiveOlderThanDays ?? 7`，每个 workspace 一次，逐个归档的任务各广播一次带 meta 的 `task_meta_changed`），再读结构：
   - groups：bootstrap 绑定到不可见 workspace 的分组不返回；
   - members、topLevelOrders：不过滤、不排序（SQLite 扫描序，与 TS 相同的 SQL）；task 顺序键只认 `JSON.stringify([workspaceKey, taskId])`，旧格式静默跳过。
5. **`applyGroupedTaskViewOrder`**：
   - 校验在事务外：scope 外的 task、不可见（缺失、已删除、已归档、已置顶）的 task、不存在的分组、同一 task 进多个分组，各自报 TS 的中文错误；provider 不符的 task 静默跳过。
   - `BEGIN IMMEDIATE`：写 bootstrap 标记 → 顶层 task 退出分组 → 按顺序 upsert 分组成员（`(i+1)*1000`）→ 删除全部分组顶层顺序和 scope 内所有 task 顶层顺序 → 按输入顺序插入顶层顺序（重复节点触发主键错误并回滚）。
   - 返回值是 legacy 的完整 grouped 视图（含成员与顶层顺序的补齐写入）。UI 忽略返回值，但写入要一致。
   - 对每个 `workspaceScopes` 广播一次 `task_meta_changed`。
6. **workspace 分组 bootstrap**：只在 grouped 视图查询里、且 bootstrap 表为空时运行。`applyGroupedTaskViewOrder` 先写标记再查询，所以在 zcode-task 现有调用路径上永远不会执行；Rust 照搬标记语义，不移植 bootstrap 本体（见第 5 节）。

### 2.2 tasks-index：搜索、归档、删除

1. **`listTaskList`（`queryTaskList`）**：
   - workspace key 去重、去空、排序；为空时直接返回空结果。
   - 条件顺序：`deleted = 0` → `workspace_key IN` → provider（zcode-task 固定 `glm`）→ kind（pinned：`pinned=1 AND archived=0`；archived：`archived=1`；其他：`pinned=0 AND archived=0`）→ 搜索时 `(LOWER(title) LIKE ? OR LOWER(searchable_text) LIKE ?)`，模式为 `%<trim 后整串小写>%`，不分词、不转义。
   - 先 `COUNT(1)`，再按 `sortBy`（created：`created_at, updated_at, task_id` 倒序；否则 `updated_at, created_at, task_id` 倒序）取 `limit`（有限正数取整）。`hasMore = total > 返回条数`。
   - 搜索时每行生成摘要：在小写正文里逐个找命中，窗口前 20、后 72 个 UTF-16 单元，前后加 `...`，空白折叠后截 140，与已有窗口重叠的跳过，最多 4 条；一条都没有时用整段正文兜底。非空时加 `searchSnippet`（第一条）与 `searchSnippets`。scope 带 `workspacePurpose` 时，行上补 `workspacePurpose`。
2. **`archiveStaleTasks`**：天数 `max(1, floor(n))`，截止时间 `now - days*86400000`。条件：未删除、未归档、未置顶、无未读、`updated_at < cutoff`、`task_status = 'completed'`；不带 provider 过滤。`BEGIN IMMEDIATE` 内只改 `archived` 列，返回更新前读出的 meta。每个归档的任务广播一次带 meta 的 `task_meta_changed`。
3. **`archiveWorkspaceTasks`**：列出该 workspace 的 glm 活跃与置顶行，逐个设为归档（每个一个事务），然后广播一次不带 taskId 的 `task_meta_changed`，最后返回归档分区。
4. **`deleteArchivedTask`**：`BEGIN IMMEDIATE` 内检查行存在、未删除、已归档，否则返回 false 且不广播；写删除墓碑并清理分组引用后提交，再广播带 meta 的 `task_deleted`。不清理 CLI 会话。
5. **`deleteArchivedTasks`**：taskIds 去重保序；为空直接返回三个空数组。逐个执行第 4 条（各自一个事务）：返回 null 记入 skipped，出错记入 failed，其余记入 deleted。只有 deleted 非空时，整批结束后广播一次不带 taskId 的 `task_deleted`。

### 2.3 window-controller

1. **搜索**：`listTaskList` 带 `search` 时，对每个 source 调用 zcode-task 的 `listTaskList`（只含该 source 的 scope，不带 limit），结果与投影行按 taskId、path、identity 对齐：有投影行时用投影 meta 与 liveStatus、activity，否则用索引 meta 并由 meta 推导 liveStatus；带回 `searchSnippets`。排序与 limit 同无搜索时。
2. **`deleteArchivedTask(s)`**：经 TaskSource 调用 zcode-task 的同名实现，再强制刷新该 source；刷新失败不改写已提交结果。批量删除 taskIds 为空时直接返回三个空数组。
3. 排序的 taskId 比较按 `localeCompare` 的常见结果（ASCII 下即字典序）。
