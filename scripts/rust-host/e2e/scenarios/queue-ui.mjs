// 输入队列在桌面界面上（docs/specs/rust-u3-stage-b.md §3.4）：同一会话只有一个输入队列，所有者是 runtime 的
// CommandInbox；桌面经 V4 conversation 的 queue 看到并改动它（删除 → deleteQueueItem，拖动 → reorderQueueItem）。
//   1. 流式输出期间连续发送三条消息：都进入 runtime 的队列，按发送顺序显示在 composer 上方的队列区；
//   2. 点第二条的删除键：它从队列区消失；
//   3. 把第三条拖到第一条之前：队列区的顺序随之改变；
//   4. 结束流式输出：剩下两条按新顺序在当前 run 之后依次执行（模型请求的先后），被删除的一条从未发给模型，
//      队列区清空。
// 另配一个用户级 UserPromptSubmit hook（把 hook 的 stdin 追加到文件）：hook 进程经工具进程账本启动
// （docs/specs/rust-tool-process-reaping.md），场景确认它对每个执行的输入都运行了；harness 在退出后断言账本为空。
// mock 的慢速流由场景控制何时结束（release），不靠时间赶在断言之前或之后。
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";

import { composer, lastUserText, sseChunk, sseText, submitComposer } from "../helpers.mjs";
import { waitFor, waitUsable } from "../resilience-helpers.mjs";

const QUEUED = ["queued one", "queued two", "queued three"];

/** 慢速流的状态：场景调用 release() 后它才结束。 */
const stream = { running: false, finished: false };
let releaseStream;
const released = new Promise((resolve) => (releaseStream = resolve));

