# App 级性能：Node 栈与 Rust 栈的整机对比

2026-10-10。分支 `perf/rust-app-level`（基于 `feat/rust-unified-host` 的 `d5ff797`）。此前的两份报告只量了一部分：[rust-performance-2026-09-25](rust-performance-2026-09-25.md) 是 Node CLI 对 Rust CLI（不含 Electron 与 Host），[rust-u3-embedded-runtime](rust-u3-embedded-runtime.md) 是 Rust Host 内两种 runtime 链路。本报告量整个桌面 App 的进程树：Electron Main、Renderer、GPU 与其他辅助进程、Host、runtime、scheduler、MCP 与工具进程，也就是用户机器为 ZCode 付出的全部内存与 CPU，以及界面的响应。

这是一次粗测。按要求没有等机器空闲：测量时同一台机器上有其他编译与测试任务，每次运行的 1 分钟 load average 都有记录（见“方法”）。每项测量 3 次（A/B/C 交错），给出中位数与最小–最大值。

## 结论

三种配置各 3 次、交错运行（startup / stream / history 三个 suite，共 27 次，全部有效），表中是中位数，括号里是最小–最大值。内存是 macOS physical footprint 的进程树合计（活动监视器的“内存”之和），CPU 是单核百分比（10 核机器上限 1000%）。

| 指标                                             | A：Node Host + TS CLI | B：Rust Host + 进程内 runtime | C：Node Host + Rust CLI | B 对 A |
| ------------------------------------------------ | --------------------: | ----------------------------: | ----------------------: | -----: |
| 内存，空闲（启动后 30 s，未开会话）              |     868 MiB (866–870) |             460 MiB (440–491) |       582 MiB (582–587) |   −47% |
| 内存，4 个 workspace 各完成一轮                  |  1479 MiB (1471–1491) |             514 MiB (511–516) |       717 MiB (716–721) |   −65% |
| 内存，8 个 workspace 各完成一轮                  |  2203 MiB (2180–2283) |             535 MiB (532–535) |       799 MiB (753–810) |   −76% |
| 每多一个 workspace                               |     193 MiB (192–205) |           10.6 MiB (9.9–10.7) |    27.9 MiB (20.4–29.7) |   −95% |
| 内存，4 个会话同时流式（均值）                   |  1717 MiB (1700–1733) |             664 MiB (662–666) |       822 MiB (821–861) |   −61% |
| CPU 合计，4 个会话同时流式                       |     52.1% (45.6–52.2) |             53.0% (51.7–53.8) |       47.5% (46.7–48.8) |   持平 |
| 其中 Host + runtime                              |                 14.7% |                          5.9% |                    8.9% |   −60% |
| 冷启动：启动 → 主界面可用                        |    4.08 s (3.78–4.11) |            1.79 s (1.68–2.09) |      2.59 s (2.23–3.35) |   −56% |
| 首条消息：Enter → 首段可见                       |      740 ms (595–790) |              274 ms (188–351) |        199 ms (176–211) |   −63% |
| 新 workspace 的首条消息首段                      |      762 ms (761–928) |              146 ms (144–161) |        149 ms (130–160) |   −81% |
| 退出：SIGTERM → 进程树全部结束（8 个 workspace） |    1.50 s (1.41–2.48) |            0.24 s (0.24–0.25) |      1.41 s (1.24–1.42) |   −84% |

- **内存是最大的差别。** Electron 自己的部分（Main、Renderer、GPU、network service 等辅助进程）三种配置相同，空闲时约 405 MiB，4 个 workspace 时约 465–490 MiB。差别全在其余部分：空闲时 A 约 460 MiB（Node Host 158、scheduler 55、预热的 TS CLI 170、它启动的 node_repl MCP 79），B 只有 Rust Host 的 40 MiB，C 约 180 MiB（Node Host 101、scheduler 55、Rust CLI 22）。每多一个 workspace，A 多一个 TS CLI（约 125 MiB）和一个 node_repl MCP（约 78 MiB），B 只多约 10 MiB（其中 Rust Host 约 2 MiB，其余是 Renderer 里多出的会话数据；Renderer 有 V8 回收时机带来的 ±10 MiB 起伏，这个数只能看量级）。所以 workspace 越多差距越大：8 个时整个 App 从 2.2 GiB 降到 0.54 GiB。
- **流式时总 CPU 没有下降。** 1 路约 39–40%，4 路约 48–53%，三种配置的差别在噪声范围内。大头是 Renderer（26–34%）和 GPU（约 8%），三种配置相同。Host 与 runtime 的 CPU 确实少了（4 路时 A 14.7%、C 8.9%、B 5.9%），但 B 的 Renderer、Main 与 network service 进程反而多用了约 10 个百分点（见下文“意外”），抵消了这部分节省。
- **启动、首条消息与退出明显更快。** B 的主界面可用时间是 A 的 44%，Host 数据库就绪 1.1 s 对 3.5 s。首条消息首段从 740 ms 降到 274 ms；以后每打开一个新 workspace，A 都要冷启动一个 TS CLI 子进程（首段约 760 ms），B 与 C 约 150 ms。退出时 B 只有 6 个进程要收，0.24 s 结束；A 有 23 个，1.4–2.5 s。
- **界面响应没有差别。** 三种配置在 1 路和 4 路流式时都没有 Renderer 长任务（≥ 50 ms）；按键到字符上屏 p50 约 23–27 ms、最大约 35 ms；点侧栏切换会话到内容可见约 42–48 ms；mock 发出一段到它出现在 DOM 的 p50 在 28–55 ms，p95 在 50–70 ms。这些差别都在一帧到两帧之内，3 次之间的波动与配置之间的差别同量级。
- **C 说明了 Host 与 runtime 各自的贡献。** 只把 runtime 换成 Rust（C），每个 workspace 的成本就从 193 MiB 降到 28 MiB，新 workspace 首段与 B 相同；但 Node Host 与 Node scheduler 仍在（约 155 MiB），启动慢约 0.8 s，退出与 A 同量级。B 在此基础上再省掉这两个 Node 进程，并把 runtime 收进 Host 进程（每个 workspace 约 10 MiB，而不是一个 22 MiB 的子进程）。

