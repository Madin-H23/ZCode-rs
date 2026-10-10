// Gateway 越过出站上限关闭连接之后，窗口自己重新接入同一个 Host（docs/specs/rust-u2-gateway.md §2.6、
// docs/specs/rust-host-resilience.md §2.5）。
//
// Host 以测试用的小上限启动（ZCODE_HOST_DEBUG_OUTBOUND_HARD_LIMIT，只在调试构建中生效）。上限小于高水位，
// saturated 不会发生，终端输出不被流控暂停：Renderer 卡住时终端大量输出，积压很快越过上限，Gateway
// 以 Close 4001 `outbound-backlog` 关闭窗口的连接（真实的越过上限，不是注入的关闭）。
//
//   1. 第 1 次：窗口自己 reload（页面标记消失），接入同一个 Host（pid 不变、Host 日志多一次 attach），
//      没有停在“正在重连”，会话按快照恢复。
//   2. 防循环：紧接着再越过上限 3 次。第 2、3 次仍自动重新接入（等待页 data-state="reattaching"，
//      第 n/3 次）；第 4 次（60 s 内）不再 reload，停在“连接多次中断”页（data-state="closed"）。
//   3. 点“重新加载”：清空计数后 reload，接入同一个 Host，对话完整，新连接上收发消息正常。
import { readFile } from "node:fs/promises";
import { join } from "node:path";

import { sendMessage, sseText } from "../helpers.mjs";
import { hostLogLines, hostsOf, waitFor, waitUsable } from "../resilience-helpers.mjs";

/** 测试用的出站上限：小于 1 MiB 的高水位，流控不介入。 */
const HARD_LIMIT = 512 * 1024;
const FLOOD_LINE = "gateway-close-recovery-flood-0123456789";
/** 每次洪泛的行数（约 80 MB）：连接关闭后没有读者，PTY 很快读完，命令随之结束。 */
const FLOOD_LINES = 2_000_000;
/** Renderer 卡住的时长：期间不读 WebSocket。 */
const STALL_MS = 4_000;
const BACKLOG_CLOSE = /outbound backlog exceeded/;

export const mockRules = [
  {
    name: "echo",
    match: () => true,
    respond: ({ response, text }) => response.end(sseText(`Reply to: ${text}`)),
  },
];

export function env() {
  return { ZCODE_HOST_DEBUG_OUTBOUND_HARD_LIMIT: String(HARD_LIMIT) };
}

/** 场景故意制造的 Host 日志：越过上限的关闭与启动时的覆盖说明。 */
export const expectedHostLog = [BACKLOG_CLOSE, /ZCODE_HOST_DEBUG_OUTBOUND_HARD_LIMIT=/];

const bodyHas = (page, text, timeout = 60_000) =>
  page.waitForFunction((needle) => document.body.innerText.includes(needle), text, { timeout });

const lostScreen = (page, state) =>
  page.locator(`[data-testid="host-connection-lost"]${state ? `[data-state="${state}"]` : ""}`);

/** 页面标记：reload 之后新文档里没有它。导航中 evaluate 会失败，按“还没换”处理。 */
async function markDocument(page, round) {
  await page.evaluate((value) => {
    window.__zcodeE2eCloseRound = value;
  }, round);
}

async function documentRound(page) {
  return page.evaluate(() => window.__zcodeE2eCloseRound ?? null).catch(() => "navigating");
}

async function attachments(home) {
  return (await hostLogLines(home)).filter((line) => /attachment \S+ connected/.test(line)).length;
}

async function backlogCloses(home) {
  return (await hostLogLines(home)).filter((line) => BACKLOG_CLOSE.test(line)).length;
}

async function onlyHostPid(mainPid) {
  const hosts = await hostsOf(mainPid);
  if (hosts.length !== 1) throw new Error(`expected one Rust Host, found ${hosts.length}`);
  return hosts[0].pid;
}

