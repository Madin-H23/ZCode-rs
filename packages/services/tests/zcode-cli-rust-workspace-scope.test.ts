// Workspace 由请求给出（spec rust-cli-architecture「Workspace 作用域」）：进程 cwd 只是缺省值，
// 同一 cwd 的多个进程可以共存；会话、工具、插件、MCP 与技能都按请求或会话所属的 workspace 解析。
import assert from "node:assert/strict";
import test from "node:test";
import { access, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { z } from "zod";
import {
  zcodeMcpListResultSchema,
  zcodePluginsListResultSchema,
  zcodeSkillsReferenceCatalogResultSchema,
} from "@zcode/shared";
import { v4ConversationSubscribeResultSchema } from "@zcode/shared/zcode-protocol-v4";
import { fixture, type Harness } from "./zcode-cli-rust-fixture.js";
import { stdioServer } from "./zcode-cli-rust-mcp-fixture.js";

async function put(path: string, value: unknown) {
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, typeof value === "string" ? value : JSON.stringify(value));
}

const ref = (workspacePath: string, workspaceIdentity?: string) => ({
  workspacePath,
  workspaceKey: workspaceIdentity ?? workspacePath,
  ...(workspaceIdentity ? { workspaceIdentity } : {}),
});

/** Host 的会话订阅携带 attachment workspace：冷加载只接受该 workspace 的会话。 */
function subscribeIn(h: Harness, id: string, workspace: ReturnType<typeof ref>) {
  return h.client.request(
    "v4/conversation/subscribe",
    {
      topic: `conversation/${id}`,
      connectionId: "fixture-desktop",
      clientMode: "desktop-continuous",
      workspace,
    },
    v4ConversationSubscribeResultSchema,
  );
}

const listSchema = z.object({
  sessions: z.array(z.object({ sessionId: z.string() }).passthrough()),
});

/** 某个 sessions-index 话题的初始快照里的会话 id。 */
async function indexed(h: Harness, workspaceId: string) {
  const sub = await h.subscribe(`sessions-index/${workspaceId}`);
  const frame = await h.wait(
    (m) =>
      m.params?.subscriptionId === sub.ack.subscriptionId &&
      m.params.frame?.payload?.kind === "snapshot",
  );
  return frame.params.frame.payload.snapshot.sessions.map(
    (s: { sessionId: string }) => s.sessionId,
  );
}

function sessionRow(db: string, id: string) {
  const conn = new DatabaseSync(db, { readOnly: true });
  try {
    const row = conn
      .prepare("select directory, path, workspace_id from session where id = ?")
      .get(id) as Record<string, unknown> | undefined;
    return row ? { ...row } : undefined;
  } finally {
    conn.close();
  }
}

test("Two app-server processes on the same cwd coexist and each serves its requests", async () => {
  const f = await fixture();
  try {
    const first = f.start();
    const second = f.start();
    const [a, b] = await Promise.all([first.create(), second.create()]);
    await Promise.all([
      first.subscribe(`conversation/${a}`),
      second.subscribe(`conversation/${b}`),
    ]);
    await Promise.all([
      first.command(first.envelope("sendText", a, { text: "hello" })),
      second.command(second.envelope("sendText", b, { text: "hello" })),
    ]);
    await Promise.all([first.completed(a), second.completed(b)]);
    // 两个进程共用同一会话库：各自写入的会话都在库中。
    assert.ok(sessionRow(f.db, a));
    assert.ok(sessionRow(f.db, b));
    assert.deepEqual([first.schemaErrors, second.schemaErrors], [[], []]);
  } finally {
    await f.close();
  }
});

