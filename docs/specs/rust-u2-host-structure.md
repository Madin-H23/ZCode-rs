# U2：Host 服务的结构（crate 拆分与依赖方向）

日期：2026-10-09。上级规划：[rust-unified-host.md](rust-unified-host.md) 的 U2。本文规定第一步：把 `zcode-host-services` 一个 crate（约 12 万行）拆成分层的多个 crate，让依赖方向由 Cargo 强制。Gateway、任务监督与有界通道是 U2 的后续步骤，见第 6 节。

## 1. 拆分前的事实

- `services` 内 28 个顶层模块，互相依赖只有两簇：`agent ↔ tasks ↔ tasks_index`，以及 `automation ↔ bots / off_peak / conversation_share / integration / cua_permission`；另有 `settings ↔ support`（一处纯函数调用）。
- 其余依赖都是单向的，可以排成层。
- 外部（`host-core`、集成测试）按 `zcode_host_services::<module>::…` 引用。

## 2. 目标结构

依赖只能从下往上引用更靠前的层。

| 层  | crate                   | 模块                                                                                                            | 约行数 |
| --- | ----------------------- | --------------------------------------------------------------------------------------------------------------- | ------ |
| 1   | `zcode-host-base`       | `store`、`support`、`main_bridge`、`environment`（含 `ServiceContext`）、`broadcast`、`testing`                 | 2.0k   |
| 2   | `zcode-host-settings`   | `settings`                                                                                                      | 3.3k   |
| 3   | `zcode-host-providers`  | `providers`                                                                                                     | 4.8k   |
| 4   | `zcode-host-sessions`   | `agent`、`tasks`、`tasks_index`                                                                                 | 27k    |
| 5   | `zcode-host-account`    | `account`                                                                                                       | 16.6k  |
| 5   | `zcode-host-extensions` | `extensions`、`settings_sync`                                                                                   | 13.4k  |
| 5   | `zcode-host-workspace`  | `file`、`file_watcher`、`git`、`terminal`、`media_preview`、`prompt_attachment_transfer`、`system`、`workspace` | 11.5k  |
| 6   | `zcode-host-automation` | `automation`、`off_peak`、`bots`、`conversation_share`、`integration`、`cua_permission`                         | 31k    |
| 7   | `zcode-host-remote`     | `remote`                                                                                                        | 10k    |
| 8   | `zcode-host-services`   | 组装（`install`）与对外门面                                                                                     | 0.4k   |

- 表外的 crate：`zcode-host-rpc`、`zcode-host-control`、`zcode-host-kit` 是与业务无关的基础，在第 1 层之下；`zcode-host-gateway`（连接层，[rust-u2-gateway.md](rust-u2-gateway.md)）只依赖这三者与 `zcode-async`，与第 1 层同级、与 `zcode-host-base` 互不依赖，并且只允许 `zcode-host-core` 与 `zcode-host` 依赖它（业务 service 不认识连接层）；`zcode-host-core`（组装）与 `zcode-host`（二进制）在第 8 层之上。
- `zcode-host-runtime-link`（到 runtime 的链路，[rust-host-cli-boundary.md](rust-host-cli-boundary.md) §2）同样与第 1 层同级：只依赖 `zcode-host-kit`、`zcode-proc`，以及 feature `embedded` 打开时的 `zcode-cli-rust`；与 `zcode-host-base`、`zcode-host-gateway` 互不依赖。只允许 `zcode-host-sessions`（agent 子系统）、`zcode-host-remote`（借用它的子进程工具启动远端连接器）、`zcode-host-services` 的测试（打开 `embedded`）与 `zcode-host`（二进制）依赖它。
- 第 5 层的三个 crate 互不依赖。
- 有环的两簇各自留在一个 crate 内（`sessions`、`automation`）。把它们再拆细需要先把环改成事件或端口，属于后续步骤。
- `zcode-host-services` 重新导出所有模块（`pub use zcode_host_sessions::agent;` 等），外部的 `zcode_host_services::<module>` 路径不变。
- `crates/services/tests/` 下的集成测试暂留原处，经门面的路径引用各层；随后续步骤逐个移到被测的那一层。核心对话链路（`agent_cli.rs`）已移到 `crates/zcode-host/tests/`：它的子进程链路用合并二进制的 CLI 入口（`CARGO_BIN_EXE_zcode-host`），只有二进制所在的 crate 能拿到。
- 层号与“只允许谁依赖”由 `scripts/rust/check-host-layers.mjs` 检查；脚本里第 1 层之下的基础编为 0–2，表中第 1 层起对应脚本的 3 起。

