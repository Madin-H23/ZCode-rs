#!/usr/bin/env node
// Rust Host 的测试入口（docs/specs/rust-host-r0.md §6）：契约清单一致性、cargo 测试、
// 以及用真实 TS 客户端连接构建出的 zcode-host。
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { resolve } from "node:path";
import { requireCargoTools } from "../rust/cargo-tools.mjs";

const root = resolve(import.meta.dirname, "../..");
const manifest = resolve(root, "apps/zcode-host/Cargo.toml");
const binary = resolve(
  root,
  "apps/zcode-host/target/debug",
  process.platform === "win32" ? "zcode-host.exe" : "zcode-host",
);

function run(command, args, env = process.env) {
  return new Promise((resolveRun, rejectRun) => {
    const child = spawn(command, args, { cwd: root, env, stdio: "inherit" });
    child.on("error", rejectRun);
    child.on("exit", (code) =>
      code === 0
        ? resolveRun()
        : rejectRun(new Error(`${command} ${args.join(" ")} exited with ${code}`)),
    );
  });
}

await run(process.execPath, ["scripts/rust-host/contract-inventory.mjs", "--check"]);
// tasks-index 的账本 checksum 与对拍 fixture 必须与当前 TS 实现一致（Rust 测试 include_str! 它们）。
await run(process.execPath, [
  "--import",
  "tsx",
  "scripts/rust-host/tasks-index-fixtures.ts",
  "--check",
]);
// agent 子系统发给 Renderer 的事实校验与 TS zod schema 对拍（agent/fixtures/schema-cases.json）。
await run(process.execPath, [
  "--import",
  "tsx",
  "scripts/rust-host/agent-schema-fixtures.ts",
  "--check",
]);
await run(process.execPath, [
  "--import",
  "tsx",
  "scripts/rust-host/tasks-index-groups-fixtures.ts",
  "--check",
]);
// R6 automation 区域：automation / 闲时任务仓库、cron 计算与 bots 的对拍 fixture（同上，由 TS 实现生成）；
// U4 调度状态机用到的 repo 操作与结算（docs/specs/rust-u4-app-host.md §2.4）。
for (const script of [
  "automation-fixtures.ts",
  "off-peak-fixtures.ts",
  "bots-fixtures.ts",
  "scheduler-fixtures.ts",
]) {
  await run(process.execPath, ["--import", "tsx", `scripts/rust-host/${script}`, "--check"]);
}
// 扩展服务的对拍 fixture 由真实 TS 服务生成（docs/specs/rust-host-extensions.md §6）。
await run(process.execPath, [
  "--import",
  "tsx",
  "scripts/rust-host/extensions-fixtures.ts",
  "--check",
]);
// 跨区域收尾（docs/specs/rust-host-integration.md §6）：反向请求参数的 zod flatten、settings-sync 手动导入。
for (const script of ["zod-flatten-fixtures.ts", "settings-sync-fixtures.ts"]) {
  await run(process.execPath, ["--import", "tsx", `scripts/rust-host/${script}`, "--check"]);
}
// setting.updateDataBaseDir（docs/specs/rust-host-setting.md §6）：path.win32、安装目录守卫、fs.cp 复制与
// 写设置的对拍 fixture（settings/fixtures/data-dir-parity.json）由真实 TS 代码生成。
await run(process.execPath, [
  "--import",
  "tsx",
  "scripts/rust-host/setting-data-dir-fixtures.ts",
  "--check",
]);
await run("cargo", ["fmt", "--all", "--manifest-path", manifest, "--check"]);
await run("cargo", [
  "clippy",
  "--manifest-path",
  manifest,
  "--all-targets",
  "--",
  "-D",
  "warnings",
]);
// 远端 workspace 的测试连接本仓库构建的 zcode-server.cjs（docs/specs/rust-host-remote.md §7）；缺失时先构建。
if (!existsSync(resolve(root, "packages/server/dist/remote/zcode-server.cjs"))) {
  await run("pnpm", ["--filter", "@zcode/server", "build:remote"]);
}
await requireCargoTools(["nextest"]);
await run("cargo", ["nextest", "run", "--workspace", "--manifest-path", manifest]);
// “只有 Host”形态（docs/specs/rust-host-cli-boundary.md §3.3、§3.5）：不链接 CLI 的构建能编译，没有
// `cli` 入口，不给 ZCODE_AGENT_SERVER_COMMAND 时启动 runtime 失败。用单独的 target 目录，不覆盖下面
// 交给 TS 测试的合并形态二进制。zcode-host 的集成测试只有一个二进制 `all`
// （docs/specs/rust-engineering-practices.md §4.2），只在这个形态编译的模块按测试名选出。
await run("cargo", [
  "nextest",
  "run",
  "--manifest-path",
  manifest,
  "--target-dir",
  resolve(root, "apps/zcode-host/target/host-only"),
  "-p",
  "zcode-host-runtime-link",
  "-p",
  "zcode-host",
  "--no-default-features",
  "-E",
  "package(zcode-host-runtime-link) | (binary(all) & test(/^suite::host_only::/))",
]);
await run("cargo", ["build", "--manifest-path", manifest]);

// 用 Renderer 实际使用的客户端连接真实二进制；测试放在 client 包内，按公开入口导入 @zcode/shared。
const tests = [
  "packages/client/test/rustHostConnection.test.ts",
  // Main 发起的 MessagePort attachment（web-remote-replayable）经 WebSocket 桥接（rust-host-tasks.md）。
  "packages/desktop/test/rustHostPortBridge.test.ts",
  // Main 的自动重启策略（纯逻辑，docs/specs/rust-host-resilience.md §2.1）。
  "packages/desktop/test/rustHostRestartPolicy.test.ts",
  // Host 主动关闭连接后 Renderer 的处理：状态码 → 动作、重新接入的次数上限（纯逻辑，§2.5）。
  "packages/desktop/test/hostConnectionClose.test.ts",
  // 一个 App 一个 Rust Host（纯逻辑，docs/specs/rust-u4-app-host.md §2.1、§2.6、§2.8）：
  // 代次与重启、窗口接入、设置写入与 setting-changed 的应用、按 windowId 路由。
  "packages/desktop/test/rustAppHostCore.test.ts",
  "packages/desktop/test/rustAppHostSettings.test.ts",
  // owner 的接线：拉起失败按意外退出处理、窗口级动作在 endpoint 交付之后（Electron 与进程层用 mock）。
  "packages/desktop/test/rustAppHostWiring.test.ts",
  // 启动状态中继：App 级 Host 退出后不向新页面回放上一代的失败；Node 窗口 Host 不变。
  "packages/desktop/test/databaseStartupRelayExit.test.ts",
  // Node 路径 reload 回退期间同一窗口两个 Host：一个 Host 退出只清它自己的连接与 route。
  "packages/desktop/test/desktopRemoteSessionsHostExit.test.ts",
  // U4 新增控制消息的严格校验（§4.1）。
  "packages/shared/test/rustU4ControlMessages.test.ts",
];
// 一部分 Main 单测需要替换 `electron` 等模块（node:test 的 mock.module）。
await run(
  process.execPath,
  ["--import", "tsx", "--experimental-test-module-mocks", "--test", ...tests],
  {
    ...process.env,
    ZCODE_RUST_HOST_BINARY: binary,
  },
);
