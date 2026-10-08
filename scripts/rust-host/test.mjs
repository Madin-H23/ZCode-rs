#!/usr/bin/env node
// Rust Host 的测试入口（docs/specs/rust-host-r0.md §6）：契约清单一致性、cargo 测试、
// 以及用真实 TS 客户端连接构建出的 zcode-host。
import { spawn } from "node:child_process";
import { resolve } from "node:path";

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
// 扩展服务的对拍 fixture 由真实 TS 服务生成（docs/specs/rust-host-extensions.md §6）。
await run(process.execPath, [
  "--import",
  "tsx",
  "scripts/rust-host/extensions-fixtures.ts",
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
await run("cargo", ["test", "--manifest-path", manifest]);
await run("cargo", ["build", "--manifest-path", manifest]);

// 用 Renderer 实际使用的客户端连接真实二进制；测试放在 client 包内，按公开入口导入 @zcode/shared。
const tests = ["packages/client/test/rustHostConnection.test.ts"];
await run(process.execPath, ["--import", "tsx", "--test", ...tests], {
  ...process.env,
  ZCODE_RUST_HOST_BINARY: binary,
});