export const mockRules = [
  {
    name: "counting",
    match: ({ hasTools, text }) => hasTools && text.includes("please count slowly"),
    respond: async ({ response, sleep }) => {
      stream.running = true;
      response.write(sseChunk({ role: "assistant", content: "counting:" }, null));
      let done = false;
      released.then(() => (done = true));
      for (let index = 1; !done && !response.destroyed; index += 1) {
        await sleep(250);
        response.write(sseChunk({ content: ` counting-tick-${index}` }, null));
      }
      response.end(
        sseChunk({ content: " counting-done" }, null) + sseChunk({}, "stop") + "data: [DONE]\n\n",
      );
      stream.running = false;
      stream.finished = !response.destroyed;
    },
  },
  {
    // 不带 tools 的辅助请求（会话标题）。
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

const hookLog = (temp) => join(temp, "user-prompt-submit-hook.log");

export async function seed({ home, temp }) {
  // 用户级 hooks（~/.zcode/cli/config.json）：不需要项目 hook 的信任审批。
  const dir = join(home, ".zcode", "cli");
  await mkdir(dir, { recursive: true });
  await writeFile(
    join(dir, "config.json"),
    `${JSON.stringify(
      {
        hooks: {
          enabled: true,
          events: {
            UserPromptSubmit: [
              {
                hooks: [
                  {
                    type: "command",
                    command: `cat >> '${hookLog(temp)}'; echo >> '${hookLog(temp)}'`,
                    shell: true,
                  },
                ],
              },
            ],
          },
        },
      },
      null,
      2,
    )}\n`,
  );
}

const bodyHas = (page, text, timeout = 60_000) =>
  page.waitForFunction((needle) => document.body.innerText.includes(needle), text, { timeout });

async function typeAndSubmit(page, text) {
  const input = await composer(page);
  await input.click();
  await page.keyboard.insertText(text);
  await submitComposer(page);
}

/** 主对话请求（带 tools）按到达顺序的最后一条用户消息。 */
function mainRequestTexts(model) {
  return model.requests
    .map((request) => {
      try {
        const parsed = JSON.parse(request.body);
        if (!Array.isArray(parsed.tools) || parsed.tools.length === 0) return null;
        return lastUserText(parsed.messages);
      } catch {
        return null;
      }
    })
    .filter((text) => text !== null);
}

/** 队列区当前的项（按显示顺序）：queueItemId 与文本。 */
async function queueRows(page) {
  return page.locator('[data-testid="v4-queue"] li[data-queue-item-id]').evaluateAll((rows) =>
    rows
      .sort((a, b) => Number(a.dataset.index) - Number(b.dataset.index))
      .map((row) => ({
        id: row.dataset.queueItemId,
        text: row.querySelector("span[title]")?.getAttribute("title") ?? row.innerText,
      })),
  );
}

async function waitQueue(page, expected, what) {
  return waitFor(
    async () => {
      const rows = await queueRows(page);
      return rows.map((row) => row.text).join("|") === expected.join("|") ? rows : null;
    },
    { timeout: 30_000, what },
  ).catch(async (error) => {
    throw new Error(`${error.message}; queue shows ${JSON.stringify(await queueRows(page))}`);
  });
}

/**
 * 按住 `movingId` 的拖动把手，拖到 `targetId` 那一行的上半部再放开（dnd-kit 的 PointerSensor 先要移动
 * 6 px 才开始拖动；closestCenter 判定落点是目标行，于是移到它之前）。
 */
async function dragBefore(page, movingId, targetId) {
  const handle = page.locator(
    `[data-v4-queue-drag-handle="true"][data-queue-item-id="${movingId}"]`,
  );
  const target = page.locator(`[data-testid="v4-queue-item-${targetId}"]`);
  await handle.click({ trial: true, timeout: 10_000 });
  const from = await handle.boundingBox();
  const to = await target.boundingBox();
  if (!from || !to) throw new Error("queue rows have no layout box");
  const x = from.x + from.width / 2;
  const y = from.y + from.height / 2;
  await page.mouse.move(x, y);
  await page.mouse.down();
  await page.mouse.move(x, y - 10, { steps: 5 });
  await page.mouse.move(x, to.y + to.height / 4, { steps: 15 });
  await page.mouse.up();
}

export async function run({ page, shots, model, temp }) {
  await waitUsable(page);
  await typeAndSubmit(page, "please count slowly");
  await bodyHas(page, "counting-tick-2 ");

  // 1. 流式输出期间连续发送三条：都进入 runtime 的队列，按发送顺序显示。
  for (const text of QUEUED) await typeAndSubmit(page, text);
  const rows = await waitQueue(page, QUEUED, "three queued messages in send order");
  await page.screenshot({ path: join(shots, "01-queued.png") });
  const id = Object.fromEntries(rows.map((row) => [row.text, row.id]));
  console.info(`[e2e] three messages queued behind the running turn: ${JSON.stringify(id)}`);

  // 2. 删除第二条。
  await page.getByTestId(`v4-queue-item-delete-${id["queued two"]}`).click();
  await waitQueue(page, ["queued one", "queued three"], "the deleted item to leave the queue");
  await page.screenshot({ path: join(shots, "02-deleted.png") });
  console.info("[e2e] deleting a queued item removed it from the queue");

  // 3. 把第三条拖到第一条之前。
  await dragBefore(page, id["queued three"], id["queued one"]);
  await waitQueue(page, ["queued three", "queued one"], "the dragged item to move first");
  await page.screenshot({ path: join(shots, "03-reordered.png") });
  if (!stream.running) throw new Error("the running turn ended before the queue was edited");
  console.info("[e2e] dragging a queued item reordered the queue");

  // 4. 结束流式输出：剩下两条按新顺序执行，删除的一条从未发出。
  releaseStream();
  await bodyHas(page, "counting-done");
  await bodyHas(page, "Reply to: queued one");
  await waitFor(async () => (await queueRows(page)).length === 0, {
    timeout: 30_000,
    what: "the queue area to empty",
  });
  await page.screenshot({ path: join(shots, "04-drained.png") });
  const order = mainRequestTexts(model);
  const counting = order.indexOf("please count slowly");
  const three = order.indexOf("queued three");
  const one = order.indexOf("queued one");
  if (counting < 0 || three <= counting || one <= three)
    throw new Error(
      `queued messages did not run after the turn in queue order: ${JSON.stringify(order)}`,
    );
  if (order.includes("queued two"))
    throw new Error(`the deleted queue item reached the model: ${JSON.stringify(order)}`);
  if (await page.evaluate(() => document.body.innerText.includes("Reply to: queued two")))
    throw new Error("the deleted queue item got a reply");
  console.info(
    `[e2e] the queue drained in its edited order after the turn: ${JSON.stringify(order)}`,
  );

  // hook：每个执行的输入都经过了 UserPromptSubmit hook（hook 的 stdin 带着提交的 prompt）。
  const hooked = await readFile(hookLog(temp), "utf8").catch(() => "");
  const missing = ["please count slowly", "queued three", "queued one"].filter(
    (text) => !hooked.includes(JSON.stringify(text).slice(1, -1)),
  );
  if (missing.length > 0)
    throw new Error(`the UserPromptSubmit hook did not run for ${JSON.stringify(missing)}`);
  console.info("[e2e] the user UserPromptSubmit hook ran for every executed input");
}
