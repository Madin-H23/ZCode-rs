#!/usr/bin/env node
// App 级性能对比：整个桌面 App（Electron Main、Renderer、GPU 与其他辅助进程、Host、runtime、scheduler、
// 工具进程）在三种配置下的占用与响应（报告：docs/reports/rust-app-performance-2026-10-10.md）。
//
//   A  Node Host（utility 进程）+ TS CLI 子进程（每个 workspace 一个）+ Node scheduler：今天发布的组合
//   B  Rust Host（release）+ 进程内 Rust runtime（每个 workspace 一个实例）：目标
//   C  Node Host + Rust CLI 子进程（release）：npm 启动器的组合，用来区分 Host 与 runtime 各自的贡献
//
// 三种配置用同一份 Electron、同一份 Main / Renderer 生产构建（tsup + vite build，NODE_ENV=production，
// 加载 out/ 而不是 Vite 开发服务器）、同一个本地 mock 模型与夹具。每次运行是一个隔离的 Electron 实例：
// 复用 e2e harness（scripts/rust-host/e2e/）的临时 HOME、私有 CDP 端口、离线控制面与 mock 模型，
// 不读写 ~/.zcode，不访问外部服务。
//
// 用法（仓库根目录）：
//   node scripts/rust-host/perf/app-perf.mjs build
//       生产形态的构建：zcode-host 与 zcode-cli-rust 的 release、TS CLI 单文件 bundle
//       （scripts/build-desktop-agent-cli.mjs，与打包链相同）、desktop 生产构建
//   node scripts/rust-host/perf/app-perf.mjs seed-history
//       用配置 A 生成长历史夹具（固定路径，见 history-fixture.mjs）
//   node scripts/rust-host/perf/app-perf.mjs run --config A|B|C --suite startup|stream|history [--out f.json] [--keep]
//                                           [--host-binary <zcode-host>]
//       单个配置跑一次（一个配置一条命令）。--host-binary 换一个 Rust Host 构建（例如另一个提交的 release），
//       只对 B 生效，用来在同一测量方法下对比代码改动前后
//   node scripts/rust-host/perf/app-perf.mjs batch --out <dir> [--reps 3] [--suites startup,stream,history]
//                                             [--configs A,B,C] [--max-load <n>] [--redo-load <n>] [--no-warmup]
//       交错运行（每轮轮换起始配置：ABC、BCA、CAB…）。每次运行都记录开始与结束时的 1 分钟 load average；
//       给了 --max-load 时先等 load 降到它以下（最多 15 分钟），给了 --redo-load 时 load 超过它的运行作废
//       重跑；缺省都不做（机器共用，2026-10-10 的数据就是在其他编译任务并行时测的）。检查未通过的运行
//       作废并重跑（最多 2 次）。开始前每个配置先跑一次 startup 作为预热，不计入结果。
//   node scripts/rust-host/perf/app-perf.mjs report --out <dir>
//       汇总 <dir>/runs/*.json：每个指标的中位数与最小–最大值（Markdown，另写 <dir>/summary.json）
//
// suite：
//   startup  冷启动（启动 → composer 可点、Host 数据库就绪、发送键可用、首段、回复完成），之后 N = 1 / 4 / 8
//            个 workspace 各完成一轮、静置 35 s 后的分角色占用，最后正常退出计时
//   stream   启动静置 30 s 后的空闲占用与 60 s 空闲 CPU；1 路与 4 路（不同 workspace）20 ms 一段的流式窗口
//            （各 20 s；4 路量两次：4 个会话的订阅都在保温期内，与保温结束后只订阅可见会话）：分角色 CPU
//            （辅助进程按种类拆开）、footprint、Renderer 长任务；窗口外量切换会话延迟；退出计时
//   history  冷加载 120 轮的长历史会话：点开到最后一轮可见、滚到顶到第一轮可见，之后的占用；退出计时
// CPU 窗口与稳态快照期间断开 Playwright、暂停页面探针（suites.mjs 的 detached）：Playwright 总会打开 CDP
// Network 域，B 的每个 WebSocket 帧都会变成经 Main 转发的 CDP 事件，抬高 B 的读数（报告“流式 CPU 为什么
// 持平”）。2026-10-10 首次测量时 stream suite 的窗口是连着 Playwright、开着探针、在窗口里打字与切换会话量的，
// 另有一轮工具负载；复测改成现在的样子，两次的 stream 读数不能直接比。
import { existsSync } from "node:fs";
import { mkdir, readdir, readFile, writeFile } from "node:fs/promises";
import { loadavg } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { parseArgs } from "node:util";

import { runCommand } from "../e2e/profile.mjs";
import { seedHistoryFixture } from "./history-fixture.mjs";
import { renderReport } from "./report.mjs";
import {
  CONFIGS,
  ROOT,
  RUST_CLI_RELEASE,
  RUST_HOST_RELEASE,
  SUITES,
  TS_CLI_BUNDLE,
  runSuite,
} from "./runner.mjs";

const [command, ...rest] = process.argv.slice(2);
const { values: options } = parseArgs({
  args: rest,
  options: {
    config: { type: "string" },
    suite: { type: "string" },
    out: { type: "string" },
    keep: { type: "boolean" },
    reps: { type: "string", default: "3" },
    suites: { type: "string", default: "startup,stream,history" },
    configs: { type: "string", default: "A,B,C" },
    "max-load": { type: "string", default: "Infinity" },
    "redo-load": { type: "string", default: "Infinity" },
    "no-warmup": { type: "boolean" },
    "host-binary": { type: "string" },
  },
});

