// Gateway 的出站队列与水位（docs/specs/rust-u2-gateway.md §2.3）在桌面 App 里的检查。
//
//   1. 大输出：一个回合里 7 轮、共约 1.5 MiB 的流式文本（runtime 单次回复的文本上限是 256 KiB，所以分轮，
//      每轮之后一个只读工具调用）；开始输出后让 Renderer 主线程卡住 6 s（不读 WebSocket），出站积压越过
//      高水位。之后全部 49 个文本块各出现恰好一次。另记下卡住之后收到的字节数与快照帧数（水位暂停 →
//      合并或快照恢复，具体走哪条取决于积压时机，只记录不断言）。
//   2. 终端洪泛：终端里运行 `yes`，期间 Renderer 卡住 15 s。终端输出按连接流控（terminal/flow.rs），
//      连接不被出站上限关闭（harness 断言 Host 日志没有 `outbound backlog exceeded`），Host 内存不随输出
//      增长；Ctrl+C 之后终端与对话都可用。
import { execFile } from "node:child_process";
import { readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { promisify } from "node:util";

import { composer, sseChunk, sseText, submitComposer } from "../helpers.mjs";
import { hostsOf, waitFor, waitUsable } from "../resilience-helpers.mjs";

const exec = promisify(execFile);

const ROUNDS = 7;
const BLOCKS_PER_ROUND = 7;
const BLOCK_BYTES = 32 * 1024;
/** 终端洪泛期间 Host 常驻内存允许的增长：流控之后只有水位量级；没有流控时每秒涨 40–50 MB。 */
const FLOOD_RSS_GROWTH_LIMIT_KIB = 128 * 1024;
const FLOOD_LINE = "gateway-terminal-flood-0123456789";

function block(round, index) {
  const tag = `[blk-${String(round).padStart(2, "0")}-${index}]`;
  const word = "lorem ";
  return `${tag} ${word.repeat(Math.floor((BLOCK_BYTES - tag.length - 3) / word.length))}\n\n`;
}

export const mockRules = [
  {
    name: "big",
    match: ({ hasTools, text }) => hasTools && text.includes("big output"),
    respond: async ({ response, messages, sleep, workspace }) => {
      const round = messages.filter((message) => message.role === "tool").length;
      if (round === 0) {
        response.write(sseChunk({ role: "assistant", content: "BIG-OUTPUT-START\n\n" }, null));
        // 留出时间让场景在输出开始后卡住 Renderer。
        await sleep(1500);
      } else {
        response.write(sseChunk({ role: "assistant", content: "" }, null));
      }
      if (round >= ROUNDS) {
        response.end(
          sseChunk({ content: "BIG-OUTPUT-END" }, null) + sseChunk({}, "stop") + "data: [DONE]\n\n",
        );
        return;
      }
      for (let index = 0; index < BLOCKS_PER_ROUND; index += 1) {
        response.write(sseChunk({ content: block(round, index) }, null));
      }
      const call = {
        index: 0,
        id: `call_read_${round}`,
        type: "function",
        // 每轮读不同的文件：runtime 会提醒连续重复的相同工具调用。
        function: {
          name: "Read",
          arguments: JSON.stringify({ file_path: join(workspace, `part-${round}.txt`) }),
        },
      };
      response.end(
        sseChunk({ tool_calls: [call] }, null) + sseChunk({}, "tool_calls") + "data: [DONE]\n\n",
      );
    },
  },
  {
    name: "echo",
    match: () => true,
    respond: ({ response, text }) => response.end(sseText(`Reply to: ${text}`)),
  },
];

export async function seed({ home, workspace }) {
  // 大上下文窗口：多轮大输出留在历史里，不触发自动压缩。
  const path = join(home, ".zcode", "v2", "provider_config.json");
  const config = JSON.parse(await readFile(path, "utf8"));
  for (const rule of config.config.modelConfigRules.providerModelRules) {
    rule.config.properties = { contextWindow: 4_000_000 };
  }
  await writeFile(path, JSON.stringify(config));
  for (let round = 0; round < ROUNDS; round += 1)
    await writeFile(join(workspace, `part-${round}.txt`), `part ${round}\n`);
}

const bodyHas = (page, text, timeout = 120_000) =>
  page.waitForFunction((needle) => document.body.innerText.includes(needle), text, { timeout });

async function typeAndSubmit(page, text) {
  const input = await composer(page);
  await input.click();
  await page.keyboard.insertText(text);
  await submitComposer(page);
}

/** 让 Renderer 主线程忙 `ms` 毫秒：期间不读 WebSocket，Host 的出站积压增长。 */
async function blockRenderer(page, ms) {
  await page.evaluate((duration) => {
    const end = Date.now() + duration;
    while (Date.now() < end) {
      // 忙等
    }
  }, ms);
}

async function rssKib(pid) {
  const { stdout } = await exec("ps", ["-o", "rss=", "-p", String(pid)]);
  return Number(stdout.trim());
}

/** 统计页面收到的 V4 帧：字节数与快照帧数（分片帧按 logicalFrameId 重组后判断）。 */
function watchFrames(page) {
  const stats = { bytes: 0, snapshots: 0 };
  const fragments = new Map();
  const isSnapshot = (frame) => frame.includes('"kind":"snapshot"');
  page.on("websocket", (socket) =>
    socket.on("framereceived", ({ payload }) => {
      const buffer = Buffer.isBuffer(payload) ? payload : Buffer.from(payload);
      stats.bytes += buffer.length;
      const text = buffer.toString("latin1");
      if (text.includes('"wireVersion":3,"kind":"complete"')) {
        if (isSnapshot(text)) stats.snapshots += 1;
        return;
      }
      const match = text.match(
        /"kind":"fragment".*?"logicalFrameId":"([^"]+)".*?"fragmentIndex":(\d+),"fragmentCount":(\d+).*?"dataBase64":"([^"]*)"/,
      );
      if (!match) return;
      const [, id, index, count, data] = match;
      const parts = fragments.get(id) ?? [];
      parts[Number(index)] = Buffer.from(data, "base64");
      fragments.set(id, parts);
      if (parts.filter(Boolean).length === Number(count)) {
        fragments.delete(id);
        if (isSnapshot(Buffer.concat(parts).toString("utf8"))) stats.snapshots += 1;
      }
    }),
  );
  return stats;
}

async function bigOutput(page, shots, frames) {
  await typeAndSubmit(page, "please produce big output");
  await bodyHas(page, "BIG-OUTPUT-START");
  const before = { ...frames };
  await blockRenderer(page, 6_000);
  await bodyHas(page, "BIG-OUTPUT-END");
  // 中间几轮折叠在 “Worked for …” 里：展开后检查每一块。
  // 回合结束后才出现折叠条（进行中显示 “Working for …”）：等文档顺序里 BIG-OUTPUT-END 之前出现
  // 第二个 “Worked for”（第一个属于上一回合），再展开它。
  await waitFor(
    () =>
      page.evaluate(() => {
        const leaves = [...document.querySelectorAll("body *")].filter(
          (element) => element.childElementCount === 0,
        );
        const end = leaves.find((element) => element.textContent?.includes("BIG-OUTPUT-END"));
        const toggles = leaves
          .filter((element) => (element.textContent?.trim() ?? "").startsWith("Worked for"))
          .filter(
            (element) =>
              end && element.compareDocumentPosition(end) & Node.DOCUMENT_POSITION_FOLLOWING,
          );
        if (toggles.length < 2) return false;
        toggles.at(-1).setAttribute("data-e2e-big-turn", "true");
        return true;
      }),
    { timeout: 60_000, what: "the big turn to finish" },
  );
  await page.locator('[data-e2e-big-turn="true"]').click();
  await bodyHas(page, "[blk-00-0]", 30_000);
  await page.screenshot({ path: join(shots, "01-big-output.png") });
  const check = await page.evaluate(
    ([rounds, perRound]) => {
      const text = document.body.innerText;
      const missing = [];
      const duplicated = [];
      for (let round = 0; round < rounds; round += 1) {
        for (let index = 0; index < perRound; index += 1) {
          const tag = `[blk-${String(round).padStart(2, "0")}-${index}]`;
          const first = text.indexOf(tag);
          if (first < 0) missing.push(tag);
          else if (text.indexOf(tag, first + 1) >= 0) duplicated.push(tag);
        }
      }
      return { missing, duplicated };
    },
    [ROUNDS, BLOCKS_PER_ROUND],
  );
  if (check.missing.length > 0 || check.duplicated.length > 0)
    throw new Error(`big output rendered incompletely: ${JSON.stringify(check)}`);
  const received = ((frames.bytes - before.bytes) / 1024 / 1024).toFixed(1);
  console.info(
    `[e2e] ${ROUNDS * BLOCKS_PER_ROUND} blocks of ${BLOCK_BYTES / 1024} KiB rendered exactly once after a 6 s renderer stall; ` +
      `${received} MiB received after the stall began, ${frames.snapshots - before.snapshots} snapshot frame(s)`,
  );
}

async function terminalFlood(page, shots, workspace, host) {
  await page.getByRole("button", { name: "Toggle terminal" }).first().click();
  await page.waitForTimeout(2500);
  const rssBefore = await rssKib(host);
  // shell 先建标记文件再开始洪泛：标记出现时输出已经在涌入（终端可能画在 canvas 上，不能读 DOM 文本）。
  await page.keyboard.type(`touch flood-started; yes ${FLOOD_LINE}`);
  await page.keyboard.press("Enter");
  await waitFor(
    async () =>
      readFile(join(workspace, "flood-started"))
        .then(() => true)
        .catch(() => false),
    { timeout: 60_000, what: "the flood to start" },
  );
  await page.waitForTimeout(1_000);
  await blockRenderer(page, 15_000);
  const rssAfter = await rssKib(host);
  await page.screenshot({ path: join(shots, "02-flooding.png") });
  if (await page.locator('[data-testid="host-connection-lost"]').count())
    throw new Error("the window lost its Host connection during the terminal flood");
  if (rssAfter - rssBefore > FLOOD_RSS_GROWTH_LIMIT_KIB)
    throw new Error(`the Host grew by ${rssAfter - rssBefore} KiB during the terminal flood`);
  await page.keyboard.press("Control+C");
  const started = Date.now();
  await page.keyboard.type("echo after-$((6*7)) > after-flood.txt");
  await page.keyboard.press("Enter");
  await waitFor(
    async () =>
      (await readFile(join(workspace, "after-flood.txt"), "utf8").catch(() => "")).trim() ===
      "after-42",
    { timeout: 120_000, what: "the terminal to run a command after the flood" },
  );
  await page.screenshot({ path: join(shots, "03-after-flood.png") });
  console.info(
    `[e2e] terminal flood with a 15 s renderer stall: Host RSS ${rssBefore} -> ${rssAfter} KiB, connection kept, ` +
      `terminal ran a command ${Date.now() - started} ms after Ctrl+C`,
  );
}

export async function run({ page, shots, workspace, mainPid }) {
  await waitUsable(page);
  await typeAndSubmit(page, "start a session");
  await bodyHas(page, "Reply to: start a session");
  // reload 之后的新连接才能被观察到（Playwright 只报告之后建立的 WebSocket）。
  const frames = watchFrames(page);
  await page.reload();
  await waitUsable(page);
  try {
    await bodyHas(page, "Reply to: start a session", 15_000);
  } catch {
    await page.getByText("start a session").first().click();
  }
  await bigOutput(page, shots, frames);
  const [{ pid: host }] = await hostsOf(mainPid());
  await terminalFlood(page, shots, workspace, host);
  await typeAndSubmit(page, "still responsive after the flood");
  await bodyHas(page, "Reply to: still responsive after the flood");
  console.info("[e2e] the conversation still answers after the flood");
}
