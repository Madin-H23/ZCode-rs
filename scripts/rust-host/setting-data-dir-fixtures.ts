// setting.updateDataBaseDir 的对拍 fixture（docs/specs/rust-host-setting.md §6）。
// 在临时沙盒里用真实 TS 代码执行 setting-data-dir-scenarios.ts 的输入：Node `path.win32` 的 normalize / resolve / join、
// `validateDataBaseDirTarget`（平台固定为 win32，cwd 与 `=X:` 由场景给定）、`copyDataDirectory`（fs.cp）
// 与 `settingService.updateDataBaseDir`，记录返回值或错误（name / code / message）与落盘树（类型、mode、
// 内容、软链目标）以及 setting.json；沙盒根路径统一替换成 `<ROOT>`。
// Rust 侧 settings/data_dir/parity_tests.rs include_str! 嵌入并逐项复现。
// 用法：pnpm exec tsx scripts/rust-host/setting-data-dir-fixtures.ts [--check]
import { execFileSync } from "node:child_process";
import {
  chmod,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  readlink,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, relative, resolve, win32 } from "node:path";
import { format } from "oxfmt";
import {
  copyDataDirectory,
  setDataBaseDir,
  validateDataBaseDirTarget,
} from "../../packages/services/src/paths.js";
import { createSettingService } from "../../packages/services/src/setting/settingService.js";
import {
  CASES,
  type CopyCase,
  type Entry,
  type UpdateCase,
  VALIDATION,
  WIN32_CWD,
  WIN32_DRIVE_CWDS,
  WIN32_INPUTS,
  WIN32_JOINS,
} from "./setting-data-dir-scenarios.js";

const root = resolve(import.meta.dirname, "../..");
const fixturePath = resolve(
  root,
  "apps/zcode-host/crates/settings/src/settings/fixtures/data-dir-parity.json",
);

function errorRecord(error: unknown): Record<string, unknown> {
  const value = error as { name?: unknown; code?: unknown; message?: unknown };
  return {
    name: value.name,
    ...(value.code === undefined ? {} : { code: value.code }),
    message: value.message,
  };
}

async function walkDirs(base: string, visit: (path: string) => Promise<void>): Promise<void> {
  let stat;
  try {
    stat = await lstat(base);
  } catch {
    return;
  }
  if (!stat.isDirectory()) return;
  await visit(base);
  for (const name of await readdir(base)) await walkDirs(join(base, name), visit);
}

/** 建树，再把所有目录设成 0755、普通文件 0644，最后按条目设定 mode（深的先设），与 umask 无关。 */
async function buildTree(sandbox: string, entries: Entry[]): Promise<void> {
  for (const entry of entries) {
    const target = join(sandbox, entry.path);
    if (entry.type === "dir") {
      await mkdir(target, { recursive: true });
      continue;
    }
    await mkdir(dirname(target), { recursive: true });
    if (entry.type === "file") await writeFile(target, entry.content.replaceAll("<ROOT>", sandbox));
    else if (entry.type === "symlink")
      await symlink(entry.target.replaceAll("<ROOT>", sandbox), target);
    else execFileSync("mkfifo", [target]);
  }
  const dirs: string[] = [];
  await walkDirs(sandbox, async (path) => {
    dirs.push(path);
  });
  for (const path of dirs) {
    await chmod(path, 0o755);
    for (const name of await readdir(path)) {
      const child = join(path, name);
      const stat = await lstat(child);
      if (stat.isFile()) await chmod(child, 0o644);
    }
  }
  const withMode = entries
    .filter(
      (entry): entry is Extract<Entry, { mode?: number }> =>
        (entry.type === "dir" || entry.type === "file") && entry.mode !== undefined,
    )
    .sort((left, right) => right.path.split("/").length - left.path.split("/").length);
  for (const entry of withMode) await chmod(join(sandbox, entry.path), entry.mode!);
}

/**
 * 只比较 `.zcode/v2` 子树内的 mode：其外的目录由 `mkdir -p` 创建，`setting.json` 由原子写创建，
 * 两者的 mode 都随 umask 变化，不同机器上不可比。
 */
function modeTracked(rel: string): boolean {
  return /(^|\/)\.zcode\/v2(\/|$)/.test(rel) && !rel.endsWith("/setting.json");
}

async function dumpPath(sandbox: string, path: string, out: unknown[]): Promise<void> {
  const rel = relative(sandbox, path);
  let stat;
  try {
    stat = await lstat(path);
  } catch {
    out.push({ path: rel, type: "missing" });
    return;
  }
  const mode = modeTracked(rel) ? { mode: stat.mode & 0o777 } : {};
  if (stat.isSymbolicLink()) {
    out.push({ path: rel, type: "symlink", target: await readlink(path) });
  } else if (stat.isDirectory()) {
    out.push({ path: rel, type: "dir", ...mode });
    for (const name of (await readdir(path)).sort()) await dumpPath(sandbox, join(path, name), out);
  } else if (stat.isFIFO()) {
    out.push({ path: rel, type: "fifo" });
  } else {
    out.push({ path: rel, type: "file", ...mode, content: await readFile(path, "utf8") });
  }
}

