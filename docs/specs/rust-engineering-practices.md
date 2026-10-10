# Rust 工程实践（借鉴 Codex）

日期：2026-10-09。来源：对 `~/bearspace/codex/codex-rs`（`633ab19`）的调研，并与 `apps/zcode-host`、`apps/zcode-cli-rust` 的现状逐项对照。适用范围：[rust-unified-host.md](rust-unified-host.md) 的 U0 起，`rust/` 下的所有 crate；U0 之前，先在两个现有 Cargo workspace 中落地。

每条实践都写明四件事：做法、Codex 出处、我们的现状、落地阶段。只收录在我们的决策下（单进程、每个 workspace 一个 runtime 实例、TS/zod 为契约来源，见 rust-unified-host.md 第 14 节）能落地的做法。不借鉴的做法和理由放在第 9 节。

## 1. 总览

| #   | 实践                                                                  | 现状                                                                                                                 | 阶段             | 成本   |
| --- | --------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- | ---------------- | ------ |
| E1  | 固定工具链，统一构建配置                                              | 没有 `rust-toolchain.toml`；工具链版本（`cargo +1.95.0`）和 `crt-static` 都只写在 Windows workflow 里                | U0 第 1 步       | 1 小时 |
| E2  | workspace 级 clippy lints                                             | CLI 只有 `unsafe_op_in_unsafe_fn`；两边都只跑默认 lint 集加 `-D warnings`                                            | U0 第 1 步       | 半天   |
| E3  | `clippy.toml`：禁止持锁跨 await，用 `disallowed-methods` 收口唯一入口 | 无                                                                                                                   | U0 第 1–2 步     | 半天   |
| E4  | 库 crate 禁止打印到 stdout / stderr                                   | 两边目前都是 0 处 `println!`，但没有约束                                                                             | U0 第 1 步       | 1 小时 |
| E5  | nextest 配置、cargo-deny、cargo-shear                                 | 无                                                                                                                   | U0 第 1 步       | 半天   |
| E6  | PR CI：三平台，加汇总准入                                             | 只有一个 Windows 打包 workflow（构建，加两个 crate 的测试）；`check:zcode-cli-rust` 和 `test:rust-host` 都不在 CI 里 | U0 第 1 步       | 1 天   |
| E7  | 任务监督：不允许分离的 spawn，panic 必须变成错误事件                  | Host 有 139 处分离的 `tokio::spawn`；CLI run 任务 panic 后会话永远停在 running                                       | U0 第 2 步       | 1–2 天 |
| E8  | 子进程唯一入口                                                        | Host 与 CLI 各有一套实现，已发现 4 个缺陷                                                                            | U0 第 2 步       | 2–3 天 |
| E9  | 文件、路径、秘密、日志的基础类型                                      | CLI 至少 8 处手写原子写，Host 另有多处；5 份 `home()`；API key 是裸 `String`                                         | U0 第 2 步       | 1–2 天 |
| E10 | 测试 harness：进程内 TestHost 加 mock 模型服务器                      | Rust 侧只有 trait 级的 fake；HTTP / SSE 的端到端测试都在 TS                                                          | U0 第 3 步       | 2–3 天 |
| E11 | 快照测试与整体相等断言                                                | 没有 insta 和 pretty_assertions                                                                                      | U0 第 3 步       | 半天   |
| E12 | 每个 crate 只有一个集成测试二进制                                     | CLI 有 11 个                                                                                                         | U0 第 3 步       | 1 小时 |
| E13 | 契约漂移检查：失败时打印 diff 和重新生成的命令                        | V4 命令已有；方法名列表和 wire 常量靠手工同步                                                                        | U0 / U2          | 半天   |
| E14 | 清理 `unwrap` / `expect`，之后改为 deny                               | CLI 约 651 / 85 处，Host 约 1164 / 211 处（含文件内测试模块，数字偏高）                                              | U0 起逐步推进    | 持续   |
| E15 | 评审规则：改动规模、破坏性变更清单、模型上下文                        | 部分已有（AGENTS.md 的 spec 先行、400 行上限）                                                                       | 立即             | 0      |
| E16 | 发布：符号归档、写入 commit、版本校验、Linux musl                     | `strip = true`，不保留符号；只构建 win32-x64                                                                         | U8（musl 在 U5） | 1–2 天 |

