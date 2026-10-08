# Rust Host R5：扩展（skills、plugins、commands、memory、hooks、MCP、subagents）

日期：2026-10-08。上级规格：[rust-host.md](rust-host.md) 第 5 节 R5。分支：`feat/rust-host-p-extensions`。

## 1. 范围

在 Rust Host 上实现下列 channel，使设置页的技能、插件、命令、Memory、Hooks、MCP、Subagent 页面，以及 Composer 的 `/` 选择器在 Rust Host 与 Node Host 上行为一致。Node 实现位于 `packages/services/src/<channel>/`。

| channel       | 成员                                                                                                                                                                                                                       | 数据来源                                                |
| ------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------- |
| `skills`      | `list`、`setEnabled`、`buildPromptContext`、`copyToCommon`、`removeFromCommon`、`deleteSkill`                                                                                                                              | 技能目录、插件目录、`~/.zcode/cli/config.json`          |
| `skill-sync`  | `listLocalUserSkillCandidates`、`listRemoteUserSkillStatuses`、`exportSkillsArchive`、`checkRemoteUserSkillWriteAccess`、`importSkillsArchive`                                                                             | 用户技能目录，tar.gz 归档                               |
| `plugins`     | `getOverview`、`addMarketplace`、`removeMarketplace`、`updateMarketplace`、`installPlugin`、`uninstallPlugin`、`setPluginEnabled`                                                                                          | 已退役：Node 固定回空概览，其余抛错                     |
| `plugin-sync` | `listLocalUserPluginCandidates`、`listRemoteUserPluginStatuses`、`exportPluginsArchive`、`exportMarketplaceSourceArchive`、`importPluginsArchive`、`checkRemoteUserPluginWriteAccess`、`importMarketplaceSourceArchive`    | `plugins.dirs`、插件目录、marketplace 清单，tar.gz 归档 |
| `commands`    | `list`、`writeCommandFile`、`updateCommandFile`、`deleteCommandFile`、`setCommandEnabled`、`getPrimaryUserCommandsDirectory`                                                                                               | 命令目录、插件目录、`config.json` 的 `command`          |
| `memory`      | `listProjectMemories`、`readProjectMemoryFile`                                                                                                                                                                             | `{dataBaseDir}/.zcode/cli/memories/projects`            |
| `hooks`       | `loadHooks`、`saveHooks`、`grantWorkspaceHookTrust`                                                                                                                                                                        | 用户与项目 `config.json`、旧 settings、trust store；CLI |
| `mcp-sync`    | `loadMcpFromUserDirectory`、`saveMcpToUserDirectory`、`listLocalUserMcpCandidates`、`listRemoteUserMcpStatuses`、`exportMcpServers`、`checkRemoteUserMcpWriteAccess`、`importMcpServers`、`listWorkspaceMcpServerStatuses` | `config.json` 的 `mcp.servers`、`.agents/mcp.json`；CLI |
| `subagents`   | `list`、`setEnabled`、`setBuiltInModelOverride`、`setPluginAgentModelOverride`、`getPrimaryUserAgentsDirectory`、`createAgent`、`updateAgent`、`deleteAgent`；另有 Host 内部的 `prepareRuntimeState`                       | agents 目录、`{storage}/v2/agents-state.json`、插件目录 |

`zcode-agent` 与 `plugin-management` 两个 channel（插件 lane、`getSkillReferenceCatalog` 等）属于 agent 子系统，不在本文范围。

## 2. 产品规则

### 2.1 通用

1. **路径根**与 Node 相同：用户目录取 `HOME`，其次 `USERPROFILE`（`HostEnvironment.user_home`）；Memory 取 `{dataBaseDir}/.zcode`。`storage.dir` 配置项按 Node 规则解析（`~/` 展开、相对路径相对进程 cwd、末段不是 `cli` 时追加 `cli`）。
2. **参数与结果**：参数取第 0 个位置参数（对象），字段名、可选字段、结果的键与键序都与 Node 相同。可选字段缺省时不输出该键。
3. **错误**：错误文案与 Node 相同（UI 会直接展示，部分中文）；带 `code` 的错误保留 `code`（`SKILL_SYNC_SIZE_LIMIT_EXCEEDED` 附 `data`，`PROJECT_MEMORY_*`，`ZCODE_AGENT_MCP_STATUS_MODE_UNSUPPORTED`）。
4. **JSON 文本**：读取按 `JSON.parse` 语义（重复键取最后一个，数组下标形态的键排在前面）；写回按 `JSON.stringify(value, null, 2)`（数字按 ECMAScript 规则输出），末尾加换行，与各 Node 写入方逐字节一致。
5. **YAML frontmatter**：技能、Subagent 的 frontmatter 先按 YAML 解析，失败时走与 Node 相同的宽松读取。
6. **排序**：`localeCompare` 用 `support::collate` 的 ICU 根排序近似。
7. **目录遍历顺序**：Node `readdir` 在 Unix 上经 libuv `scandir(3)` 按 `strcmp` 排序，而命令列表、同名去重（先到先得）都依赖遍历顺序；Rust 的 `fsx::read_dir` 同样按字节序排序（Windows 两边都用文件系统顺序）。

