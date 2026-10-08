// 账号场景：已登录的 Z.ai 账号（隔离 HOME 里的加密凭据）+ 127.0.0.1 mock 的 ZCode / Z.ai 业务后端。
// 验证 Rust Host 的账号栈：读取 Node 格式的 credentials.json、按账号鉴权查询权益、把账号 provider
// 发布进模型选择，并把个人 Coding Plan key 缓存回凭据库。不点登录、不发模型请求到账号 provider，
// 所有请求只到本地 mock。
import { createCipheriv, createHash, randomBytes } from "node:crypto";
import { once } from "node:events";
import { readFile, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { join } from "node:path";

import { composer } from "../helpers.mjs";

const SECRET = "rust-host-e2e-account";
const CODING_PLAN_KEY_ID =
  "account-provider:coding-plan:account:zai-individual-coding-plan:account:user-1:api-key";

const backend = { origin: "", requests: [], server: null };

function encrypt(plaintext) {
  const key = createHash("sha256").update(SECRET).digest();
  const iv = randomBytes(12);
  const cipher = createCipheriv("aes-256-gcm", key, iv);
  const encrypted = Buffer.concat([cipher.update(plaintext, "utf-8"), cipher.final()]);
  return `enc:v1:${iv.toString("base64url")}.${cipher.getAuthTag().toString("base64url")}.${encrypted.toString("base64url")}`;
}

function jwt() {
  const encode = (value) => Buffer.from(JSON.stringify(value)).toString("base64url");
  return `${encode({ alg: "none" })}.${encode({ exp: Math.floor(Date.now() / 1000) + 86_400 })}.sig`;
}

const JWT = jwt();

function respond(path) {
  const now = Math.floor(Date.now() / 1000);
  switch (path) {
    case "/api/v1/zcode-plan/billing/balance":
      return {
        code: 0,
        data: {
          server_time: now,
          plans: [
            {
              user_plan_id: "up1",
              plan_id: "zcode-v3-start-plan",
              status: "active",
              ends_at: now + 86_400,
            },
          ],
          balances: [
            {
              user_plan_id: "up1",
              plan_id: "zcode-v3-start-plan",
              capabilities: ["model:glm-5.3-flash"],
              total_units: 100,
              used_units: 10,
              remaining_units: 90,
            },
          ],
        },
      };
    case "/api/biz/customer/getCustomerInfo":
      return {
        code: 200,
        data: {
          organizations: [
            { organizationId: "org-1", projects: [{ projectId: "proj-1", projectName: "默认项目" }] },
          ],
        },
      };
    case "/api/biz/v1/organization/org-1/projects/proj-1/api_keys":
      return { code: 200, data: [{ name: "zcode-api-key", apiKey: "key-abc" }] };
    case "/api/biz/v1/organization/org-1/projects/proj-1/api_keys/copy/key-abc":
      return { code: 200, data: { secretKey: "secret-xyz" } };
    case "/api/biz/subscription/list":
      return {
        code: 200,
        data: [
          {
            productId: "pro",
            productName: "GLM Coding Pro",
            status: "VALID",
            inCurrentPeriod: true,
            billingCycle: "monthly",
          },
        ],
      };
    case "/api/v1/client/configs":
      // 内置 Provider 配置没有 CDN 发布：Host 保持随包配置。
      return { code: 0, data: { configs: {} } };
    default:
      return { code: 404, msg: "not mocked" };
  }
}

/** 启动 mock 后端并让整个进程树指向它。 */
export async function env() {
  backend.server = createServer((request, response) => {
    const path = (request.url ?? "").split("?")[0];
    backend.requests.push({
      method: request.method,
      path,
      authorization: request.headers.authorization ?? "",
    });
    request.resume();
    response.writeHead(200, { "content-type": "application/json" });
    response.end(JSON.stringify(respond(path)));
  });
  backend.server.listen(0, "127.0.0.1");
  await once(backend.server, "listening");
  backend.server.unref();
  backend.origin = `http://127.0.0.1:${backend.server.address().port}`;
  return {
    ZCODE_BASE_URL: backend.origin,
    ZAI_BUSINESS_BASE_URL: backend.origin,
    ZAI_OAUTH_ORIGIN: backend.origin,
    BIGMODEL_API_BASE_URL: backend.origin,
    ZCODE_CREDENTIAL_SECRET: SECRET,
  };
}

/** 隔离 profile：Node 格式的加密凭据 + 选中 Z.ai 个人 Coding Plan。 */
export async function seed({ home }) {
  const config = join(home, ".zcode", "v2");
  const credentials = {
    "oauth:active_provider": encrypt("zai"),
    "oauth:zai:access_token": encrypt("zai-business-token"),
    zcodejwttoken: encrypt(JWT),
    "oauth:zai:user_info": encrypt(JSON.stringify({ user_id: "user-1", name: "E2E Alice" })),
  };
  await writeFile(
    join(config, "credentials.json"),
    `${JSON.stringify(credentials, null, 2)}\n`,
    { mode: 0o600 },
  );
  const settingsPath = join(config, "setting.json");
  const settings = JSON.parse(await readFile(settingsPath, "utf8"));
  settings.providerFamilyConnectionSelections = { zai: { kind: "individual-coding-plan" } };
  await writeFile(settingsPath, JSON.stringify(settings));
}

export async function run({ page, shots, home }) {
  try {
    // 已登录但本设备还没有 onboarding 记录：先出现引导（与 Node Host 相同），像用户一样跳过。
    // 跳过会经 Rust Host 的 onboarding-record.dismissOnboarding 写入记录。
    const skip = page.getByText("Skip", { exact: true }).first();
    await skip.waitFor({ state: "visible", timeout: 60_000 });
    await page.screenshot({ path: join(shots, "00-onboarding.png") });
    // 引导有多步，每步都可以跳过；直到出现 composer。
    for (let step = 0; step < 6 && (await skip.isVisible().catch(() => false)); step += 1) {
      await skip.click();
      await page.waitForTimeout(800);
    }
    await composer(page);
    // 侧栏账号区：OAuth 恢复出的用户名与个人套餐（订阅查询）徽标。
    await page.getByText("E2E Alice", { exact: true }).first().waitFor({ state: "visible" });
    await page.getByText("Pro", { exact: true }).first().waitFor({ state: "visible" });
    // 当前模型来自已选的个人 Coding Plan（账号层 current）；模型菜单列出账号 provider。
    await page.getByText("GLM-5.3", { exact: true }).first().click();
    await page.waitForTimeout(800);
    // 分组：Z.ai 个人套餐（GLM-5.3、GLM-5.3-Flash）、Start Plan（只含余额白名单里的 GLM-5.3-Flash）、Mock。
    await page.getByText("Individual", { exact: true }).first().waitFor({ state: "visible" });
    await page.getByText("Start Plan", { exact: true }).first().waitFor({ state: "visible" });
    const flash = await page.getByText("GLM-5.3-Flash", { exact: true }).count();
    if (flash !== 2) throw new Error(`expected GLM-5.3-Flash under both plans, got ${flash}`);
    await page.screenshot({ path: join(shots, "01-account-models.png") });
    await page.keyboard.press("Escape");
  } catch (error) {
    await page.screenshot({ path: join(shots, "01-account-models-failed.png") });
    throw error;
  }
  console.info("[e2e] account providers reached the model menu");

  const balance = backend.requests.find(
    (request) => request.path === "/api/v1/zcode-plan/billing/balance",
  );
  if (balance?.authorization !== `Bearer ${JWT}`) {
    throw new Error(`start plan balance not queried with the ZCode JWT: ${JSON.stringify(balance)}`);
  }
  const subscription = backend.requests.find(
    (request) => request.path === "/api/biz/subscription/list",
  );
  if (subscription?.authorization !== "key-abc.secret-xyz") {
    throw new Error(`subscription not queried with the coding plan key: ${JSON.stringify(subscription)}`);
  }
  // Host 把个人 Coding Plan key 按账号身份写回同一份（Node 格式的）凭据库。
  const stored = JSON.parse(await readFile(join(home, ".zcode", "v2", "credentials.json"), "utf8"));
  if (!String(stored[CODING_PLAN_KEY_ID] ?? "").startsWith("enc:v1:")) {
    throw new Error(`coding plan key was not cached: ${Object.keys(stored).join(", ")}`);
  }
  console.info(
    `[e2e] account backend requests: ${[...new Set(backend.requests.map((request) => request.path))].join(", ")}`,
  );
  backend.server?.close();
}