## 2. 静态检查与编码约定

### 2.1 workspace lints（E2）

放在根 `Cargo.toml` 的 `[workspace.lints]` 中，每个 crate 都写 `[lints] workspace = true`（由第 5 节的清单检查保证）。

**直接 deny**（Codex `codex-rs/Cargo.toml` `[workspace.lints.clippy]`，存量少，修完即可开启）：

- 并发：`await_holding_lock`、`await_holding_invalid_type`；
- 冗余：`redundant_clone`、`redundant_closure`、`redundant_closure_for_method_calls`、`unnecessary_to_owned`、`unnecessary_lazy_evaluations`、`needless_collect`、`needless_borrow`、`needless_question_mark`；
- 手写替代：`manual_map`、`manual_filter`、`manual_find`、`manual_flatten`、`manual_retain`、`manual_strip`、`manual_ok_or`、`manual_unwrap_or`、`manual_range_contains`、`manual_clamp`；
- 其他：`uninlined_format_args`、`trivially_copy_pass_by_ref`、`disallowed_methods`。

**按 crate 清零后 deny**：`unwrap_used`、`expect_used`（E14）。CI 以 `-D warnings` 运行 clippy，warn 级别等同于报错，所以不能在 workspace 上先开 warn；做法是某个 crate 的非测试代码清零后，在它的 crate 根加 `#![deny(clippy::unwrap_used, clippy::expect_used)]`。`rust/` 下的新 crate 从第一天起在 workspace 级别 deny。`clippy.toml` 中设置 `allow-unwrap-in-tests = true` 和 `allow-expect-in-tests = true`，让测试不受影响。U0 第 1 步已对 CLI 的 core-api、headless、host、net、state、tui 与 Host 的 control、rpc、kit、host-core 开启。

### 2.2 `clippy.toml`（E3）

```toml
allow-unwrap-in-tests = true
allow-expect-in-tests = true
await-holding-invalid-types = [
    "tokio::sync::MutexGuard",
    "tokio::sync::RwLockReadGuard",
    "tokio::sync::RwLockWriteGuard",
]
large-error-threshold = 256
```

`disallowed-methods` 用来把能力收口到唯一入口，与 rust-unified-host.md 5.3 一致（Codex 用它强制 sqlx 只经 `codex-state` 的 shim 使用）：

| 被禁止的调用                                                 | 唯一允许的位置                    | 理由                                       |
| ------------------------------------------------------------ | --------------------------------- | ------------------------------------------ |
| `std::env::var`、`var_os`、`vars`、`current_dir`             | `zcode-host-core`、`zcode` 二进制 | 进程环境只读一次，构成 `RuntimeEnv`        |
| `tokio::spawn`、`tokio::task::spawn_blocking`                | `zcode-async`（`TaskScope`）      | 每个任务都有归属，panic 不会丢失（E7）     |
| `std::process::Command::new`、`tokio::process::Command::new` | `zcode-proc`                      | 进程组 / Job Object / env 只实现一次（E8） |
| `rusqlite::Connection::open*`                                | `zcode-sqlite`                    | WAL、busy_timeout、迁移只配置一次          |
| `reqwest::Client::builder`                                   | `zcode-net`                       | 代理、CA、身份请求头只配置一次             |
| `std::fs::rename`（用于写配置或数据文件时）                  | `zcode-kit::atomic_write`         | fsync、权限、临时文件清理只实现一次（E9）  |

允许的位置在对应文件或模块上用 `#[allow(clippy::disallowed_methods)]` 标注，并写明理由。

### 2.3 stdout 保护（E4）