### 2.2 skills

1. 扫描根依次为：从 workspace 向上到 worktree 根（含 `.git`）每层的 `.zcode/skills`、`.agents/skills`；用户级 `~/.zcode/skills`、`~/.agents/skills`；启用插件的技能目录。根先按“被已存在的更深根覆盖”去重，再按 realpath 去重。
2. 每个根做有界深度优先遍历（深度 8，跳过 `node_modules` 等目录和除 `.system` 外的点目录，软链目录按 realpath 去重），产出 `SKILL.md`。
3. 用户级 `.agents/skills` 中，与 `.zcode/skills` 同目录名或同名（小写、trim）的技能被跳过。同一 realpath 只保留首次出现。
4. id 为 `glm:<scope>:<name>:<sha256(realpath)前 12 位>`；`path` 为 realpath，`sourcePath` 为扫描到的原始路径。`description` 超过 1024 字符时记诊断并跳过。
5. 启用状态只存在 `config.json` 的 `skills.<SKILL.md 路径（/ 分隔）>.enable`；启用是默认值，写入时删除键，对象为空时删除 `skills`。
6. `buildPromptContext` 只激活 prompt 中 `$name` 提到且启用的技能，追加 `<available_skills>` 块，并向 `~/.zcode/v2/skills-audit.log` 追加一行。
7. `copyToCommon` 不覆盖已存在目录；`removeFromCommon` 只删通用目录内的技能；`deleteSkill` 拒绝插件技能，只允许删除 realpath 后父目录位于受控根之内的目录项（软链只删链接）。

### 2.3 commands

1. ZCode 来源依次合并 workspace 的 `.zcode/commands`、`.agents/commands`，再合并用户级的同两个目录；`.zcode` 读到命令后同 scope 的 `.agents` 不再参与；按名称去重（先到先得）。
2. 插件命令只读，来自启用插件的 `commands` 目录，按名称排序。
3. 禁用状态存在 `config.json` 的 `command.<文件路径>.enable=false`；改名时迁移禁用状态，删除时清除。

### 2.4 subagents

1. 内置 `general-purpose`、`Explore`；用户级 `{storage}/agents`，workspace 级 `.zcode/agents`；插件 agent 来自启用插件，裸名唯一且不冲突时追加运行时别名。
2. 状态文件 `{storage}/v2/agents-state.json`：`disabledAgentIds`、`builtInModelSelectionOverrides`、`pluginAgentModelSelectionOverrides`；Host 内的写入串行。
3. **`prepareRuntimeState`**：每次启动 CLI 前（spawn 环境计算阶段）等待一次迁移：用户 agents 目录下 Markdown 的旧 Provider 写法、状态文件里的旧覆盖字段导入为结构化选择。迁移持文件锁、写前比较原文与文件身份，失败只记 warn、保留原文件，不阻断启动。

### 2.5 hooks

1. 项目 hooks 从 workspace 向上到 worktree 根，每层候选 `zcode.json`、`.zcode/config.json`（自顶向下）；用户 hooks 来自 `~/.zcode/cli/config.json`；另读 `.agents/settings.json`、`.claude/settings.json` 的旧格式（只读、默认不启用）。
2. 声明 digest 与 bundle digest 与 TS（`workspace-hook-digest.ts`）逐字节一致；trust store 不可读或结构不合规时整体视为损坏（fail closed），返回 `trustStoreCorrupt: true`。
3. `saveHooks` 同时写用户与当前 workspace 的 `config.json`（原子写，0600）。
4. `grantWorkspaceHookTrust` 转发给该 workspace 的 CLI（`workspace/hooks/trustGrant`），Host 不直接写 trust store。

### 2.6 mcp-sync

