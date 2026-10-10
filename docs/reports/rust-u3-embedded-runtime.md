# U3 A 期：嵌入式 runtime 与子进程链路对照

2026-10-09。分支 `feat/rust-u3-e2e`（基于 `feat/rust-unified-host` 的 `dba383e`）。规格见 [rust-u3-single-process.md](../specs/rust-u3-single-process.md) §3 最后一条：N 个 workspace 同时流式时，比较两条链路的控制 RPC 延迟、Host 进程 RSS 与 CPU（子进程链路另加各 CLI 进程），以及 workspace runtime 的冷启动就绪时间。

## 结论

- 控制 RPC：两条链路的 p50 都在 0.2–0.3 ms，p95 在 1 ms 以内，差异落在本机负载造成的抖动范围内。到 runtime 的往返（`queryConversationCommandsV4`）嵌入式略快（p50 约 0.19 ms 对 0.24–0.27 ms），少了一次进程间管道。
- 内存：嵌入式每多一个 workspace，Host RSS 约多 2–4 MiB；子进程链路每个 CLI 进程约 35–37 MiB RSS、约 20–22 MiB footprint。N = 8 时合计 RSS 78 MiB 对 331 MiB，footprint 49 MiB 对 193 MiB。代价是嵌入式 Host 空闲 RSS 多约 6 MiB（runtime 代码，以及启动时在进程内完成的会话库准备）。
- CPU：流式窗口内，N = 8 时嵌入式 Host 8.6%（单核百分比），子进程链路 Host 3.3% 加 CLI 合计 7.9%，共 11.2%。N = 1 时嵌入式 3.2% 对 3.8%。
- 冷启动就绪：除第一个 workspace 外，嵌入式约 17 ms，子进程约 25 ms；在 N 路流式的同时冷启动一个新 workspace，结果相同（约 18 ms 对 25 ms）。第一个 workspace 两条链路都在 80–260 ms，波动大，含一次性开销。

## 方法

测量代码是 `apps/zcode-host/crates/zcode-host/tests/suite/control_latency.rs` 的 `runtime_link_comparison`（`#[ignore]`），与 R2 验收的 `control_rpc_p95_while_four_workspaces_stream` 共用 Host 启动、mock 模型与 RPC 客户端：

```sh
cd apps/zcode-host
# 嵌入式（Host 的缺省链路）
ZCODE_PERF_WORKSPACES=4 cargo test --release -p zcode-host --test all suite::control_latency::runtime_link_comparison -- --ignored --nocapture
# 子进程链路
ZCODE_TEST_CLI_RUST_BINARY=$PWD/../zcode-cli-rust/target/release/zcode-cli-rust ZCODE_PERF_WORKSPACES=4 cargo test --release ...（同上）
```

每次运行：

1. 用发布构建的 `zcode-host`（`lto = "thin"`）启动一个 Host，环境清空后只给临时 HOME、PATH 与 `NO_PROXY`；发 `init-local`，等到 `ready`（含会话库准备）。子进程链路的 CLI 是同一棵树的 `apps/zcode-cli-rust` 发布构建。
2. 像 Renderer 一样建一条 desktop-continuous 的 WebSocket 连接并完成 V4 握手。所有调用和流式帧都走这一条连接，队头阻塞计入延迟。
3. 冷启动就绪：对 N 个 workspace 依次调用 `zcode-agent.initialize`，计时到返回 `available: true`。这包括启动 runtime 实例、等存储就绪、过 provider 门禁、推送 app 偏好。此时没有流式。
4. 在 N 个 workspace 各建一个会话并订阅。本地 mock 的 OpenAI 兼容模型每 20 ms 发一段，共 2000 段（约 40 s）。
5. 等每路都出帧后进入采样窗口。交替调用 150 次 `setting.get`（只在 Host 内处理）和 `zcode-agent.queryConversationCommandsV4{clock:true}`（经 Host 到 runtime 再返回），每对之间停 25 ms，窗口约 4.1 s。窗口内每路收到约 95 帧（runtime 合并了部分增量）。
6. 资源：每 200 ms 用 `ps` 读一次 Host 及其 `zcode-cli-rust` 子进程的 RSS，取窗口内均值；CPU 取窗口首尾累计 CPU 时间之差除以窗口墙钟时间，以单核百分比表示；窗口结束时用 macOS `footprint` 读一次实际占用（`ps` 的 RSS 含分配器已释放、可复用的页）。
7. 负载下的冷启动：N 路仍在流式时，对第 N+1 个 workspace 调 `initialize` 并计时。