所有库 crate 的 `lib.rs` 加 `#![deny(clippy::print_stdout, clippy::print_stderr)]`。原因：stdio 本身就是协议通道，包括 Main 控制通道和远端的 `zcode serve --stdio`。（Codex 在 core、app-server 中这样做。）

### 2.4 编码约定（写进 AGENTS.md 的 Rust 小节）

以下取自 Codex `AGENTS.md`，并按我们的情况筛选：

- **调用处要能看懂参数含义。** 不用 `bool` 或含义不明的 `Option` 位置参数，比如 `foo(false)`、`bar(None)`；改用枚举、newtype 或具名方法。
- **`match` 尽量穷尽**，不写 `_ =>` 通配分支。新增枚举变体时，编译器会指出所有需要处理的地方。
- **新 trait 要写文档注释**，说明它的职责以及实现方应当怎样实现。
- **异步追踪**：在函数定义上加 `#[tracing::instrument]`，不在调用处给 future 套 `.instrument()`。加之前先看被调方是否已经加过。
- **模块默认私有**，crate 的公开 API 显式导出，并尽量小；不为测试扩大公开面。
- **不写只被调用一次的小 helper。**
- **格式化参数内联**：写 `format!("{name}")`，不写 `format!("{}", name)`（由 lint 保证）。
- **新 trait 的异步方法**：不用作 `dyn` 的 trait 用原生写法 `fn f(&self) -> impl Future<Output = T> + Send`；用作 `dyn` 的端口继续用 `async-trait`。存量不迁移，Codex 的禁令是为了编译速度，对我们收益低。
- **模块大小**：沿用我们更严的 400 行上限（`scripts/check-zcode-cli-rust-boundaries.mjs`），U0 起扩展到所有 crate。

## 3. 工具链、依赖与构建（E1、E5）

- **`rust-toolchain.toml`**：固定具体版本（Codex 固定在 `1.95.0`），包含 `clippy`、`rustfmt`、`rust-src`。升级工具链单独提交。
- **`.cargo/config.toml`**：
  - Windows MSVC 的 `crt-static`，从 workflow 的环境变量挪进来，让本地构建与 CI 一致；
  - 测试与主线程的栈大小（Codex 用 8 MiB 的 `RUST_MIN_STACK`），避免深递归的解析器在 Windows 的 1 MiB 默认栈上溢出。
- **workspace 清单检查**（Codex `verify_cargo_workspace_manifests.py`）：每个 crate 都继承 `[workspace.package]` 和 `[lints]`；依赖版本只写在 `[workspace.dependencies]` 中。并入现有的架构检查脚本。
- **cargo-deny**：检查安全公告、许可证白名单和重复版本告警。每条忽略都必须写明理由和移除条件（Codex `deny.toml`）。
- **cargo-shear**：检查未使用的依赖。平台相关依赖造成的误报，用 `[workspace.metadata.cargo-shear]` 显式忽略。
- **profile**（Codex `codex-rs/Cargo.toml`）：

  | profile     | 设置                                                                               | 用途                              |
  | ----------- | ---------------------------------------------------------------------------------- | --------------------------------- |
  | `dev`       | `debug = "limited"`                                                                | 保留行号和回溯，加快本地构建      |
  | `release`   | `lto = "thin"`、`codegen-units = 4`、`debug = "line-tables-only"`、`strip = false` | 打包时先归档符号，再 strip（E16） |
  | `profiling` | 继承 release，`debug = "full"`、`lto = false`                                      | 火焰图、samply                    |
  | `ci-test`   | 继承 test，`debug = "limited"`、`opt-level = 0`                                    | 降低 CI 磁盘占用                  |

## 4. 测试（E10–E12）

### 4.1 nextest（E5）

`rust/.config/nextest.toml`：