1. 读：workspace 与用户级各自优先 `.zcode` 的 `mcp.servers`，为空时退回 `.agents/mcp.json` 的 `mcpServers`。读到旧 `enable` 字段时就地折叠成 `enabled` 并回写。
2. 写：`upsert`、`delete` 只写 ZCode 文件；`set-enabled` 写 server 自身的 `enabled`，并清理旧的按目录覆盖。JSON 损坏时报错，不覆盖。
3. `listWorkspaceMcpServerStatuses` 转发给该 workspace 的 CLI（`mcp/list`）；`mode:"status"` 遇到旧 CLI 不认识 `mode` 时回 `ZCODE_AGENT_MCP_STATUS_MODE_UNSUPPORTED`。

### 2.7 skill-sync、plugin-sync

归档格式与 Node 相同（ustar + gzip，owner 名 `zcode`，目录按 `localeCompare` 排序，plugin 归档末尾带 `.zcode-plugin-sync.json`），因此 Rust 导出的归档可由远端 Node 导入，反之亦然。导入先解到临时目录、校验路径不越界与大小上限，再逐个复制；目标已存在或同名时跳过。

### 2.8 memory

只读。列出 `projects/<id>/memory/*.md`（不跟随软链，`MEMORY.md` 为 index），读取时校验路径、文件身份与 5 MiB 上限。

## 3. 共享文件与锁

`~/.zcode/cli/config.json` 的写入方：本文的 skills、commands、mcp-sync、hooks、plugin-sync，Node 的 settings-sync，Main 的 MCP 用户目录 IPC，以及 CLI 的插件命令。修复前所有写入方都是“读 → 改 → 写”且不持锁，并发时后写者覆盖前者。

规则（同 S0 对 `setting.json` 的修复）：

1. 整个读改写持跨进程锁 `<config.json>.lock/`（`atomicFileLock.ts` 协议，Rust 为 `store::with_file_lock`），锁内原子写（同目录临时文件 + rename，0600）。
2. Rust Host 的所有写入都走 `extensions::cli_config::update`。Node Host 服务（skills、commands、mcp-sync、hooks、plugin-sync）与 Main 的 MCP 用户目录改为同一锁协议（`@zcode/shared/node` 的 `updateJsonObjectFileLocked`）。
3. `agents-state.json` 的写入持同一协议的锁（Node 的 `atomicWriteText` 已持锁写；读改写整体持锁由 Rust 侧补齐）。

```text
Rust Host (skills.setEnabled)          Node Host / Main (commands, mcp)          CLI
 acquire config.json.lock ──┐
 read → mutate → tmp+rename │          acquire …（等待）
 release ───────────────────┘──────►   read（看到 Rust 的修改）→ mutate → write → release
                                                                                  plugins/setEnabled：
                                                                                  不持锁（残余风险，见 §5）
```

## 4. 状态所有者与事件顺序

| 状态                                      | 唯一所有者                                              |
| ----------------------------------------- | ------------------------------------------------------- |
| 技能/命令/agent 的 Markdown 与目录        | 文件系统（每次调用重新扫描，Host 不缓存）               |
| 启用覆盖（skills、command、mcp、plugins） | `~/.zcode/cli/config.json`（持锁读改写）                |
| Subagent 状态                             | `agents-state.json`；Host 内写入经 `SubagentStore` 串行 |
| hooks trust                               | CLI（Host 只读 trust store）                            |
| MCP 运行态、插件运行态                    | 各 workspace 的 CLI 进程                                |

这些 channel 没有事件。UI 在每次修改后重新调用 `list`；CLI 侧的生效依赖 `zcode-agent.onAgentRuntimeRestarted`（AgentRuntime 所有，本文不改）。

Composer 的 `/` 选择器有三个数据源，只有 AGENTS 分组来自本文：COMMANDS 是 CLI 上报的命令（`useSlashCommands`），SKILLS 是 `zcode-agent.getSkillReferenceCatalog`（agent 子系统），AGENTS 是 `subagents.list`。设置页 Skills、Commands 页里的“插件分组”按 `plugin-management` 的已安装列表聚合，也不由本文决定。`prepareRuntimeState` 的顺序：

```text
AgentRuntime.start(target)
 └─ spawn_env：设置页网络变量
     └─ env contributor（extensions）：await SubagentStore.prepare_runtime_state()
           ├─ 迁移用户 agents Markdown（每个文件持锁、比较原文后 rename）
           └─ 迁移 agents-state.json（与 Host 内其他状态写入串行）
     └─ 其余 contributor、命令自带环境
 └─ spawn CLI（CLI 只读到迁移后的文件）→ generation>1 时 onAgentRuntimeRestarted
```

## 5. 有意与 Node 不同之处