## 结果

### 冷启动与首条消息（startup suite，8 个 workspace 的 profile）

| 指标（ms）                                          |                A |                 B |                C |
| --------------------------------------------------- | ---------------: | ----------------: | ---------------: |
| 启动 → composer 可点                                | 4078 (3783–4108) |  1790 (1682–2085) | 2587 (2231–3351) |
| 启动 → Host 数据库就绪                              | 3487 (3371–3585) | 1094 (763–1360) † | 1935 (1769–2610) |
| 启动 → 发送键可用                                   | 4971 (4741–5034) |  1883 (1759–2251) | 2661 (2327–3454) |
| 首条消息：Enter → 首段可见                          |    740 (595–790) |   274 (188–351) † |    199 (176–211) |
| 首条消息：Enter → 回复完整（40 段 × 20 ms）         | 1601 (1465–1645) |   1083 (992–1146) |  1003 (998–1023) |
| 第 2–8 个 workspace 的首段（每次运行取 7 个的中位） |    762 (761–928) |     146 (144–161) |    149 (130–160) |

† 表示 3 次的最大与最小之差超过中位数的 50%，不足以支撑细的比较。stream 与 history 两个 suite 也各记了冷启动，结论相同（主界面可用，stream：A 4.1 s、B 1.8 s、C 2.5 s；history：A 3.5 s、B 1.7 s、C 2.1 s）。

A 慢的可能原因有两处（从代码看出，没有逐段计时）：Node Host 在一个 Worker 线程里加载 16 MB 的 TS bundle 做会话库的存储准备（`packages/desktop/src/host/storagePreparationProcesses.ts`：非原生 runtime 用 `new Worker(entry)`），然后预热的 TS CLI 子进程自己再加载一次；C 的存储准备交给 Rust CLI 进程（`storagePreparationMode: "process"`），B 在 Host 进程内完成。首条消息首段 B 比 C 慢约 75 ms 且波动大（188–351 ms），本次没有拆开。

### 稳态内存（每个角色，footprint 中位数，MiB；括号内为进程数）

| 角色                   |      空闲 A |      空闲 B |      空闲 C |       N = 4 A |     N = 4 B |      N = 4 C |       N = 8 A |     N = 8 B |      N = 8 C |
| ---------------------- | ----------: | ----------: | ----------: | ------------: | ----------: | -----------: | ------------: | ----------: | -----------: |
| Main                   |         111 |         109 |         110 |           109 |         104 |          109 |           110 |         106 |          111 |
| Renderer               |         148 |         151 |         151 |           234 |         211 |          210 |           158 |         223 |          216 |
| GPU                    |         137 |         137 |         137 |           133 |         133 |          134 |           132 |         134 |          134 |
| 其他 Electron 辅助进程 |         8.4 |         8.6 |         8.3 |      15.9 (2) |    17.0 (2) |     16.3 (2) |      15.9 (2) |    17.0 (2) |     16.3 (2) |
| Host                   |         158 |        40.4 |         101 |           110 |        46.0 |          108 |           114 |        54.7 |          111 |
| scheduler              |        55.2 |           — |        55.5 |          54.2 |           — |         54.0 |          45.1 |           — |         33.0 |
| runtime 子进程         |         170 |           — |        22.2 |       500 (4) |           — |     88.3 (4) |       998 (8) |           — |      179 (8) |
| node_repl MCP          |        79.0 |           — |           — |       318 (4) |           — |            — |       627 (8) |           — |            — |
| **合计**               | **868** (8) | **460** (5) | **582** (7) | **1479** (15) | **514** (6) | **717** (11) | **2203** (23) | **535** (6) | **799** (15) |

