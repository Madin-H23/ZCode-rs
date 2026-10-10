# ZCode Rust

这是把 ZCode 桌面端的 Host 与 agent runtime（CLI）迁移到 Rust 的工作仓库：

- **Rust CLI**（[`apps/zcode-cli-rust/`](apps/zcode-cli-rust/)）：agent runtime，取代 TypeScript CLI。可以单独运行（`app-server --stdio`、`-p`），也可以作为库嵌进 Host。
- **Rust Host**（[`apps/zcode-host/`](apps/zcode-host/)）：取代 Node Host。一个 App 只有一个 Host 进程，所有窗口接入它；runtime 在 Host 进程内运行，每个本地 workspace 一个实例，不再为每个 workspace 起 CLI 子进程。
- **共用 crate**（[`rust/`](rust/)）：两边都用的基础代码（进程树、原子写、任务作用域、workspace 身份等）。

Host 与 CLI 在代码上保持隔离、可以分开编译，边界只有 App 协议（[边界规格](docs/specs/rust-host-cli-boundary.md)）。打包在一起的 `zcode-host` 二进制也能直接当 CLI 用（见下文）。

**发布版的默认组合仍是 Node Host 加 TypeScript CLI。** Rust Host 只在开发构建中通过开关启用；切换默认之前要完成功能对齐、全平台验收与打包签名（规划见 [rust-unified-host.md](docs/specs/rust-unified-host.md) 第 12 节）。

## 当前进度

| 阶段  | 内容                                                                                          | 状态                                                           |
| ----- | --------------------------------------------------------------------------------------------- | -------------------------------------------------------------- |
| U0    | 护栏（工具链、lints、nextest、cargo-deny、cargo-shear、CI）与共用 crate                       | 第 1、2 步完成；net / sqlite / git 的抽取与测试 harness 未开始 |
| U1    | runtime 库化：嵌入入口、显式环境、同进程重启与多实例                                          | 完成                                                           |
| U2    | Host 结构：`services` 拆成分层 crate、任务作用域、Gateway（调用顺序、有界出站队列、断开原因） | 主体完成；Host 业务 crate 的任务归属与有界通道在收尾           |
| U3    | Host 进程内运行 runtime；Host 与 CLI 的结构隔离与 `cli` 入口；输入队列归一；工具进程回收      | 完成                                                           |
| U4    | 一个 App 一个 Host：多窗口、调度移入 Host、设置经 Host 写入、App 级崩溃重启                   | 完成，桌面 E2E 通过                                            |
| U5–U8 | SSH / WSL / Docker 远端、Web、手机远控、切换默认与打包                                        | 未开始                                                         |

验证范围：只在 macOS（Apple Silicon）上跑过桌面 E2E；Windows 与 Linux 只做了编译检查或 CI 构建。最近一次完全合并后的桌面 E2E：进程内 runtime 32 个场景通过，子进程链路（合并二进制的 `cli` 入口与独立 CLI）各 8 个通过，Node Host 路径 5 个通过。

## 通过 npm 使用 Rust runtime

