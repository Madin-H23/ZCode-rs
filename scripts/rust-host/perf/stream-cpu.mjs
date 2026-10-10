#!/usr/bin/env node
// 流式 CPU 剖析：4 个会话同时流式时，A（Node Host + TS CLI，Renderer 经 MessagePort）与 B（Rust Host +
// 进程内 runtime，Renderer 经回环 WebSocket）的 Renderer / Main / 辅助进程 CPU 为什么不同
// （docs/reports/rust-app-performance-2026-10-10.md 的“流式 CPU 为什么持平”）。
//
// 每次运行一个隔离的 App（与 app-perf.mjs 相同的 harness、构建、mock 模型与 8 个 workspace 的 profile），
// 先 reload 一次让 Renderer 装上消息计数（rpc-tap.mjs），再起 4 路 20 ms 一段的长流，之后按顺序：
//   report   与 App 级报告的 4 路窗口相同（Playwright 连着、页面探针开着、打字 12 次、切换会话 4 次）
//   warm     4 个会话的订阅都还在保温期内：H / C 交替两轮（各 4 s）；再切一遍会话重新保温，C 条件下
//            录 8 s Renderer CPU profile
//            H = Playwright 连着、探针开着（报告的测量条件）；C = 暂停探针、断开 Playwright（没有 CDP 客户端
//            连着 Renderer），窗口后再连回
//   steady   保温结束（只订阅可见会话）：先抓包 3 s（Renderer 收到的每条消息，事后解码分类；B 另用 CDP
//            Network 域数 WebSocket 帧），再 H / C 交替两轮（各 5 s；第一轮 H 期间经 inspector 临时包住 Main
//            的 EventEmitter.emit 计事件数），然后 H 与 C 各录 5 s Chromium trace（trace-summary.mjs：每个
//            进程、线程、任务种类的 CPU），C 条件下 Renderer 与 Main 各 10 s CPU profile（cpu-profile.mjs，
//            直连 CDP / inspector，只开 Profiler 域），最后 macOS `sample` Host 5 s
// 每个窗口记分进程 CPU（ps 的累计 CPU 时间差）、Renderer 收发消息数与 Main 日志行数；结束时检查 4 路流
// 覆盖了全部窗口、warm 窗口都在保温期内，退出 App。
//
// 用法（仓库根目录，先 `node scripts/rust-host/perf/app-perf.mjs build`）：
//   node scripts/rust-host/perf/stream-cpu.mjs run --config A|B --out <dir> [--name <run>] [--host-binary <f>]
//   node scripts/rust-host/perf/stream-cpu.mjs batch --out <dir> [--reps 2] [--max-load <n>]   # AB、BA 交错
//   node scripts/rust-host/perf/stream-cpu.mjs report --out <dir>
// 只用于测量：不改产品代码；页面补丁、emit 包装都只在这个隔离实例里生效。
import { closeSync, openSync } from "node:fs";
import { mkdir, mkdtemp, readdir, rm, writeFile } from "node:fs/promises";
import { loadavg, tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { parseArgs } from "node:util";

import { launchApp } from "../e2e/app.mjs";
import { composer } from "../e2e/helpers.mjs";
import { startMockModel, startOfflineControlPlane } from "../e2e/profile.mjs";
import { waitUsable } from "../e2e/resilience-helpers.mjs";
import { connectCdp, profileFor, rendererTargetUrl, summarizeProfile } from "./cpu-profile.mjs";
import { sampleProcess } from "./host-sample.mjs";
import { expectedMarkers, perfMockRules, startMarker } from "./mock.mjs";
import {
  installProbes,
  measureClickUntilVisible,
  setProbesActive,
  waitMarker,
  watchMarkers,
} from "./page-probes.mjs";
import { helperKind, mainWindowRendererPids } from "./processes.mjs";
import { decodeCapture, readTap, rpcTapInit, startCapture, takeCapture } from "./rpc-tap.mjs";
import { CONFIGS, RUST_HOST_RELEASE, seedWorkspaces } from "./runner.mjs";
import {
  connectPage,
  countCdpFrames,
  cpuBetween,
  cpuTable,
  cpuWindow,
  EMIT_TAP_INSTALL,
  EMIT_TAP_TAKE,
  mainLogCounts,
  streamsCover,
} from "./stream-cpu-lib.mjs";
import { report } from "./stream-cpu-report.mjs";
import { newTaskIn, send, WORKSPACE_COUNT } from "./suites.mjs";
import { recordTrace, summarizeTrace } from "./trace-summary.mjs";

const STREAM_CHUNKS = 9000;
/** 保温期内（4 个会话都订阅着）每个 H / C 窗口的长度。 */
const WARM_MS = 4_000;
/** 保温结束后（只剩可见会话的订阅）每个 H / C 窗口的长度。 */
const STEADY_MS = 5_000;
const CAPTURE_MS = 3_000;
const TRACE_MS = 5_000;
const PROFILE_MS = 10_000;
const WARM_PROFILE_MS = 8_000;
/** Renderer 离开会话后保温的时长（v4 session data lease 的 keepWarmMs，suites.mjs 的 SETTLE_MS 同源）。 */
const KEEP_WARM_MS = 30_000;
const TAGS = ["s4w2", "s4w3", "s4w4", "s4w1"];
const NAMES = ["ws2", "ws3", "ws4", "ws1"];

export async function runOnce(configId, outDir, name, { hostBinary = RUST_HOST_RELEASE } = {}) {
  const config = CONFIGS[configId];
  const temp = await mkdtemp(join(tmpdir(), `zcode-stream-cpu-${configId}-`));
  const home = join(temp, "home");
  const log = { sent: {}, toolResults: {} };
  const workspaces = Array.from({ length: WORKSPACE_COUNT }, (_, index) =>
    join(temp, `ws${index + 1}`),
  );
  const model = await startMockModel(workspaces[0], perfMockRules(log));
  await seedWorkspaces(home, workspaces, model.baseUrl);
  const controlPlane = await startOfflineControlPlane();
  const env = {
    ZCODE_BASE_URL: controlPlane.origin,
    NODE_ENV: "production",
    ...(config.host === "rust" ? { ZCODE_RUST_HOST_BINARY: hostBinary } : {}),
  };
  const profileDir = join(outDir, "profiles", name);
  await mkdir(profileDir, { recursive: true });
  const result = {
    name,
    config: configId,
    startedAt: new Date().toISOString(),
    load1: { start: loadavg()[0] },
    checks: [],
  };
  const output = openSync(join(temp, "electron-output.log"), "a");
  const covered = [];
  let app;
  let current = null;
  try {
    app = await launchApp({ host: config.host, temp, home, cli: config.cli, env, output });
    current = { browser: app.browser, page: app.page };
    await waitUsable(current.page);
    // 计数补丁要在页面脚本之前装上：加 init script 后 reload 一次（Renderer reload 是支持的重新接入路径）。
    await current.page.context().addInitScript(rpcTapInit);
    await current.page.reload();
    await waitUsable(current.page);
    result.transports = await current.page.evaluate(() => window.__rpcTap?.transports ?? null);
    await installProbes(current.page);
    await sleep(5000);

    const page = () => current.page;
    for (let index = 0; index < TAGS.length; index += 1) {
      await newTaskIn(page(), NAMES[index]);
      await watchMarkers(page(), [startMarker(TAGS[index])]);
      await send(page(), `perf:stream:${TAGS[index]}:${STREAM_CHUNKS}`);
      await waitMarker(page(), startMarker(TAGS[index]));
    }
    // 与 App 级报告的 4 路窗口相同：探针盯着可见会话（ws1）的进度标记，每帧检查一次 DOM 文本。
    await watchMarkers(page(), expectedMarkers("s4w1", STREAM_CHUNKS).slice(1));
    await sleep(2000);

    const rendererPids = await mainWindowRendererPids(app.main);
    const table = await cpuTable(app.pid, rendererPids);
    const hostPid = table.find((row) => row.role === "host")?.pid;
    const roleOfPid = new Map(
      table.map((row) => [row.pid, row.role === "helper" ? helperKind(row.command) : row.role]),
    );

    /** H：Playwright 连着、探针开着（App 级报告的测量条件）。 */
    const harnessWindow = async (ms) => {
      await setProbesActive(page(), true);
      await sleep(300);
      const window = await cpuWindow(app.pid, rendererPids, ms);
      window.tap = await readTap(page(), window.from, window.to);
      covered.push(["harness", window.from, window.to]);
      return window;
    };
    /** C：暂停探针、断开 Playwright（connectOverCDP 的 close 只断开连接，不关 App），窗口后再连回。 */
    const cleanWindow = async (ms, during) => {
      await setProbesActive(page(), false);
      await current.browser.close();
      current = null;
      await sleep(1000);
      let window;
      if (during) {
        const before = await cpuTable(app.pid, rendererPids);
        const from = Date.now();
        const extra = await during();
        const to = Date.now();
        const cpu = cpuBetween(before, await cpuTable(app.pid, rendererPids), (to - from) / 1000);
        window = { from, ...extra, to, cpu };
      } else {
        window = await cpuWindow(app.pid, rendererPids, ms);
      }
      current = await connectPage(app.cdpPort);
      window.tap = await readTap(page(), window.from, window.to);
      covered.push(["clean", window.from, window.to]);
      // 重新连接的 CDP 初始化（Playwright 打开各个域）不要落进下一个窗口。
      await sleep(1000);
      return window;
    };

    // report：与 App 级报告（suites.mjs 的 4 路流式窗口）相同：每秒在 composer 敲一个字符共 12 个、清空，
    // 再依次点侧栏切到另外三个会话并切回 ws1，每次切换后停 1.5 s。
    const reportBefore = await cpuTable(app.pid, rendererPids);
    const reportFrom = Date.now();
    const input = await composer(page());
    await input.click();
    for (let index = 0; index < 12; index += 1) {
      await page().keyboard.press("x");
      await sleep(1000);
    }
    await page().keyboard.press("Meta+A");
    await page().keyboard.press("Backspace");
    const switchMs = [];
    for (const tag of [...TAGS.slice(0, 3), "s4w1"]) {
      const locator = page()
        .getByText(`perf:stream:${tag}:${STREAM_CHUNKS}`, { exact: true })
        .first();
      switchMs.push(await measureClickUntilVisible(page(), locator, startMarker(tag)));
      await sleep(1500);
    }
    const reportTo = Date.now();
    result.report = {
      from: reportFrom,
      to: reportTo,
      switchMs,
      cpu: cpuBetween(
        reportBefore,
        await cpuTable(app.pid, rendererPids),
        (reportTo - reportFrom) / 1000,
      ),
    };
    result.report.tap = await readTap(page(), reportFrom, reportTo);
    covered.push(["report", reportFrom, reportTo]);

    // warm：刚切换过，4 个会话的 conversation 订阅都还在（Renderer 离开会话后保温 30 s）。H/C 交替两轮，
    // 抵消窗口先后的漂移；都在保温结束之前做完。
    result.warm = { harness: [], clean: [] };
    for (let round = 0; round < 2; round += 1) {
      result.warm.harness.push(await harnessWindow(WARM_MS));
      result.warm.clean.push(await cleanWindow(WARM_MS));
    }
    result.warm.withinKeepWarm = Date.now() - reportTo < KEEP_WARM_MS - 4500;

    // warm profile：再依次切一遍会话让 4 个订阅重新保温，C 条件下录 8 s Renderer CPU profile。
    for (const tag of [...TAGS.slice(0, 3), "s4w1"]) {
      await page()
        .getByText(`perf:stream:${tag}:${STREAM_CHUNKS}`, { exact: true })
        .first()
        .click();
      await sleep(1500);
    }
    const rewarmTo = Date.now();
    let warmProfile;
    const warmProfileWindow = await cleanWindow(0, async () => {
      const renderer = await connectCdp(await rendererTargetUrl(app.cdpPort));
      warmProfile = await profileFor(renderer, WARM_PROFILE_MS);
      renderer.close();
      return {};
    });
    await writeFile(join(profileDir, "renderer.warm.cpuprofile"), JSON.stringify(warmProfile));
    result.warm.profile = { ...warmProfileWindow, renderer: await summarizeProfile(warmProfile) };

    // steady：等保温结束，只剩可见会话（ws1）的 conversation 订阅，另外 3 路只有遥测与列表事实。
    const settleUntil = rewarmTo + KEEP_WARM_MS + 3000;
    if (Date.now() < settleUntil) await sleep(settleUntil - Date.now());
    // capture：抓包（H 条件），B 另用 CDP Network 域数 WebSocket 帧。
    await startCapture(page());
    const captureFrom = Date.now();
    const cdpFrames =
      config.host === "rust" ? await countCdpFrames(page(), CAPTURE_MS) : await sleep(CAPTURE_MS);
    const captured = await takeCapture(page());
    const captureSec = (Date.now() - captureFrom) / 1000;
    result.capture = {
      ...decodeCapture(captured, captureSec),
      cdpWebSocketFrames: cdpFrames ?? null,
    };
    await writeFile(join(profileDir, "capture.json"), JSON.stringify(captured));

    result.steady = { harness: [], clean: [] };
    for (let round = 0; round < 2; round += 1) {
      if (round === 0) await app.main.evaluate(EMIT_TAP_INSTALL);
      const harness = await harnessWindow(STEADY_MS);
      if (round === 0) harness.mainEvents = await app.main.evaluate(EMIT_TAP_TAKE);
      harness.mainLog = await mainLogCounts(home, harness.from, harness.to);
      result.steady.harness.push(harness);
      const clean = await cleanWindow(STEADY_MS);
      clean.mainLog = await mainLogCounts(home, clean.from, clean.to);
      result.steady.clean.push(clean);
    }

    // trace：H 与 C 各 5 s（contentTracing 经 Main inspector，不需要 Playwright）。
    await setProbesActive(page(), true);
    await sleep(300);
    const traceBefore = await cpuTable(app.pid, rendererPids);
    const harnessTrace = await recordTrace(
      app.main,
      join(profileDir, "harness.trace.json"),
      TRACE_MS,
    );
    covered.push(["harness trace", harnessTrace.from, harnessTrace.to]);
    result.steady.harnessTrace = {
      ...harnessTrace,
      cpu: cpuBetween(
        traceBefore,
        await cpuTable(app.pid, rendererPids),
        (Date.now() - harnessTrace.from) / 1000,
      ),
      processes: await summarizeTrace(harnessTrace.file, TRACE_MS / 1000, roleOfPid),
    };
    const cleanTrace = await cleanWindow(0, async () => {
      const trace = await recordTrace(app.main, join(profileDir, "clean.trace.json"), TRACE_MS);
      return { file: trace.file };
    });
    result.steady.cleanTrace = {
      ...cleanTrace,
      processes: await summarizeTrace(cleanTrace.file, TRACE_MS / 1000, roleOfPid),
    };

    // profile：C 条件，直连 Renderer 的 CDP 与 Main 的 inspector，只开 Profiler。
    let rendererProfile;
    let mainProfile;
    const profileWindow = await cleanWindow(0, async () => {
      const renderer = await connectCdp(await rendererTargetUrl(app.cdpPort));
      [rendererProfile, mainProfile] = await Promise.all([
        profileFor(renderer, PROFILE_MS),
        profileFor({ call: (method, params) => app.main.call(method, params) }, PROFILE_MS),
      ]);
      renderer.close();
      return {};
    });
    await writeFile(join(profileDir, "renderer.cpuprofile"), JSON.stringify(rendererProfile));
    await writeFile(join(profileDir, "main.cpuprofile"), JSON.stringify(mainProfile));
    result.profile = {
      ...profileWindow,
      renderer: await summarizeProfile(rendererProfile),
      main: await summarizeProfile(mainProfile),
    };

    // sample：Host 的原生调用栈（Electron 的进程看 trace：Electron Framework 没有符号）。
    if (hostPid) {
      const sampleFrom = Date.now();
      result.samples = {
        host: await sampleProcess(hostPid, 5, join(profileDir, "host.sample.txt")),
      };
      covered.push(["sample", sampleFrom, Date.now()]);
    }

    // mock 每 10 段（约 200 ms）记一次标记：多等一会儿，覆盖检查才看得到窗口之后的标记。
    await sleep(1500);
    result.checks.push({
      name: "renderer tap installed",
      ok: (result.transports ?? []).length > 0,
    });
    result.checks.push({ name: "warm windows within keep-warm", ok: result.warm.withinKeepWarm });
    for (const [label, from, to] of covered)
      result.checks.push({
        name: `4 streams cover ${label} ${from}`,
        ok: streamsCover(log, TAGS, from, to),
      });
    result.load1.end = loadavg()[0];
    result.valid = result.checks.every((check) => check.ok);
  } catch (error) {
    result.error = String(error.stack ?? error);
    result.valid = false;
  } finally {
    await current?.browser?.close().catch(() => undefined);
    if (app) result.quit = await app.stop().catch((error) => ({ error: String(error) }));
    closeSync(output);
    model.server.close();
    controlPlane.server.close();
    result.temp = temp;
    result.streams = Object.fromEntries(
      TAGS.map((tag) => {
        const sent = log.sent[tag] ?? [];
        return [tag, { markers: sent.length, first: sent[0]?.sentAt, last: sent.at(-1)?.sentAt }];
      }),
    );
    await mkdir(join(outDir, "runs"), { recursive: true });
    await writeFile(join(outDir, "runs", `${name}.json`), `${JSON.stringify(result, null, 2)}\n`);
    if (result.valid) await rm(temp, { recursive: true, force: true });
  }
  return result;
}

// ---------------------------------------------------------------------------------------------

const [command, ...rest] = process.argv.slice(2);
const { values: options } = parseArgs({
  args: rest,
  options: {
    config: { type: "string" },
    out: { type: "string" },
    name: { type: "string" },
    reps: { type: "string", default: "2" },
    "max-load": { type: "string", default: "Infinity" },
    "host-binary": { type: "string" },
  },
});
if (!options.out) throw new Error("--out <dir> is required");
const outDir = resolve(options.out);
switch (command) {
  case "run": {
    if (!CONFIGS[options.config]) throw new Error("--config A|B is required");
    // --host-binary：换一个同源码、不剥符号的 Rust Host release 构建，只为让 `sample` 的调用栈有函数名。
    const result = await runOnce(
      options.config,
      outDir,
      options.name ?? `${options.config}-manual`,
      {
        hostBinary: options["host-binary"] ? resolve(options["host-binary"]) : RUST_HOST_RELEASE,
      },
    );
    console.info(
      `[stream-cpu] ${result.name}: ${result.valid ? "valid" : `INVALID ${result.error ?? JSON.stringify(result.checks.filter((check) => !check.ok))}`}`,
    );
    break;
  }
  case "batch": {
    const reps = Number(options.reps);
    const existing = new Set(await readdir(join(outDir, "runs")).catch(() => []));
    for (let rep = 1; rep <= reps; rep += 1) {
      for (const config of rep % 2 ? ["A", "B"] : ["B", "A"]) {
        const name = `${config}-${rep}`;
        if (existing.has(`${name}.json`)) continue;
        for (let attempt = 0; attempt < 2; attempt += 1) {
          // 机器共用：给了 --max-load 时先等 1 分钟 load 降到它以下（最多 10 分钟）。
          const deadline = Date.now() + 10 * 60_000;
          while (loadavg()[0] > Number(options["max-load"]) && Date.now() < deadline)
            await sleep(15_000);
          console.info(
            `[stream-cpu] ${name} attempt ${attempt + 1}, load ${loadavg()[0].toFixed(1)}`,
          );
          const result = await runOnce(config, outDir, attempt ? `${name}-retry${attempt}` : name);
          console.info(
            `[stream-cpu] ${result.name}: ${result.valid ? "valid" : `INVALID ${result.error?.split("\n")[0] ?? ""}`}`,
          );
          if (result.valid) break;
        }
      }
    }
    break;
  }
  case "report":
    await report(outDir);
    break;
  default:
    throw new Error("usage: stream-cpu.mjs run|batch|report --out <dir>");
}