- “空闲”取自 stream suite（启动后静置 30 s，只有活动 workspace 的 runtime 被预热，没有会话）；N = 4、8 取自 startup suite（每个 workspace 完成一轮后静置 35 s）。各角色的中位数相加不一定等于合计的中位数。
- 同一状态的 RSS（`ps`）合计：空闲 A 1601 / B 725 / C 983 MiB，N = 8 时 A 4536 / B 880 / C 1401 MiB。A 的 RSS 在 3 次之间相差很大（N = 8：2722–5055 MiB），是 macOS 在内存压力下压缩、换出页面的结果；footprint 稳定在 2180–2283 MiB。
- Node Host 的 footprint 在 A 里随时间下降（空闲 158 → N = 4 时 110 MiB），在 C 里稳定在约 100–110 MiB：A 多出的部分推测来自上面说的存储准备 Worker，结束后被回收。Rust Host 从 37–40 MiB 涨到 55 MiB（N = 8），每个 workspace 约 2–2.5 MiB（runtime 实例加会话缓存），与 [U3 报告](rust-u3-embedded-runtime.md) 的 2–4 MiB / workspace 一致。
- Renderer 在 N = 4 与 N = 8 之间有 50–80 MiB 的起伏（A 的 N = 8 反而比 N = 4 少），是 V8 的回收时机，三种配置都有，不代表 Host 的差别。

### 流式（stream suite）

20 ms 一段的 mock 流，采样窗口约 20 s（4 路时另加切换会话的约 7 s）。CPU 为单核百分比，footprint 为窗口内每 2 s 一次读数的均值（MiB）。

| 角色                   | 1 路 CPU A |        B |        C | 4 路 CPU A |        B |        C | 4 路 footprint A |       B |       C |
| ---------------------- | ---------: | -------: | -------: | ---------: | -------: | -------: | ---------------: | ------: | ------: |
| Main                   |        0.9 |      1.7 |      0.8 |        0.8 |      3.2 |      0.7 |              107 |     105 |     107 |
| Renderer               |       25.7 |     27.1 |     26.5 |       28.2 |     34.2 |     30.0 |              256 |     285 |     280 |
| GPU                    |        7.3 |      7.4 |      7.9 |        8.0 |      8.1 |      7.7 |              201 |     206 |     201 |
| 其他 Electron 辅助进程 |        0.0 |      0.6 |      0.0 |        0.1 |      1.6 |      0.1 |             16.7 |    16.3 |    16.2 |
| Host                   |        2.1 |      2.2 |      2.5 |        3.4 |      5.9 |      4.3 |             73.7 |    52.5 |    69.8 |
| scheduler              |        0.0 |        — |      0.0 |        0.0 |        — |      0.0 |             53.8 |       — |    54.2 |
| runtime 子进程         |        4.5 |        — |      1.4 |       11.3 |        — |      4.6 |              681 |       — |    95.3 |
| node_repl MCP          |        0.0 |        — |        — |        0.0 |        — |        — |              317 |       — |       — |
| **合计**               |   **39.8** | **39.1** | **39.1** |   **52.1** | **53.0** | **47.5** |         **1717** | **664** | **822** |

| 响应指标                                    |                A |                B |                C |
| ------------------------------------------- | ---------------: | ---------------: | ---------------: |
| Renderer 长任务（≥ 50 ms），1 路 / 4 路     |            0 / 0 |            0 / 0 |            0 / 0 |
| 按键 → 字符上屏 p50，1 路 / 4 路（ms）      |      26.3 / 23.4 |      26.3 / 27.0 |      24.8 / 26.5 |
| 按键 → 字符上屏 最大，1 路 / 4 路（ms）     |      34.2 / 33.7 |      33.1 / 34.8 |      35.2 / 33.7 |
| 点侧栏切换会话 → 内容可见（ms，4 次的中位） | 47.6 (43.8–50.4) | 41.7 (41.6–44.7) | 44.6 (43.2–47.6) |
| mock 发出 → DOM 可见 p50，1 路（ms）        |       43 (32–43) |       42 (42–57) |       40 (40–51) |
| mock 发出 → DOM 可见 p95，1 路（ms）        |       51 (49–52) |       50 (50–67) |       49 (48–69) |
| mock 发出 → DOM 可见 p50，4 路（ms）        |     28 (22–43) † |       55 (39–61) |       46 (42–46) |
| mock 发出 → DOM 可见 p95，4 路（ms）        |       50 (32–54) |       68 (47–69) |       70 (58–72) |

