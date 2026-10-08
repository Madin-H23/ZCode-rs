// Rust Host 扩展服务的对拍 fixture（docs/specs/rust-host-extensions.md §6）。
// 用真实 TS 服务在临时目录里按 extensions-scenarios.ts 执行调用，把文件树、调用与归一化后的结果
// 写进 apps/zcode-host/crates/services/src/extensions/fixtures/extensions-parity.json；
// Rust 测试 include_str! 后在自己的临时目录里复现并逐项比较。
// 用法：pnpm exec tsx scripts/rust-host/extensions-fixtures.ts [--check]
import { mkdir, mkdtemp, readFile, realpath, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { format } from "oxfmt";
import { CALLS, DUMPS, FILES, type ScenarioCall } from "./extensions-scenarios.js";

const repo = resolve(import.meta.dirname, "../..");
const fixturePath = resolve(
  repo,
  "apps/zcode-host/crates/services/src/extensions/fixtures/extensions-parity.json",
);

function substitute(text: string, root: string): string {
  return text
    .replaceAll("${HOME}", join(root, "home"))
    .replaceAll("${WS}", join(root, "ws"))
    .replaceAll("${ROOT}", root);
}

function pointer(value: unknown, path: string): unknown {
  if (path === "" || path === "/") return value;
  let current: unknown = value;
  for (const raw of path.split("/").slice(1)) {
    const key = raw.replaceAll("~1", "/").replaceAll("~0", "~");
    current = (current as Record<string, unknown> | undefined)?.[key];
  }
  return current;
}

/** `$ref:<i>:<pointer>` / `$find:<i>:<array pointer>:<field>=<value>:<pointer>`。 */
function resolveParams(value: unknown, results: unknown[], root: string): unknown {
  if (typeof value === "string") {
    if (value.startsWith("$ref:")) {
      const [, index, path] = /^\$ref:(\d+):(.*)$/.exec(value)!;
      return pointer(results[Number(index)], path!);
    }
    if (value.startsWith("$find:")) {
      const [, index, arrayPath, field, expected, path] =
        /^\$find:(\d+):([^:]*):([^=]+)=(.*):(\/.*)$/.exec(value)!;
      const items = pointer(results[Number(index)], arrayPath!) as Array<Record<string, unknown>>;
      const item = items.find((candidate) => candidate[field!] === expected);
      if (!item) throw new Error(`fixture reference not found: ${value}`);
      return pointer(item, path!);
    }
    return substitute(value, root);
  }
  if (Array.isArray(value)) return value.map((item) => resolveParams(item, results, root));
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value).map(([key, item]) => [key, resolveParams(item, results, root)]),
    );
  }
  return value;
}

/** 抹掉临时根、路径相关的 hash 与时间；Rust 侧 `normalize` 与此逐条对应。 */
function normalize(value: unknown, root: string, key = ""): unknown {
  if (value instanceof Uint8Array) return "<BYTES>";
  if (key === "updatedAt" || key === "archiveBytes") return "<N>";
  if (key === "discoveredAt") return "<TIME>";
  if (typeof value === "string") {
    let text = value.replaceAll(root, "<ROOT>");
    text = text.replace(/^(glm:[^:]+:.*):[0-9a-f]{12}$/u, "$1:<H>");
    if (key === "id" && /^[0-9a-f]{64}$/u.test(text)) text = "<SHA>";
    return text;
  }
  if (Array.isArray(value)) return value.map((item) => normalize(item, root));
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value).map(([entryKey, item]) => [entryKey, normalize(item, root, entryKey)]),
    );
  }
  return value;
}

function normalizeFile(text: string | null, root: string): string | null {
  if (text === null) return null;
  return text.replaceAll(root, "<ROOT>").replace(/"createdAt":\d+/gu, '"createdAt":"<T>"');
}

async function createTree(root: string) {
  for (const file of FILES) {
    const target = join(root, file.path);
    await mkdir(dirname(target), { recursive: true });
    if (file.symlink) {
      await symlink(substitute(file.symlink, root), target);
    } else {
      await writeFile(target, substitute(file.content ?? "", root));
    }
  }
}

