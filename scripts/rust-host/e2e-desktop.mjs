#!/usr/bin/env node
// Rust Host 的桌面端到端验证（docs/specs/rust-host-r2.md 第 6 节）。
//
// 每个场景启动一个与开发者环境完全隔离的 Electron 实例：Main 起 Rust Host，Agent 用 Rust CLI，
// 模型是本地 mock 的 OpenAI 兼容端点；经 CDP（私有端口）驱动界面。
// 不使用固定的 Vite 5174 / CDP 9229 端口，不读写 ~/.zcode，遥测端点置空。
//
// 用法：node scripts/rust-host/e2e-desktop.mjs [--scenario <name>]... [--skip-build] [--keep] [--explore]
//   --scenario    要运行的场景（scripts/rust-host/e2e/scenarios/<name>.mjs），可重复；默认 core
//   --skip-build  复用已有的 out/ 与 Rust 构建
//   --keep        结束后保留临时目录（截图、日志都在里面）
//   --explore     只启动并每隔几秒截图，便于查看界面
// 环境变量 ZCODE_E2E_CLI_RUST_BINARY 可指定 Rust CLI 可执行文件（默认用本仓库的 release 构建）。
//
// 场景模块导出：
//   run(context)           必需；context = { page, shots, model, workspace, home, temp, relaunch }
//   mockRules              可选；按顺序匹配的 mock 模型规则 { name, match(request), respond(request) }
//   seed(context)          可选；在启动 App 之前补充隔离 profile（context = { home, workspace, temp }）
//   env(context)           可选；返回追加给 Electron 进程树的环境变量（例如指向场景自己的 mock 后端）
import { spawn } from "node:child_process";
import { once } from "node:events";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { createRequire } from "node:module";
import { createServer as createNetServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { parseArgs } from "node:util";

import { lastUserText } from "./e2e/helpers.mjs";

const root = resolve(import.meta.dirname, "../..");
const desktop = join(root, "packages/desktop");
const require = createRequire(join(desktop, "package.json"));
const { values: options } = parseArgs({
  options: {
    scenario: { type: "string", multiple: true },
    "skip-build": { type: "boolean" },
    keep: { type: "boolean" },
    explore: { type: "boolean" },
  },
});

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

async function runScenario(name, cli) {
  const scenario = await import(`./e2e/scenarios/${name}.mjs`);
  const temp = await mkdtemp(join(tmpdir(), `zcode-rust-host-e2e-${name}-`));
  const home = join(temp, "home");
  const workspace = join(temp, "workspace");
  const shots = join(temp, "shots");
  await mkdir(shots, { recursive: true });
  console.info(`[e2e:${name}] temp dir: ${temp}`);
  const model = await startMockModel(workspace, scenario.mockRules ?? []);
  await seedProfile(home, workspace, model.baseUrl);
  await scenario.seed?.({ home, workspace, temp });
  // Host 启动即检查内置 Provider 配置的 CDN 更新（与 Node Host 相同）：ZCode 控制面默认指向本地
  // mock，不访问真实服务；场景可以用 env() 换成自己的 mock 后端。
  const env = {
    ZCODE_BASE_URL: new URL(model.baseUrl).origin,
    ...((await scenario.env?.({ home, workspace, temp })) ?? {}),
  };
  let app;
  let failed = false;
  try {
    app = await launchApp({ temp, home, cli, env });
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
      relaunch: async () => {
        await app.stop();
        app = await launchApp({ temp, home, cli, env });
        return app.page;
      },
    });
    console.info(`[e2e:${name}] passed`);
  } catch (error) {
    failed = true;
    console.error(`[e2e:${name}] failed:`, error);
  } finally {
    await app?.stop();
    model.server.close();
    if (!options.keep && !failed) await rm(temp, { recursive: true, force: true });
    else console.info(`[e2e:${name}] kept ${temp}`);
  }
  return !failed;
}

async function main() {
  const cli =
    process.env.ZCODE_E2E_CLI_RUST_BINARY?.trim() ||
    join(root, "apps/zcode-cli-rust/target/release/zcode-cli-rust");
  if (!existsSync(cli)) throw new Error(`Rust CLI release build missing: ${cli}`);
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
async function launchApp({ temp, home, cli, env = {} }) {
  const cdpPort = await freePort();
  const electronBinary = require("electron");
  const electron = spawn(electronBinary, [".", `--remote-debugging-port=${cdpPort}`], {
    cwd: desktop,
    env: {
      ...process.env,
      ZCODE_ENV: "production",
      ZCODE_DESKTOP_HOST: "rust",
      ZCODE_AGENT_SERVER_COMMAND: cli,
      ZCODE_AGENT_SERVER_RUNTIME: "zcode-cli-rust",
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
      ...env,
    },
    stdio: ["ignore", "inherit", "inherit"],
    detached: process.platform !== "win32",
  });
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
    return { page: await appPage(browser), stop };
  } catch (error) {
    await stop();
    throw error;
  }
}

await main();