“mock 发出 → DOM 可见”包括 runtime 合并增量、Host 转发、Renderer 渲染，以及页面每帧检查一次 DOM 的取样误差（最多约 17 ms）。4 路时 A 的 p50 低一些，B、C（都是 Rust runtime）高 15–25 ms，但 A 自己 3 次之间就在 22–43 ms 之间；这可能与 Rust runtime 合并增量的节奏有关（U3 报告里也观察到 runtime 合并了部分增量），本次没有拆开。

### 工具负载一轮

模型先调用 Bash `seq 1 200000`，再 Read 一个 10 万行文件的前 2000 行，最后回复 40 段。

| 指标                            |                A |                B |                C |
| ------------------------------- | ---------------: | ---------------: | ---------------: |
| Enter → 回复完整（ms）          | 1463 (1460–1650) | 1053 (1038–1060) | 1202 (1156–1232) |
| Host + runtime 的 CPU 时间（s） | 0.55 (0.52–0.55) | 0.14 (0.13–0.15) | 0.18 (0.16–0.21) |
| footprint 合计峰值（MiB）       | 1676 (1644–1687) |    772 (763–781) |    922 (918–972) |

这一轮的会话在 ws1，此时 4 路流式刚结束，A 有 5 个 TS CLI 与 5 个 MCP 在场，所以内存峰值主要是前面留下的进程，不是工具本身。CPU 时间只比 Host 与 runtime：采样窗口要等最后一次 footprint 读数结束，A 的进程多、读得慢，窗口约 3.6 s，B 约 1.6 s，Renderer 等进程在窗口尾部的空闲 CPU 会混进合计，所以合计不可比。Bash 子进程在两次采样之间就结束了，它的 CPU 没有计入（三种配置相同）。

### 长会话冷加载（history suite）

120 轮的会话（mock 每轮回约 2.3 KB Markdown，含代码块与列表；每 10 轮一次 Read）。会话库约 2 MB，265 条消息、529 个 part，由配置 A 生成，三种配置读同一份。

| 指标                                  |                A |             B |             C |
| ------------------------------------- | ---------------: | ------------: | ------------: |
| 点开侧栏里的会话 → 最后一轮可见（ms） |    779 (654–896) | 258 (250–275) | 255 (247–295) |
| 滚到顶 → 第一轮可见（ms）             |       78 (75–81) |    81 (76–84) |    74 (72–91) |
| 打开后 footprint 合计（MiB）          | 1155 (1131–1163) | 732 (731–737) | 873 (856–879) |
| 其中 Renderer / Main（MiB）           |        379 / 147 |     393 / 171 |     384 / 175 |

点开会话的时间里 Renderer 的工作三种配置相同，A 多出的约 520 ms 应在 TS runtime 读取并投递这段历史（B、C 都是 Rust runtime 读 Node 的会话库），本次没有拆开计时。滚动由 Renderer 的虚拟列表完成，与 Host 无关。

### 退出

| 指标                                                 |                  A |             B |                C |
| ---------------------------------------------------- | -----------------: | ------------: | ---------------: |
| 8 个 workspace 后：SIGTERM → 进程树全部结束（ms）    | 1502 (1412–2480) † | 244 (235–248) | 1413 (1243–1419) |
| 4 路流式后（5 个 workspace 有 runtime）（ms）        |      897 (891–897) | 240 (240–246) |   761 (728–1061) |
| 长会话后（1 个 workspace）（ms）                     |   423 (414–2724) † | 248 (237–253) | 407 (396–1869) † |
| 退出前的进程数（8 个 workspace / 流式后 / 长会话后） |        23 / 15 / 8 |     6 / 6 / 5 |      15 / 11 / 7 |

每次都断言 Main 走完退出流程、没有残留进程。A 与 C 的退出时间随子进程数增长，B 与 workspace 数无关。A、C 各有一次 1.9–2.7 s 的长尾，发生在 load 较高的第一轮。

## 意外与需要跟进的地方