通过 [`@mbears/zcode-rs`](https://www.npmjs.com/package/@mbears/zcode-rs) 启动已安装的 ZCode，无需克隆仓库或自行编译 Rust。

- 已安装 **ZCode 3.14.3**；启动器会拒绝其他 App 版本。
- 已安装 **Node.js 22.16.0 或更高版本**（含 npm / npx）。
- 当前 npm 版本为 **0.1.0**，已发布 **macOS Apple Silicon（ARM64）** 和 **Windows x64** 平台包。

先完整退出 ZCode；Windows 若有托盘图标，也需要从托盘退出。然后在终端或 PowerShell 中执行：

```sh
# 检查 App 版本和当前平台的 Rust 包
npx @mbears/zcode-rs@0.1.0 app doctor

# 使用 Rust runtime 启动 ZCode
npx @mbears/zcode-rs@0.1.0 app launch
```

npm 会自动选择对应平台的预编译包。`app launch` 使用已安装 App 的界面，并为这次启动选择本地 Rust runtime；远端 SSH/WSL runtime 不随之切换。这里只替换 CLI，Host 仍是 App 内置的 Node Host；Rust Host 目前只能在开发构建中使用（见下文）。

**恢复原版：**完整退出这次启动的 ZCode，再从原来的应用图标、桌面或开始菜单快捷方式打开，即使用原版内置 CLI。启动器没有覆盖 App 安装文件或修改原快捷方式，无需重装或卸载 npm 包；回退不会撤销使用期间产生的会话和文件改动。

当前为测试版，Rust 功能范围和已知限制见下文及 [启动器说明](apps/zcode-cli-rust/npm/README.md)。Windows x64 已通过原生编译、公开 npm 安装、exe 执行和启动器检查，尚未覆盖真实 Windows 桌面 UI 端到端测试；详见 [验收记录](docs/reports/rust-npm-app-launcher-2026-09-28.md)。

## 环境要求

- Rust：工具链由 [`rust-toolchain.toml`](rust-toolchain.toml) 固定（1.95.0），rustup 会自动安装。检查与测试脚本还需要 `cargo-nextest`、`cargo-deny`、`cargo-shear`，缺失时脚本会给出安装命令。
- Node.js 与 pnpm：桌面联调、集成测试与仓库脚本需要，版本以 [`mise.toml`](mise.toml) 为准。
- macOS、Linux 或 Windows。

首次准备仓库依赖：

```sh
pnpm install
pnpm build
```

## 在桌面 App 中使用 Rust Host

先构建 Host（缺省使用 debug 构建）：

```sh
cargo build --locked --manifest-path apps/zcode-host/Cargo.toml
```

然后以 Rust Host 启动桌面开发版：

```sh
ZCODE_DESKTOP_HOST=rust pnpm dev:desktop
```

- 使用 release 构建时，另设 `ZCODE_RUST_HOST_BINARY=$PWD/apps/zcode-host/target/release/zcode-host`。
- runtime 缺省在 Host 进程内运行。需要对照子进程链路时，设 `ZCODE_AGENT_SERVER_COMMAND=<zcode-cli-rust 可执行文件>` 与 `ZCODE_AGENT_SERVER_RUNTIME=zcode-cli-rust`。
- 打包版本不读这个开关。不设 `ZCODE_DESKTOP_HOST` 时是今天发布的 Node Host。

只想在 Node Host 下换用 Rust CLI（不使用 Rust Host）时：

```sh
pnpm dev:desktop:zcode-cli-rust          # release 构建
pnpm dev:desktop:zcode-cli-rust --debug  # debug 构建
```

需要隔离实验数据或使用 fixture 模型时加 `--config /absolute/path/model.json --data-dir /absolute/path/rust-experiment`。

## 只运行 CLI

独立的 CLI 二进制：

```sh
cargo build --locked --manifest-path apps/zcode-cli-rust/Cargo.toml
apps/zcode-cli-rust/target/debug/zcode-cli-rust app-server --stdio \
  --cwd "$PWD" \
  --data-dir "$PWD/.zcode-runtime/rust"
```

合并的 Host 二进制也可以当 CLI 用，参数、输出与退出码和独立 CLI 完全相同：

```sh
apps/zcode-host/target/debug/zcode-host cli --version
apps/zcode-host/target/debug/zcode-host cli app-server --stdio --cwd "$PWD"
apps/zcode-host/target/debug/zcode-host cli -p "总结这个仓库"
```

把它链接或复制成名为 `zcode-cli-rust` 的文件后直接运行，效果相同。

- App Server 的 stdout 只输出协议帧，诊断信息写入 stderr。
- `--cwd` 指定缺省工作区，请求可以指向其他工作区。
- `--data-dir` 只放 Rust 工具缓存与进程账本，默认 `~/.zcode/rust`。
- 会话与 Node 共用数据库，默认 `~/.zcode/cli/db/db.sqlite`，路径可由存储配置覆盖；只设置 `--data-dir` 不会隔离会话库。
- 直接运行时可以用 `--config /absolute/path/model.json` 指定单模型配置，并在环境变量中提供配置所需的 API key。接入桌面 App 时不需要，App 会提供 Provider Registry 与账号设置。

## 构建形态

| 形态         | 命令                                                                                         | 内容                                                                  |
| ------------ | -------------------------------------------------------------------------------------------- | --------------------------------------------------------------------- |
| 合并（缺省） | `cargo build --manifest-path apps/zcode-host/Cargo.toml -p zcode-host`                       | Host、进程内 runtime、`cli` 入口                                      |
| 只有 Host    | `cargo build --manifest-path apps/zcode-host/Cargo.toml -p zcode-host --no-default-features` | 不链接任何 CLI 代码；需要用 `ZCODE_AGENT_SERVER_COMMAND` 指定外部 CLI |
| 只有 CLI     | `cargo build --manifest-path apps/zcode-cli-rust/Cargo.toml`                                 | 独立的 `zcode-cli-rust`                                               |

三种形态都在 `pnpm check:rust` 中编译与检查。

## 检查与测试

```sh
pnpm check:rust            # 三个 workspace 的边界与分层检查、fmt、clippy（含三种构建形态）、cargo-deny、cargo-shear
pnpm test:rust             # 共用 crate（rust/）的测试
pnpm test:zcode-cli-rust   # CLI 的 Rust 测试，以及 Node 驱动的 App 集成测试
pnpm test:rust-host        # Host 的 Rust 测试、对拍样例漂移检查、TS 客户端连接真实二进制
pnpm e2e:rust-host         # 桌面 E2E（隔离的 Electron、mock 模型）
```

桌面 E2E 的常用参数：`--scenario <名字>`（可重复，场景在 `scripts/rust-host/e2e/scenarios/`）、`--runtime child`（子进程链路）、`--host node`（今天发布的 Node Host 路径）、`--keep`（保留日志与截图）。测试用 `cargo nextest run --workspace`：`apps/zcode-cli-rust` 的根 `Cargo.toml` 同时是一个 package，不加 `--workspace` 只处理根包。

## Rust runtime 的功能对齐

截至 2026-09-28，Rust runtime 的以下能力已有实现及自动化或实机验收记录；勾选只表示对应范围完成，不代表全部平台、供应商和交互组合均已验收：

- [x] Session、队列、冷恢复、compact 和 workspace identity 隔离；与 Node 共用会话库，支持交替续聊、崩溃恢复与不同会话并发写入
- [x] OpenAI Chat Completions、OpenAI Responses、Anthropic Messages
- [x] Provider Registry、模型选择、账号 overlay、每请求 Host 鉴权
- [x] build/edit/yolo/auto 权限模式、独立 Plan 状态、审批规则、hooks 与工作区信任
- [x] Read/Write/Edit/Glob/Grep/Bash、Edit 宽松匹配、后台任务、WebFetch/WebSearch、AskUserQuestion、Todo
- [x] MCP、Skill、插件管理与官方 MCP 鉴权；子代理、Goal、retry/edit、fork、选区侧聊和文件回退核心链路
- [x] 文本、图片、PDF、视频附件；图片缩放、媒体 Read 和请求媒体预算
- [x] `-p` 无头模式、自动标题、用量统计、实时遥测和本地首字时间
- [x] V4 桌面/手机分 profile 投递、有界增量回放、snapshot 恢复与流控（协议集成测试）
- [x] macOS ARM64、Windows x64 npm 平台包发布；macOS 原版 App 启动、切回 Node 与再切回 Rust 的基础验收

剩余 TODO：

- [ ] MCP OAuth、legacy SSE 自动重连、真实服务器兼容矩阵与插件商店 UI 全流程
- [ ] 主代理记忆、目录级规则、完整 prompt/output style，以及附件和工具的剩余兼容差分
- [ ] 自动任务、Cron/OffPeak、浏览器/CUA、Node REPL；子代理 transcript artifact 与子树预算聚合
- [ ] 超大会话按需投影、产物回收与文件恢复边界；跨窗口、多会话、远端/手机和真实供应商完整验收
- [ ] Windows 桌面端到端、Linux/macOS x64/Windows ARM64 原生验收与发行、SSH/WSL 部署、完整升级回退和性能发布门槛

动态工作流不在当前迁移范围，TUI 只保留子命令入口。分项证据与剩余边界见[对齐清单](docs/specs/rust-parity-remaining.md)、[共用会话库规格](docs/specs/rust-m11-node-storage.md)和 [npm 启动器验收记录](docs/reports/rust-npm-app-launcher-2026-09-28.md)。默认 runtime 仍为 TypeScript。

## 性能

只比较 runtime 本身（不含 Electron 与 Host）：2026-09-25 在同一台 Apple M1 Max 上使用本地 SSE fixture、8 回合单 Session、5 次交错测量的结果如下。Node.js 使用当次 CLI bundle，Rust 使用切换到 Node 会话库后的 release 二进制：

| 指标             | Node.js |     Rust |
| ---------------- | ------: | -------: |
| 空闲 RSS         | 416 MiB | 17.8 MiB |
| 负载采样峰值 RSS | 520 MiB | 28.7 MiB |
| 累计 CPU time    |  0.92 s |   0.05 s |
| 8 回合耗时       |  1.50 s |   0.26 s |

这组数据只代表当次本地 fixture 和构建形态，测量时机器存在其他高负载进程。大历史冷读取与驻留另有专项数据；真实供应商、Electron/Host 总进程、MCP 工具负载、Windows/Linux 和移动远控的性能仍需单独验收。完整口径见[性能报告](docs/reports/rust-performance-2026-09-25.md)；[2026-09-23 报告](docs/reports/rust-node-resource-parity-2026-09-23.md)保留为历史基线。

App 级（整个桌面进程树：Main、Renderer、Host、runtime）的对比正在测量，完成后补在这里。Host 内嵌 runtime 与 Host 加 CLI 子进程两种链路的对比见 [U3 报告](docs/reports/rust-u3-embedded-runtime.md)。

## 上游同步记录

- **v3.14.3**：同步上游 ZCode v3.14.3 更新，保留本仓库的 Rust CLI 迁移实现。

## 目录说明

| 目录                                                                | 作用                                                                                             |
| ------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| `apps/zcode-cli-rust/src`                                           | CLI 的组合根、嵌入入口与 CLI 入口                                                                |
| `apps/zcode-cli-rust/crates/protocol`、`core`、`core-api`、`domain` | App 协议、Session 核心与稳定接口                                                                 |
| `apps/zcode-cli-rust/crates/model`、`tools`、`state`、`host`、`net` | 模型、工具、存储、宿主与网络适配器                                                               |
| `apps/zcode-cli-rust/crates/app-server`、`delivery`                 | App Server 前端与 V4 投递                                                                        |
| `apps/zcode-host/crates/zcode-host`、`host-core`、`gateway`         | Host 二进制与入口分派、组装、连接层                                                              |
| `apps/zcode-host/crates/runtime-link`                               | Host 中唯一认识 runtime 的 crate：进程内与子进程两种链路                                         |
| `apps/zcode-host/crates/base` … `remote`、`services`                | 分层的 Host 业务服务（分层见 [rust-u2-host-structure.md](docs/specs/rust-u2-host-structure.md)） |
| `rust/crates/kit`、`async`、`proc`、`shared`                        | 两边共用的基础 crate                                                                             |
| `docs/specs/`                                                       | 架构、迁移与验收规格                                                                             |
| `docs/reports/`                                                     | 验收与性能报告                                                                                   |

整体规划与决策见 [rust-unified-host.md](docs/specs/rust-unified-host.md)，工程实践见 [rust-engineering-practices.md](docs/specs/rust-engineering-practices.md)，Host 与 CLI 的边界见 [rust-host-cli-boundary.md](docs/specs/rust-host-cli-boundary.md)，CLI 的完整能力与配置见 [`apps/zcode-cli-rust/README.md`](apps/zcode-cli-rust/README.md)。