| Node 行为                                                                 | Rust Host                                                 | 理由                                                                                              |
| ------------------------------------------------------------------------- | --------------------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| skills、commands 在原文件上 `writeFile`，不持锁                           | 持锁原子写，0600                                          | 修复跨进程丢更新；0600 与 mcp-sync、hooks、CLI 一致（文件含 MCP 密钥）                            |
| `mcp/list` 走独立的 mcp-status lane（插件管理 workspace，5 分钟空闲回收） | 发给该 workspace 自己的 CLI（`AgentRuntime::get_client`） | Rust CLI 校验请求 workspace 与进程 workspace 一致；Rust Host 尚无 mcp-status lane（agent 子系统） |
| CLI 写插件启用状态（`plugins/setEnabled`）不持锁                          | 不变                                                      | 不改 CLI；与 Host 写入同时发生的窗口只在用户同时操作两个页面时出现                                |
| settings-sync 写 `config.json` 不持锁                                     | 不在本文                                                  | settings-sync 属 R1，由其负责方接入同一 helper                                                    |
| Subagent `mcpServers` 对象数组用 `yaml` 库 `stringify` 写回               | 自带的 YAML 块风格输出                                    | 常见结构（字符串、数字、布尔、嵌套对象/数组）输出相同；特殊字符串一律双引号                       |
| YAML 解析用 `yaml` 包                                                     | `yaml-rust2`                                              | name/description 等标量结果相同；非法 YAML 都回退到相同的宽松读取                                 |
| `localeCompare` 为 ICU                                                    | ASCII 范围一致的近似                                      | 只影响非 ASCII 名称的并列排序                                                                     |
| `prepareRuntimeState` 的状态文件迁移遇到 IO 错误时 reject，CLI 不启动     | 记 warn 后照常启动                                        | env contributor 不能失败；迁移失败时 CLI 读到的是未迁移的旧字段，与迁移前行为相同                 |
| gzip 用 Node zlib                                                         | flate2（默认压缩级别）                                    | tar 内容逐字节相同，gzip 字节可能不同；`archiveBytes` 随之不同，两边都能解对方的归档              |
| JSON 解析失败的报错文案来自 V8                                            | 来自 serde_json                                           | 只出现在配置损坏时的错误消息里，前缀（`无法解析 MCP 配置文件 …`）相同                             |

## 6. 验收

- `cargo test -p zcode-host-services extensions`（`--lib`）：
  - **差分**：`pnpm exec tsx scripts/rust-host/extensions-fixtures.ts [--check]` 用真实 TS 服务在 `extensions-scenarios.ts` 描述的文件树上执行 57 次调用（9 个 channel 的全部本地成员，含报错路径），把结果与 11 个文件的最终内容写进 `extensions/fixtures/extensions-parity.json`；Rust 的 `parity_tests` 在同样的文件树上经真实 channel 复现，结果（抹掉临时根、路径相关 hash 与时间）与文件内容逐项相同。hooks 的声明 digest 与 bundle digest 不做归一化，直接比较。`scripts/rust-host/test.mjs` 先跑 `--check`；
  - 单元：frontmatter（YAML 与宽松读取）、命令文件生成、JSON 键序与数字、tar 头与越界路径、Subagent Markdown 与旧 Provider 迁移、hooks schema、MCP 路径改写、锁内并发。
- `cargo test -p zcode-host-services --test extensions_interop`（需要 node 与 tsx）：
  - Rust 与 TS 两个进程各写 150 次 `config.json`（命令开关、MCP upsert），最终 300 个键都在；用修复前的 TS 写入方运行时失败；
  - Rust 导出的 skill / plugin 归档由 TS 解包（含插件元数据与可执行位），TS 导出的 skill 归档由 Rust 导入。
- `node --import tsx --test packages/services/test/cliConfigCrossProcess.test.ts`：4 个 Node 进程并发写 `config.json` 不丢键（修复前失败）。
- 真实 App（`ZCODE_E2E_CLI_RUST_BINARY=… node scripts/rust-host/e2e-desktop.mjs --scenario <name>`）：
  - `extensions-memory`：Memory 设置页列出预置项目与文件；磁盘新增文件后刷新，列表随之更新；
  - `extensions-skills`：`/` 选择器 AGENTS 列出内置与用户 Subagent；设置页 Skills 列出用户（`.zcode` 与 `.agents`）与 workspace 技能，关闭开关写入 `config.json`；Commands 页列出用户与 workspace 命令，关闭开关写入 `config.json` 且不覆盖技能开关；SKILLS 分组在 `getSkillReferenceCatalog` 实现后必须列出预置技能；
  - `extensions-plugins`：marketplace 安装的插件 agent 出现在 `/` 选择器与 Subagents 页的插件分组，选模型写入 `agents-state.json` 的插件覆盖；插件页可用（列表由 plugin-management 提供）。