- **A 每个 workspace 都起一个 node_repl MCP。** TS CLI 为官方插件（Browser Use / Computer Use 共用的 node_repl 宿主）启动 `zcode-node-repl-mcp`，每个约 78 MiB footprint、约 200 MiB RSS，占了 A 每个 workspace 成本的四成。B、C（Rust runtime）在这份开发检出里没有启动它：Rust 的官方插件定义里有 node_repl（`apps/zcode-cli-rust/crates/plugins/src/official.rs`），但本次没有查清是开发检出里没有播种插件目录，还是 Rust runtime 按需启动。所以这部分差别有一半是“功能没跑”，不全是实现效率。去掉 MCP 后 A 的合计：空闲 789 MiB、N = 4 约 1161 MiB、N = 8 约 1576 MiB、4 路流式约 1400 MiB，B 仍然分别少 42%、56%、66%、53%。
- **B 的 Renderer、Main 与 network service 进程 CPU 更高。** 4 路流式时 B 的 Renderer 34.2%（A 28.2%、C 30.0%），Main 3.2%（A、C 0.7–0.8%），network service 等辅助进程 1.6%（A、C 0.1%）；1 路和工具负载时方向相同。A 与 C 的 Renderer 经 MessagePort 连 Node Host，B 的 Renderer 经回环 WebSocket 连 Rust Host，WebSocket 的数据要经过 Chromium 的 network service 进程，这是可能的原因，但没有验证；也可能是 Rust runtime 推送增量的频率不同（见上面的端到端延迟）。这部分抵消了 Host 与 runtime 省下的 CPU，值得单独剖析。
- **Read 的大文件行为不同。** 搭夹具时发现：不带 `limit` 读 3.7 MB 的文件，TS CLI 按 `READ_MAX_FILE_SIZE_BYTES`（256 KB）拒绝（工具结果约 200 字符），Rust runtime 直接返回前 2000 行（约 73 KB）。测量里改为显式 `limit: 2000`，两边读到相同的内容。这是一个对齐问题，需要另行确认哪一边符合预期。
- 生成长会话时，TS CLI 在第 90 轮左右发给模型的请求里，最后一条用户消息不再是用户输入（推测是 runtime 追加的用户角色上下文）；mock 原来只看最后一条用户消息，因此答错。本次只改了 mock（从后往前找带提示词的用户消息），没有深究，也没有用 Rust runtime 驱动同样的 90 轮对照。

## 方法

### 三种配置

三种配置用同一份 Electron（41.0.3）、同一份 Main / preload / Host / scheduler / Renderer 生产构建、同一个本地 mock 模型与同一组夹具，只换 Host 与 runtime：

| 配置 | Host                                                                           | runtime                                                                                                  | 代表什么                        |
| ---- | ------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------- | ------------------------------- |
| A    | Node Host（Electron utility 进程，标题 `zcode-host-local-1`）                  | TS CLI，每个 workspace 一个子进程（Electron-as-Node 运行 `zcode.cjs`）；另有 Node scheduler utility 进程 | 今天发布的组合                  |
| B    | Rust Host（release 构建，`ZCODE_DESKTOP_HOST=rust`、`ZCODE_RUST_HOST_BINARY`） | Rust runtime 在 Host 进程内，每个 workspace 一个实例                                                     | 目标                            |
| C    | Node Host                                                                      | Rust CLI 子进程（release，`ZCODE_AGENT_SERVER_COMMAND` + `ZCODE_AGENT_SERVER_RUNTIME=zcode-cli-rust`）   | 区分 Host 与 runtime 各自的贡献 |

构建（`node scripts/rust-host/perf/app-perf.mjs build`）：

- Main / preload / Host / scheduler：`tsup`，Renderer：`vite build`，都是 `NODE_ENV=production`（`packages/desktop/scripts/run-production-build.mjs`，与打包链同一个脚本：压缩、无 sourcemap）。Renderer 从 `out/renderer/index.html` 以 `file://` 加载，不经 Vite 开发服务器（`ELECTRON_RENDERER_URL` 置空）。App 进程树设 `ZCODE_ENV=production` 与 `NODE_ENV=production`。
- TS CLI：`scripts/build-desktop-agent-cli.mjs` 产出的单文件 bundle `apps/zcode-cli/packages/cli/dist/zcode.cjs`（16.0 MB），打包链（`prepare-agent-node-bundle.mjs`）调用的是同一个脚本，再把同一个文件复制到 `resources/glm/zcode.cjs`。开发检出里 Node Host 经 `resolveBundledWorkspaceZCodeAgentCommand` 找到 `dist/zcode.cjs`，打包态经 `resolveElectronRuntimeZCodeAgentCommand` 找到 `resources/glm/zcode.cjs`，两者都用 Electron 自带的 Node（`ELECTRON_RUN_AS_NODE=1`）运行同一份 JS。字节码（`build-desktop-agent-bytecode.mjs`）只在显式设 `ZCODE_DESKTOP_AGENT_BYTECODE=1` 的开发实验里使用，打包态的解析不走它，所以发布的是 JS bundle，本次测量与之一致。
- Rust：`cargo build --release`（`lto = "thin"`、`strip = true`），`zcode-host` 与 `zcode-cli-rust` 都从 `d5ff797` 构建。没有用调试构建。

做不到与发布一致的部分及其可能影响：

- 没有打包：没有 ASAR、没有代码签名、App 从外接 SSD 上的仓库目录运行。Main 与 Host 的模块从散文件加载，ASAR 通常能少一些文件打开开销；签名与公证只影响首次启动。这些对三种配置相同，主要影响冷启动的绝对值。
- Rust Host 的开关只在未打包时生效（`isRustDesktopHostEnabled`），B 本来就只能在开发检出里测。
- 开发检出里远端资源走仓库里的 mock-cdn（`resolveRemoteAssetDirs`），打包态走 CDN 缓存；测量不涉及远端 workspace。

### 隔离与驱动

