#!/usr/bin/env node
// 所有 Rust workspace 的静态检查（docs/specs/rust-u0-foundation.md §2）：边界、fmt、clippy、
// cargo-deny、cargo-shear。CI 的 rust-ci.yml 跑同样的命令，本地一条命令得到相同结论。
// clippy 覆盖三种构建形态（docs/specs/rust-host-cli-boundary.md §3.3）：只有 CLI（CLI workspace）、
// 合并（Host workspace 缺省 feature）、只有 Host（zcode-host --no-default-features）。
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
await run(process.execPath, ["scripts/rust/check-host-cli-boundary.mjs"]);
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
  if (workspace === "apps/zcode-host") {
    // “只有 Host”形态：只有这两个 crate 按 feature 分支（runtime-link 的 embedded、zcode-host 的
    // embedded-runtime）；--all-targets 也覆盖只在这个形态编译的 tests/suite/host_only.rs。
    await run("cargo", [
      "clippy",
      "--locked",
      "--manifest-path",
      manifest,
      "-p",
      "zcode-host-runtime-link",
      "-p",
      "zcode-host",
      "--no-default-features",
      "--all-targets",
      "--",
      "-D",
      "warnings",
    ]);
  }
  // deny.toml 在仓库根；cargo-deny 从当前目录读取它。
  await run("cargo", ["deny", "--manifest-path", manifest, "check", "--hide-inclusion-graph"]);
  // cargo-shear 按当前目录定位 workspace。
  await run("cargo", ["shear"], resolve(root, workspace));
}
