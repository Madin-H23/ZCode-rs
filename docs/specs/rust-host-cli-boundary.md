# Host 与 CLI 的边界：结构隔离、可分开编译、单二进制多入口

日期：2026-10-09。用户决定（见 [rust-unified-host.md](rust-unified-host.md) 第 14 节 D-U13、D-U14）。本文约束 `apps/zcode-host`（Host）与 `apps/zcode-cli-rust`（CLI，即 agent runtime）之间的关系，优先级高于 [rust-u3-stage-b.md](rust-u3-stage-b.md) 中与之冲突的步骤。

## 1. 目标

1. **运行时是一个进程**：桌面打包的是一个二进制，Host 在进程内运行 runtime（[rust-u3-single-process.md](rust-u3-single-process.md)）。
2. **代码上是两个产品**：Host 与 CLI 各自成 workspace，经一条窄边界相连；任何一方都能单独编译、单独发布。以后要拆回两个二进制时，不需要改业务代码。
3. **同一个二进制可以只当 CLI 用**：打包在一起之后，仍有一个入口让它像独立的 CLI 一样运行。

## 2. 结构

```text
rust/                      两边共用的基础 crate：zcode-kit、zcode-async、zcode-proc、zcode-shared
apps/zcode-cli-rust/       CLI：自成 Cargo workspace。库（runtime、嵌入入口、CLI 入口）+ 独立二进制 zcode-cli-rust
apps/zcode-host/           Host：自成 Cargo workspace
  crates/runtime-link/     Host 中唯一认识 runtime 的 crate：链路类型，进程内与子进程两种实现
  crates/zcode-host/       二进制：Host 入口，以及转交给 CLI 入口的 `cli` 子命令
```

三个 Cargo workspace 的划分保留，不合并成一个（修订 D-U11）。

## 3. 规则

### 3.1 依赖方向

| 规则                         | 说明                                                                                                                                                                                                                  |
| ---------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| CLI 不依赖 Host              | `apps/zcode-cli-rust` 的任何 crate 都不引用 `zcode-host-*`                                                                                                                                                            |
| Host 只经一个 crate 依赖 CLI | 只有 `zcode-host-runtime-link`（feature `embedded`）与二进制 `zcode-host`（`cli` 子命令，同一 feature）可以依赖 CLI，而且只能依赖 CLI 的顶层 crate `zcode-cli-rust`                                                   |
| 只用公开入口                 | Host 一侧只能使用 `zcode_cli_rust::{embed, runtime::RuntimeOptions, serve::AppServerOptions, logging, log_retention, cli}`；不得引用 `zcode-cli-*` 的内部 crate（domain、core、tools、delivery 等）                   |
| 共用代码放 `rust/`           | 两边都要用的代码不放在任何一方：与 JS 语义一致的字符串 / JSON 工具进 `zcode-kit`；两边共用的产品级格式与规则（workspace 身份、provider 选项映射、hook 信任记录等）进 `zcode-shared`。CLI 的 `domain` 不再被 Host 引用 |

### 3.2 边界是 App 协议

- Host 与 runtime 之间只交换 App 协议的消息：请求与回复、V4 帧、runtime 发给 Host 的请求与通知。协议不因为两者在同一个进程而增加“只有进程内才能用”的通道。
- 同一份协议有两种传输，Host 在链路之上的代码不区分它们：

| 链路   | 传输                                   | 编译条件                       |
| ------ | -------------------------------------- | ------------------------------ |
| 进程内 | 内存管道，runtime 是 Host 进程里的任务 | feature `embedded`（缺省开启） |
| 子进程 | CLI 进程的 stdio                       | 总是可用，不链接任何 CLI 代码  |

- 因此 runtime → Host 的请求（账号头、官方 MCP 鉴权）、provider 配置的传递、控制面的两个 lane、Host 按连接保存的订阅归属都保持今天的形态。它们是协议边界的一部分，不是待清理的重复。
- 进程内可以做的优化必须保持协议边界，例如在内存里直接传递消息值而不经过文本（消息类型放在共用 crate 里）。是否值得做由测量决定：目前到 runtime 的往返 p50 约 0.2 ms。

### 3.3 三种构建形态

