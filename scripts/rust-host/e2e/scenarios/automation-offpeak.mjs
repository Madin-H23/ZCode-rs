// 闲时任务：mock 网关模式（ZCODE_OFFPEAK_MOCK=1）下在 Automations 页的“闲时”标签创建任务，
// Rust Host 取号落库，卡片出现在列表里；只连 127.0.0.1 上的进程内 mock 网关。
import { readFile, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { once } from "node:events";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";

import { composer } from "../helpers.mjs";

const TITLE = "E2E idle task";

async function freePort() {
  const server = createServer();
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const { port } = server.address();
  server.close();
  return port;
}

/** 模拟已授权的闲时 Provider：Rust Host 还没有账号（R4），用个人配置把它覆盖成 api-key 接入。 */
export async function seed({ home }) {
  const file = join(home, ".zcode", "v2", "provider_config.json");
  const config = JSON.parse(await readFile(file, "utf8"));
  config.config.providerConfigRules.providerRules.push({
    providerId: "account:bigmodel-offpeak-idle-plan",
    config: { access: { type: "api-key", apiKey: "sk-offpeak-e2e" } },
  });
  await writeFile(file, JSON.stringify(config));
}

export async function env() {
  return {
    ZCODE_OFFPEAK_MOCK: "1",
    ZCODE_OFFPEAK_MOCK_PORT: String(await freePort()),
    ZCODE_OFFPEAK_MOCK_READY_DELAY_MS: "600000",
  };
}

function rows(home) {
  const database = new DatabaseSync(join(home, ".zcode", "v2", "tasks-index.sqlite"), {
    readOnly: true,
  });
  try {
    return database.prepare("SELECT * FROM off_peak_tasks").all();
  } finally {
    database.close();
  }
}

async function waitFor(description, check, timeout = 30_000) {
  const deadline = Date.now() + timeout;
  for (;;) {
    const value = await check();
    if (value) return value;
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${description}`);
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
}

export async function run({ page, shots, home }) {
  await composer(page);
  await page.getByTestId("automations-open").click();
  await page.waitForTimeout(1500);
  await page.screenshot({ path: join(shots, "offpeak-01-automations.png") });
  // 空态只有大卡里的两个创建入口；灰度（mock）开启时才出现“闲时任务”入口。
  const open = page.getByRole("button", { name: "Create idle-time task" });
  await open.first().click({ trial: true });
  await open.first().click();
  await page.waitForTimeout(1000);
  await page.screenshot({ path: join(shots, "offpeak-03-form.png") });
  await page.getByTestId("offpeak-form-title").fill(TITLE);
  await page.getByTestId("offpeak-form-instructions").fill("tidy up the README");
  const submit = page.getByTestId("offpeak-edit-submit");
  await waitFor("create enabled", () => submit.isEnabled());
  await submit.click();
  const [row] = await waitFor("off_peak_tasks row", () => {
    const all = rows(home);
    return all.length ? all : null;
  });
  await page.waitForTimeout(1500);
  await page.screenshot({ path: join(shots, "offpeak-04-after-submit.png") });
  // 有任务后列表分“定时 / 闲时”两个标签；闲时卡片在第二个标签下。
  const tabs = page.getByTestId("offpeak-tab");
  if (await tabs.isVisible().catch(() => false)) await tabs.getByRole("button").last().click();
  const card = page.getByTestId("offpeak-card").filter({ hasText: TITLE });
  await card.first().waitFor({ state: "visible", timeout: 30_000 });
  await card.first().click({ trial: true });
  await page.screenshot({ path: join(shots, "offpeak-05-card.png") });
  if (
    row.title !== TITLE ||
    row.status !== "queued" ||
    !row.server_ticket_id?.startsWith("mock-ticket-")
  )
    throw new Error(`unexpected off-peak row: ${JSON.stringify(row)}`);
  if (!JSON.parse(row.model_selection).providerId.includes("offpeak-idle-plan"))
    throw new Error(`unexpected selection: ${row.model_selection}`);
  console.info(`[e2e] off-peak task queued with ticket ${row.server_ticket_id}`);
}
