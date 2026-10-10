// 任务只经 zcode-async 启动（docs/specs/rust-u2-task-scope.md §2 第 5 条）：在非测试代码里找直接启动
// tokio 任务的写法。runtime（scripts/check-zcode-cli-rust-boundaries.mjs）与 Host
// （check-host-spawns.mjs）共用这里的扫描。
import { readdir, readFile } from "node:fs/promises";
import { basename, dirname, relative, resolve } from "node:path";

export const SPAWN =
  /\btokio\s*::\s*(?:task\s*::\s*)?(?:spawn|spawn_blocking|spawn_local|JoinSet)\b|\btask\s*::\s*(?:spawn|spawn_blocking|spawn_local)\s*\(|(?<![.\w])(?:spawn_blocking|spawn_local)\s*\(|\bHandle\s*::\s*current\s*\(\s*\)\s*\.\s*spawn/;

/** 测试文件：`tests.rs`、`*_tests.rs`，以及 tests/、examples/、benches/ 目录下的文件。 */
function isTestFile(file) {
  return /(?:^|\/)(?:tests|examples|benches)\//.test(file) || /(?:^|\/|_)tests\.rs$/.test(file);
}

/** 只在测试中编译的条件：`cfg(test)`、`cfg(all(test, …))`、`cfg(any(test, feature = "test-support"))`。 */
const TEST_CFG =
  /^(\s*)#\[cfg\((?:test|all\(test\b.*|any\(test,\s*feature\s*=\s*"test-support"\))\)\]\s*$/;

/** 去掉只在测试中编译的条目（rustfmt 格式：条目以同一缩进的 `}` 结束）。 */
function withoutTestItems(code) {
  const lines = code.split("\n");
  const kept = [];
  for (let index = 0; index < lines.length; index++) {
    const attribute = TEST_CFG.exec(lines[index]);
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
 * 文件 `path` 里以测试条件声明的外部模块（`#[cfg(test)] mod x;`，可带 `#[path = "…"]`）对应的文件。
 * 这些文件只在测试中编译，即使名字不像测试文件（`tests_commands.rs`、`fake_remote.rs`、`test_support.rs`）。
 */
function declaredTestModules(path, code) {
  const lines = code.split("\n");
  const modules = [];
  for (let index = 0; index < lines.length; index++) {
    if (!TEST_CFG.test(lines[index])) continue;
    let item = index + 1;
    let explicit;
    while (item < lines.length && /^\s*#\[/.test(lines[item])) {
      explicit ??= /^\s*#\[path\s*=\s*"([^"]+)"\]/.exec(lines[item])?.[1];
      item++;
    }
    const declared = /^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;/.exec(lines[item] ?? "");
    if (!declared) continue;
    const directory = dirname(path);
    const owner = basename(path, ".rs");
    const base = ["mod", "lib", "main"].includes(owner) ? directory : resolve(directory, owner);
    if (explicit) modules.push(resolve(directory, explicit));
    else modules.push(resolve(base, `${declared[1]}.rs`), resolve(base, declared[1], "mod.rs"));
  }
  return modules;
}

async function rustFiles(directory) {
  const files = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = resolve(directory, entry.name);
    if (entry.isDirectory()) files.push(...(await rustFiles(path)));
    else if (path.endsWith(".rs")) files.push(path);
  }
  return files;
}

/**
 * `directory` 下非测试 `.rs` 文件中匹配 `pattern` 的文件，路径相对 `base`（`exceptions` 用同样的相对路径）。
 */
export async function findSpawns(directory, { base, exceptions = new Set(), pattern = SPAWN }) {
  const files = await rustFiles(directory);
  const sources = new Map();
  const testModules = new Set();
  for (const path of files) {
    const source = await readFile(path, "utf8");
    sources.set(path, source);
    for (const module of declaredTestModules(path, source)) testModules.add(module);
  }
  const found = [];
  for (const path of files) {
    const file = relative(base, path).replaceAll("\\", "/");
    if (isTestFile(file) || testModules.has(path) || exceptions.has(file)) continue;
    const code = withoutTestItems(sources.get(path).replace(/\/\/[^\n]*/g, ""));
    if (pattern.test(code)) found.push(file);
  }
  return found;
}
