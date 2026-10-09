#!/usr/bin/env node
// rust/ workspace（基础 crate）的测试入口；应用 workspace 的测试见 test:zcode-cli-rust 与 test:rust-host。
import { spawn } from "node:child_process";
import { resolve } from "node:path";
import { requireCargoTools } from "./cargo-tools.mjs";

const root = resolve(import.meta.dirname, "../..");

await requireCargoTools(["nextest"]);
await new Promise((resolveRun, rejectRun) => {
  const child = spawn(
    "cargo",
    ["nextest", "run", "--locked", "--workspace", "--manifest-path", "rust/Cargo.toml"],
    { cwd: root, stdio: "inherit" },
  );
  child.on("error", rejectRun);
  child.on("exit", (code) =>
    code === 0 ? resolveRun() : rejectRun(new Error(`cargo nextest exited with ${code}`)),
  );
});