test("A process started in workspace A serves plugins, MCP status and skills of workspace B from B's files", async () => {
  const f = await fixture();
  try {
    const b = join(f.root, "workspace-b");
    const plugin = join(f.root, "plugins-src/b-demo");
    await put(join(plugin, ".zcode-plugin/plugin.json"), {
      name: "b-demo",
      version: "1.0.0",
      description: "Plugin of workspace B",
    });
    const mcp = await stdioServer(f.root);
    await put(join(b, ".zcode/config.json"), {
      plugins: { dirs: [plugin] },
      mcp: {
        servers: { "b-echo": { type: "stdio", command: process.execPath, args: [mcp.script] } },
      },
    });
    await put(
      join(b, ".agents/skills/b-skill/SKILL.md"),
      "---\nname: b-skill\ndescription: Skill of workspace B\n---\nBody",
    );
    // 进程 cwd 是 A（fixture 的 workspace），B 只出现在请求参数里。
    const h = f.start();
    const ws = (path: string) => ({ workspace: { workspacePath: path } });

    const pluginsB = await h.client.request("plugins/list", ws(b), zcodePluginsListResultSchema);
    const demo = pluginsB.plugins.find((p) => p.id === "b-demo@inline");
    assert.equal(demo?.rootSource, "workspace");
    const pluginsA = await h.client.request(
      "plugins/list",
      ws(f.cwd),
      zcodePluginsListResultSchema,
    );
    assert(!pluginsA.plugins.some((p) => p.id === "b-demo@inline"));

    const connected = await h.client.request(
      "mcp/list",
      { ...ws(b), mode: "connect" },
      zcodeMcpListResultSchema,
    );
    assert.equal(connected.statuses["b-echo"]?.status, "connected");
    assert.equal(connected.statuses["b-echo"]?.toolCount, 1);
    const status = await h.client.request(
      "mcp/list",
      { ...ws(b), mode: "status" },
      zcodeMcpListResultSchema,
    );
    assert.equal(status.statuses["b-echo"]?.status, "connected");
    // status 只报告同一 workspace 最近一次 connect 的结果；A 没有做过 connect。
    const statusA = await h.client.request(
      "mcp/list",
      { ...ws(f.cwd), mode: "status" },
      zcodeMcpListResultSchema,
    );
    assert.deepEqual(statusA.statuses, {});
    // A 的 connect 以替换语义收敛 mcp-status 租约：B 的 server 不在 A 的结果里。
    const connectedA = await h.client.request(
      "mcp/list",
      { ...ws(f.cwd), mode: "connect" },
      zcodeMcpListResultSchema,
    );
    assert.equal(connectedA.statuses["b-echo"], undefined);

    const skillsB = await h.client.request(
      "skills/referenceCatalog",
      ws(b),
      zcodeSkillsReferenceCatalogResultSchema,
    );
    assert.equal(skillsB.authority, "workspace");
    assert.equal(
      skillsB.skills.find((s) => s.name === "b-skill")?.description,
      "Skill of workspace B",
    );
    const skillsA = await h.client.request(
      "skills/referenceCatalog",
      ws(f.cwd),
      zcodeSkillsReferenceCatalogResultSchema,
    );
    assert(!skillsA.skills.some((s) => s.name === "b-skill"));
    assert.deepEqual(h.schemaErrors, []);
  } finally {
    await f.close();
  }
});

