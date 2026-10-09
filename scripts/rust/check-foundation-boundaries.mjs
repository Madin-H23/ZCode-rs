#!/usr/bin/env node
// rust/ 下基础 crate 的边界（docs/specs/rust-u0-foundation.md §3.1）：
// 不依赖任何应用 crate；单个文件不超过 400 行（与 CLI 相同的上限）。
import { readdir, readFile } from "node:fs/promises";
import { relative, resolve } from "node:path";

const root = resolve(import.meta.dirname, "../../rust/crates");
const failures = [];

async function walk(directory) {
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = resolve(directory, entry.name);
    if (entry.isDirectory()) {
      await walk(path);
      continue;
    }
    const file = relative(root, path).replaceAll("\\", "/");
    const source = await readFile(path, "utf8");
    if (entry.name === "Cargo.toml" && /^\s*zcode-(?:cli|host)-[\w-]*\s*[.=]/m.test(source)) {
      failures.push(`${file}: depends on an application crate`);
    }
    if (!path.endsWith(".rs")) continue;
    if (source.split("\n").length > 400) failures.push(`${file}: exceeds 400 lines`);
    if (/\bzcode_(?:cli|host)_\w+\s*::/.test(source.replace(/\/\/[^\n]*/g, ""))) {
      failures.push(`${file}: imports an application crate`);
    }
  }
}

await walk(root);
if (failures.length) {
  for (const failure of failures) console.error(failure);
  process.exitCode = 1;
} else console.log("Rust foundation boundaries: OK");