脚本在 `scripts/rust-host/perf/`，复用桌面 E2E harness（`scripts/rust-host/e2e/`，本次把 mock 模型、离线控制面与 profile 播种从 `e2e-desktop.mjs` 抽到 `e2e/profile.mjs` 共用）：每次运行一个全新的 Electron 实例，临时 HOME（整个进程树的 `HOME`、数据目录、Electron userData 都在临时目录），私有 CDP 端口，本地 OpenAI 兼容 mock 模型，ZCode 控制面指向一律 404 的本地桩，遥测端点置空。不读写 `~/.zcode`，不访问外部服务。Main 带 `--inspect`（harness 用它枚举 webContents），Renderer 经 CDP 由 Playwright 驱动，三种配置相同。Electron 的终端输出写到临时目录的文件里。

profile：8 个本地 workspace（各是一个已提交的 git 仓库）都在侧栏，与多项目的老用户相同；启动时 Main 只预热活动的那一个。

一条命令跑一个配置的一个 suite：

```sh
node scripts/rust-host/perf/app-perf.mjs run --config A|B|C --suite startup|stream|history
node scripts/rust-host/perf/app-perf.mjs batch --out <dir> --reps 3   # 交错运行全部
node scripts/rust-host/perf/app-perf.mjs report --out <dir>           # 汇总
```

### 进程归属与读数

从 harness 拉起的 Electron Main 向下遍历进程树（`ps -o pid,ppid,rss,time,command`），每个进程归一个角色：

| 角色      | 判定                                                                               |
| --------- | ---------------------------------------------------------------------------------- |
| main      | harness 启动的 Electron Main                                                       |
| renderer  | 主窗口的 Renderer（Main 里加载 `/renderer/index.html` 的 webContents 的 OS pid）   |
| gpu       | `--type=gpu-process`                                                               |
| helper    | 其他 Electron 辅助进程（network service 等 utility）                               |
| host      | Node Host（标题 `zcode-host-local-<n>`）或 Rust Host（Main 的子进程 `zcode-host`） |
| scheduler | 其余以 Node 运行的 utility 进程（Node scheduler，`zcode-cron-scheduler`）          |
| runtime   | CLI 子进程：TS CLI（标题 `zcode-cli`）或 `zcode-cli-rust`                          |
| mcp       | Host / runtime 下的 MCP 服务进程（A 里 TS CLI 起的 `zcode-node-repl-mcp`）         |
| tool      | Host / runtime 下的其他子进程（Bash、git、存储准备等）                             |

内存同时报 `ps` 的 RSS 和 macOS physical footprint（`footprint -j`，活动监视器的“内存”）。macOS 的 RSS 含共享页与分配器已释放、可复用的页，内存紧张时又会被压缩或换出而变小，同一状态两次读数可以差很多；footprint 是“用户付出”的数字，结论以它为准。CPU 是窗口首尾累计 CPU 时间之差除以墙钟，单核百分比（10 核机器上限 1000%）。窗口内退出的短命进程（Bash 等）只计到最后一次采样，工具负载的 CPU 因此偏低。

### suite

- **startup**：启动 → composer 真实可点（hit-test）；启动 → Host 数据库就绪（Main 日志 `[database-startup] terminal … "status":"ready"` 的时间戳）；启动 → 发送键可用；首条消息 Enter → 首段出现在 DOM、→ 回复完整（40 段、每段 20 ms）。之后在第 2–8 个 workspace 各新建一个会话、完成一轮；N = 1、4、8 时静置 35 s（Renderer 离开会话后保温 30 s 才释放订阅）再取一次分角色快照。最后正常退出。
- **stream**：启动后静置 30 s，取空闲快照（只有活动 workspace 预热了 runtime，没有会话）。1 路：ws1 一个 1500 段（20 ms 一段，约 30 s）的流，进入稳定输出后采样 20 s，同时每秒在 composer 里敲一个字符。4 路：ws2–ws4、ws1 各起一个 2000 段的流，ws1 可见；采样窗口内先敲 12 个字符，再依次点侧栏切到另外三个会话并切回。然后工具负载一轮。最后正常退出。
  - 长任务：Renderer 的 `PerformanceObserver("longtask")`（≥ 50 ms）。
  - 按键延迟：keydown 事件时间戳 → 字符出现在 composer 之后的下一帧。
  - 切换延迟：click 事件时间戳 → 目标会话的首段标记出现在可见文本（`innerText`）之后的下一帧。
  - 增量端到端延迟：mock 每 10 段写一个标记词并记下写出时刻，页面记下它首次出现在 DOM 的时刻（每帧检查一次），两者之差；harness、mock 与页面用同一个系统时钟。
  - 工具负载：模型先调用 Bash `seq 1 200000`，再 Read 一个 10 万行、3.7 MB 文件的前 2000 行（显式 `limit`），最后回复 40 段。
