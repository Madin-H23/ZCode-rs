// e2e-desktop.mjs 的界面流程：发送一条消息，等 mock 模型的回复流式出现在对话里。
import { join } from "node:path";

async function composer(page) {
  const candidates = [
    page.locator('textarea[placeholder*="Ask ZCode"]'),
    page.locator('[contenteditable="true"]'),
  ];
  for (let attempt = 0; attempt < 120; attempt += 1) {
    for (const candidate of candidates) {
      const first = candidate.first();
      if (await first.isVisible().catch(() => false)) return first;
    }
    await page.waitForTimeout(500);
  }
  throw new Error("composer not found");
}

export async function runCoreFlow({ page, shots, model, mockReply }) {
  const input = await composer(page);
  await page.screenshot({ path: join(shots, "01-ready.png") });
  await input.click();
  await page.keyboard.type("say hello");
  await page.keyboard.press("Enter");
  try {
    await page.getByText(mockReply).first().waitFor({ state: "visible", timeout: 60_000 });
  } finally {
    await page.screenshot({ path: join(shots, "02-after-send.png") });
  }
  if (model.requests.length === 0) throw new Error("mock model never received a request");
  console.info(`[e2e] reply streamed; model requests=${model.requests.length}`);
}
