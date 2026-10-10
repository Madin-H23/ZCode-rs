import assert from "node:assert/strict";
import test from "node:test";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { createAgentToolEntry } from "../../../apps/zcode-cli/packages/core/src/tool/handlers/agent.js";
import { askUserQuestionToolEntry } from "../../../apps/zcode-cli/packages/core/src/tool/handlers/ask-user-question.js";
import { createBashProviderDescription } from "../../../apps/zcode-cli/packages/core/src/tool/handlers/bash-prompt.js";
import { editToolEntry } from "../../../apps/zcode-cli/packages/core/src/tool/handlers/edit.js";
import { globToolEntry } from "../../../apps/zcode-cli/packages/core/src/tool/handlers/glob.js";
import { grepToolEntry } from "../../../apps/zcode-cli/packages/core/src/tool/handlers/grep.js";
import {
  createEnterPlanModeToolEntry,
  exitPlanModeToolEntry,
} from "../../../apps/zcode-cli/packages/core/src/tool/handlers/plan-mode.js";
import { readToolEntry } from "../../../apps/zcode-cli/packages/core/src/tool/handlers/read.js";
import { sendMessageToolEntry } from "../../../apps/zcode-cli/packages/core/src/tool/handlers/send-message.js";
import { skillToolEntry } from "../../../apps/zcode-cli/packages/core/src/tool/handlers/skill.js";
import { taskOutputToolEntry } from "../../../apps/zcode-cli/packages/core/src/tool/handlers/task-output.js";
import { taskStopToolEntry } from "../../../apps/zcode-cli/packages/core/src/tool/handlers/task-stop.js";
import {
  todoReadToolEntry,
  todoWriteToolEntry,
} from "../../../apps/zcode-cli/packages/core/src/tool/handlers/todo.js";
import { webFetchToolEntry } from "../../../apps/zcode-cli/packages/core/src/tool/handlers/webfetch.js";
import { webSearchToolEntry } from "../../../apps/zcode-cli/packages/core/src/tool/handlers/websearch.js";
import { writeToolEntry } from "../../../apps/zcode-cli/packages/core/src/tool/handlers/write.js";
import type { ToolExecutionModelContext } from "../../../apps/zcode-cli/packages/core/src/tool/types.js";
import {
  parseAgentProfileFromMarkdown,
  type AgentProfile,
} from "../../../apps/zcode-cli/packages/core/src/subagent/profile.js";
import { fixture } from "./zcode-cli-rust-fixture.js";

interface Definition {
  function: { name: string; description: string; parameters: unknown };
}

function assertText(actual: string | undefined, expected: string | undefined, label: string) {
  assert.equal(typeof actual, "string", `${label}: Rust description missing`);
  assert.equal(typeof expected, "string", `${label}: Node description missing`);
  assert.equal(actual, expected, `${label}: description differs`);
  assert.deepEqual(Buffer.from(actual!, "utf8"), Buffer.from(expected!, "utf8"), `${label}: bytes`);
}

function description(definitions: Definition[], name: string): string {
  const tool = definitions.find((entry) => entry.function.name === name);
  assert(tool, `${name}: definition missing`);
  return tool.function.description;
}

async function driver() {
  const root = await mkdtemp(join(tmpdir(), "native-tool-prompts-"));
  const child = spawn(
    resolve(
      `apps/zcode-cli-rust/target/debug/examples/tool_fixture${process.platform === "win32" ? ".exe" : ""}`,
    ),
    [root],
  );
  const exited = once(child, "close");
  const lines = createInterface({ input: child.stdout });
  let errors = "";
  child.stderr.on("data", (chunk) => (errors += chunk));
  return {
    async definitions(input: Record<string, unknown> = {}): Promise<Definition[]> {
      const next = once(lines, "line");
      child.stdin.write(`${JSON.stringify({ definitions: true, ...input })}\n`);
      const [line] = await next;
      return JSON.parse(line).definitions;
    },
    async close() {
      child.stdin.end();
      try {
        assert.deepEqual(await exited, [0, null], errors);
      } finally {
        lines.close();
        await rm(root, { recursive: true, force: true });
      }
    },
  };
}

const branch = { embeddedSearchEnabled: false, dynamicWorkflowEnabled: false } as const;

test("Rust's 18 built-in descriptions match the same Node factory branch byte for byte", async () => {
  const d = await driver();
  try {
    // 这里只比较相同能力的 factory；不把 Node 默认 embedded runtime 当作同一分支。
    const entries = [
      readToolEntry,
      writeToolEntry,
      editToolEntry,
      globToolEntry,
      grepToolEntry,
      {
        metadata: {
          name: "Bash",
          description: createBashProviderDescription({
            embeddedSearchEnabled: false,
            defaultTimeoutMs: 120_000,
            maxTimeoutMs: 600_000,
          }),
        },
      },
      taskOutputToolEntry,
      taskStopToolEntry,
      skillToolEntry,
      askUserQuestionToolEntry,
      todoReadToolEntry,
      todoWriteToolEntry,
      createAgentToolEntry(branch),
      sendMessageToolEntry,
      webFetchToolEntry,
      webSearchToolEntry,
      createEnterPlanModeToolEntry({ embeddedSearchEnabled: false }),
      exitPlanModeToolEntry,
    ];
    const definitions = await d.definitions();
    assert.deepEqual(
      definitions.map((entry) => entry.function.name).sort(),
      entries.map((entry) => entry.metadata.name).sort(),
    );
    assert.equal(definitions.length, 18);
    for (const entry of entries) {
      assertText(
        description(definitions, entry.metadata.name),
        entry.metadata.description,
        entry.metadata.name,
      );
    }
  } finally {
    await d.close();
  }
});

