#!/usr/bin/env node
// Host 与 CLI 的边界（docs/specs/rust-host-cli-boundary.md §3.1、§5）：
//
// 1. CLI（apps/zcode-cli-rust）的任何 crate 都不引用 Host（zcode-host*）。
// 2. Host 中只有 runtime-link 与二进制 zcode-host 可以依赖 CLI，只依赖顶层 crate zcode-cli-rust，
//    而且是可选的普通依赖（关掉 feature 就是“只有 Host”形态）。
// 3. Host 源码只用 zcode_cli_rust 的公开入口（ALLOWED），不引用 zcode-cli-* 的内部 crate。
// 4. rust/ 下的共用 crate 不依赖任何一方。
// 5. “只有 Host”形态（zcode-host --no-default-features）的普通依赖树里没有 zcode-cli-* 包。
//
// 清单用 `cargo metadata --no-deps` 读取（真实包名、依赖种类、可选与路径），源码去掉注释后按文本扫描。
import { execFile } from "node:child_process";
import { readdir, readFile } from "node:fs/promises";
import { relative, resolve, sep } from "node:path";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);
const root = resolve(import.meta.dirname, "../..");
const CLI = resolve(root, "apps/zcode-cli-rust");
const HOST = resolve(root, "apps/zcode-host");
const SHARED = resolve(root, "rust");
/** 可以依赖 CLI 的 Host crate（目录名）。 */
const CLI_DEPENDENTS = new Set(["runtime-link", "zcode-host"]);
/** Host 可以使用的 zcode_cli_rust 路径：等于其中一项，或以“该项::”开头。 */
const ALLOWED = [
  "embed",
  "runtime::RuntimeOptions",
  "serve::AppServerOptions",
  "logging",
  "log_retention",
  "cli",
];

const failures = [];
const show = (path) => relative(root, path).replaceAll(sep, "/");

async function cargo(args) {
  const { stdout } = await execFileAsync("cargo", args, { cwd: root, maxBuffer: 64 << 20 });
  return stdout;
}

/** workspace 成员：`{name, dir, dependencies}`（dependencies 来自 cargo metadata）。 */
async function members(workspace) {
  const metadata = JSON.parse(
    await cargo([
      "metadata",
      "--format-version",
      "1",
      "--no-deps",
      "--locked",
      "--manifest-path",
      resolve(workspace, "Cargo.toml"),
    ]),
  );
  return metadata.packages.map((item) => ({
    name: item.name,
    dir: resolve(item.manifest_path, ".."),
    dependencies: item.dependencies,
  }));
}

const inside = (path, directory) => path === directory || path.startsWith(directory + sep);

/** 一个包的 `.rs` 文件；workspace 根同时是包时不进入其他成员（crates/）与 target/。 */
async function sources(directory, skip = new Set()) {
  const files = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = resolve(directory, entry.name);
    if (entry.isDirectory()) {
      if (entry.name === "target" || entry.name === "node_modules" || skip.has(path)) continue;
      files.push(...(await sources(path, skip)));
    } else if (entry.name.endsWith(".rs")) files.push(path);
  }
  return files;
}