两条链路按 N = 1、4、8 交错运行，每组 3 次，共 18 次，表中取 3 次的中位数。整套跑了两轮。

机器：Apple M1 Max（10 核），64 GB，macOS 15.7.7，rustc 1.95.0。测量期间其他任务在同一台机器上并发编译 Rust、运行 Node 测试。第二轮的 1 分钟 load average 是 47–75，第一轮是 121–221。下面以第二轮为主，第一轮放在最后，用来说明负载对延迟的影响。

## 结果（第二轮，load 47–75）

时间单位 ms：

| N   | 链路   | `setting.get` p50 / p95 | 到 runtime 的往返 p50 / p95 | 冷启动就绪（第 2 个起，中位） | 流式中再冷启动一个 |
| --- | ------ | ----------------------- | --------------------------- | ----------------------------- | ------------------ |
| 1   | 嵌入式 | 0.30 / 0.77             | 0.20 / 0.77                 | —                             | 17.9               |
| 1   | 子进程 | 0.31 / 1.65             | 0.27 / 0.84                 | —                             | 25.8               |
| 4   | 嵌入式 | 0.29 / 0.65             | 0.19 / 0.32                 | 16.7                          | 18.6               |
| 4   | 子进程 | 0.31 / 0.81             | 0.27 / 0.58                 | 26.3                          | 25.9               |
| 8   | 嵌入式 | 0.30 / 0.53             | 0.19 / 0.33                 | 17.6                          | 17.4               |
| 8   | 子进程 | 0.28 / 0.43             | 0.24 / 0.43                 | 24.8                          | 24.8               |

第一个 workspace 的冷启动就绪，三次的中位数：嵌入式 95 / 139 / 216 ms（N = 1 / 4 / 8），子进程 99 / 112 / 84 ms。各次在 78–264 ms 之间，与 N 和链路没有一致的关系。它含有只发生一次的开销，例如第一次加载 provider 注册表，本测量没有拆开它。

内存单位 MiB，CPU 为单核百分比：

| N   | 链路   | Host RSS 空闲 | Host RSS 冷启动后 | Host RSS 流式均值 | CLI RSS 流式均值合计 | 合计 RSS | footprint Host / CLI 合计 | CPU Host / CLI 合计 |
| --- | ------ | ------------- | ----------------- | ----------------- | -------------------- | -------- | ------------------------- | ------------------- |
| 1   | 嵌入式 | 30.8          | 49.4              | 60.0              | —                    | 60.0     | 32 / —                    | 3.2 / —             |
| 1   | 子进程 | 25.2          | 43.7              | 47.7              | 36.8                 | 84.6     | 30 / 22                   | 2.1 / 1.7           |
| 4   | 嵌入式 | 31.3          | 57.4              | 71.0              | —                    | 71.0     | 42 / —                    | 5.6 / —             |
| 4   | 子进程 | 25.7          | 42.9              | 46.7              | 145.2                | 192.4    | 29 / 85                   | 2.4 / 4.6           |
| 8   | 嵌入式 | 30.3          | 64.4              | 78.3              | —                    | 78.3     | 49 / —                    | 8.6 / —             |
| 8   | 子进程 | 24.5          | 44.0              | 47.2              | 283.5                | 331.3    | 29 / 164                  | 3.3 / 7.9           |

