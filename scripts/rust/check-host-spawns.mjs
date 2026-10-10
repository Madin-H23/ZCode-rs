#!/usr/bin/env node
// Host 的并发规则（docs/specs/rust-u2-task-scope.md §2 第 5 条、§6.5），扫描 apps/zcode-host/crates 下全部
// crate 的非测试代码：
// - 任务只经 zcode-async 的 Spawner / owned / blocking 启动：不得直接 tokio::spawn / spawn_blocking /
//   JoinSet，也不得取运行时句柄自行 spawn；
// - 通道都有界：不得使用 tokio 的无界 mpsc（unbounded_channel / UnboundedSender / UnboundedReceiver）；
// - 不持 tokio 锁跨 .await：clippy 的 await_holding_invalid_type 已是 deny，这里禁止用 allow / expect 绕过
//   （测试代码同样禁止）。
// clippy.toml 的 disallowed-methods 由三个 Rust workspace 共用，runtime（apps/zcode-cli-rust）还有直接的
// spawn 与无界通道，所以这两条暂由本脚本强制（理由见 §6.5）。
import { readdir, readFile } from "node:fs/promises";
import { relative, resolve } from "node:path";
import { SPAWN, findSpawns } from "./task-spawns.mjs";

const crates = resolve(import.meta.dirname, "../../apps/zcode-host/crates");
const spawn = new RegExp(`${SPAWN.source}|\\bHandle\\s*::\\s*(?:try_)?current\\b`);
const unbounded =
  /\bunbounded_channel\b|\bmpsc\s*::\s*unbounded\b|\bUnbounded(?:Sender|Receiver)\b/;
const awaitHolding = /\b(?:allow|expect)\s*\(\s*clippy\s*::\s*await_holding_invalid_type\b/;

async function rustFiles(directory) {
  const files = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = resolve(directory, entry.name);
    if (entry.isDirectory()) {
      if (entry.name !== "target") files.push(...(await rustFiles(path)));
    } else if (path.endsWith(".rs")) files.push(path);
  }
  return files;
}

const failures = [];
for (const file of await findSpawns(crates, { base: crates, pattern: spawn })) {
  failures.push(`${file}: starts a task outside zcode-async (use a Spawner, owned or blocking)`);
}
for (const file of await findSpawns(crates, { base: crates, pattern: unbounded })) {
  failures.push(
    `${file}: uses an unbounded channel (use a bounded one and document what happens when it is full)`,
  );
}
for (const path of await rustFiles(crates)) {
  const code = (await readFile(path, "utf8")).replace(/\/\/[^\n]*/g, "");
  if (awaitHolding.test(code)) {
    const file = relative(crates, path).replaceAll("\\", "/");
    failures.push(
      `${file}: allows await_holding_invalid_type (shrink the critical section, use an owner task, or a std Mutex without awaits)`,
    );
  }
}
if (failures.length) {
  for (const failure of failures) console.error(failure);
  process.exitCode = 1;
} else
  console.log("Rust Host concurrency (task spawns, bounded channels, no locks across await): OK");
