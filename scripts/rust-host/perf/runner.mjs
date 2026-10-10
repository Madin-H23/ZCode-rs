// App 级性能测量的一次运行：按配置启动一个隔离的 Electron 实例（复用 e2e harness：临时 HOME、私有 CDP
// 端口、本地 mock 模型、离线控制面），执行一个 suite（suites.mjs），写一份 JSON 结果。用法见 app-perf.mjs。
import { closeSync, openSync } from "node:fs";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { loadavg, tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

import { assertCleanQuit, launchApp, logTime } from "../e2e/app.mjs";
import {
  seedProfile,
  seedWorkspace,
  startMockModel,
  startOfflineControlPlane,
  writeProviderConfig,
} from "../e2e/profile.mjs";
import { hostLogLines, waitUsable } from "../e2e/resilience-helpers.mjs";
import { copyHistoryFixture } from "./history-fixture.mjs";
import { BIG_FILE, perfMockRules } from "./mock.mjs";
import { installProbes } from "./page-probes.mjs";
import { stillRunning, treeIdentities } from "./processes.mjs";
import { SUITE_RUNNERS, WORKSPACE_COUNT } from "./suites.mjs";

export const ROOT = resolve(import.meta.dirname, "../../..");
export const RUST_HOST_RELEASE = join(ROOT, "apps/zcode-host/target/release/zcode-host");
export const RUST_CLI_RELEASE = join(ROOT, "apps/zcode-cli-rust/target/release/zcode-cli-rust");
export const TS_CLI_BUNDLE = join(ROOT, "apps/zcode-cli/packages/cli/dist/zcode.cjs");

/** 三种配置：同一份 Electron、Renderer / Main 生产构建、mock 模型与夹具，只换 Host 与 runtime。 */
export const CONFIGS = {
  A: { id: "A", host: "node", cli: null, label: "Node Host + TS CLI 子进程（今天发布的组合）" },
  B: { id: "B", host: "rust", cli: null, label: "Rust Host + 进程内 Rust runtime（目标）" },
  C: { id: "C", host: "node", cli: RUST_CLI_RELEASE, label: "Node Host + Rust CLI 子进程" },
};
export const SUITES = ["startup", "stream", "history"];

/** Main 日志里第一条匹配行的时刻（本地时间戳 → epoch ms）；等到出现或超时。 */
async function logLineTime(home, pattern, timeout = 60_000) {
  const deadline = Date.now() + timeout;
  for (;;) {
    const line = (await hostLogLines(home)).find((candidate) => pattern.test(candidate));
    if (line) return logTime(line);
    if (Date.now() > deadline) return null;
    await sleep(250);
  }
}

async function seedWorkspaces(home, workspaces, baseUrl) {
  await seedProfile(home, workspaces[0], baseUrl);
  for (const workspace of workspaces.slice(1)) await seedWorkspace(home, workspace);
  await writeFile(
    join(workspaces[0], BIG_FILE),
    `${Array.from({ length: 100_000 }, (_, line) => `line ${line} lorem ipsum dolor sit amet`).join("\n")}\n`,
  );
  // 8 个本地 workspace 都在侧栏（与老用户的多项目 profile 一致）；启动时 Main 只预热活动的那一个。
  const settingPath = join(home, ".zcode", "v2", "setting.json");
  const setting = JSON.parse(await readFile(settingPath, "utf8"));
  setting.lastWorkspaceSession = workspaces.map((workspacePath) => ({
    kind: "local",
    workspacePath,
    workspacePurpose: "project",
  }));
  await writeFile(settingPath, JSON.stringify(setting));
}

/** 正常退出：SIGTERM 给 Main，直到 Main 与退出前的全部后代都不在了。 */
async function measureQuit(app, config, home) {
  const identities = await treeIdentities(app.pid);
  const startedAt = Date.now();
  let allGoneMs = null;
  const stopping = app.stop();
  while (Date.now() - startedAt < 75_000) {
    if ((await stillRunning(identities)).length === 0) {
      allGoneMs = Date.now() - startedAt;
      break;
    }
    await sleep(100);
  }
  const quit = await stopping;
  let clean = true;
  let error = null;
  try {
    await assertCleanQuit(config.host, home, quit);
  } catch (caught) {
    clean = false;
    error = String(caught.message ?? caught);
  }
  return {
    processes: identities.length,
    mainExitMs: quit.waitedMs,
    allGoneMs,
    graceful: quit.graceful,
    clean,
    error,
  };
}

/**
 * 运行一个 suite，返回结果对象（也写到 `outFile`）。`valid` 为 false 时结果不进统计：有检查未通过、
 * 运行抛错或退出不干净。
 */
export async function runSuite(suite, configId, { outFile, keep = false, label = "" } = {}) {
  const config = CONFIGS[configId];
  if (!config) throw new Error(`unknown configuration ${configId}`);
  if (!SUITE_RUNNERS[suite]) throw new Error(`unknown suite ${suite}`);
  const temp = await mkdtemp(join(tmpdir(), `zcode-app-perf-${suite}-${configId}-`));
  const home = join(temp, "home");
  const log = { sent: {}, toolResults: {} };
  let workspaces;
  if (suite === "history") {
    workspaces = [await copyHistoryFixture(home)];
  } else {
    workspaces = Array.from({ length: WORKSPACE_COUNT }, (_, index) =>
      join(temp, `ws${index + 1}`),
    );
  }
  const model = await startMockModel(workspaces[0], perfMockRules(log));
  if (suite === "history") await writeProviderConfig(home, model.baseUrl);
  else await seedWorkspaces(home, workspaces, model.baseUrl);
  const controlPlane = await startOfflineControlPlane();
  const env = {
    ZCODE_BASE_URL: controlPlane.origin,
    NODE_ENV: "production",
    ...(config.host === "rust" ? { ZCODE_RUST_HOST_BINARY: RUST_HOST_RELEASE } : {}),
  };
  const result = {
    suite,
    config: configId,
    label,
    startedAt: new Date().toISOString(),
    load1: { start: loadavg()[0] },
    coldStart: {},
    snapshots: [],
    checks: [],
    valid: false,
  };
  const output = openSync(join(temp, "electron-output.log"), "a");
  let app;
  try {
    const spawnAt = Date.now();
    app = await launchApp({ host: config.host, temp, home, cli: config.cli, env, output });
    await waitUsable(app.page);
    result.coldStart.usableMs = Date.now() - spawnAt;
    await installProbes(app.page);
    const context = { app, page: app.page, result, log, workspaces, home, spawnAt };
    await SUITE_RUNNERS[suite](context);
    const hostReadyAt = await logLineTime(home, /\[database-startup\] terminal .*"status":"ready"/);
    result.coldStart.hostReadyMs = hostReadyAt === null ? null : hostReadyAt - spawnAt;
    result.load1.end = loadavg()[0];
    result.quit = await measureQuit(app, config, home);
    app = undefined;
    result.checks.push({ name: "clean quit", ok: result.quit.clean, detail: result.quit.error });
    result.valid = result.checks.every((check) => check.ok);
  } catch (error) {
    result.error = String(error.stack ?? error);
    await app?.page.screenshot({ path: join(temp, "failure.png") }).catch(() => undefined);
  } finally {
    await app?.stop().catch(() => undefined);
    closeSync(output);
    model.server.close();
    controlPlane.server.close();
    result.temp = temp;
    if (outFile) {
      await mkdir(resolve(outFile, ".."), { recursive: true });
      await writeFile(outFile, `${JSON.stringify(result, null, 2)}\n`);
    }
    if (!keep && result.valid) await rm(temp, { recursive: true, force: true });
  }
  return result;
}