- `retries = 1`，`slow-timeout = { period = "30s", terminate-after = 2 }`，输出 JUnit；
- 用 test-groups 限制会启动子进程的测试的并发数，例如真实 CLI 进程、shell 生命周期、MCP stdio 测试；
- 规定只能通过 `just test` / `pnpm` 脚本运行测试，不直接调用 `cargo test`，让本地与 CI 的配置一致。

### 4.2 每个 crate 一个集成测试二进制（E12）

把 `tests/*.rs` 合并为 `tests/all.rs` 加 `tests/suite/<name>.rs`，只链接一次。单元测试继续用现有的 `*_tests.rs` 加 `#[path]` 约定（Codex 同样如此）。

- **CLI（已完成）**：根包 `zcode-cli-rust` 原来的 14 个集成测试二进制（每个都链接整个运行时）合并为 `tests/all.rs`，它只有 `mod suite;`；每个原文件是 `tests/suite/` 下的一个模块，测试名形如 `suite::embed::…`，nextest 的二进制 id 是 `zcode-cli-rust::all`。原先逐字复制的辅助代码（读 HTTP 请求、固定时钟）放在 `tests/suite/common.rs`。新测试加在 `tests/suite/` 下，不在 `tests/` 下新建 `*.rs`。
  - 只跑一部分：`cargo nextest run --manifest-path apps/zcode-cli-rust/Cargo.toml --workspace -E 'binary(all) & test(/^suite::embed::/)'`；`scripts/zcode-cli-rust-node-storage-check.mjs` 用 `cargo test --test all suite::node_storage::`。
  - 成员 crate 自己的集成测试（`bash-parse` 2 个、`delivery` 1 个）只链接各自的 crate，二进制只有几 MB，暂不合并。
  - 合并后 CLI workspace 的测试二进制从 33 个（共约 709 MB）减到 20 个（约 313 MB），根包集成测试从 14 个（约 513 MB）变成 1 个（约 116 MB），测试数不变。改 `crates/core` 的一个函数体后构建全部测试二进制，CPU 时间约减半；只改一个测试文件时要重新编译并链接整个 `all`，比原来只重编一个测试二进制略慢（粗测多约 0.4 s）。
