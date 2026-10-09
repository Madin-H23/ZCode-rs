#!/usr/bin/env node
// 所有 Rust workspace 的静态检查（docs/specs/rust-u0-foundation.md §2）：边界、fmt、clippy、
// cargo-deny、cargo-shear。CI 的 rust-ci.yml 跑同样的命令，本地一条命令得到相同结论。
import { spawn } from "node:child_process";
import { resolve } from "node:path";
import { requireCargoTools } from "./cargo-tools.mjs";

const root = resolve(import.meta.dirname, "../..");
const workspaces = ["apps/zcode-cli-rust", "apps/zcode-host", "rust"];

function run(command, args, cwd = root) {
  return new Promise((resolveRun, rejectRun) => {
    const child = spawn(command, args, { cwd, stdio: "inherit" });
    child.on("error", rejectRun);
    child.on("exit", (code) =>
      code === 0
        ? resolveRun()
        : rejectRun(new Error(`${command} ${args.join(" ")} exited with ${code}`)),
    );
  });
}

await requireCargoTools(["deny", "shear"]);
await run(process.execPath, ["scripts/check-zcode-cli-rust-boundaries.mjs"]);
await run(process.execPath, ["scripts/rust/check-foundation-boundaries.mjs"]);
await run(process.execPath, ["scripts/rust/check-host-layers.mjs"]);
await run(process.execPath, ["scripts/rust/check-host-spawns.mjs"]);
for (const workspace of workspaces) {
  const manifest = `${workspace}/Cargo.toml`;
  await run("cargo", ["fmt", "--all", "--manifest-path", manifest, "--check"]);
  await run("cargo", [
    "clippy",
    "--locked",
    "--workspace",
    "--all-targets",
    "--manifest-path",
    manifest,
    "--",
    "-D",
    "warnings",
  ]);
  // deny.toml 在仓库根；cargo-deny 从当前目录读取它。
  await run("cargo", ["deny", "--manifest-path", manifest, "check", "--hide-inclusion-graph"]);
  // cargo-shear 按当前目录定位 workspace。
  await run("cargo", ["shear"], resolve(root, workspace));
}
