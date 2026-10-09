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

- 第 5 层的三个 crate 互不依赖。
- 有环的两簇各自留在一个 crate 内（`sessions`、`automation`）。把它们再拆细需要先把环改成事件或端口，属于后续步骤。
- `zcode-host-services` 重新导出所有模块（`pub use zcode_host_sessions::agent;` 等），外部的 `zcode_host_services::<module>` 路径不变。

## 3. 规则

1. **依赖方向**：crate 只能依赖表中层号更小的 crate，由 Cargo 保证；新增跨层依赖必须先改本表。
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

| 步骤     | 内容                                                                                                                   |
| -------- | ---------------------------------------------------------------------------------------------------------------------- |
| 拆环     | `agent ↔ tasks` 改为事件订阅；`automation ↔ bots` 改为构造时传入的端口；之后把 `tasks_index`、`bots` 拆成独立 crate    |
| 显式组装 | 去掉 `OnceLock` / `bind_*` / `attach_*` 这类事后注入，`install` 按依赖顺序构造                                         |
| Gateway  | 连接注册表、方法表与串行范围、出站队列策略；40 个 channel 的注册移到 `zcode-gateway-legacy`                            |
| 并发     | 所有 spawn 归入任务作用域；无界通道改为有界并写明溢出策略；存量的 `#[expect(clippy::await_holding_invalid_type)]` 清零 |