- **Host（已完成）**：原来的 25 个集成测试二进制合并为每个 crate 一个 `tests/all.rs`：`crates/services`（13 个，现在是 `zcode-host-services::all`）、`crates/zcode-host`（10 个，`zcode-host::all`）、`crates/rpc`（2 个，`zcode-host-rpc::all`）。做法同 CLI：每个原文件是 `tests/suite/` 下的一个模块，测试名形如 `suite::end_to_end::…`；夹具留在 `tests/fixtures/`，模块里写 `include_str!("../fixtures/…")`。新测试加在 `tests/suite/` 下，不在 `tests/` 下新建 `*.rs`。
  - `crates/rpc` 的两个文件只链接 rpc crate，合并前后都只有几 MB；合并是为了 Host 只有一条规则，两个文件也共用同一份夹具读取。
  - 按 feature 分支的模块在文件里用 `#![cfg(…)]` 标注：`agent_cli`、`cli_entry`、`restart_settlement` 只在合并形态（缺省）编译，`host_only` 只在关掉 `embedded-runtime` 时编译。`scripts/rust-host/test.mjs` 与 CI 的“只有 Host”一步原来用 `binary(host_only)` 选它，现在按测试名选：`-E 'package(zcode-host-runtime-link) | (binary(all) & test(/^suite::host_only::/))'`（10 个测试，与合并前相同）。
  - 只跑一部分：`cargo nextest run --manifest-path apps/zcode-host/Cargo.toml -p zcode-host -E 'binary(all) & test(/^suite::end_to_end::/)'`，或 `cargo test -p zcode-host --test all suite::end_to_end::`。
  - 同一个 crate 内逐字重复的辅助代码放在 `tests/suite/common.rs`：services 是控制消息的接收端（`Control`、`NullSink`）、`runtime_vars`、`write_provider_config`、`chunk`、`jwt`、`now_seconds`、`repo_root`、`find_node`、`server_bundle`；zcode-host 是 `runtime_env`、`provider_config`、`UNSOLICITED_REPORT`、`DEV_ORIGIN`、`Socket`；rpc 是 `fixture`、`hex`。各文件里写法不同的 mock 模型与 `Host` / `Rpc` 夹具没有合并；`services` 与 `zcode-host` 之间相同的辅助代码（mock 模型、provider 配置）也还各有一份，跨 crate 共用要等 §4.3、§4.4 的 `test_support` crate（E10）。
  - `scripts/rust/check-host-spawns.mjs` 按路径里的 `tests/` 识别测试代码，`tests/suite/` 下的模块照旧不受“任务只经 zcode-async 启动、通道有界”的检查。
  - nextest 仍然每个测试一个进程，合并不改变测试之间的隔离；合并前后各跑 3 次以上全量测试，没有出现失败或重试。直接用 `cargo test --test all` 时，一个二进制里所有模块的测试在同一个进程内并发（合并前只有同一文件里的测试如此）：测试没有改进程级状态（环境变量、cwd），`services` 与 `zcode-host` 各试跑一次通过；时间敏感的 `control_latency` 以 nextest 或按名字单独运行的结果为准。
  - 合并后 Host workspace 的测试二进制从 40 个（共约 1964 MB）减到 18 个（约 1112 MB），集成测试从 25 个（约 1176 MB）变成 3 个（约 323 MB：services 190 MB、zcode-host 128 MB、rpc 5 MB），测试数不变（553 个通过、1 个 `#[ignore]`；“只有 Host”形态 10 个）。粗测（macOS，10 核，调试构建，机器同时有别的负载，load average 12–50；各 3 次取中位数）：改 `crates/settings` 的一个函数体后构建全部测试二进制（`cargo nextest run --workspace --no-run`），耗时 23.1 s → 15.8 s，CPU 时间（user + sys）105 s → 57 s；同样的改动后跑完全部测试（`cargo nextest run --workspace`），40.9 s → 34.4 s，CPU 时间 154 s → 107 s，其中执行测试的约 17–18 s 不变。只改一个测试文件时要重新编译并链接整个 `all`：改 `services` 的 `tasks_cli.rs` 2.1 s → 2.5 s，改 `zcode-host` 的 `end_to_end.rs`（原来的二进制用不到 Host 服务与 runtime，只有 11 MB）1.6 s → 2.3 s。

### 4.3 进程内 TestHost（E10）

参照 Codex `core/tests/common/test_codex.rs`：

```rust
let host = TestHost::builder()
    .with_settings(|s| s.model = "test-model".into())
    .with_clock(ManualClock::at(T0))     // 注入时间
    .build(&mock_model)                  // 临时 HOME、确定性 id
    .await?;
let session = host.create_session(&workspace).await?;
session.send_text("hi").await?;
let finished = session.wait_for(|e| matches!(e, Event::Finished { .. })).await?;
```

- 进程内 transport 就是测试入口，测试经由真实的 Gateway 方法表驱动 HostCore，不调用内部函数。
- 用 `#[ctor]` 在测试进程启动时打开确定性 id、设置 `INSTA_WORKSPACE_ROOT`。
- 测试辅助代码放在独立的 `test_support` crate 中，不放在产品 crate 的 cargo feature 后面，避免同一个 crate 被编译两遍。

### 4.4 mock 模型服务器（E10）

参照 Codex `core/tests/common/responses.rs` 和 `streaming_sse.rs`：

- 用 `sse(vec![ev_created(id), ev_text_delta("…"), ev_tool_call(..), ev_completed(id)])` 这类构造器生成 SSE 响应，按我们支持的几种模型协议各写一套；
- `mount_sse_sequence` 按顺序返回多轮响应，返回的 `ResponseMock` 记录每个请求，供断言请求体；
- 匹配器对每个请求做不变量检查，例如工具结果必须有对应的调用；违反时测试直接失败；
- 另写一个可以按测试指令逐块放行的流式服务器，用来测试流到一半时取消、断流恢复和空闲超时；
- 默认 mock 掉所有模型列表、配置等辅助接口，测试绝不访问真实网络。

