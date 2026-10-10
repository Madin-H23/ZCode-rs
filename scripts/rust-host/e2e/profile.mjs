// 桌面 E2E harness（scripts/rust-host/e2e-desktop.mjs）的隔离环境：离线控制面、本地 mock 模型、
// 隔离 HOME 里的 profile 与 workspace。App 级性能测量（scripts/rust-host/perf/）复用同一套。
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdir, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

import { lastUserText } from "./helpers.mjs";

export function runCommand(command, args, cwd, env = process.env) {
  return new Promise((done, fail) => {
    const child = spawn(command, args, { cwd, env, stdio: "inherit" });
    child.once("error", fail);
    child.once("exit", (code) =>
      code === 0 ? done() : fail(new Error(`${command} ${args.join(" ")} exited ${code}`)),
    );
  });
}

/** 离线的 ZCode 控制面：所有请求 404（JSON），等同于控制面不可用。 */
export async function startOfflineControlPlane() {
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
export async function startMockModel(workspace, rules) {
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

/** 一个已提交的 git 仓库，供 Git 状态与改动面板使用；git 配置只在临时 HOME 内生效。 */
export async function seedWorkspace(home, workspace) {
  await mkdir(workspace, { recursive: true });
  await writeFile(join(workspace, "README.md"), "# e2e workspace\n");
  const gitEnv = { ...process.env, HOME: home, GIT_CONFIG_NOSYSTEM: "1" };
  await runCommand("git", ["init", "-q", "-b", "main"], workspace, gitEnv);
  await runCommand("git", ["add", "."], workspace, gitEnv);
  await runCommand(
    "git",
    ["-c", "user.name=e2e", "-c", "user.email=e2e@example.invalid", "commit", "-qm", "init"],
    workspace,
    gitEnv,
  );
}

/** Provider 配置：唯一的 provider 指向本地 mock 模型（`baseUrl`）。 */
export async function writeProviderConfig(home, baseUrl) {
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
}

export async function seedProfile(home, workspace, baseUrl) {
  const config = join(home, ".zcode", "v2");
  await writeProviderConfig(home, baseUrl);
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
  await seedWorkspace(home, workspace);
}
