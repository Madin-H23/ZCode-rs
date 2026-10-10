// 桌面 E2E harness（scripts/rust-host/e2e-desktop.mjs）的 App 进程层：启动隔离的 Electron 实例、
// 连接 renderer（CDP）与 Main（inspector），正常退出，以及每个场景结束时的进程表与退出断言。
import { spawn } from "node:child_process";
import { once } from "node:events";
import { createWriteStream } from "node:fs";
import { readdir, readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { createServer as createNetServer } from "node:net";
import { join, resolve } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

import { rustDataDir } from "./ledger-helpers.mjs";
import { connectMainInspector } from "./main-process.mjs";
import {
  descendantsOf,
  hostLogLines,
  hostsOf,
  processTable,
  runtimeProcessesUnder,
} from "./resilience-helpers.mjs";

const root = resolve(import.meta.dirname, "../../..");
const desktop = join(root, "packages/desktop");
const require = createRequire(join(desktop, "package.json"));

/**
 * 正常退出时等 Electron 进程结束的上限。Main 自己的退出流程（dispose Host → app.exit）在预算（9 s，
 * appShutdownPolicy.ts）内完成，由日志断言；之后进程的系统级收尾在这台共享机器上（外接盘、高负载）
 * 偶尔要 5–15 s（采样时进程已没有线程），所以这里放宽到 60 s，只把“没退出”当失败。
 */
const QUIT_BUDGET_MS = 60_000;
export async function freePort() {
  const server = createNetServer();
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const { port } = server.address();
  server.close();
  return port;
}

async function waitForCdp(port, child) {
  for (let attempt = 0; attempt < 240; attempt += 1) {
    if (child.exitCode !== null) throw new Error(`Electron exited early (${child.exitCode})`);
    try {
      const response = await fetch(`http://127.0.0.1:${port}/json/version`);
      if (response.ok) return;
    } catch {
      // 还没监听
    }
    await sleep(500);
  }
  throw new Error("CDP endpoint did not come up");
}

/** 当前所有主窗口的 renderer 页面（加载 index.html 的页面），按 CDP 发现顺序。 */
function mainPages(browser) {
  return browser
    .contexts()
    .flatMap((context) => context.pages())
    .filter((page) => !page.isClosed() && page.url().includes("index.html"));
}

async function appPage(browser) {
  for (let attempt = 0; attempt < 120; attempt += 1) {
    const [page] = mainPages(browser);
    if (page) return page;
    await sleep(500);
  }
  throw new Error("renderer page not found");
}

/** Electron utility 进程里以 Node 运行的那一类（Node scheduler；Node Host 启动初期也是这种命令行）。 */
const NODE_UTILITY = /--type=utility.*--utility-sub-type=node\.mojom\.NodeService/;
/** Node Host 加载后把进程标题改成 `zcode-host-local-<窗口>`（packages/desktop/src/host/index.ts）。 */
const NODE_HOST = /^zcode-host-local-\d+\s*$/;

/**
 * 每个场景结束时的进程表断言（docs/specs/rust-u4-app-host.md §6）：从 harness 拉起的 Electron Main
 * 向下看。Rust 开关下恰好一个 Rust Host、没有任何以 Node 运行的 utility 进程（Node Host 与 Node
 * scheduler 都是这种进程）；嵌入式 runtime 下也没有 CLI 子进程（assertRuntimeLink 已查）。
 */
export async function assertAppProcesses(host, mainPid) {
  const table = await processTable();
  const descendants = descendantsOf(table, mainPid);
  const rustHosts = await hostsOf(mainPid);
  const nodeHosts = descendants.filter(
    (row) => row.ppid === mainPid && NODE_HOST.test(row.command.trim()),
  );
  const nodeUtilities = descendants.filter((row) => NODE_UTILITY.test(row.command));
  const describe = (rows) =>
    rows.map((row) => `${row.pid} ${row.command.slice(0, 200)}`).join("; ");
  if (host === "rust") {
    if (rustHosts.length !== 1)
      throw new Error(
        `expected exactly one Rust Host under Main ${mainPid}, found ${rustHosts.length}: ${describe(rustHosts)}`,
      );
    if (nodeHosts.length > 0 || nodeUtilities.length > 0)
      throw new Error(
        `Rust Host run has Node Host / scheduler processes: ${describe([...nodeHosts, ...nodeUtilities])}`,
      );
  } else {
    if (rustHosts.length > 0)
      throw new Error(`Node Host run started a Rust Host: ${describe(rustHosts)}`);
    if (nodeHosts.length === 0) throw new Error(`no Node Host under Main ${mainPid}`);
  }
  console.info(
    `[e2e] process table (${host}): ${rustHosts.length} Rust Host, ${nodeHosts.length} Node Host, ${nodeUtilities.length} other Node utility (scheduler) under Main ${mainPid}`,
  );
}

/**
 * App 正常退出（SIGTERM 只发给 Main，与 Dock 退出同一条 before-quit 路径）之后：Main 在预算内退出，
 * 它拉起的进程一个不剩；Rust Host 收到了 dispose（而不是被强杀）。
 */
/** Main 日志行开头的本地时间戳（`[2026-10-09 21:23:39.426]`）→ 毫秒。 */
export function logTime(line) {
  const match = line?.match(/^\[(\d{4}-\d{2}-\d{2}) (\d{2}:\d{2}:\d{2}\.\d{3})\]/);
  return match ? new Date(`${match[1]}T${match[2]}`).getTime() : Number.NaN;
}

export async function assertCleanQuit(host, home, quit) {
  if (!quit.graceful) throw new Error(`the app did not quit within ${quit.waitedMs}ms`);
  if (quit.leftovers.length > 0)
    throw new Error(
      `processes left after quitting: ${quit.leftovers
        .map((row) => `${row.pid} ${row.command.slice(0, 160)}`)
        .join("; ")}`,
    );
  const lines = await hostLogLines(home);
  const quitStart = lines.findLastIndex((line) => line.includes("[app-quit] forceQuit enabled"));
  const exiting = lines.findLastIndex((line) => line.includes("[app-quit] exiting prepared app"));
  if (quitStart < 0 || exiting < quitStart)
    throw new Error("Main did not finish its quit sequence (no 'exiting prepared app')");
  const mainQuitMs = logTime(lines[exiting]) - logTime(lines[quitStart]);
  if (host === "rust") {
    const disposing = lines.findLastIndex((line) => line.includes("disposing the app host"));
    if (disposing < quitStart) throw new Error("Main never disposed the app host on quit");
    if (!lines.slice(disposing).some((line) => line.includes("dispose received; exiting")))
      throw new Error("the Rust Host did not exit through dispose on quit");
  }
  console.info(
    `[e2e] clean quit: Main quit sequence ${mainQuitMs}ms, process gone after ${quit.waitedMs}ms, no leftover processes`,
  );
}

/**
 * Host 日志里的异常报告（docs/specs/rust-u2-gateway.md §2.3、§2.4，rust-u2-task-scope.md）：任务 panic
 * （作用域上报 `task panicked: scope=…`，以及 panic hook 的 `panic: panicked at …`）、出站积压越过上限而
 * 关闭连接、退出时 runtime 任务没有收口而被中止（`aborting, still running`）；runtime 的 JSONL 日志里的
 * `task.panicked` 与关闭时仍在运行而被中止的任务。这些都不该在正常场景里出现；
 * 场景故意制造的（例如故障注入的 panic）用模块导出 `expectedHostLog`（正则数组）声明。
 */
const HOST_LOG_ALARM = /panicked|outbound backlog exceeded|hard limit|still running|aborting/;
export async function assertHostLogClean(home, expected = []) {
  // 一条日志可能跨多行（panic 的消息在下一行）：按行首时间戳切成条目再判断。
  const entries = [];
  for (const line of await hostLogLines(home)) {
    if (/^\[\d{4}-\d{2}-\d{2} /.test(line) || entries.length === 0) entries.push(line);
    else entries[entries.length - 1] += `\n${line}`;
  }
  // runtime 自己的 JSONL 日志（嵌入式与子进程两种链路都写在隔离 HOME 里）：任务 panic、关闭时被中止的任务。
  const runtimeDir = join(home, ".zcode", "cli", "log");
  const runtimeNames = (await readdir(runtimeDir).catch(() => [])).filter((name) =>
    name.endsWith(".jsonl"),
  );
  const runtimeLines = (
    await Promise.all(runtimeNames.map((name) => readFile(join(runtimeDir, name), "utf8")))
  )
    .join("\n")
    .split("\n")
    .filter((line) => /"task\.panicked"|still running/.test(line));
  const alarms = [
    ...entries.filter((entry) => entry.includes("[host-log]") && HOST_LOG_ALARM.test(entry)),
    ...runtimeLines,
  ].filter((entry) => !expected.some((pattern) => pattern.test(entry)));
  if (alarms.length > 0)
    throw new Error(
      `Host log reports problems:\n${alarms.map((entry) => entry.slice(0, 600)).join("\n")}`,
    );
  console.info(
    "[e2e] Host and runtime logs: no panics, no outbound hard-limit closes, no aborted runtime tasks",
  );
}

/**
 * 本次运行确实走了选定的 runtime 链路。Host 每启动一个 runtime 实例写一行
 * `agent[<key>#<代次>] started pid=<pid>`（经 Main 落盘），嵌入式实例没有 pid：
 *   embedded  没有一行带 pid，Electron Main 的进程树里也没有 CLI 进程；
 *   child     至少一行带 pid（启动时的预热就会拉起一个）。
 */
export async function assertRuntimeLink(runtime, home, mainPid) {
  const lines = (await hostLogLines(home)).filter((line) => /agent\[.*\] started pid=/.test(line));
  const withPid = lines.filter((line) => /started pid=\d+/.test(line));
  if (runtime === "embedded") {
    if (withPid.length > 0)
      throw new Error(`embedded run started CLI processes:\n${withPid.join("\n")}`);
    const processes = mainPid ? await runtimeProcessesUnder(mainPid) : [];
    if (processes.length > 0)
      throw new Error(
        `embedded run has CLI processes: ${processes.map((row) => row.command).join("; ")}`,
      );
  } else if (withPid.length === 0) {
    throw new Error("child run never started a CLI process");
  }
  console.info(
    `[e2e] runtime link ${runtime}: ${lines.length} runtime starts, ${withPid.length} with a pid`,
  );
}

/**
 * 启动一个隔离的 Electron 实例并连上它的 renderer 页面。`output` 是 Electron stdout / stderr 的去向
 * （缺省继承 harness 的终端；性能测量传一个文件描述符，免得终端输出本身成为负载）。
 */
export async function launchApp({
  host,
  temp,
  home,
  cli,
  env = {},
  args = [],
  output = "inherit",
}) {
  const cdpPort = await freePort();
  const electronBinary = require("electron");
  // runtime 链路只由 --runtime 决定：先清掉开发者环境里可能残留的选择变量。
  const inherited = Object.fromEntries(
    Object.entries(process.env).filter(([key]) => !key.startsWith("ZCODE_AGENT_SERVER_")),
  );
  // Node Host（--host node）是今天发布的组合：不设 ZCODE_DESKTOP_HOST，Node Host 按自己的规则找 agent
  // runtime（本仓库的 TS CLI 构建，apps/zcode-cli/packages/cli/dist/zcode.cjs）。
  if (host === "node") delete inherited.ZCODE_DESKTOP_HOST;
  const appEnv = {
    ...inherited,
    ...env,
    ZCODE_ENV: "production",
    ...(host === "rust" ? { ZCODE_DESKTOP_HOST: "rust" } : {}),
    ...(cli
      ? { ZCODE_AGENT_SERVER_COMMAND: cli, ZCODE_AGENT_SERVER_RUNTIME: "zcode-cli-rust" }
      : {}),
    // Main 直接用 os.homedir() 读 setting.json 并据此下发预热 workspace；不覆盖 HOME 就会
    // 读到开发者真实的最近项目，并在那些目录里启动 CLI。所以整个进程树都换成临时 HOME。
    HOME: home,
    USERPROFILE: home,
    ZCODE_DATA_BASE_DIR: home,
    ZCODE_DESKTOP_HOME_DIR: home,
    ZCODE_CLI_RUST_DATA_DIR: rustDataDir(home),
    ZCODE_DESKTOP_USER_DATA_DIR: join(temp, "electron"),
    ZCODE_DESKTOP_SESSION_DATA_DIR: join(temp, "electron-session"),
    ZCODE_DESKTOP_APPLICATION_NAME: "ZCode Rust Host E2E",
    ZCODE_DISABLE_FIXED_REMOTE_DEBUGGING_PORT: "1",
    // 测试实例不对外上报：遥测、ARMS 与 OTLP 端点一律置空（显式空值覆盖 dotenv）。
    ZCODE_TELEMETRY_REPORT_ENDPOINT: "",
    ZCODE_ARMS_RUM_ENDPOINT: "",
    OTEL_EXPORTER_OTLP_ENDPOINT: "",
    OTEL_EXPORTER_OTLP_TRACES_ENDPOINT: "",
    ELECTRON_RENDERER_URL: "",
  };
  // Main 的 inspector 只供场景执行 Main 侧动作（销毁窗口、没有窗口时重新打开），见 e2e/main-process.mjs。
  const inspectPort = await freePort();
  const electron = spawn(
    electronBinary,
    [".", `--remote-debugging-port=${cdpPort}`, `--inspect=127.0.0.1:${inspectPort}`, ...args],
    {
      cwd: desktop,
      env: appEnv,
      stdio: ["ignore", output, output],
      detached: process.platform !== "win32",
    },
  );
  const secondInstance = async (extraArgs) => {
    const child = spawn(electronBinary, [".", ...extraArgs], {
      cwd: desktop,
      env: appEnv,
      stdio: ["ignore", "inherit", "inherit"],
    });
    const done = once(child, "exit");
    const timedOut = await Promise.race([done.then(() => false), sleep(20_000).then(() => true)]);
    if (timedOut) child.kill("SIGKILL");
  };
  const exited = once(electron, "exit");
  const signal = (name) => {
    if (electron.exitCode !== null || electron.signalCode !== null || !electron.pid) return;
    try {
      process.kill(process.platform === "win32" ? electron.pid : -electron.pid, name);
    } catch {
      electron.kill(name);
    }
  };
  let main;
  /**
   * 正常退出：SIGTERM 只发给 Main（Electron 转成 app.quit，走 before-quit 的 Host 清理），等它退出；
   * 再看它拉起的进程（同一进程组）是否都已结束。超时或有残留时整组强杀，结果交给调用方判断。
   */
  const stop = async () => {
    const startedAt = Date.now();
    main?.close();
    if (electron.exitCode !== null || electron.signalCode !== null || !electron.pid)
      return { graceful: true, leftovers: [], waitedMs: 0 };
    try {
      process.kill(electron.pid, "SIGTERM");
    } catch {
      // 已经退出
    }
    // 退出前记下 Main 的全部后代：退出后它们会被收养（ppid 变成 1），CLI 还可能在自己的进程组里。
    const before =
      process.platform === "win32" ? [] : descendantsOf(await processTable(), electron.pid);
    const graceful = await Promise.race([
      exited.then(() => true),
      sleep(QUIT_BUDGET_MS).then(() => false),
    ]);
    const waitedMs = Date.now() - startedAt;
    // 残留：退出前的后代仍存活（同一 pid、同一命令行），或仍在 Main 的进程组里。
    const after = process.platform === "win32" ? [] : await processTable();
    const leftovers = after.filter(
      (row) =>
        row.pgid === electron.pid ||
        before.some((old) => old.pid === row.pid && old.command === row.command),
    );
    if (!graceful || leftovers.length > 0) {
      signal("SIGTERM");
      const timedOut = await Promise.race([
        exited.then(() => false),
        sleep(10_000).then(() => true),
      ]);
      if (timedOut) signal("SIGKILL");
      for (const row of leftovers) {
        try {
          process.kill(row.pid, "SIGKILL");
        } catch {
          // 已经退出
        }
      }
      await exited;
    }
    return { graceful, leftovers, waitedMs };
  };
  try {
    await waitForCdp(cdpPort, electron);
    const { chromium } = require("playwright-core");
    const browser = await chromium.connectOverCDP(`http://127.0.0.1:${cdpPort}`);
    const page = await appPage(browser);
    main = await connectMainInspector(inspectPort);
    // Renderer console 落到临时目录：失败时能看到 UI 自己的判断日志（CDP 接上之前的输出会缺失）。
    const consoleLog = createWriteStream(join(temp, "renderer-console.log"), { flags: "a" });
    page.on("console", (message) => {
      consoleLog.write(`[${new Date().toISOString()}] [${message.type()}] ${message.text()}\n`);
    });
    exited.then(() => consoleLog.end());
    // 之后出现的主窗口（ZCODE_DEV_EXTRA_MAIN_WINDOW、重新打开）同样记录 console。
    browser.contexts().forEach((context) =>
      context.on("page", (other) =>
        other.on("console", (message) => {
          consoleLog.write(
            `[${new Date().toISOString()}] [${message.type()}] [${other.url().slice(-40)}] ${message.text()}\n`,
          );
        }),
      ),
    );
    return {
      page,
      browser,
      pages: () => mainPages(browser),
      main,
      stop,
      secondInstance,
      pid: electron.pid,
      // 性能测量（scripts/rust-host/perf/stream-cpu.mjs）要绕开 Playwright 直接连 CDP 与 Main inspector。
      cdpPort,
      inspectPort,
    };
  } catch (error) {
    await stop();
    throw error;
  }
}
