# Rust / Node 同名工具描述逐字符对齐

## 范围

仅修改发给模型的工具 `description` 字符串及其拼接，Node 当前源码为唯一文本来源。保留空格、换行、标点、Unicode 和顺序，不做 trim 或归一化。输入 schema、工具执行、能力开关、权限、profile 发现、session 调度和请求生命周期均不变。

对照相同输入和已支持的能力分支：`embeddedSearchEnabled:false`、`dynamicWorkflowEnabled:false`，Bash 默认 timeout 120000 / 600000 ms。Node 默认 runtime 的 embedded search 分支、Bash cwd/profile 执行差异、profile 发现顺序差异不在本次修复范围；文本相等不表示这些能力已实现。

## 文本所有者与拼接

- Node ToolEntry / description factory 持有原文；现有生成脚本导出 Rust 文本资产，`--check` 阻止漂移。
- Rust tools 层只选择静态描述；Read 的普通/PDF 两份全文从 Node 生成，沿用原来的 supportsPdf 判断。PDF 行位置由 Node 决定。
- Rust domain 只格式化 Agent 描述目录：内置顺序、同名替换、Tools 后缀与 Node 相同；不修改传入 profile，不改变发现或执行策略。已有 app 调用点将目录替换进原描述，不再附加第二份目录。
- WebSearch 沿用现有月份填充。EnterPlanMode / ExitPlanMode 沿用现有 Node 生成资产。
- 无新增状态、IO 或异步边界。生成失败或严格文本比较失败即验收失败，不用运行时兜底掩盖漂移。

## 验收

1. 全部 18 个同名内置工具的 description 与当前 Node 导出严格相等，同时检查 UTF-8 字节；已有 schema 保持不变。
2. Read 普通/PDF 分支完整文本相等，PDF 行不重复，位置与 Node 相同。
3. Agent 相同 profile 输入覆盖内置、自定义 Explore、空/缺省/通配 tools、disallowed 名称与别名、重复名称、输入顺序、Unicode 和占位符样文本；只影响输出描述。
4. 捕获真实 Rust 模型请求，验证运行时 Agent 目录没有二次追加，Read PDF 描述正确。
5. 生成资产检查、针对性测试、Rust 格式/边界检查、仓库 typecheck 和 lint 报告真实结果。

## 验证记录（2026-10-11）

- 修改前：新增的真实请求测试复现 Agent 目录追加差异。
- 修改后：`zcode-cli-rust-tool-prompts.test.ts`、`zcode-cli-rust-tool-parity.test.ts`、`zcode-cli-rust-subagents.test.ts` 合计 11/11 通过，含 18 个工具的严格文本比较。
- 首轮扩展回归曾有一项 `proto.staleRevision`；该项单独重跑和整组最终重跑均通过，未修改调度代码。
- 工具资产生成器 `--check`、根目录 `pnpm typecheck`、测试 tsconfig 类型检查、`pnpm lint` 均通过；全库 lint 为 0 errors / 70 个已有 warnings，改动脚本与测试定点 lint 为 0 warnings。
- Rust workspace 全 targets Clippy（`-D warnings`）、Rust/改动文本格式检查、Rust 边界检查和架构检查通过；架构新违规为 0。