- **history**：一个 120 轮的长会话（约 2.3 KB Markdown 回复一轮，每 10 轮带一次 Read；用配置 A 驱动 mock 生成，会话库与 tasks-index 由 Node 与 Rust 共用），三种配置复制同一份 HOME 冷启动。点开侧栏里的会话 → 最后一轮的标记出现在 DOM；再把消息列表滚到顶 → 第一轮可见；之后静置 10 s 取快照。最后正常退出。
- **退出**：SIGTERM 只发给 Main（与 Dock 退出同一条 `before-quit` 路径），计时到退出前的 Main 及其全部后代都不在了；同时断言 Main 走完退出流程、没有残留进程（harness 的 `assertCleanQuit`）。

正确性：每次运行检查每条回复的全部标记按序出现（流式窗口里的 4 路在窗口结束后逐个切过去确认末段），工具负载的两个工具结果都回到了模型，历史会话能滚到第一轮，退出干净。任何一项不满足，这次运行作废重跑。本次 27 次测量运行全部有效。

### 机器与负载

Apple M1 Max（10 核）、64 GB，macOS 15.7.7，Electron 41.0.3，Node 24.14.0（harness），rustc 1.95.0。仓库在外接 SSD 上。测量时间 2026-10-10 13:23–14:22（本地时间），机器上同时有其他会话的编译与测试任务。

每次运行的开始与结束都记了 1 分钟 load average：27 次运行的中位数 21.6，范围 5.7–38.9（逐次数值见附录）。第 1、2 轮（13:23–13:50）大多在 15–39，第 3 轮（14:09–14:22）大多在 6–21；三轮的读数没有随 load 系统性变化（例如 B 的空闲合计 440 / 491 / 460 MiB，A 的 4 路流式 CPU 52.2 / 45.6 / 52.1%），但时间类指标的长尾（退出、Host 就绪）集中在 load 较高的运行里。

预热：构建之后每个配置都先跑过几次调试性的运行（不计入），批量运行本身没有再单独预热。

## 偏差与没有测量的内容

已知偏差：

- 开发检出，不是安装包：没有 ASAR、没有签名、从外接 SSD 加载散文件（见“三种配置”）。三种配置相同，主要影响冷启动的绝对值。
- Electron 自身的部分（约 400–490 MiB）三种配置相同。所以整个 App 的相对节省比只看 runtime 的数字小：runtime 单独对比时 Rust 的空闲 RSS 是 Node 的 4%（[09-25 报告](rust-performance-2026-09-25.md)），整个 App 空闲 footprint 是 53%。
- 只有 macOS / arm64。Windows 与 Linux 的进程模型（尤其 Windows 上子进程的开销）不同，没有测。
- 模型是本地回环上的 mock：没有网络延迟、没有真实供应商的分块节奏；20 ms 一段比真实模型的输出密。
- harness 给 Main 开了 `--inspect`，Renderer 接着 CDP 与 Playwright，页面里装了探针（MutationObserver 与每帧一次的 DOM 文本检查）；三种配置相同，但都会让 Main 与 Renderer 的 CPU 偏高一点。
- C 不完全等于 npm 启动器（`apps/zcode-cli-rust/npm`）：启动器让未修改的 v3.14.3 Host 为每个 workspace 先起一个 Electron-as-Node 的 `bridge.cjs`，再由它起 Rust CLI；本次 C 由 Node Host 直接启动 Rust CLI，没有这层 bridge 进程，是启动器组合的下限。
- 每项只有 3 次，标 † 的指标 3 次之间差别超过中位数的一半，只能看量级。短命的工具进程（Bash）的 CPU 没有计入。
- footprint 在窗口里每 2 s 读一次，峰值可能漏掉更短的尖峰。
- 长会话由 A 生成；Rust runtime 读 Node 的会话库是既定的共用格式（[rust-m11-node-storage](../specs/rust-m11-node-storage.md)），没有另用 Rust 生成一份对照。

没有测量：

- 10 分钟以上的长时间运行与内存增长斜率（按要求本次不做 soak）。
- 远端（SSH / WSL）workspace、手机远控、多窗口。
- MCP 与插件较多的 profile、真实账号与供应商。
- 打包后的安装包、安装体积、首次启动（签名校验与解包）、磁盘 IO、耗电。
- Renderer 侧的帧率与 GPU 合成开销（只量了长任务与交互延迟）。

## 附录：逐次读数

时间为本地时间，load 为 1 分钟 load average；读数单位 ms 与 MiB（footprint 合计）。完整的 JSON（含每个进程的明细）由 `app-perf.mjs batch` 写在输出目录的 `runs/`，汇总由 `app-perf.mjs report` 生成。