/** 让 Renderer 主线程忙 `ms` 毫秒：期间不读 WebSocket。 */
async function stallRenderer(page, ms) {
  await page.evaluate((duration) => {
    const end = Date.now() + duration;
    while (Date.now() < end) {
      // 忙等
    }
  }, ms);
}

/** 打开终端，确认 shell 在运行（它建出标记文件），再开始洪泛并让 Renderer 卡住。 */
async function floodWhileStalled(page, workspace, round) {
  await page.getByRole("button", { name: "Toggle terminal" }).first().click();
  await page.locator(".xterm").first().waitFor({ state: "visible", timeout: 30_000 });
  await page.waitForTimeout(1_000);
  const ready = `close-recovery-ready-${round}`;
  await page.keyboard.type(`touch ${ready}`);
  await page.keyboard.press("Enter");
  await waitFor(
    async () =>
      readFile(join(workspace, ready))
        .then(() => true)
        .catch(() => false),
    { timeout: 30_000, what: `the terminal of round ${round} to run a command` },
  );
  // 洪泛在卡住之后才开始（sleep 1）：输出全部堆在不读的连接上。
  await page.keyboard.type(`sleep 1; yes ${FLOOD_LINE} | head -n ${FLOOD_LINES}`);
  await page.keyboard.press("Enter");
  await stallRenderer(page, STALL_MS);
}

/** 一次越过上限：返回窗口之后的样子（`reloaded` 或停在 `closed` 页）。 */
async function forceBacklogClose(context, round) {
  const { page, home, workspace } = context;
  const closesBefore = await backlogCloses(home);
  await markDocument(page, round);
  await floodWhileStalled(page, workspace, round);
  await waitFor(async () => (await backlogCloses(home)) > closesBefore, {
    timeout: 30_000,
    what: `the Host to close the connection for its backlog (round ${round})`,
  });
  return waitFor(
    async () => {
      if ((await documentRound(page)) === null) return "reloaded";
      if (await lostScreen(page, "closed").count()) return "closed";
      return null;
    },
    { timeout: 60_000, interval: 100, what: `the window to react to the close (round ${round})` },
  );
}

/** reload 之后：可用、没有停在断开页、接入的还是同一个 Host。 */
async function assertReattached(context, round, host, attachmentsBefore) {
  const { page, home, mainPid } = context;
  await waitUsable(page);
  if (await lostScreen(page).count())
    throw new Error(`round ${round}: the reloaded window still shows the connection-lost page`);
  const pid = await onlyHostPid(mainPid());
  if (pid !== host) throw new Error(`round ${round}: the Host changed (${host} -> ${pid})`);
  const now = await attachments(home);
  if (now <= attachmentsBefore)
    throw new Error(`round ${round}: the reloaded window did not attach to the Host again`);
  return now;
}