async function runCase(sandbox: string, spec: CopyCase | UpdateCase): Promise<unknown> {
  await buildTree(sandbox, spec.tree);
  const sub = (value: string) => value.replaceAll("<ROOT>", sandbox);
  let outcome: Record<string, unknown> = { ok: true };
  try {
    if (spec.kind === "copy") {
      await copyDataDirectory(join(sandbox, spec.from), join(sandbox, spec.to));
    } else {
      setDataBaseDir(join(sandbox, spec.current));
      const service = createSettingService();
      await service.updateDataBaseDir(spec.newDir === null ? undefined : sub(spec.newDir));
    }
  } catch (error) {
    outcome = { ok: false, error: errorRecord(error) };
  } finally {
    setDataBaseDir(null);
  }
  const after: unknown[] = [];
  for (const path of spec.dump) await dumpPath(sandbox, join(sandbox, path), after);
  return { outcome, after };
}

/** 只读目录与 0555 目录先放开权限，才能删掉沙盒。 */
async function unlock(path: string): Promise<void> {
  await walkDirs(path, (dirPath) => chmod(dirPath, 0o755));
}

function captureWin32(): unknown {
  return {
    cwd: WIN32_CWD,
    driveCwds: WIN32_DRIVE_CWDS,
    paths: WIN32_INPUTS.map((input) => ({
      input,
      normalize: win32.normalize(input),
      resolve: win32.resolve(input),
    })),
    joins: WIN32_JOINS.map((parts) => ({ parts, join: win32.join(...parts) })),
  };
}

/**
 * win32.resolve 读 `process.cwd()` 与 `process.env[\`=X:\`]`。POSIX 上不能设置名字带 `=` 的环境变量，
 * 所以临时换掉整个 `process.env` 对象（path 模块每次调用时才读它）。
 */
function withWin32Process<T>(cwd: string, driveCwds: Record<string, string>, run: () => T): T {
  const savedCwd = process.cwd;
  const savedEnv = process.env;
  process.cwd = () => cwd;
  process.env = { ...savedEnv, ...driveCwds };
  try {
    return run();
  } finally {
    process.cwd = savedCwd;
    process.env = savedEnv;
  }
}

function captureValidation(): unknown {
  return VALIDATION.map((spec) => {
    const driveCwds = Object.fromEntries(
      Object.entries(spec.env).filter(([key]) => key.startsWith("=")),
    );
    const result = withWin32Process(spec.cwd ?? WIN32_CWD, driveCwds, () =>
      validateDataBaseDirTarget(spec.target, {
        platform: spec.platform ?? "win32",
        env: spec.env,
        appInstallDir: spec.appInstallDir ?? null,
      }),
    );
    return { ...spec, result };
  });
}

async function main(): Promise<void> {
  const win32Result = withWin32Process(WIN32_CWD, WIN32_DRIVE_CWDS, captureWin32);
  const validation = captureValidation();

  const savedEnv = { HOME: process.env.HOME, USERPROFILE: process.env.USERPROFILE };
  const savedDesktopHome = process.env.ZCODE_DESKTOP_HOME_DIR;
  const savedLog = console.log;
  const cases: unknown[] = [];
  try {
    // settingService 每次写入都 console.log 完整设置；采集时静音。
    console.log = () => undefined;
    delete process.env.ZCODE_DESKTOP_HOME_DIR;
    for (const spec of CASES) {
      const sandbox = await mkdtemp(join(tmpdir(), "zcode-data-dir-fixtures-"));
      process.env.HOME = join(sandbox, "home");
      process.env.USERPROFILE = join(sandbox, "home");
      try {
        const result = await runCase(sandbox, spec);
        cases.push(
          JSON.parse(
            JSON.stringify({ ...spec, ...(result as object) }).replaceAll(sandbox, "<ROOT>"),
          ),
        );
      } finally {
        await unlock(sandbox);
        await rm(sandbox, { recursive: true, force: true });
      }
    }
  } finally {
    console.log = savedLog;
    process.env.HOME = savedEnv.HOME;
    process.env.USERPROFILE = savedEnv.USERPROFILE;
    if (savedDesktopHome !== undefined) process.env.ZCODE_DESKTOP_HOME_DIR = savedDesktopHome;
  }

  const content = `${JSON.stringify(
    {
      source: "scripts/rust-host/setting-data-dir-fixtures.ts",
      win32: win32Result,
      validation,
      cases,
    },
    null,
    2,
  )}\n`;
  const formatted = await format(fixturePath, content);
  if (formatted.errors.length) throw new Error("Cannot format data-dir-parity.json");
  if (process.argv.includes("--check")) {
    const current = await readFile(fixturePath, "utf8").catch(() => "");
    if (current !== formatted.code) {
      console.error(
        "[setting-data-dir-fixtures] data-dir-parity.json is stale; run pnpm exec tsx scripts/rust-host/setting-data-dir-fixtures.ts",
      );
      process.exitCode = 1;
    } else {
      console.log(`[setting-data-dir-fixtures] up to date: ${CASES.length} cases`);
    }
  } else {
    await mkdir(dirname(fixturePath), { recursive: true });
    await writeFile(fixturePath, formatted.code);
    console.log(`[setting-data-dir-fixtures] wrote: ${CASES.length} cases`);
  }
}

await main();