## 3. 规则

1. **依赖方向**：crate 只能依赖表中层号更小的 crate；同层的 crate 互不依赖。Cargo 保证无环，`scripts/rust/check-host-layers.mjs`（`pnpm check:rust` 的一步）按本表检查越层与同层依赖，包括 dev-dependencies，以及只允许特定 crate 依赖的 crate（`zcode-host-gateway`、`zcode-host-runtime-link`）。Host 与 CLI 之间的依赖另由 `scripts/rust/check-host-cli-boundary.mjs` 检查（[rust-host-cli-boundary.md](rust-host-cli-boundary.md) §5）。新增 crate 或跨层依赖必须先改本表和脚本里的层号。
2. **模块内的路径不改**：每个 crate 的 `lib.rs` 用 `use zcode_host_<lower>::<module>;` 把下层模块引入 crate 根，原有的 `crate::<module>::…` 路径保持有效。
3. **可见性最小放开**：只把编译器报告的、确实被上层 crate 用到的 `pub(crate)` / `pub(super)` 条目改为 `pub`；其余保持原样。
4. **测试替身跨 crate 复用**：用 crate 的 `test-support` feature（`#[cfg(any(test, feature = "test-support"))]`），由使用方在 `[dev-dependencies]` 中开启；测试辅助代码不进入正式构建。目前只有 `zcode-host-automation`（`bots::test_support`，被 `remote` 的测试使用）。
5. **测试跟着被测代码走**：验证某层行为的测试放在那一层。`support/zod` 下对拍 automation / off-peak 反向校验的测试与样例移到 `zcode-host-automation`。
6. **为拆环所做的下沉**：`settings::schema::endpoint_origin`（纯函数）移到 `support::http`，`settings` 重新导出；`ServiceContext` 移到 `environment` 模块。

## 4. 做法

拆分是机械改动，由可重复执行的脚本完成（移动模块、生成各 crate 的清单与 `lib.rs`、按编译器的私有性报错放开可见性、给只被测试用到的导入加 `#[cfg(test)]`），再用 cargo-shear 裁掉各 crate 多余的依赖。生成样例的脚本（`scripts/rust-host/*-fixtures.ts`）与文档中的路径随之更新。

## 5. 验收

- `cargo clippy --workspace --all-targets -- -D warnings`、`cargo nextest run --workspace`、`cargo shear`、`cargo deny` 在 Host workspace 通过，测试数量不减少。
- `pnpm test:rust-host`（样例漂移检查、Rust 测试、TS 客户端连接真实二进制）通过。
- 桌面 E2E 场景通过。
- `zcode-host-services/src` 中只剩组装代码。

## 6. U2 的后续步骤（不在本文范围）

| 步骤     | 内容                                                                                                                          |
| -------- | ----------------------------------------------------------------------------------------------------------------------------- |
| 拆环     | `agent ↔ tasks` 改为事件订阅；`automation ↔ bots` 改为构造时传入的端口；之后把 `tasks_index`、`bots` 拆成独立 crate           |
| 显式组装 | 去掉 `OnceLock` / `bind_*` / `attach_*` 这类事后注入，`install` 按依赖顺序构造                                                |
| Gateway  | 已完成连接层（连接注册表、串行键、出站队列，[rust-u2-gateway.md](rust-u2-gateway.md)）；方法表与 `zcode-gateway-legacy` 在 U9 |
| 并发     | 所有 spawn 归入任务作用域；无界通道改为有界并写明溢出策略；存量的 `#[expect(clippy::await_holding_invalid_type)]` 清零        |