现有 TS 端到端测试（`packages/services/tests/zcode-cli-rust-*.test.ts`）继续保留，作为跨语言契约的验收。

### 4.5 快照与断言（E11）

- 用 insta 给这几类输出做快照：V4 帧、会话快照、发给模型的完整请求（脱敏后），以及 prompt 和上下文的渲染结果。（Codex `context_snapshot.rs` 对模型可见的上下文做快照，任何上下文变化都会在 review 中显现。）
- 用 `pretty_assertions`，比较整个对象，不逐个字段比较。
- 不为静态定义的值写测试；功能删除后，不补“它已经不存在”的反向测试。

### 4.6 启动真实二进制的测试

参照 Codex `app-server/tests/common/test_app_server.rs`：

- 通过 `CARGO_BIN_EXE_*` 找到二进制；
- 使用临时 HOME 和清理过的环境；
- `kill_on_drop`；
- 每次读取都带超时，超时信息里附上已收到的输出；
- stderr 转发到测试日志。

需要网络的测试统一用 `skip_if_no_network!()` 跳过，不在测试内部判断。

## 5. CI（E6）

参照 Codex `.github/workflows/rust-ci.yml` 和 `blocking-ci.yml`：

- **PR 准入**，在 macOS、Linux、Windows 上运行：
  - `cargo fmt --check`；
  - `cargo clippy --all-targets -- -D warnings`；
  - nextest；
  - 架构检查，包括依赖方向、唯一入口和清单检查；
  - cargo-shear、cargo-deny；
  - `pnpm test:rust-host` 的 wire 夹具与差分测试。
- **按路径过滤**：先用一个 job 根据 `git diff` 输出各模块是否改动，没改动的 job 直接跳过。
- **单一汇总准入**：最后一个 `required` job 汇总所有结果，被跳过或取消的 job 一律按失败处理。分支保护只依赖这一个 job。
- **干净工作区**：每个 job 结束时检查 `git status` 是否干净，防止生成物漂移被忽略。
- **缓存**：cargo home 按 `Cargo.lock` 与 `rust-toolchain.toml` 的哈希作 key，只在未命中时保存，保存失败不影响 job 结果。
- **Windows**：把 `CARGO_TARGET_DIR` 放到仓库外的 Dev Drive，开启 git 长路径支持。
- **合入后**：在更多目标平台上跑 release 构建和全量 E2E。

## 6. 发布（E16）

- **符号**：release 构建保留 `line-tables-only`，打包脚本先把 dSYM / `.debug` / `.pdb` 归档为单独的产物，再 strip 二进制。崩溃报告因此可以符号化。
- **版本**：发布 tag 必须与 `Cargo.toml` 的版本一致；用 `option_env!("ZCODE_GIT_COMMIT")` 把 commit 写入二进制，`zcode --version` 输出它。
- **Linux musl**：U5 远端部署需要静态二进制（x64、arm64）。Codex 用 Zig 作为 musl 的链接器。
- **签名**：macOS 与 Electron 使用同一证书并做公证；Windows 签名。远端二进制是否附带签名校验，在 U5 子规格中决定。
- **npm**（沿用现有 `pack-zcode-cli-rust-npm.mjs`）：先发布各平台包；版本已存在时视为成功，使重试是幂等的；改用 OIDC 发布，不保存 token。

## 7. 运行时编码模式

这些做法属于代码层面。U0 先在 L0 crate 中提供实现，后续阶段逐步迁移存量代码。

