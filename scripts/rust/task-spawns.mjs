// 任务只经 zcode-async 启动（docs/specs/rust-u2-task-scope.md §2 第 5 条）：在非测试代码里找直接启动
// tokio 任务的写法。runtime（scripts/check-zcode-cli-rust-boundaries.mjs）与 Host 中已迁移的 crate
// （check-host-spawns.mjs）共用这里的扫描。
import { readdir, readFile } from "node:fs/promises";
import { relative, resolve } from "node:path";

export const SPAWN =
  /\btokio\s*::\s*(?:task\s*::\s*)?(?:spawn|spawn_blocking|spawn_local|JoinSet)\b|\btask\s*::\s*(?:spawn|spawn_blocking|spawn_local)\s*\(|(?<![.\w])(?:spawn_blocking|spawn_local)\s*\(|\bHandle\s*::\s*current\s*\(\s*\)\s*\.\s*spawn/;

/** 测试文件：`tests.rs`、`*_tests.rs`，以及 tests/、examples/、benches/ 目录下的文件。 */
function isTestFile(file) {
  return /(?:^|\/)(?:tests|examples|benches)\//.test(file) || /(?:^|\/|_)tests\.rs$/.test(file);
}

/** 去掉 `#[cfg(test)]` 标注的条目（rustfmt 格式：条目以同一缩进的 `}` 结束）。 */
function withoutTestItems(code) {
  const lines = code.split("\n");
  const kept = [];
  for (let index = 0; index < lines.length; index++) {
    const attribute = /^(\s*)#\[cfg\(test\)\]\s*$/.exec(lines[index]);
    if (!attribute) {
      kept.push(lines[index]);
      continue;
    }
    let item = index + 1;
    while (item < lines.length && /^\s*#\[/.test(lines[item])) item++;
    let end = item;
    if (/\{\s*$/.test(lines[item] ?? "")) {
      while (end < lines.length && lines[end] !== `${attribute[1]}}`) end++;
    }
    index = end;
  }
  return kept.join("\n");
}

/**
 * `directory` 下非测试 `.rs` 文件中匹配 `pattern` 的文件，路径相对 `base`（`exceptions` 用同样的相对路径）。
 */
export async function findSpawns(directory, { base, exceptions = new Set(), pattern = SPAWN }) {
  const found = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = resolve(directory, entry.name);
    if (entry.isDirectory()) {
      found.push(...(await findSpawns(path, { base, exceptions, pattern })));
      continue;
    }
    const file = relative(base, path).replaceAll("\\", "/");
    if (!path.endsWith(".rs") || isTestFile(file) || exceptions.has(file)) continue;
    const code = withoutTestItems((await readFile(path, "utf8")).replace(/\/\/[^\n]*/g, ""));
    if (pattern.test(code)) found.push(file);
  }
  return found;
}
