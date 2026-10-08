#!/usr/bin/env node
// Rust Host 的桌面端到端验证（docs/specs/rust-host-r2.md 第 5 节）。
//
// 启动一个与开发者环境完全隔离的 Electron 实例：Main 起 Rust Host，Agent 用 Rust CLI，
// 模型是本地 mock 的 OpenAI 兼容端点；经 CDP（私有端口）驱动界面走一遍核心聊天流程。
// 不使用固定的 Vite 5174 / CDP 9229 端口，不读写 ~/.zcode。
//
// 用法：node scripts/rust-host/e2e-desktop.mjs [--skip-build] [--keep] [--explore]
//   --skip-build  复用已有的 out/ 与 Rust 构建
//   --keep        结束后保留临时目录（截图、日志都在里面）
//   --explore     只启动并每隔几秒截图，便于查看界面
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

const root = resolve(import.meta.dirname, "../..");
const desktop = join(root, "packages/desktop");
const require = createRequire(join(desktop, "package.json"));
const { values: options } = parseArgs({
  options: {
    "skip-build": { type: "boolean" },
    keep: { type: "boolean" },
    explore: { type: "boolean" },
  },
});

export const MOCK_REPLY = "Hello from the Rust Host mock model";

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

export const WRITTEN_REPLY = "The mock model wrote the file";

function sseChunk(delta, finish) {
  return `data: ${JSON.stringify({
    id: "mock",
    object: "chat.completion.chunk",
    created: 0,
    model: "mock-model",
    choices: [{ index: 0, delta, finish_reason: finish }],
    ...(finish ? { usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 } } : {}),
  })}\n\n`;
}

function lastUserText(messages) {
  const user = [...messages].reverse().find((message) => message.role === "user");
  return typeof user?.content === "string" ? user.content : JSON.stringify(user?.content ?? "");
}

/**
 * 按最后一条用户消息选择场景（带 tools 的主对话请求才会走工具场景）：
 * - 含 "write file"：先回一个 Write 工具调用（需要用户批准），工具结果回来后回 WRITTEN_REPLY；
 * - 含 "long task"：慢速流，用来测 Stop；
 * - 其他：一次性回 MOCK_REPLY。
 */
async function startMockModel(workspace) {
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
        // 非 JSON 请求按默认场景回答
      }
      const messages = parsed.messages ?? [];
      const hasTools = Array.isArray(parsed.tools) && parsed.tools.length > 0;
      const text = lastUserText(messages);
      response.writeHead(200, { "content-type": "text/event-stream" });
      if (hasTools && messages.at(-1)?.role === "tool") {
        response.end(
          sseChunk({ role: "assistant", content: WRITTEN_REPLY }, null) +
            sseChunk({}, "stop") +
            "data: [DONE]\n\n",
        );
        return;
      }
      if (hasTools && text.includes("write file")) {
        const call = {
          index: 0,
          id: "call_write_1",
          type: "function",
          function: {
            name: "Write",
            arguments: JSON.stringify({
              file_path: join(workspace, "hello.txt"),
              content: "hi from e2e\n",
            }),
          },
        };
        response.end(
          sseChunk({ role: "assistant", tool_calls: [call] }, null) +
            sseChunk({}, "tool_calls") +
            "data: [DONE]\n\n",
        );
        return;
      }
      if (hasTools && text.includes("long task")) {
        response.write(sseChunk({ role: "assistant", content: "Working" }, null));
        for (let index = 0; index < 300 && !response.destroyed; index += 1) {
          await sleep(200);
          response.write(sseChunk({ content: "." }, null));
        }
        response.end(sseChunk({}, "stop") + "data: [DONE]\n\n");
        return;
      }
      response.end(
        sseChunk({ role: "assistant", content: MOCK_REPLY }, null) +
          sseChunk({}, "stop") +
          "data: [DONE]\n\n",
      );
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

async function main() {
  const temp = await mkdtemp(join(tmpdir(), "zcode-rust-host-e2e-"));
  const home = join(temp, "home");
  const workspace = join(temp, "workspace");
  const shots = join(temp, "shots");
  await mkdir(shots, { recursive: true });
  console.info(`[e2e] temp dir: ${temp}`);
  const cli = join(root, "apps/zcode-cli-rust/target/release/zcode-cli-rust");
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

  const model = await startMockModel(workspace);
  await seedProfile(home, workspace, model.baseUrl);
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
    },
    stdio: ["ignore", "inherit", "inherit"],
    detached: process.platform !== "win32",
  });
  const stopElectron = () => {
    if (electron.exitCode !== null || !electron.pid) return;
    try {
      process.kill(process.platform === "win32" ? electron.pid : -electron.pid, "SIGTERM");
    } catch {
      electron.kill("SIGTERM");
    }
  };
  let failed = false;
  try {
    await waitForCdp(cdpPort, electron);
    const { chromium } = require("playwright-core");
    const browser = await chromium.connectOverCDP(`http://127.0.0.1:${cdpPort}`);
    const page = await appPage(browser);
    if (options.explore) {
      for (let index = 0; index < 8; index += 1) {
        await sleep(4000);
        await page.screenshot({ path: join(shots, `explore-${index}.png`) });
      }
      console.info(`[e2e] screenshots in ${shots}`);
      return;
    }
    const { runCoreFlow } = await import("./e2e-desktop-flow.mjs");
    await runCoreFlow({
      page,
      shots,
      model,
      workspace,
      mockReply: MOCK_REPLY,
      writtenReply: WRITTEN_REPLY,
    });
    console.info("[e2e] core flow passed");
  } catch (error) {
    failed = true;
    console.error("[e2e] failed:", error);
  } finally {
    stopElectron();
    await sleep(1500);
    model.server.close();
    if (!options.keep && !failed) await rm(temp, { recursive: true, force: true });
    else console.info(`[e2e] kept ${temp}`);
  }
  if (failed) process.exitCode = 1;
}

await main();
