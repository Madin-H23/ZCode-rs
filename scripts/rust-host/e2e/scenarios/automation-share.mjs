// conversation-share：zcode://share/import deep link 触发导入。Rust Host 从 127.0.0.1 上的 mock 分享
// 服务取 continuation（复核完整性）、下载并校验结果物、落盘只读副本，再经 Rust CLI 创建带
// sharedContext 历史的会话；界面打开导入的会话。不连任何真实服务。
//
// 分享发布入口（顶栏分享按钮）要求登录，Rust Host 还没有账号，所以这里同时断言它是隐藏的。
import { createHash } from "node:crypto";
import { once } from "node:events";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { join } from "node:path";

import { composer } from "../helpers.mjs";

const SHARE_CODE = "e2e-share-code";
const ARTIFACT = Buffer.from("# Shared notes\n");
let server;

function canonical(value) {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  return `{${Object.keys(value)
    .sort()
    .map((key) => `${JSON.stringify(key)}:${canonical(value[key])}`)
    .join(",")}}`;
}

const sha256 = (input) => createHash("sha256").update(input).digest("hex");

function continuation(origin) {
  const rows = [
    { kind: "turnHeader", rowId: 1, productTurnId: "t1", state: "completed" },
    { kind: "userInput", rowId: 2, productTurnId: "t1", text: "What changed?" },
    { kind: "assistantText", rowId: 3, productTurnId: "t1", text: "The notes were updated." },
  ];
  const artifacts = [
    {
      artifact_id: "a1",
      logical_artifact_key: "notes",
      producer_product_turn_id: "t1",
      artifact_version: 1,
      state: "current",
      ref: "zcode-artifact://share/a1",
      artifact_type: "md",
      display_name: "notes.md",
      extension: "md",
      mime_type: "text/markdown",
      size_bytes: ARTIFACT.length,
      sha256: sha256(ARTIFACT),
    },
  ];
  const integrity = {
    projection_sha256: sha256(canonical(rows)),
    artifact_set_sha256: sha256(canonical(artifacts)),
  };
  return {
    schema_version: 1,
    import_grant_id: "grant-1",
    import_grant_expires_at: Date.now() + 60_000,
    share: {
      share_id: "share-e2e",
      title: "E2E shared chat",
      access_mode: "public_importable",
      created_at: 1,
      expires_at: Date.now() + 60_000,
    },
    rows,
    artifacts: artifacts.map((artifact) => ({
      ...artifact,
      download_url: `${origin}/download/a1`,
      download_url_expires_at: Date.now() + 60_000,
    })),
    integrity,
  };
}

export async function env() {
  server = createServer((request, response) => {
    const origin = `http://127.0.0.1:${server.address().port}`;
    if (request.url === "/download/a1") {
      response.writeHead(200, { "content-type": "text/markdown" });
      response.end(ARTIFACT);
      return;
    }
    if (request.method === "POST" && request.url === `/api/v1/shares/${SHARE_CODE}/continuation`) {
      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify({ code: 0, msg: "ok", data: continuation(origin) }));
      return;
    }
    response.writeHead(404, { "content-type": "application/json" });
    response.end(JSON.stringify({ code: 3211, msg: "not found" }));
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  return { ZCODE_BASE_URL: `http://127.0.0.1:${server.address().port}` };
}

async function waitFor(description, check, timeout = 60_000) {
  const deadline = Date.now() + timeout;
  for (;;) {
    const value = await check();
    if (value) return value;
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${description}`);
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
}

export async function run({ page, shots, workspace, secondInstance }) {
  try {
    await composer(page);
    // 冷启动 argv 里的 deep link 要等 renderer ready；用第二个实例把它转交给运行中的 App。
    await secondInstance([`zcode://share/import?code=${SHARE_CODE}`]);
    const title = page.getByText("E2E shared chat").first();
    await title.waitFor({ state: "visible", timeout: 60_000 });
    await page.waitForTimeout(1000);
    await page.screenshot({ path: join(shots, "share-01-imported.png") });
    const root = join(workspace, ".zcode-share", "share-e2e");
    const copy = JSON.parse(await readFile(join(root, "shared-conversation.json"), "utf8"));
    if (copy.title !== "E2E shared chat" || copy.rows.length !== 3)
      throw new Error(`unexpected imported copy: ${JSON.stringify(copy).slice(0, 200)}`);
    const artifact = await readFile(join(root, "shared-artifacts", "notes.md"));
    if (!artifact.equals(ARTIFACT)) throw new Error("imported artifact bytes differ");
    if (existsSync(join(root, ".zcode-share-import.json")))
      throw new Error("import marker was not removed after commit");
    await waitFor("imported session in the sidebar", () =>
      page
        .getByText(/From Share: E2E shared chat|来自分享：E2E shared chat/)
        .first()
        .isVisible(),
    );
    // 未登录：分享发布入口隐藏。
    if (await page.getByTestId("conversation-share-trigger").count())
      throw new Error("share trigger should be hidden while logged out");
    console.info("[e2e] shared conversation imported");
  } finally {
    server?.close();
  }
}
