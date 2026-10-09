// Gateway 的调用顺序（docs/specs/rust-u2-gateway.md §2.2）在桌面 App 里的检查。
//
// 改变会话状态的 zcode-agent 调用（V4 命令等）按会话串行键在整个 Host 内按到达顺序逐个执行：
//   1. 忙碌会话里连续发送两条消息：队列顺序等于发送顺序，之后按这个顺序执行（模型请求的先后）；
//   2. 另一个会话流式输出期间，在新会话里应答权限与提问：不同会话的键互不等待，应答立即生效；
//      新会话流式输出时排队的消息不挡住随后的权限应答，在工具结果之后执行；
//   3. 回到第一个会话，它的流式输出完整结束。
// mock 的慢速流由场景控制何时结束（release），不靠时间赶在断言之前或之后。
import { readFile } from "node:fs/promises";
import { join } from "node:path";

import {
  composer,
  lastUserText,
  sseChunk,
  sseText,
  sseToolCall,
  submitComposer,
} from "../helpers.mjs";
import { waitFor, waitUsable } from "../resilience-helpers.mjs";

const QUESTION = "Which lane do you prefer?";

/** 慢速流的状态：场景调用 release(name) 后它才结束。 */
const tickers = new Map();
function ticker(name) {
  let entry = tickers.get(name);
  if (!entry) {
    let release;
    const released = new Promise((resolve) => (release = resolve));
    entry = { name, running: false, finished: false, release, released };
    tickers.set(name, entry);
  }
  return entry;
}

/** 慢速流：release 之前每 250 ms 一段文本；之后以 `finish`（缺省普通结束）收尾。 */
function slowStream(name, label, finish) {
  return async ({ response, sleep, workspace }) => {
    const state = ticker(name);
    state.running = true;
    response.write(sseChunk({ role: "assistant", content: `${label}:` }, null));
    let released = false;
    state.released.then(() => (released = true));
    for (let index = 1; !released && !response.destroyed; index += 1) {
      await sleep(250);
      response.write(sseChunk({ content: ` ${label}-tick-${index}` }, null));
    }
    response.end(
      sseChunk({ content: ` ${label}-done` }, null) +
        (finish ? finish(workspace) : sseChunk({}, "stop") + "data: [DONE]\n\n"),
    );
    state.running = false;
    state.finished = !response.destroyed;
  };
}

const toolResult = (messages) => {
  const content = messages.at(-1)?.content;
  return typeof content === "string" ? content : JSON.stringify(content ?? "");
};

export const mockRules = [
  {
    name: "tool-result",
    match: ({ hasTools, messages }) => hasTools && messages.at(-1)?.role === "tool",
    respond: ({ response, messages }) => {
      const result = toolResult(messages);
      if (result.includes("Blue")) return response.end(sseText("ANSWER_BLUE_OK"));
      return response.end(sseText("The mock model wrote the file"));
    },
  },
  {
    // 先慢速输出一段（期间可以排队），再请求写文件（需要权限）。
    name: "stream-then-write",
    match: ({ hasTools, text }) => hasTools && text.includes("stream then write file"),
    respond: slowStream("prelude", "prelude", (workspace) => {
      const call = {
        index: 0,
        id: "call_write_ordered",
        type: "function",
        function: {
          name: "Write",
          arguments: JSON.stringify({
            file_path: join(workspace, "ordered.txt"),
            content: "written while another session streams\n",
          }),
        },
      };
      return (
        sseChunk({ tool_calls: [call] }, null) + sseChunk({}, "tool_calls") + "data: [DONE]\n\n"
      );
    }),
  },
  {
    name: "ask",
    match: ({ hasTools, text }) => hasTools && text.includes("ask me"),
    respond: ({ response }) =>
      response.end(
        sseToolCall("AskUserQuestion", {
          questions: [
            {
              question: QUESTION,
              header: "Lane",
              options: [
                { label: "Red", description: "A warm lane" },
                { label: "Blue", description: "A cool lane" },
              ],
              multiSelect: false,
            },
          ],
        }),
      ),
  },
  {
    name: "counting",
    match: ({ hasTools, text }) => hasTools && text.includes("count slowly"),
    respond: slowStream("queue", "counting"),
  },
  {
    name: "streaming",
    match: ({ hasTools, text }) => hasTools && text.includes("keep streaming"),
    respond: slowStream("background", "streaming"),
  },
  {
    // 不带 tools 的辅助请求（会话标题）：回用户原文，侧栏标题因此就是第一条消息。
    name: "title",
    match: ({ hasTools }) => !hasTools,
    respond: ({ response, text }) => response.end(sseText(text)),
  },
  {
    name: "echo",
    match: () => true,
    respond: ({ response, text }) => response.end(sseText(`Reply to: ${text}`)),
  },
];

const bodyHas = (page, text, timeout = 60_000) =>
  page.waitForFunction((needle) => document.body.innerText.includes(needle), text, { timeout });

async function typeAndSubmit(page, text) {
  const input = await composer(page);
  await input.click();
  await page.keyboard.insertText(text);
  await submitComposer(page);
}

/** 主对话请求（带 tools）按到达顺序的最后一条用户消息。 */
function mainRequests(model) {
  return model.requests
    .map((request) => {
      try {
        const parsed = JSON.parse(request.body);
        if (!Array.isArray(parsed.tools) || parsed.tools.length === 0) return null;
        return { last: parsed.messages.at(-1), text: lastUserText(parsed.messages) };
      } catch {
        return null;
      }
    })
    .filter(Boolean);
}