- p99 与最大值不进表。两条链路在个别运行里都出现过 10–90 ms 的单次长尾，最大一次是嵌入式 N = 1 的 86.6 ms，子进程 N = 8 有一次 58.2 ms；同组的其他运行没有。本机负载下无法把它归因到链路上。
- 子进程链路的 Host 空闲 RSS 低约 6 MiB，因为启动时的会话库准备在 CLI 子进程里完成，Host 里也没有 runtime 代码。

## 第一轮（load 121–221）

| N   | 链路   | `setting.get` p50 / p95 | 到 runtime 的往返 p50 / p95 | 合计 RSS 流式均值 | CPU Host / CLI 合计 |
| --- | ------ | ----------------------- | --------------------------- | ----------------- | ------------------- |
| 1   | 嵌入式 | 0.28 / 0.77             | 0.19 / 0.36                 | 63.3              | 2.9 / —             |
| 1   | 子进程 | 0.33 / 2.43             | 0.32 / 3.02                 | 82.0              | 2.0 / 1.7           |
| 4   | 嵌入式 | 0.45 / 6.56             | 0.26 / 2.20                 | 70.9              | 5.6 / —             |
| 4   | 子进程 | 0.54 / 7.08             | 0.62 / 10.35                | 192.3             | 2.7 / 5.1           |
| 8   | 嵌入式 | 0.34 / 1.81             | 0.21 / 1.06                 | 77.9              | 7.8 / —             |
| 8   | 子进程 | 0.38 / 4.46             | 0.33 / 5.23                 | 335.5             | 3.6 / 9.7           |

内存与 CPU 两轮基本一致。延迟的 p95 在同一组的三次之间相差可达十倍（例如嵌入式 N = 4：6.6、6.9、0.5 ms），说明这一轮的延迟反映的主要是机器负载，不用来比较链路。

## 桌面 E2E 中看到的调试构建差异

桌面 E2E 用的是调试构建的 Host（故障注入只在调试构建里有），runtime 也随之是调试构建；子进程模式缺省用 CLI 的发布构建。用一个临时探针场景（未提交）测量“输入框可点、输入完文字之后，发送键还要多久才可用”，每种 3 次：

| Host / runtime             | 发送键在输入后多久可用 |
| -------------------------- | ---------------------- |
| 调试 Host + 嵌入式 runtime | 0.50–1.17 s            |
| 调试 Host + 发布构建 CLI   | ≤ 22 ms                |
| 调试 Host + 调试构建 CLI   | 0.34–0.53 s            |

发送键可用之前，Composer 把 Enter 当作换行（`LexicalChatInput` 的 `submitDisabled`）。E2E 原来输入后立即按 Enter，在嵌入式模式下第一条消息有时发不出去；改用调试构建的 CLI 跑子进程模式，同一场景 4 次全部失败在同一位置。所以这是 harness 的时序假设，被调试构建的 runtime 放大，与链路无关。现在 harness 先等发送键可点，再按 Enter。嵌入式比调试 CLI 再慢一些，可能是 runtime 与调试构建的 Host 服务共用同一个 tokio 运行时，样本只有 3 次，没有进一步拆分。

## 没有测量的内容

- 只测了 macOS / arm64，没有 Windows 与 Linux。
- 负载只有纯文本流式。没有工具执行（Bash 等子进程）、MCP、大附件、长历史会话，也没有长时间运行下的内存增长。
- 没有经过 Renderer：延迟是 Host 侧 WebSocket 往返，不含界面渲染。
- 每个 workspace 只有一个会话。“一个 Engine 管所有 workspace”（rust-unified-host.md D-U12）不在 A 期范围内，没有测量。
- runtime 实例崩溃的恢复时间没有单独计时。E2E 只验证了行为：发送失败，下一条消息由新一代 runtime 处理，Host pid 不变。