| 形态         | 命令                                              | 内容                                                                         |
| ------------ | ------------------------------------------------- | ---------------------------------------------------------------------------- |
| 合并（缺省） | `cargo build -p zcode-host`                       | Host、进程内 runtime、`cli` 入口                                             |
| 只有 Host    | `cargo build -p zcode-host --no-default-features` | 不链接任何 CLI 代码；必须用 `ZCODE_AGENT_SERVER_COMMAND` 指定 CLI 可执行文件 |
| 只有 CLI     | 在 `apps/zcode-cli-rust` 下 `cargo build`         | 独立的 `zcode-cli-rust`（不变）                                              |

三种形态都在 `pnpm check:rust` 中编译；“只有 Host”形态另外检查依赖树里没有 `zcode-cli-*`。

### 3.4 单二进制多入口

合并形态的二进制按下面的顺序决定以哪种身份运行：

1. 可执行文件名（去掉扩展名）是 `zcode-cli-rust`：作为 CLI 运行，全部参数交给 CLI。用于把合并的二进制以链接或副本的方式放在只认识 CLI 的调用方面前。
2. 第一个参数是 `cli`：作为 CLI 运行，其余参数交给 CLI。例如 `zcode-host cli -p "…"`、`zcode-host cli app-server --stdio`。
3. 其余情况：作为 Host 运行（今天的参数，Main 这样启动它）。

- 作为 CLI 运行时，行为与独立的 `zcode-cli-rust` 完全相同：同一个入口函数、同样的参数、退出码、stdout 只承载协议。Host 的任何初始化都不执行。
- CLI 入口函数在 CLI 的库里（`zcode_cli_rust::cli`），独立二进制与合并二进制都只是调用它。
- “只有 Host”形态没有 `cli` 入口：使用时报错并说明这个构建不含 CLI。

### 3.5 runtime 的选择（Host 运行时）

| `ZCODE_AGENT_SERVER_COMMAND` | `ZCODE_AGENT_SERVER_RUNTIME` | 合并形态                | 只有 Host 形态      |
| ---------------------------- | ---------------------------- | ----------------------- | ------------------- |
| 未设置                       | 未设置或 `embedded`          | 进程内                  | 启动 runtime 时报错 |
| 已设置                       | `zcode-cli-rust`             | 子进程（该可执行文件）  | 同左                |
| 已设置                       | 未设置                       | 协议替身（Host 测试用） | 同左                |
| 其他组合                     |                              | 报错                    | 报错                |

## 4. 状态所有者

不变。链路之上（实例表、代次、惰性重启、订阅归属）归 Host；链路之下（会话事实、输入队列、投递）归 runtime。链路本身（选择、启动、结束原因、回收）归 `zcode-host-runtime-link`。

## 5. 强制

- `scripts/rust/check-host-cli-boundary.mjs`（`pnpm check:rust` 的一步）：3.1 的四条规则按清单与源码检查；“只有 Host”形态的依赖树检查。
- `pnpm check:rust` 对三种形态都跑 clippy。
- 测试：
  - `zcode-host cli` 与以 `zcode-cli-rust` 为名运行，输出与独立 CLI 相同（`--version` / 无效参数的退出码、`app-server --stdio` 的握手、`-p` 对假模型的一次运行）。
  - 驱动 runtime 的 Host 测试在两种链路上都通过（子进程链路用合并二进制的 `cli` 入口或独立 CLI）。

## 6. 对既有规划的影响

| 原计划                                                                                                         | 现在                                                                             |
| -------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| U3 B 期：删除子进程链路（D-U9 的后半句）                                                                       | 保留，作为链路的第二种实现                                                       |
| U3 B 期：控制面改为直接调用 runtime 库、反向请求改为端口、provider 输入改为进程内的值、Host 直接使用投递 crate | 不做：它们让 Host 依赖 runtime 的内部，违反 3.1 与 3.2                           |
| U3 B 期：投递逻辑抽成独立 crate（已完成）                                                                      | 保留，是 CLI 内部的整理                                                          |
| U3 B 期：输入队列归一（D-U6）、已接受输入的测试                                                                | 保留，经协议实现                                                                 |
| D-U11：三个 workspace 合并为 `rust/`                                                                           | 不合并；共用代码进 `rust/`                                                       |
| D-U12：一个 Engine 管所有 workspace                                                                            | 改为每个本地 workspace 一个 runtime 实例：一条链路对应一个实例，对两种传输都成立 |
