// bots 场景（docs/specs/rust-host-bots.md §8）：隔离 profile 里预置一个已绑定的 Telegram Bot，Bot API
// 指向 127.0.0.1 上的 mock。Rust Host 启动即长轮询，mock 投递一条用户消息；Host 用目标 Host 的首选
// 模型建会话（Rust CLI + mock 模型），经 V4 会话帧把回复送回 mock 的 sendMessage，offset 在回调成功
// 后提交。界面侧栏出现这个 Bot 创建的任务，打开后能看到同一段回复。不连任何真实服务。
import { once } from "node:events";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { join } from "node:path";

import { composer, sseText } from "../helpers.mjs";

const TOKEN = "e2e-telegram-token";
const CHAT = 4242;
const PROMPT = "bot ping from telegram e2e";
const TITLE = "Telegram bot task";
const REPLY = "Bot reply streamed from the mock model";

export const mockRules = [
  // 标题生成（不带 tools）回固定标题，侧栏行可按标题定位。
  {
    name: "title",
    match: ({ hasTools }) => !hasTools,
    respond: ({ response }) => response.end(sseText(TITLE)),
  },
  {
    name: "bot prompt",
    match: ({ text }) => text.includes(PROMPT),
    respond: ({ response }) => response.end(sseText(REPLY)),
  },
  {
    name: "default",
    match: () => true,
    respond: ({ response }) => response.end(sseText("ok")),
  },
];

const telegram = { server: null, calls: [], delivered: false };

function respond(response, body) {
  response.writeHead(200, { "content-type": "application/json" });
  response.end(JSON.stringify(body));
}

export async function env() {
  telegram.server = createServer((request, response) => {
    let raw = "";
    request.on("data", (chunk) => (raw += chunk));
    request.on("end", () => {
      const method = request.url.split("/").pop();
      if (!request.url.startsWith(`/bot${TOKEN}/`)) {
        response.writeHead(404).end();
        return;
      }
      let body = {};
      try {
        body = JSON.parse(raw || "{}");
      } catch {
        // 非 JSON 请求体按空对象记录
      }
      telegram.calls.push({ method, body });
      if (method === "getMe")
        return respond(response, { ok: true, result: { first_name: "E2E Bot" } });
      if (method === "getUpdates") {
        if (!telegram.delivered) {
          telegram.delivered = true;
          return respond(response, {
            ok: true,
            result: [
              {
                update_id: 1,
                message: {
                  message_id: 1,
                  text: PROMPT,
                  chat: { id: CHAT, type: "private" },
                  from: { id: CHAT, first_name: "Tester" },
                },
              },
            ],
          });
        }
        // 长轮询：没有新消息时稍后返回空批次。
        setTimeout(() => respond(response, { ok: true, result: [] }), 500);
        return;
      }
      return respond(response, { ok: true, result: true });
    });
  });
  telegram.server.listen(0, "127.0.0.1");
  await once(telegram.server, "listening");
  return { ZCODE_TELEGRAM_API_BASE_URL: `http://127.0.0.1:${telegram.server.address().port}` };
}

export async function seed({ home }) {
  const config = join(home, ".zcode", "v2");
  await mkdir(config, { recursive: true });
  await writeFile(
    join(config, "bot-config.v3.json"),
    `${JSON.stringify(
      {
        version: 3,
        bots: [
          {
            id: "tg-e2e",
            name: "E2E Bot",
            provider: "telegram",
            enabled: true,
            credentialRef: "bot:tg-e2e:credential",
            providerUserId: String(CHAT),
            allowedWorkspaces: ["*"],
            allowedCommands: {
              status: true,
              new: true,
              workspace: true,
              model: true,
              mode: true,
              thoughtLevel: true,
              reply: true,
            },
            currentOptions: {},
            replyMode: "assistant_changes",
          },
        ],
      },
      null,
      2,
    )}\n`,
  );
  // 凭据存储兼容旧明文值（非 enc:v1: 前缀按明文读），不需要在测试里复现加密密钥。
  await writeFile(
    join(config, "credentials.json"),
    `${JSON.stringify({ "bot:tg-e2e:credential": TOKEN }, null, 2)}\n`,
  );
}

async function waitFor(description, check, timeout = 90_000) {
  const deadline = Date.now() + timeout;
  for (;;) {
    const value = await check();
    if (value) return value;
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${description}`);
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
}

export async function run({ page, shots, model, home }) {
  try {
    await composer(page);
    // Host 启动后自行对齐 Bot：同步菜单、清 webhook、开始长轮询。
    await waitFor("Telegram long polling", () =>
      telegram.calls.some((call) => call.method === "getUpdates"),
    );
    const commands = telegram.calls.find((call) => call.method === "setMyCommands");
    if (!commands || commands.body.commands?.[0]?.command !== "help")
      throw new Error(`setMyCommands missing: ${JSON.stringify(commands)}`);
    await waitFor("mock model to receive the bot prompt", () =>
      model.requests.some((request) => request.body.includes(PROMPT)),
    );
    // 回复经 V4 会话帧送回 Bot：assistant_changes 在轮次收口时整段发出。
    const sent = await waitFor("the streamed reply at Telegram sendMessage", () =>
      telegram.calls.find((call) => call.method === "sendMessage" && call.body.text === REPLY),
    );
    if (sent.body.chat_id !== String(CHAT) || sent.body.parse_mode !== "Markdown")
      throw new Error(`unexpected sendMessage body: ${JSON.stringify(sent.body)}`);
    if (!telegram.calls.some((call) => call.method === "sendChatAction"))
      throw new Error("typing action was never sent");
    // offset 在回调成功后提交为 update_id + 1，下一次 getUpdates 带上它。
    await waitFor("offset committed", () =>
      telegram.calls.some((call) => call.method === "getUpdates" && call.body.offset === 2),
    );
    const state = JSON.parse(
      await readFile(join(home, ".zcode", "v2", "bot-state.v3.json"), "utf8"),
    );
    const context = state.bots["tg-e2e"];
    if (context?.mode !== "task" || !context.activeTaskId || context.telegramOffset !== 2)
      throw new Error(`unexpected bot context: ${JSON.stringify(context)}`);

    // 侧栏出现 Bot 创建的任务（标题取首条消息，与 Node 的 deriveTaskTitle 相同）；打开它能看到同一段回复。
    const row = page.getByText(PROMPT, { exact: true }).first();
    await row.waitFor({ state: "visible", timeout: 60_000 });
    await page.screenshot({ path: join(shots, "bots-00-sidebar.png") });
    await row.click({ trial: true });
    await row.click();
    const bubble = page.getByText(REPLY).first();
    await bubble.waitFor({ state: "visible", timeout: 30_000 });
    await bubble.click({ trial: true });
    await page.screenshot({ path: join(shots, "bots-01-task.png") });
    console.info(`[e2e] bot task ${context.activeTaskId} replied through Telegram mock`);
  } finally {
    telegram.server?.close();
  }
}
