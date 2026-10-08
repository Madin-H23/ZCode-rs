// agent 子系统场景（docs/specs/rust-host-agent.md §6）：
// 1. 启动经过 preparing_session_storage：Main 的启动状态镜像带着会话库的迁移基线；
// 2. Composer 的 "/" 菜单列出 workspace 技能：getSkillReferenceCatalog 经 chat lane 读取。
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";

import { composer, sseText } from "../helpers.mjs";

const SKILL_NAME = "e2e-hello-skill";
const REPLY = "Hello from the agent subsystem scenario";

export const mockRules = [
  {
    name: "default",
    match: () => true,
    respond: ({ response }) => response.end(sseText(REPLY)),
  },
];

/** workspace 级技能：CLI 从 `<workspace>/.zcode/skills/<name>/SKILL.md` 发现。 */
export async function seed({ workspace }) {
  const directory = join(workspace, ".zcode", "skills", SKILL_NAME);
  await mkdir(directory, { recursive: true });
  await writeFile(
    join(directory, "SKILL.md"),
    `---\nname: ${SKILL_NAME}\ndescription: Says hello for the Rust Host E2E\n---\n\nSay hello.\n`,
  );
}

async function startupSnapshot(page) {
  return page.evaluate(
    () =>
      new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error("no startup snapshot")), 10_000);
        const onMessage = (event) => {
          if (event.source !== window || event.data?.type !== "zcode:database-startup-state") return;
          clearTimeout(timer);
          window.removeEventListener("message", onMessage);
          resolve(event.data.state);
        };
        window.addEventListener("message", onMessage);
        window.postMessage(
          { type: "zcode:database-startup-control", control: { action: "snapshot" } },
          "*",
        );
      }),
  );
}

export async function run({ page, shots }) {
  const input = await composer(page);
  await page.screenshot({ path: join(shots, "agent-01-ready.png") });

  const state = await startupSnapshot(page);
  if (state.phase !== "ready") throw new Error(`startup not ready: ${JSON.stringify(state)}`);
  const sessionBaselines = (state.migrationBaselines ?? []).filter(
    (baseline) => baseline.databaseKind === "session",
  );
  if (sessionBaselines.length === 0) {
    throw new Error(`session storage was not prepared: ${JSON.stringify(state)}`);
  }
  console.info(`[e2e:agent] startup prepared ${sessionBaselines.length} session database(s)`);

  // "/" 菜单：技能目录来自 chat lane 的 skills/referenceCatalog。
  await input.click();
  await page.keyboard.type("/");
  const skill = page.getByText(SKILL_NAME).first();
  try {
    await skill.waitFor({ state: "visible", timeout: 30_000 });
  } finally {
    await page.screenshot({ path: join(shots, "agent-02-slash-menu.png") });
  }
  // 命中测试：菜单项可点击（不被遮挡），而不只是 DOM 可见。
  await skill.click({ trial: true });
  console.info("[e2e:agent] workspace skill listed in the slash menu");
  await page.keyboard.press("Escape");
}