| #   | 模式                                                                                                                                                                                                                     | Codex 出处                                                                              | 我们要修的地方                                                                                                                      |
| --- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| R1  | **任务监督**：`TaskScope::spawn` 返回的句柄 drop 时会 abort 任务；panic 时转成 `Err(TaskPanicked)` 送给所属作用域；关机时逐层取消并等待                                                                                  | `AbortOnDropHandle`、`JoinSet`、`tasks/mod.rs`                                          | CLI `core/src/app/run.rs:163`（run 任务 panic 后不发 `Finished`）；Host 139 处分离的 spawn                                          |
| R2  | **取消**：用 tokio-util 的 `token.run_until_cancelled(fut)` 或一个小扩展 `or_cancel`，取代手写的 `select!{cancel…}`                                                                                                      | `async-utils` `OrCancelExt`                                                             | CLI 约 19 处手写的 `select!`                                                                                                        |
| R3  | **有界通道，溢出策略写明**：区分必须送达的消息（Finished、权限请求）和可丢弃的消息（可丢弃的标记 `Lagged`）；慢连接只影响它自己                                                                                          | `CHANNEL_CAPACITY = 128`、`server_notification_requires_delivery`                       | Host 30 个无界 mpsc                                                                                                                 |
| R4  | **ArcSwap 快照发布**：热更新的配置（provider、hooks、MCP 运行时）整份替换，读的一方不必等                                                                                                                                | `McpRuntime{current: ArcSwap<..>}`                                                      | CLI `mcp_hub.rs` 的 `RwLock` 加 gate；Host 与每个 CLI 各自每秒轮询 provider 文件                                                    |
| R5  | **子进程**：Unix 用 `setsid`（失败时退回 `setpgid`）、`killpg` 兜底、Linux `PR_SET_PDEATHSIG`；Windows 用 Job Object，以 `CREATE_SUSPENDED` 启动后再加入 Job；输出头尾各保留一半；shell 探测有回退；cmd.exe 用 `raw_arg` | `utils/pty/src/{process_group.rs,win/job.rs}`、`head_tail_buffer.rs`、`shell_detect.rs` | CLI `tool_process.rs:67`（cmd 引号）、`process_tree.rs`（依赖 `/bin/ps`）、Windows 每条命令都跑 `taskkill`；Host `agent/process.rs` |
| R6  | **文件与路径**：`atomic_write(path, bytes, mode)` 按 `create_new` 临时文件 → `sync_all` → rename → fsync 父目录的顺序执行；`AbsPath` 类型；`home()` 在 `HOME` 为空时报错                                                 | `utils/path-utils`、`secrets/src/local.rs`、`utils/absolute-path`                       | CLI 至少 8 处手写原子写，彼此不一致；5 份 `home()`，其中两份在 `HOME` 为空时回落到 cwd                                              |
| R7  | **秘密与日志**：`RedactedString`（Debug 输出 `<redacted>`）；日志按值做正则脱敏（`sk-…`、`Bearer …`、`AKIA…`）；日志文件权限 0600；退出时报告丢弃的日志行数；内存环形缓冲供反馈附带日志                                  | `utils/redacted-string`、`secrets/src/sanitizer.rs`、`feedback`                         | CLI `ModelConfig.api_key_value` 是裸 `String`；`LogGuard::dropped()` 没有被读取                                                     |
| R8  | **协议方法表**：一个声明宏生成方法枚举、分发函数和导出清单；错误回复类型化，例如队列满时回 `-32001`、序列化失败时回 `-32603`，而不是不回                                                                                 | `app-server-protocol/src/protocol/common.rs`                                            | CLI `core-api/src/transport.rs` 的 `methods!` 只生成名字；host reply 的 `error` 被当作 null（`app-server/src/stdio.rs:56`）         |
| R9  | **漂移检查**（E13）：生成物入库，测试比对时打印 unified diff，并提示应执行的重新生成命令                                                                                                                                 | `schema_fixtures_tests.rs`                                                              | 方法名列表、`WIRE_VERSION`、帧大小上限要加入现有 `--check` 脚本；出站消息在测试中按 schema 校验                                     |
| R10 | **每个工具一个 executor**：工具定义、处理函数和“能否并行”放在一起；工具名用类型化的 `ToolName`                                                                                                                           | `tools/src/tool_executor.rs`                                                            | CLI `ToolPort` 约 40 个方法；工具定义按 `d["function"]["name"]` 字符串匹配                                                          |