type Services = Record<string, Record<string, (params?: unknown) => Promise<unknown>>>;

async function loadServices(): Promise<Services> {
  // 服务在模块加载时读取 HOME（skills 的常量、paths 的数据根），必须在设置环境之后再导入。
  const { createSkillsService } =
    await import("../../packages/services/src/skills/skillsService.js");
  const { createSkillSyncService } =
    await import("../../packages/services/src/skill-sync/skillSyncService.js");
  const { createPluginsService } =
    await import("../../packages/services/src/plugins/pluginsService.js");
  const { createPluginSyncService } =
    await import("../../packages/services/src/plugin-sync/pluginSyncService.js");
  const { createCommandsService } =
    await import("../../packages/services/src/commands/commandsService.js");
  const { createMemoryService } =
    await import("../../packages/services/src/memory/memoryService.js");
  const { createHooksService } = await import("../../packages/services/src/hooks/hooksService.js");
  const { createMcpSyncService } =
    await import("../../packages/services/src/mcp-sync/mcpSyncService.js");
  const { createSubagentsService } =
    await import("../../packages/services/src/subagents/subagentsService.js");
  const silent = { debug() {}, info() {}, warn() {}, error() {} } as never;
  return {
    skills: createSkillsService({ isDesktopRuntime: true }) as never,
    "skill-sync": createSkillSyncService() as never,
    plugins: createPluginsService({ isDesktopRuntime: true }) as never,
    "plugin-sync": createPluginSyncService() as never,
    commands: createCommandsService({ isDesktopRuntime: true }) as never,
    memory: createMemoryService() as never,
    hooks: createHooksService({ logger: silent }) as never,
    "mcp-sync": createMcpSyncService() as never,
    subagents: createSubagentsService({ isDesktopRuntime: true }) as never,
  };
}

async function run(root: string) {
  const services = await loadServices();
  const raw: unknown[] = [];
  const results: unknown[] = [];
  for (const call of CALLS as ScenarioCall[]) {
    const params = resolveParams(call.params, raw, root);
    const method = services[call.channel]![call.method]!.bind(services[call.channel]);
    try {
      const result = await (call.params === undefined ? method() : method(params));
      raw.push(result);
      results.push({ result: normalize(result ?? null, root) });
    } catch (error) {
      raw.push(null);
      const failure = error as { name?: string; message?: string; code?: unknown };
      results.push({
        error: {
          name: failure.name ?? "Error",
          message: normalize(failure.message ?? String(error), root),
          ...(typeof failure.code === "string" ? { code: failure.code } : {}),
        },
      });
    }
  }
  const files: Record<string, string | null> = {};
  for (const path of DUMPS) {
    const text = await readFile(join(root, path), "utf-8").catch(() => null);
    files[path] = normalizeFile(text, root);
  }
  return { results, files };
}

const root = await realpath(await mkdtemp(join(tmpdir(), "zcode-ext-fixture-")));
const home = join(root, "home");
process.env.HOME = home;
process.env.USERPROFILE = home;
process.env.ZCODE_DATA_BASE_DIR = home;
try {
  await createTree(root);
  const { results, files } = await run(root);
  const fixture = { files: FILES, calls: CALLS, dumps: DUMPS, expected: { results, files } };
  const text = (await format(fixturePath, `${JSON.stringify(fixture, null, 2)}\n`)).code;
  if (process.argv.includes("--check")) {
    const current = await readFile(fixturePath, "utf-8").catch(() => "");
    if (current !== text) {
      console.error(`extensions fixture is stale: ${fixturePath}`);
      process.exitCode = 1;
    }
  } else {
    await mkdir(dirname(fixturePath), { recursive: true });
    await writeFile(fixturePath, text);
    console.info(`wrote ${fixturePath}`);
  }
} finally {
  await rm(root, { recursive: true, force: true });
}
