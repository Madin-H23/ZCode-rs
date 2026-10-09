#!/usr/bin/env node
// Rust Host 的桌面端到端验证（docs/specs/rust-host-r2.md 第 6 节）。
//
// 每个场景启动一个与开发者环境完全隔离的 Electron 实例：Main 起 Rust Host（调试构建），runtime 缺省
// 运行在 Host 进程内（docs/specs/rust-u3-single-process.md §2.2），模型是本地 mock 的 OpenAI 兼容端点；
// 经 CDP（私有端口）驱动界面。不使用固定的 Vite 5174 / CDP 9229 端口，不读写 ~/.zcode，遥测端点置空。
//
// 用法：node scripts/rust-host/e2e-desktop.mjs [--scenario <name>]... [--runtime embedded|child]
//                                             [--skip-build] [--keep] [--explore]
//   --scenario    要运行的场景（scripts/rust-host/e2e/scenarios/<name>.mjs），可重复；默认 core
//   --runtime     runtime 链路：embedded（缺省）= Host 的缺省链路，进程内 runtime，不需要 CLI 二进制，
//                 继承环境里的 ZCODE_AGENT_SERVER_* 一律清掉；child = 差分对照，经
//                 ZCODE_AGENT_SERVER_COMMAND 让 Host 为每个 workspace 启动 Rust CLI 子进程
//   --skip-build  复用已有的 out/ 与 Rust 构建
//   --keep        结束后保留临时目录（截图、日志都在里面）
//   --explore     只启动并每隔几秒截图，便于查看界面
// child 模式下环境变量 ZCODE_E2E_CLI_RUST_BINARY 可指定 Rust CLI 可执行文件（默认用本仓库的 release 构建，
// 需要先 cargo build --release）。
//
// 场景模块导出：
//   run(context)           必需；context = { page, shots, model, workspace, home, temp, runtime,
//                          mainPid, relaunch, secondInstance }
//   mockRules              可选；按顺序匹配的 mock 模型规则 { name, match(request), respond(request) }
//   seed(context)          可选；在启动 App 之前补充隔离 profile（context = { home, workspace, temp, runtime }）
//   env(context)           可选；返回追加给 App 进程树的环境变量（不能覆盖隔离用的 HOME 等键）
//   args(context)          可选；返回追加给 Electron 的命令行参数
// context.runtime 是 "embedded" 或 "child"；context.mainPid() 是当前 Electron Main 的 pid（relaunch 后会变），
// 进程类断言从它向下找 Rust Host（e2e/resilience-helpers.mjs）。
// run(context) 另可用 context.secondInstance(args)：以同一隔离环境再启动一次 App，
// 由单实例锁把 argv（例如 zcode:// deep link）转交给已运行的实例后退出。
import { spawn } from "node:child_process";
import { once } from "node:events";
import { createWriteStream, existsSync } from "node:fs";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { createRequire } from "node:module";
import { createServer as createNetServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { parseArgs } from "node:util";

import { lastUserText } from "./e2e/helpers.mjs";
import { hostLogLines, runtimeProcessesUnder } from "./e2e/resilience-helpers.mjs";

const root = resolve(import.meta.dirname, "../..");
const desktop = join(root, "packages/desktop");
const require = createRequire(join(desktop, "package.json"));
const { values: options } = parseArgs({
  options: {
    scenario: { type: "string", multiple: true },
    "skip-build": { type: "boolean" },
    keep: { type: "boolean" },
    explore: { type: "boolean" },
    runtime: { type: "string", default: "embedded" },
  },
});
if (!["embedded", "child"].includes(options.runtime))
  throw new Error(`--runtime must be embedded or child, got ${options.runtime}`);

function run(command, args, cwd, env = process.env) {
  return new Promise((done, fail) => {
    const child = spawn(command, args, { cwd, env, stdio: "inherit" });
    child.once("error", fail);
    child.once("exit", (code) =>
      code === 0 ? done() : fail(new Error(`${command} ${args.join(" ")} exited ${code}`)),
    );
  });
}

async function freePort() {
  const server = createNetServer();
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const { port } = server.address();
  server.close();
  return port;
}

/** 离线的 ZCode 控制面：所有请求 404（JSON），等同于控制面不可用。 */
async function startOfflineControlPlane() {
  const server = createServer((request, response) => {
    request.resume();
    response.writeHead(404, { "content-type": "application/json" });
    response.end(JSON.stringify({ code: 404, msg: "offline control plane" }));
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  return { server, origin: `http://127.0.0.1:${server.address().port}` };
}

/** mock 模型：每个请求按场景规则顺序匹配，第一条命中的规则负责应答。 */
async function startMockModel(workspace, rules) {
  const requests = [];
  const server = createServer((request, response) => {
    let body = "";
    request.on("data", (chunk) => (body += chunk));
    request.on("end", async () => {
      requests.push({ url: request.url, body });
      let parsed = {};
      try {
        parsed = JSON.parse(body);
      } catch {
        // 非 JSON 请求交给兜底规则
      }
      const messages = parsed.messages ?? [];
      const context = {
        request,
        response,
        parsed,
        messages,
        text: lastUserText(messages),
        hasTools: Array.isArray(parsed.tools) && parsed.tools.length > 0,
        workspace,
        sleep,
      };
      const rule = rules.find((candidate) => candidate.match(context));
      response.writeHead(200, { "content-type": "text/event-stream" });
      if (!rule) {
        response.end("data: [DONE]\n\n");
        return;
      }
      await rule.respond(context);
    });
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  return { server, requests, baseUrl: `http://127.0.0.1:${server.address().port}/v1` };
}

async function seedProfile(home, workspace, baseUrl) {
  const config = join(home, ".zcode", "v2");
  await mkdir(config, { recursive: true });
  await writeFile(
    join(config, "provider_config.json"),
    JSON.stringify({
      schemaVersion: 1,
      config: {
        providerOrder: ["mock-provider"],
        providerConfigRules: {
          providerRules: [
            {
              providerId: "mock-provider",
              providerName: "Mock",
              config: {
                group: "standard-personal",
                access: { type: "api-key", apiKey: "sk-e2e" },
                api: { type: "openai-chat-completions", baseUrl },
                personalModelIds: ["mock-model", "mock-model-2"],
                modelOrder: ["mock-model", "mock-model-2"],
              },
            },
          ],
        },
        modelConfigRules: {
          providerModelRules: [
            { providerId: "mock-provider", modelId: "mock-model", config: { enabled: true } },
            { providerId: "mock-provider", modelId: "mock-model-2", config: { enabled: true } },
          ],
          manualProviderModelRules: [],
        },
      },
    }),
  );
  await writeFile(
    join(config, "setting.json"),
    JSON.stringify({
      lastWorkspaceSession: [
        { kind: "local", workspacePath: workspace, workspacePurpose: "project" },
      ],
      lastActiveTabIndex: 0,
      // 没有 providerFamilyDomain 时启动门禁会一直停在登录页（useProviderAvailabilityLoginEntryGuard）。
      providerFamilyDomain: "zai",
      providerFamilyDomainMigrated: true,
    }),
  );
  // 已关闭过新手引导的本机记录（未登录身份）：Host 的 onboarding-record 据此不再弹出引导，
  // 与老用户启动时一致。需要验证引导的场景可在 seed() 里删除或改写它。
  await writeFile(
    join(config, "onboarding-record.json"),
    JSON.stringify({
      version: 2,
      deviceMid: "e2e-device",
      entries: [],
      decisions: [
        {
          userId: null,
          status: "dismissed",
          reason: "user_closed",
          decidedAt: new Date().toISOString(),
        },
      ],
    }),
  );
  await mkdir(workspace, { recursive: true });
  await writeFile(join(workspace, "README.md"), "# e2e workspace\n");
  // 一个已提交的 git 仓库，供 Git 状态与改动面板使用；配置只在临时 HOME 内生效。
  const gitEnv = { ...process.env, HOME: home, GIT_CONFIG_NOSYSTEM: "1" };
  await run("git", ["init", "-q", "-b", "main"], workspace, gitEnv);
  await run("git", ["add", "."], workspace, gitEnv);
  await run(
    "git",
    ["-c", "user.name=e2e", "-c", "user.email=e2e@example.invalid", "commit", "-qm", "init"],
    workspace,
    gitEnv,
  );
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

async function appPage(browser) {
  for (let attempt = 0; attempt < 120; attempt += 1) {
    for (const context of browser.contexts()) {
      for (const page of context.pages()) {
        if (page.url().includes("index.html")) return page;
      }
    }
    await sleep(500);
  }
  throw new Error("renderer page not found");
}

/**
 * 本次运行确实走了选定的 runtime 链路。Host 每启动一个 runtime 实例写一行
 * `agent[<key>#<代次>] started pid=<pid>`（经 Main 落盘），嵌入式实例没有 pid：
 *   embedded  没有一行带 pid，Electron Main 的进程树里也没有 CLI 进程；
 *   child     至少一行带 pid（启动时的预热就会拉起一个）。
 */
async function assertRuntimeLink(runtime, home, mainPid) {
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

async function runScenario(name, cli) {
  const runtime = options.runtime;
  const scenario = await import(`./e2e/scenarios/${name}.mjs`);
  const temp = await mkdtemp(join(tmpdir(), `zcode-rust-host-e2e-${name}-`));
  const home = join(temp, "home");
  const workspace = join(temp, "workspace");
  const shots = join(temp, "shots");
  await mkdir(shots, { recursive: true });
  console.info(`[e2e:${name}] temp dir: ${temp}`);
  const model = await startMockModel(workspace, scenario.mockRules ?? []);
  await seedProfile(home, workspace, model.baseUrl);
  await scenario.seed?.({ home, workspace, temp, runtime });
  // Host 启动即检查内置 Provider 配置的 CDN 更新（与 Node Host 相同）：ZCode 控制面默认指向本地
  // 一律 404 的桩，不访问真实服务；场景可以用 env() 换成自己的 mock 后端。
  const controlPlane = await startOfflineControlPlane();
  const env = {
    ZCODE_BASE_URL: controlPlane.origin,
    ...(await scenario.env?.({ home, workspace, temp, runtime })),
  };
  const args = (await scenario.args?.({ home, workspace, temp, runtime })) ?? [];
  let app;
  let failed = false;
  try {
    app = await launchApp({ temp, home, cli, env, args });
    if (options.explore) {
      for (let index = 0; index < 8; index += 1) {
        await sleep(4000);
        await app.page.screenshot({ path: join(shots, `explore-${index}.png`) });
      }
      console.info(`[e2e:${name}] screenshots in ${shots}`);
      return true;
    }
    await scenario.run({
      page: app.page,
      shots,
      model,
      workspace,
      home,
      temp,
      runtime,
      mainPid: () => app.pid,
      secondInstance: (extraArgs) => app.secondInstance(extraArgs),
      relaunch: async () => {
        await app.stop();
        app = await launchApp({ temp, home, cli, env, args });
        return app.page;
      },
    });
    await assertRuntimeLink(runtime, home, app.pid);
    console.info(`[e2e:${name}] passed`);
  } catch (error) {
    failed = true;
    console.error(`[e2e:${name}] failed:`, error);
    // 失败现场：截图与页面文本留在临时目录，便于区分“界面卡在哪一步”。
    await app?.page.screenshot({ path: join(shots, "failure.png") }).catch(() => undefined);
    const text = await app?.page.evaluate(() => document.body.innerText).catch(() => undefined);
    if (text !== undefined) await writeFile(join(shots, "failure.txt"), text);
  } finally {
    await app?.stop();
    model.server.close();
    controlPlane.server.close();
    if (!options.keep && !failed) await rm(temp, { recursive: true, force: true });
    else console.info(`[e2e:${name}] kept ${temp}`);
  }
  return !failed;
}

async function main() {
  // embedded 模式不需要 CLI 二进制；child 模式缺少时直接失败，不回退到嵌入式。
  let cli = null;
  if (options.runtime === "child") {
    cli =
      process.env.ZCODE_E2E_CLI_RUST_BINARY?.trim() ||
      join(root, "apps/zcode-cli-rust/target/release/zcode-cli-rust");
    if (!existsSync(cli)) throw new Error(`Rust CLI release build missing: ${cli}`);
  }
  console.info(`[e2e] runtime: ${options.runtime}${cli ? ` (${cli})` : ""}`);
  if (!options["skip-build"]) {
    await run("cargo", ["build"], join(root, "apps/zcode-host"));
    await run("node", ["scripts/ensure-local-runtime-assets.mjs"], desktop);
    await run("pnpm", ["exec", "tsup"], desktop, { ...process.env, NODE_ENV: "production" });
    await run("pnpm", ["exec", "vite", "build"], desktop, {
      ...process.env,
      NODE_ENV: "production",
    });
  }
  const scenarios = options.scenario?.length ? options.scenario : ["core"];
  const failed = [];
  for (const name of scenarios) {
    if (!(await runScenario(name, cli))) failed.push(name);
  }
  if (failed.length > 0) {
    console.error(`[e2e] failed scenarios: ${failed.join(", ")}`);
    process.exitCode = 1;
  } else {
    console.info(`[e2e] all scenarios passed: ${scenarios.join(", ")}`);
  }
}

/** 启动一个隔离的 Electron 实例并连上它的 renderer 页面。 */
async function launchApp({ temp, home, cli, env = {}, args = [] }) {
  const cdpPort = await freePort();
  const electronBinary = require("electron");
  // runtime 链路只由 --runtime 决定：先清掉开发者环境里可能残留的选择变量。
  const inherited = Object.fromEntries(
    Object.entries(process.env).filter(([key]) => !key.startsWith("ZCODE_AGENT_SERVER_")),
  );
  const appEnv = {
    ...inherited,
    ...env,
    ZCODE_ENV: "production",
    ZCODE_DESKTOP_HOST: "rust",
    ...(cli
      ? { ZCODE_AGENT_SERVER_COMMAND: cli, ZCODE_AGENT_SERVER_RUNTIME: "zcode-cli-rust" }
      : {}),
    // Main 直接用 os.homedir() 读 setting.json 并据此下发预热 workspace；不覆盖 HOME 就会
    // 读到开发者真实的最近项目，并在那些目录里启动 CLI。所以整个进程树都换成临时 HOME。
    HOME: home,
    USERPROFILE: home,
    ZCODE_DATA_BASE_DIR: home,
    ZCODE_DESKTOP_HOME_DIR: home,
    ZCODE_CLI_RUST_DATA_DIR: join(home, ".zcode", "rust"),
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
  const electron = spawn(electronBinary, [".", `--remote-debugging-port=${cdpPort}`, ...args], {
    cwd: desktop,
    env: appEnv,
    stdio: ["ignore", "inherit", "inherit"],
    detached: process.platform !== "win32",
  });
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
  const stop = async () => {
    signal("SIGTERM");
    const timedOut = await Promise.race([exited.then(() => false), sleep(10_000).then(() => true)]);
    if (timedOut) {
      signal("SIGKILL");
      await exited;
    }
  };
  try {
    await waitForCdp(cdpPort, electron);
    const { chromium } = require("playwright-core");
    const browser = await chromium.connectOverCDP(`http://127.0.0.1:${cdpPort}`);
    const page = await appPage(browser);
    // Renderer console 落到临时目录：失败时能看到 UI 自己的判断日志（CDP 接上之前的输出会缺失）。
    const consoleLog = createWriteStream(join(temp, "renderer-console.log"), { flags: "a" });
    page.on("console", (message) => {
      consoleLog.write(`[${new Date().toISOString()}] [${message.type()}] ${message.text()}\n`);
    });
    exited.then(() => consoleLog.end());
    return { page, stop, secondInstance, pid: electron.pid };
  } catch (error) {
    await stop();
    throw error;
  }
}

await main();
