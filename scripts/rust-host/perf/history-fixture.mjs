// App 级性能测量的长历史夹具（suite history）：用配置 A 驱动 HISTORY_TURNS 轮生成一个长会话，
// 保存在固定路径（会话库与 tasks-index 里存的是绝对路径），每次运行复制一份 HOME。
import { closeSync, existsSync, openSync } from "node:fs";
import { cp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

import { launchApp } from "../e2e/app.mjs";
import { seedProfile, startMockModel, startOfflineControlPlane } from "../e2e/profile.mjs";
import { waitUsable } from "../e2e/resilience-helpers.mjs";
import { HISTORY_TURNS, perfMockRules } from "./mock.mjs";
import { installProbes } from "./page-probes.mjs";
import { send } from "./suites.mjs";

export const HISTORY_FIXTURE = join(tmpdir(), "zcode-app-perf-history-v1");
/** 夹具内容变化时递增（mock.mjs 的 historyReply、HISTORY_TURNS）。 */
const HISTORY_FIXTURE_VERSION = 1;

/** 把夹具的 HOME 复制到 `home`，返回夹具的 workspace 路径。 */
export async function copyHistoryFixture(home) {
  if (!existsSync(join(HISTORY_FIXTURE, "home")))
    throw new Error("history fixture missing: run `app-perf.mjs seed-history` first");
  const version = await readFile(join(HISTORY_FIXTURE, "VERSION"), "utf8").catch(() => "");
  if (version.trim() !== String(HISTORY_FIXTURE_VERSION))
    throw new Error("history fixture is outdated: run `app-perf.mjs seed-history` again");
  await cp(join(HISTORY_FIXTURE, "home"), home, { recursive: true });
  return join(HISTORY_FIXTURE, "workspace");
}

/**
 * 长历史夹具：用配置 A（今天发布的组合）在固定路径的 HOME 与 workspace 里驱动 HISTORY_TURNS 轮
 * （每轮约 2.3 KB 的 Markdown 回复，每 10 轮带一次 Read），正常退出后保留会话库与 tasks-index。
 * 三种配置读的都是这一份（会话库格式 Node 与 Rust 共用）。
 */
export async function seedHistoryFixture() {
  await rm(HISTORY_FIXTURE, { recursive: true, force: true });
  const home = join(HISTORY_FIXTURE, "home");
  const workspace = join(HISTORY_FIXTURE, "workspace");
  const temp = join(HISTORY_FIXTURE, "seed-run");
  await mkdir(temp, { recursive: true });
  const log = { sent: {}, toolResults: {} };
  const model = await startMockModel(workspace, perfMockRules(log));
  await seedProfile(home, workspace, model.baseUrl);
  const controlPlane = await startOfflineControlPlane();
  const output = openSync(join(temp, "electron-output.log"), "a");
  const app = await launchApp({
    host: "node",
    temp,
    home,
    cli: null,
    env: { ZCODE_BASE_URL: controlPlane.origin, NODE_ENV: "production" },
    output,
  });
  let quit;
  try {
    await waitUsable(app.page);
    await installProbes(app.page);
    // 只等 runtime 走完每一轮（mock 发出了这一轮的回答，发送键重新可用），不等 DOM：长会话里 Renderer
    // 的列表虚拟化与自动滚动不保证最新一轮的文字立刻在 DOM 里，界面上的冷加载由 history suite 测。
    for (let index = 0; index < HISTORY_TURNS; index += 1) {
      await send(app.page, `perf:hist:${index}`);
      const deadline = Date.now() + 120_000;
      while (!log.sent[`h${index}`]) {
        if (Date.now() > deadline) throw new Error(`history turn ${index} never reached the model`);
        await sleep(100);
      }
      if (index % 20 === 0) console.info(`[perf] history fixture: turn ${index}/${HISTORY_TURNS}`);
    }
    // 最后一轮写完：Stop 键消失（composer 是空的，发送键此时不可用）。
    await app.page
      .locator('button[aria-label="Stop"]')
      .first()
      .waitFor({ state: "hidden", timeout: 120_000 });
    await sleep(3000);
  } catch (error) {
    await app.page
      .screenshot({ path: join(HISTORY_FIXTURE, "failure.png") })
      .catch(() => undefined);
    await writeFile(
      join(HISTORY_FIXTURE, "failure.txt"),
      await app.page.evaluate(() => document.body.innerText).catch(() => ""),
    );
    throw error;
  } finally {
    quit = await app.stop();
    closeSync(output);
    model.server.close();
    controlPlane.server.close();
  }
  if (!quit.graceful) throw new Error("history fixture run did not quit cleanly");
  // 日志不进夹具；Electron 的 userData 每次运行重新建。
  await rm(join(home, ".zcode", "v2", "logs"), { recursive: true, force: true });
  await rm(join(home, ".zcode", "cli", "log"), { recursive: true, force: true });
  await rm(temp, { recursive: true, force: true });
  await writeFile(join(HISTORY_FIXTURE, "VERSION"), `${HISTORY_FIXTURE_VERSION}\n`);
  console.info(`[perf] history fixture ready: ${HISTORY_FIXTURE}`);
}