function readProjection(supportsPdf: boolean, supportsImage = true) {
  const inputFormat = {
    supportsText: true,
    supportsImage,
    supportsPdf,
    supportsVideo: false,
    supportsAudio: false,
  };
  const context = { model: { properties: { inputFormat } } } as ToolExecutionModelContext;
  return { inputFormat, expected: readToolEntry.resolveModelContract!(context) };
}

test("Rust Read selects the full Node PDF description without changing schema or line placement", async () => {
  const d = await driver();
  try {
    // 每次重新构造定义，沿用生产调用方式；不改变模型投影生命周期。
    for (const [supportsPdf, supportsImage] of [
      [false, true],
      [true, true],
      [true, false],
      [true, true],
    ]) {
      const { inputFormat, expected } = readProjection(supportsPdf!, supportsImage!);
      const definitions = await d.definitions({ inputFormat });
      const actual = description(definitions, "Read");
      assertText(actual, expected.description, `Read ${JSON.stringify(inputFormat)}`);
      assert.deepEqual(
        definitions.find((entry) => entry.function.name === "Read")!.function.parameters,
        expected.inputSchema,
      );
      assert.equal(actual.split("- Reads PDFs").length - 1, supportsPdf ? 1 : 0);
    }
  } finally {
    await d.close();
  }
});

function profile(name: string, options: Partial<AgentProfile> = {}): AgentProfile {
  return {
    name,
    description: `${name} description`,
    source: "project",
    systemPrompt: "",
    ...options,
  };
}

test("Rust Agent catalog formatting preserves Node profile order, overrides and literal text", async () => {
  const d = await driver();
  const cases: Array<{ name: string; profiles: AgentProfile[] }> = [
    { name: "built-ins", profiles: [] },
    { name: "input order", profiles: [profile("z-last"), profile("a-first")] },
    {
      name: "project Explore and disallowed aliases",
      profiles: [
        profile("Explore", {
          tools: ["Read", "Bash", "EnterPlanMode", "ExitPlanMode", "web_search", "Grep"],
          disallowedTools: ["Bash(git status)", "WebSearch"],
        }),
      ],
    },
    {
      name: "absent, empty and wildcard tools",
      profiles: [
        profile("absent"),
        profile("empty", { tools: [] }),
        profile("wildcard", { tools: ["*"], disallowedTools: ["Read"] }),
      ],
    },
    {
      name: "duplicate overrides keep their original slot",
      profiles: [
        profile("duplicate", { description: "first" }),
        profile("between"),
        profile("duplicate", { description: "replacement", tools: ["Read"] }),
        profile("general-purpose", { description: "custom general", tools: ["Bash", "Read"] }),
      ],
    },
    {
      name: "built-in Explore keeps its description tool priority",
      profiles: [profile("Explore", { source: "built-in", tools: ["Read"] })],
    },
    {
      name: "Unicode, whitespace and placeholder-like text are literal",
      profiles: [
        profile("审阅🔎", {
          description:
            "  e\u0301 / é\r\n{profiles} {{agentProfiles}} {agentProfiles}\nAvailable agent types and the tools they have access to:",
          tools: ["Read", "mcp__演示__查看"],
        }),
      ],
    },
  ];
  try {
    for (const input of cases) {
      const before = structuredClone(input.profiles);
      const definitions = await d.definitions({ profiles: input.profiles });
      assertText(
        description(definitions, "Agent"),
        createAgentToolEntry({ ...branch, profiles: input.profiles }).metadata.description,
        `Agent ${input.name}`,
      );
      assert.deepEqual(input.profiles, before);
    }
  } finally {
    await d.close();
  }
});

test("Rust model requests contain one dynamic Agent catalog and the Node PDF Read description", async () => {
  const { inputFormat, expected } = readProjection(true);
  const f = await fixture({ config: { formatProperties: { inputFormat } } });
  try {
    const markdown = [
      "---",
      "name: prompt-fixture",
      "description: 审阅 🔎 {agentProfiles}",
      "tools: [Read, EnterPlanMode]",
      "---",
      "Synthetic prompt fixture.",
    ].join("\n");
    const parsed = parseAgentProfileFromMarkdown({ content: markdown, source: "project" });
    assert(parsed.profile);
    const directory = join(f.cwd, ".zcode", "agents");
    await mkdir(directory, { recursive: true });
    await writeFile(join(directory, "prompt-fixture.md"), markdown);
    const h = f.start();
    const id = await h.create();
    await h.subscribe(`conversation/${id}`);
    await h.command(h.envelope("sendText", id, { text: "Compare tool descriptions." }));
    await h.completed(id);
    assert.equal(f.requests.length, 1);
    const definitions = f.requests[0]!.tools as Definition[];
    const agent = description(definitions, "Agent");
    assertText(
      agent,
      createAgentToolEntry({ ...branch, profiles: [parsed.profile] }).metadata.description,
      "runtime Agent",
    );
    assert.equal(agent.split("Available agent types and the tools they have access to:").length, 2);
    assert(!agent.includes("Current profile catalog (authoritative)"));
    assertText(description(definitions, "Read"), expected.description, "runtime PDF Read");
    assert.deepEqual(h.schemaErrors, []);
  } finally {
    await f.close();
  }
});