## 8. 评审规则（E15，写进 AGENTS.md）

1. **改动规模**：非机械性改动单次不超过 800 行，复杂逻辑不超过 500 行。超出时，按实际的 diff 和依赖找出最小的可合入步骤，先合这一步。（Codex AGENTS.md “Change size guidance”）
2. **破坏性变更清单**：评审时逐项检查以下对外接口：
   - Renderer 的 `@zcode/rpc` 契约；
   - V4 wire；
   - Main ↔ Host 控制消息；
   - CLI 参数；
   - 配置文件的加载；
   - 从已有会话库恢复会话；
   - 远端链路握手（同一版本内）。
3. **模型可见的上下文**（Codex AGENTS.md “Model visible context”，直接关系到 prompt cache 命中和成本）：
   - 上下文只追加，不改写历史；
   - 避免频繁变动会导致缓存失效的内容；
   - 注入的每一项都必须有大小上限，单项不超过 10K tokens；
   - 可能超过 1K tokens 的新增项，评审时按 P0 单独审查。
4. **runtime 核心不膨胀**：新概念优先放进新的模块或 crate，Engine 只做编排。评审时有权拒绝“顺手加进 Engine”的改动。（Codex：“resist adding code to codex-core”）
5. **agent 行为的改动优先写集成测试**：经由 TestHost 和 mock 模型服务器测试，而不是只测内部函数。

## 9. 不借鉴

| 做法                                                | 理由                                                                                   |
| --------------------------------------------------- | -------------------------------------------------------------------------------------- |
| Bazel、BuildBuddy、远程执行                         | Codex 自己也标为实验性；每个 crate 都要维护 `BUILD.bazel` 和锁文件同步，成本远大于收益 |
| Dylint 实现的参数注释 lint、DotSlash                | 依赖 nightly 工具链；用 2.4 的编码约定代替                                             |
| nextest archive 分片                                | 我们的测试规模用不上                                                                   |
| 用 cargo-deny 全面禁止 `async-trait`                | 我们的端口都以 `dyn` 方式使用，迁移只换来编译速度                                      |
| ts-rs / schemars 从 Rust 生成 TS                    | 与当前“TS/zod 为来源”的方向相反；是否翻转在 D-U5（U9）再定                             |
| 中断时等 100 ms 后强制 abort                        | 违反“不用超时掩盖同步问题”；我们等待已提交的 `Finished`                                |
| 无界事件通道；`Arc<Session>` 加十几个 Mutex         | 不如我们的单一所有者 actor 加提交回执                                                  |
| sqlx 及其迁移器、OTel / Statsig 上报                | rusqlite 已经满足需要；遥测走现有通道                                                  |
| TOML 分层配置、feature 注册表                       | 与 Node 配置兼容冲突                                                                   |
| Starlark execpolicy、shell-escalation、Windows 沙箱 | 体量大，且依赖 patch 过的 zsh 或大量平台代码；OS 沙箱如需要单独立项                    |

## 10. 落地顺序

1. **U0 第 1 步：护栏，不改行为。** E1、E2 中可以直接 deny 的部分、E3 中的锁规则、E4、E5、E6。先让 CI 跑起来，此后所有改动都受这些检查约束。
2. **U0 第 2 步：L0 crate 与缺陷修复。** E7、E8、E9（R1、R5、R6、R7），同时启用对应的 `disallowed-methods` 规则；每个已确认的缺陷先补一个修复前失败的测试。
3. **U0 第 3 步：测试基础设施。** E10、E11、E12。U1 起的 runtime 改动都依赖它。
4. **贯穿各阶段：** E13 随 U2 的方法表一起落地；E14 按 crate 逐个清零后改为 deny；E15 立即写进 AGENTS.md。
5. **U5 / U8：** E16。