/** 去掉注释（行注释、文档注释、块注释）后的源码。 */
async function code(path) {
  return (await readFile(path, "utf8")).replace(/\/\*[\s\S]*?\*\//g, "").replace(/\/\/[^\n]*/g, "");
}

const skipWs = (text, at) => {
  while (at < text.length && /\s/.test(text[at])) at++;
  return at;
};

/**
 * 从 `at` 起读一棵 use 树或一条表达式路径（`a::b`、`a::{b, c::d}`、`*`），返回展开后的路径与结束位置。
 * `self` 归并到父路径，`as` 别名不影响路径本身。
 */
function readTree(text, at) {
  at = skipWs(text, at);
  if (text[at] === "{") {
    const paths = [];
    at++;
    for (;;) {
      at = skipWs(text, at);
      if (text[at] === "}" || at >= text.length) return { paths, end: at + 1 };
      const item = readTree(text, at);
      paths.push(...item.paths);
      at = skipWs(text, item.end);
      const alias = /^as\s+[A-Za-z_]\w*/.exec(text.slice(at));
      if (alias) at = skipWs(text, at + alias[0].length);
      if (text[at] === ",") at++;
    }
  }
  if (text[at] === "*") return { paths: ["*"], end: at + 1 };
  const ident = /^[A-Za-z_]\w*/.exec(text.slice(at));
  if (!ident) return { paths: [""], end: at };
  at += ident[0].length;
  const next = /^\s*::\s*/.exec(text.slice(at));
  if (next && /[A-Za-z_{*]/.test(text[at + next[0].length] ?? "")) {
    const rest = readTree(text, at + next[0].length);
    const join = (path) => (path === "" || path === "self" ? ident[0] : `${ident[0]}::${path}`);
    return { paths: rest.paths.map(join), end: rest.end };
  }
  return { paths: [ident[0] === "self" ? "" : ident[0]], end: at };
}

/** 源码中经 `zcode_cli_rust` 引用的路径（`""` 表示单独使用 crate 名，例如 `use zcode_cli_rust as x`）。 */
function cliPaths(text) {
  const paths = [];
  for (const match of text.matchAll(/\bzcode_cli_rust\b/g)) {
    const at = match.index + match[0].length;
    const next = /^\s*::\s*/.exec(text.slice(at));
    paths.push(...(next ? readTree(text, at + next[0].length).paths : [""]));
  }
  return paths;
}

const allowed = (path) =>
  ALLOWED.some((prefix) => path === prefix || path.startsWith(`${prefix}::`));

// ── 规则 1：CLI 不引用 Host ──
for (const member of await members(CLI)) {
  for (const dependency of member.dependencies) {
    if (
      dependency.name.startsWith("zcode-host") ||
      (dependency.path && inside(dependency.path, HOST))
    ) {
      failures.push(`${show(member.dir)}: CLI crate ${member.name} depends on ${dependency.name}`);
    }
  }
  const skip = new Set(member.dir === CLI ? [resolve(CLI, "crates")] : []);
  for (const file of await sources(member.dir, skip)) {
    if (/\bzcode_host\w*/.test(await code(file))) {
      failures.push(`${show(file)}: CLI source references a Host crate`);
    }
  }
}

// ── 规则 2、3：Host 只经 runtime-link 与 zcode-host、只经公开入口依赖 CLI ──
for (const member of await members(HOST)) {
  const directory = relative(resolve(HOST, "crates"), member.dir);
  const mayDepend = CLI_DEPENDENTS.has(directory);
  for (const dependency of member.dependencies) {
    const isCli =
      dependency.name.startsWith("zcode-cli-") || (dependency.path && inside(dependency.path, CLI));
    if (!isCli) continue;
    const where = `${show(member.dir)}: ${member.name} depends on ${dependency.name}`;
    if (!mayDepend) failures.push(`${where} (only ${[...CLI_DEPENDENTS].join(", ")} may)`);
    else if (dependency.name !== "zcode-cli-rust") {
      failures.push(`${where} (only the top-level crate zcode-cli-rust is allowed)`);
    } else if (dependency.kind !== null || !dependency.optional) {
      failures.push(`${where} (must be an optional normal dependency behind a feature)`);
    }
  }
  for (const file of await sources(member.dir)) {
    const text = await code(file);
    const internal = text.match(/\bzcode_cli_(?!rust\b)\w+/);
    if (internal) failures.push(`${show(file)}: uses the CLI-internal crate ${internal[0]}`);
    const paths = cliPaths(text);
    if (paths.length && !mayDepend) {
      failures.push(`${show(file)}: uses zcode_cli_rust outside ${[...CLI_DEPENDENTS].join(", ")}`);
    }
    for (const path of paths.filter((item) => !allowed(item))) {
      failures.push(
        `${show(file)}: uses zcode_cli_rust::${path || "(the crate itself)"}; ` +
          `allowed: ${ALLOWED.join(", ")}`,
      );
    }
  }
}

// ── 规则 4：rust/ 不依赖任何一方 ──
for (const member of await members(SHARED)) {
  for (const dependency of member.dependencies) {
    if (
      /^zcode-(?:cli|host)\b/.test(dependency.name) ||
      (dependency.path && inside(dependency.path, resolve(root, "apps")))
    ) {
      failures.push(`${show(member.dir)}: ${member.name} depends on ${dependency.name}`);
    }
  }
  for (const file of await sources(member.dir)) {
    if (/\bzcode_(?:cli|host)\w*/.test(await code(file))) {
      failures.push(`${show(file)}: shared source references an application crate`);
    }
  }
}

// ── 规则 5：“只有 Host”形态的依赖树 ──
async function tree(features) {
  const output = await cargo([
    "tree",
    "--locked",
    "--manifest-path",
    resolve(HOST, "Cargo.toml"),
    "-p",
    "zcode-host",
    ...features,
    "-e",
    "normal",
    "--prefix",
    "none",
    "--format",
    "{p}",
  ]);
  return new Set(output.split("\n").map((line) => line.trim().split(" ")[0]));
}
try {
  const hostOnly = [...(await tree(["--no-default-features"]))].filter((name) =>
    name.startsWith("zcode-cli-"),
  );
  if (hostOnly.length) {
    failures.push(`zcode-host --no-default-features links CLI crates: ${hostOnly.join(", ")}`);
  }
  // 对照：合并形态必须含 zcode-cli-rust，否则上面的检查读错了依赖树。
  if (!(await tree([])).has("zcode-cli-rust")) {
    failures.push(
      "zcode-host (default features) does not link zcode-cli-rust; the tree check is broken",
    );
  }
} catch (error) {
  failures.push(
    `cargo tree failed: ${
      String(error.stderr ?? error.message)
        .trim()
        .split("\n")[0]
    }`,
  );
}

if (failures.length) {
  for (const failure of failures) console.error(failure);
  process.exitCode = 1;
} else console.log("Rust Host / CLI boundary: OK");