test("Sessions created in workspace B by a process started in A store B and run tools in B", async () => {
  const f = await fixture();
  try {
    const b = join(f.root, "workspace-b");
    await mkdir(b);
    await writeFile(join(b, "AGENTS.md"), "RULE_OF_B");
    await writeFile(join(f.cwd, "AGENTS.md"), "RULE_OF_A");
    const h = f.start();
    const created = (await h.command(h.envelope("createSession", null, { workspaceId: b }))).result;
    assert(created?.type === "createSession");
    const id = created.sessionId;
    await h.subscribe(`conversation/${id}`);
    await h.command(h.envelope("sendText", id, { text: "write" }));
    await h.completed(id);
    // 工具按会话所属 workspace 执行：相对路径落在 B，不落在进程 cwd A。
    assert.equal(await readFile(join(b, "result.txt"), "utf8"), "written by Rust");
    await assert.rejects(access(join(f.cwd, "result.txt")));
    // 提示上下文（AGENTS.md）同样来自 B。
    const context = JSON.stringify(f.requests[0]!.messages);
    assert.match(context, /RULE_OF_B/);
    assert.doesNotMatch(context, /RULE_OF_A/);
    // 库中会话行属于 B（本地 workspace 不写 workspace_id）。
    assert.deepEqual(sessionRow(f.db, id), { directory: b, path: b, workspace_id: null });
    // 列表与 sessions-index 按 workspace 过滤。
    assert.deepEqual(await indexed(h, b), [id]);
    assert.deepEqual(await indexed(h, f.cwd), []);
    const listedA = await h.client.request("session/list", { workspace: ref(f.cwd) }, listSchema);
    assert(!listedA.sessions.some((s) => s.sessionId === id));
    const listedB = await h.client.request("session/list", { workspace: ref(b) }, listSchema);
    assert(listedB.sessions.some((s) => s.sessionId === id));
    await h.close();

    // 重启后仍在 A 启动：带 B 的 attachment workspace 冷加载 B 的会话，工具仍在 B 执行。
    const recovered = f.start();
    await assert.rejects(subscribeIn(recovered, id, ref(f.cwd)), /Session unavailable/);
    await subscribeIn(recovered, id, ref(b));
    await rm(join(b, "result.txt"));
    const after = recovered.messages.length;
    await recovered.command(recovered.envelope("sendText", id, { text: "write" }));
    await recovered.completed(id, after);
    assert.equal(await readFile(join(b, "result.txt"), "utf8"), "written by Rust");
    await assert.rejects(access(join(f.cwd, "result.txt")));
    assert.deepEqual(recovered.schemaErrors, []);
  } finally {
    await f.close();
  }
});

// remote identity 的路径必须以 "/" 开头；Windows 临时目录不是这种形态。
test(
  "Remote identities key sessions by identity while the path roots files",
  {
    skip: process.platform === "win32",
  },
  async () => {
    const f = await fixture();
    try {
      const b = join(f.root, "workspace-b");
      await mkdir(b);
      const identity = `remote:ssh:fixture:22:me:${b}`;
      const h = f.start();
      const created = (
        await h.command(h.envelope("createSession", null, { workspaceId: identity }))
      ).result;
      assert(created?.type === "createSession");
      const id = created.sessionId;
      await h.subscribe(`conversation/${id}`);
      await h.command(h.envelope("sendText", id, { text: "write" }));
      await h.completed(id);
      // identity 解析出的路径是文件根；库中 directory/path 为路径，workspace_id 为 identity。
      assert.equal(await readFile(join(b, "result.txt"), "utf8"), "written by Rust");
      assert.deepEqual(sessionRow(f.db, id), { directory: b, path: b, workspace_id: identity });
      // 同一路径的本地 key 与 remote identity 是两个 workspace。
      assert.deepEqual(await indexed(h, identity), [id]);
      assert.deepEqual(await indexed(h, b), []);
      const remote = await h.client.request(
        "session/list",
        { workspace: ref(b, identity) },
        listSchema,
      );
      assert(remote.sessions.some((s) => s.sessionId === id));
      const local = await h.client.request("session/list", { workspace: ref(b) }, listSchema);
      assert(!local.sessions.some((s) => s.sessionId === id));
      // 非法的 remote identity fail-closed，不当作本地路径。
      const invalid = await h
        .command(h.envelope("createSession", null, { workspaceId: "remote:ssh:broken" }))
        .then(
          (ack) => ack.status,
          (error: Error) => error.message,
        );
      assert.match(invalid, /Invalid remote workspace identity|rejected/);
      assert.deepEqual(h.schemaErrors, []);
    } finally {
      await f.close();
    }
  },
);