| 运行        | 开始  | load 开始 / 结束 | 读数                                                                                |
| ----------- | ----- | ---------------- | ----------------------------------------------------------------------------------- |
| startup-A-1 | 13:23 | 16.3 / 7.8       | 可用 3783，首段 740，N1 / N4 / N8 838 / 1471 / 2180，退出 2480                      |
| startup-B-1 | 13:25 | 7.7 / 15.3       | 可用 2085，首段 351，N1 / N4 / N8 466 / 516 / 535，退出 235                         |
| startup-C-1 | 13:27 | 15.3 / 24.8      | 可用 2587，首段 176，N1 / N4 / N8 602 / 721 / 810，退出 1413                        |
| startup-B-2 | 13:36 | 31.4 / 34.7      | 可用 1682，首段 188，N1 / N4 / N8 460 / 511 / 535，退出 248                         |
| startup-C-2 | 13:38 | 34.7 / 35.7      | 可用 2231，首段 199，N1 / N4 / N8 604 / 717 / 799，退出 1243                        |
| startup-A-2 | 13:40 | 34.0 / 38.9      | 可用 4078，首段 595，N1 / N4 / N8 849 / 1479 / 2203，退出 1412                      |
| startup-C-3 | 14:09 | 7.8 / 5.9        | 可用 3351，首段 211，N1 / N4 / N8 610 / 716 / 753，退出 1419                        |
| startup-A-3 | 14:11 | 5.7 / 11.7       | 可用 4108，首段 790，N1 / N4 / N8 848 / 1491 / 2283，退出 1502                      |
| startup-B-3 | 14:13 | 11.7 / 7.9       | 可用 1790，首段 274，N1 / N4 / N8 457 / 514 / 532，退出 244                         |
| stream-A-1  | 13:29 | 24.2 / 32.5      | 可用 4210，空闲 868，1 路 CPU 40.9%，4 路 CPU 52.2%、均值 1700，工具 1463，退出 891 |
| stream-B-1  | 13:31 | 30.4 / 24.4      | 可用 1814，空闲 440，1 路 CPU 39.1%，4 路 CPU 51.7%、均值 666，工具 1038，退出 246  |
| stream-C-1  | 13:33 | 24.4 / 18.4      | 可用 2110，空闲 582，1 路 CPU 38.3%，4 路 CPU 47.5%、均值 822，工具 1232，退出 1061 |
| stream-B-2  | 13:42 | 38.9 / 30.3      | 可用 1792，空闲 491，1 路 CPU 39.7%，4 路 CPU 53.0%、均值 664，工具 1053，退出 240  |
| stream-C-2  | 13:44 | 30.3 / 22.2      | 可用 2461，空闲 582，1 路 CPU 39.1%，4 路 CPU 48.8%、均值 821，工具 1202，退出 728  |
| stream-A-2  | 13:46 | 22.2 / 30.8      | 可用 3664，空闲 866，1 路 CPU 39.7%，4 路 CPU 45.6%、均值 1733，工具 1650，退出 897 |
| stream-C-3  | 14:15 | 8.6 / 20.6       | 可用 3000，空闲 587，1 路 CPU 39.7%，4 路 CPU 46.7%、均值 861，工具 1156，退出 761  |
| stream-A-3  | 14:17 | 20.6 / 18.9      | 可用 4100，空闲 870，1 路 CPU 39.8%，4 路 CPU 52.1%、均值 1717，工具 1460，退出 897 |
| stream-B-3  | 14:19 | 18.9 / 10.0      | 可用 1888，空闲 460，1 路 CPU 37.7%，4 路 CPU 53.8%、均值 662，工具 1060，退出 240  |
| history-A-1 | 13:35 | 18.4 / 24.2      | 可用 3518，点开 896，滚到顶 81，打开后 1131，退出 2724                              |
| history-B-1 | 13:35 | 24.2 / 29.7      | 可用 1635，点开 275，滚到顶 81，打开后 737，退出 237                                |
| history-C-1 | 13:35 | 29.7 / 31.4      | 可用 2059，点开 295，滚到顶 72，打开后 873，退出 1869                               |
| history-B-2 | 13:48 | 30.8 / 26.5      | 可用 1721，点开 250，滚到顶 76，打开后 732，退出 253                                |
| history-C-2 | 13:48 | 24.7 / 21.6      | 可用 2008，点开 247，滚到顶 74，打开后 879，退出 396                                |
| history-A-2 | 13:49 | 21.6 / 17.8      | 可用 3468，点开 779，滚到顶 74，打开后 1163，退出 423                               |
| history-A-3 | 14:21 | 9.6 / 9.3        | 可用 4082，点开 654，滚到顶 78，打开后 1155，退出 414                               |
| history-C-3 | 14:21 | 10.0 / 9.6       | 可用 2196，点开 255，滚到顶 91，打开后 856，退出 407                                |
| history-B-3 | 14:22 | 9.3 / 9.2        | 可用 1849，点开 258，滚到顶 84，打开后 731，退出 248                                |