async function build() {
  await runCommand("cargo", ["build", "--release"], join(ROOT, "apps/zcode-host"));
  await runCommand("cargo", ["build", "--release"], join(ROOT, "apps/zcode-cli-rust"));
  await runCommand("node", ["scripts/build-desktop-agent-cli.mjs"], ROOT);
  const desktop = join(ROOT, "packages/desktop");
  await runCommand("node", ["scripts/ensure-local-runtime-assets.mjs"], desktop);
  await runCommand("node", ["scripts/run-production-build.mjs"], desktop);
}

function assertBuilt() {
  for (const file of [RUST_HOST_RELEASE, RUST_CLI_RELEASE, TS_CLI_BUNDLE])
    if (!existsSync(file))
      throw new Error(`missing build output ${file}: run \`app-perf.mjs build\``);
  if (!existsSync(join(ROOT, "packages/desktop/out/renderer/index.html")))
    throw new Error("missing desktop production build: run `app-perf.mjs build`");
}

/** 等 1 分钟 load average 降到 `max` 以下，最多等 `capMs`；返回等到时的 load。 */
async function waitForLoad(max, capMs = 15 * 60_000) {
  const deadline = Date.now() + capMs;
  let announced = false;
  while (loadavg()[0] > max && Date.now() < deadline) {
    if (!announced) console.info(`[perf] load ${loadavg()[0].toFixed(1)} > ${max}; waiting`);
    announced = true;
    await sleep(15_000);
  }
  return loadavg()[0];
}

function describe(result) {
  const failed = result.checks.filter((check) => !check.ok).map((check) => check.name);
  return result.valid
    ? "valid"
    : `INVALID (${result.error ? result.error.split("\n")[0] : failed.join(", ")})`;
}

async function batch() {
  const out = resolve(options.out ?? "");
  if (!options.out) throw new Error("--out <dir> is required");
  const runsDir = join(out, "runs");
  await mkdir(runsDir, { recursive: true });
  const configs = options.configs.split(",");
  const suites = options.suites.split(",");
  const reps = Number(options.reps);
  const maxLoad = Number(options["max-load"]);
  const redoLoad = Number(options["redo-load"]);
  const existing = new Set(await readdir(runsDir));
  const once = async (suite, config, name, label) => {
    for (let attempt = 0; attempt < 3; attempt += 1) {
      const waited = await waitForLoad(maxLoad);
      const file = join(runsDir, `${name}${attempt ? `-retry${attempt}` : ""}.json`);
      console.info(
        `[perf] ${name} (${CONFIGS[config].label}) attempt ${attempt + 1}, load ${waited.toFixed(1)}`,
      );
      const result = await runSuite(suite, config, { label });
      const peak = Math.max(result.load1.start, result.load1.end ?? 0);
      if (result.valid && peak > redoLoad) {
        result.valid = false;
        result.discarded = `load ${peak.toFixed(1)} > ${redoLoad}`;
      }
      await writeFile(file, `${JSON.stringify(result, null, 2)}\n`);
      console.info(
        `[perf] ${name}: ${describe(result)}${result.discarded ? ` ${result.discarded}` : ""}`,
      );
      if (result.valid) return;
    }
  };
  if (!options["no-warmup"])
    for (const config of configs)
      if (!existing.has(`warmup-${config}.json`))
        await once("startup", config, `warmup-${config}`, "warmup");
  for (let rep = 1; rep <= reps; rep += 1) {
    for (const suite of suites) {
      const order = configs.map((_, index) => configs[(index + rep - 1) % configs.length]);
      for (const config of order) {
        const name = `${suite}-${config}-${rep}`;
        if (existing.has(`${name}.json`)) continue;
        await once(suite, config, name, "measure");
      }
    }
  }
}

async function report() {
  if (!options.out) throw new Error("--out <dir> is required");
  const out = resolve(options.out);
  const runsDir = join(out, "runs");
  const results = await Promise.all(
    (await readdir(runsDir))
      .filter((name) => name.endsWith(".json"))
      .map(async (name) => ({ name, ...JSON.parse(await readFile(join(runsDir, name), "utf8")) })),
  );
  const { markdown, summary } = renderReport(results);
  await writeFile(join(out, "summary.json"), `${JSON.stringify(summary, null, 2)}\n`);
  await writeFile(join(out, "summary.md"), markdown);
  console.info(markdown);
}

switch (command) {
  case "build":
    await build();
    break;
  case "seed-history":
    assertBuilt();
    await seedHistoryFixture();
    break;
  case "run": {
    assertBuilt();
    if (!CONFIGS[options.config]) throw new Error("--config A|B|C is required");
    if (!SUITES.includes(options.suite)) throw new Error(`--suite ${SUITES.join("|")} is required`);
    const result = await runSuite(options.suite, options.config, {
      outFile: options.out ? resolve(options.out) : undefined,
      keep: options.keep,
      label: "manual",
      ...(options["host-binary"] ? { hostBinary: resolve(options["host-binary"]) } : {}),
    });
    console.info(JSON.stringify(result, null, 2));
    console.info(`[perf] ${options.suite}-${options.config}: ${describe(result)}`);
    if (!result.valid) process.exitCode = 1;
    break;
  }
  case "batch":
    assertBuilt();
    await batch();
    break;
  case "report":
    await report();
    break;
  default:
    console.error(
      "usage: app-perf.mjs build | seed-history | run --config A|B|C --suite <suite> | batch --out <dir> | report --out <dir>",
    );
    process.exitCode = 2;
}