export async function run(context) {
  const { page, shots, home, mainPid } = context;
  await waitUsable(page);
  await sendMessage(page, "start a session");
  await bodyHas(page, "Reply to: start a session");
  const host = await onlyHostPid(mainPid());
  let attached = await attachments(home);
  const started = Date.now();

  // 1. 第 1 次越过上限：窗口自己 reload，接入同一个 Host，会话按快照恢复。
  if ((await forceBacklogClose(context, 1)) !== "reloaded") {
    await page.screenshot({ path: join(shots, "01-not-reloaded.png") });
    throw new Error("round 1: the window did not reload after the backlog close");
  }
  attached = await assertReattached(context, 1, host, attached);
  try {
    await bodyHas(page, "Reply to: start a session", 15_000);
  } catch {
    await page.getByText("start a session").first().click();
    await bodyHas(page, "Reply to: start a session", 30_000);
  }
  await page.screenshot({ path: join(shots, "01-reattached.png") });
  console.info(
    `[e2e] round 1: backlog close -> the window reloaded by itself and re-attached to Host ${host}; the conversation is back`,
  );

  // 2. 第 2、3 次仍自动重新接入，等待页带第 n/3 次。
  for (const round of [2, 3]) {
    const closesBefore = await backlogCloses(home);
    await markDocument(page, round);
    await floodWhileStalled(page, context.workspace, round);
    const waiting = lostScreen(page, "reattaching");
    await waiting.waitFor({ state: "visible", timeout: 30_000 });
    const attempt = await waiting.getAttribute("data-attempt");
    if (attempt !== String(round))
      throw new Error(`round ${round}: the waiting page shows attempt ${attempt}`);
    if (round === 3) await page.screenshot({ path: join(shots, "02-reattaching-3.png") });
    await waitFor(async () => (await documentRound(page)) === null, {
      timeout: 30_000,
      interval: 100,
      what: `the window to reload (round ${round})`,
    });
    if ((await backlogCloses(home)) <= closesBefore)
      throw new Error(`round ${round}: the Host did not report a backlog close`);
    attached = await assertReattached(context, round, host, attached);
    console.info(`[e2e] round ${round}: re-attached automatically (attempt ${round}/3)`);
  }

  // 3. 第 4 次（60 s 内）：不再 reload，停在“连接多次中断”页。
  const outcome = await forceBacklogClose(context, 4);
  const elapsed = Date.now() - started;
  if (outcome !== "closed")
    throw new Error(
      `round 4: expected the 'closed repeatedly' page, the window ${outcome} (${elapsed} ms after round 1)`,
    );
  // 停在这一页：几秒之后既没有 reload，也没有被别的状态取代。
  await page.waitForTimeout(6_000);
  if ((await documentRound(page)) !== 4 || !(await lostScreen(page, "closed").count()))
    throw new Error("round 4: the window left the 'closed repeatedly' page by itself");
  await page.getByTestId("host-connection-reload").click({ trial: true, timeout: 5_000 });
  await page.screenshot({ path: join(shots, "03-closed-repeatedly.png") });
  if ((await onlyHostPid(mainPid())) !== host) throw new Error("round 4: the Host changed");
  console.info(
    `[e2e] round 4 (${elapsed} ms after round 1): no reload; the window shows the 'closed repeatedly' page`,
  );

  // 4. “重新加载”：接入同一个 Host，对话完整，新连接上收发消息正常。
  await page.getByTestId("host-connection-reload").click();
  await waitFor(async () => (await documentRound(page)) === null, {
    timeout: 30_000,
    interval: 100,
    what: "the window to reload after clicking Reload",
  });
  await assertReattached(context, 5, host, attached);
  try {
    await bodyHas(page, "Reply to: start a session", 15_000);
  } catch {
    await page.getByText("start a session").first().click();
    await bodyHas(page, "Reply to: start a session", 30_000);
  }
  await sendMessage(page, "after the backlog closes");
  await bodyHas(page, "Reply to: after the backlog closes");
  await page.screenshot({ path: join(shots, "04-after-reload.png") });
  const closes = await backlogCloses(home);
  if (closes !== 4) throw new Error(`expected 4 backlog closes in the Host log, found ${closes}`);
  // Renderer 的判断依据是 Close 帧的状态码：它的日志（经 Main 落盘）记下 4001 与每次的决定。
  const decisions = (await hostLogLines(home))
    .filter((line) => line.includes("[host-connection] local connection closed by the Host: 4001"))
    .map((line) => line.match(/reattaching \((\d)\/3\)|automatic reattach exhausted/)?.[0]);
  const expected = ["reattaching (1/3)", "reattaching (2/3)", "reattaching (3/3)"];
  if (decisions.join() !== [...expected, "automatic reattach exhausted"].join())
    throw new Error(`unexpected Renderer decisions: ${JSON.stringify(decisions)}`);
  console.info(
    `[e2e] Reload recovered the window on the same Host ${host}; the conversation is intact and answers`,
  );
}