async function queueTexts(page) {
  return page
    .locator('[data-testid="v4-queue"] li[data-queue-item-id]')
    .evaluateAll((rows) =>
      rows
        .sort((a, b) => Number(a.dataset.index) - Number(b.dataset.index))
        .map((row) => row.querySelector("span[title]")?.getAttribute("title") ?? row.innerText),
    );
}

export async function run({ page, shots, model, workspace }) {
  // 1. 忙碌会话里连续发送两条消息。
  await waitUsable(page);
  await typeAndSubmit(page, "please count slowly");
  await bodyHas(page, "counting-tick-2 ");
  await typeAndSubmit(page, "queued alpha");
  await typeAndSubmit(page, "queued beta");
  const queued = await waitFor(
    async () => {
      const texts = await queueTexts(page);
      return texts.length === 2 ? texts : null;
    },
    { timeout: 30_000, what: "two queued messages" },
  );
  await page.screenshot({ path: join(shots, "01-queued.png") });
  if (queued.join("|") !== "queued alpha|queued beta")
    throw new Error(`queue order differs from the send order: ${JSON.stringify(queued)}`);
  ticker("queue").release();
  await bodyHas(page, "counting-done");
  await bodyHas(page, "Reply to: queued beta");
  const order = mainRequests(model).map((request) => request.text);
  const alpha = order.indexOf("queued alpha");
  const beta = order.indexOf("queued beta");
  if (alpha < 0 || beta < 0 || alpha > beta)
    throw new Error(`queued messages ran out of order: ${JSON.stringify(order)}`);
  await page.screenshot({ path: join(shots, "02-drained.png") });
  console.info("[e2e] two messages sent back to back were queued and ran in send order");

  // 2. 这个会话继续流式输出，同时在另一个会话里应答权限与提问。
  await typeAndSubmit(page, "please keep streaming");
  await bodyHas(page, "streaming-tick-2 ");
  await page.getByText("New task", { exact: true }).first().click();
  await typeAndSubmit(page, "please stream then write file ordered.txt");
  await bodyHas(page, "prelude-tick-2 ");
  // 这个会话忙碌时排队一条消息；之后的权限应答（同一会话的键）不能被它挡住，它在工具结果之后执行。
  await typeAndSubmit(page, "queued during the prelude");
  await waitFor(async () => (await queueTexts(page)).length === 1, {
    timeout: 30_000,
    what: "the message queued during the prelude",
  });
  ticker("prelude").release();
  await page.getByText("Permission required").first().waitFor({ timeout: 60_000 });
  await page.screenshot({ path: join(shots, "03-permission-pending.png") });
  await page.getByRole("button", { name: "Confirm" }).click();
  await bodyHas(page, "The mock model wrote the file");
  const written = await readFile(join(workspace, "ordered.txt"), "utf8");
  if (written !== "written while another session streams\n")
    throw new Error(`unexpected file content: ${written}`);
  await bodyHas(page, "Reply to: queued during the prelude");
  const requests = mainRequests(model);
  const toolTurn = requests.findIndex(
    (request) => request.last?.role === "tool" && !toolResult([request.last]).includes("Blue"),
  );
  const queuedTurn = requests.findIndex((request) => request.text === "queued during the prelude");
  if (toolTurn < 0 || queuedTurn < toolTurn)
    throw new Error(
      `the queued message ran before the permission's tool result: ${JSON.stringify(requests.map((r) => r.last?.role ?? r.text))}`,
    );
  await typeAndSubmit(page, "please ask me a question");
  await page.getByText(QUESTION).first().waitFor({ timeout: 60_000 });
  const blue = page.getByText("Blue", { exact: true }).first();
  await blue.click({ trial: true });
  await blue.click();
  // 单题单选通常选中即提交；问题卡片仍在且有可用的提交键时点它（只点一次）。
  const answered = page.getByText("ANSWER_BLUE_OK").first();
  let submitted = false;
  await waitFor(
    async () => {
      if (await answered.isVisible().catch(() => false)) return true;
      const submit = page.getByRole("button", { name: "Submit", exact: true }).first();
      if (!submitted && (await submit.isEnabled({ timeout: 100 }).catch(() => false))) {
        submitted = await submit
          .click({ timeout: 1_000 })
          .then(() => true)
          .catch(() => false);
      }
      return false;
    },
    { timeout: 60_000, what: "the answer to reach the model" },
  );
  await page.screenshot({ path: join(shots, "04-answered.png") });
  if (!ticker("background").running)
    throw new Error("the other session's stream ended before the answers were checked");
  console.info(
    "[e2e] a permission and a question were answered in one session while another session streamed",
  );

  // 3. 回到第一个会话：它的流式输出完整结束。
  ticker("background").release();
  await waitFor(async () => ticker("background").finished, {
    timeout: 30_000,
    what: "the first session's stream to finish",
  });
  await page.getByText("please count slowly", { exact: true }).first().click();
  await bodyHas(page, "streaming-done");
  await page.screenshot({ path: join(shots, "05-back-to-first.png") });
  console.info("[e2e] the streaming session finished while the other session was in front");
}
