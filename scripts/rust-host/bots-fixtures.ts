// Rust Host bots 的对拍 fixture（docs/specs/rust-host-automation.md 第 7 节）。
// 用真实 TS 实现生成：Bot 文案表（messages.ts 两种 locale 的原始模板）、命令解析、配置归一化与旧版
// 配置/状态导入后的 schema 输出；Rust 侧 include_str! 嵌入并逐项复现。
// 用法：pnpm exec tsx scripts/rust-host/bots-fixtures.ts [--check]
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { format } from "oxfmt";
import { botsConfigFileSchema, botsStateFileSchema, type BotConfig } from "../../packages/shared/src/index.js";
import { normalizeBotConfig } from "../../packages/services/src/bots/botConfigHelpers.js";
import { parseBotCommand } from "../../packages/services/src/bots/commandParser.js";
import { formatBotMessage, type BotMessageId } from "../../packages/services/src/bots/messages.js";
import {
  importLegacyBotConfig,
  importLegacyBotState,
} from "../../packages/services/src/bots/storageMigration.js";
import { normalizeAllowedWorkspaces } from "../../packages/services/src/bots/workspaceHelpers.js";

const root = resolve(import.meta.dirname, "../..");
const fixtureDir = resolve(root, "apps/zcode-host/crates/services/src/bots/fixtures");

async function messageTable(): Promise<Record<string, Record<string, string>>> {
  const source = await readFile(
    resolve(root, "packages/services/src/bots/messages.ts"),
    "utf8",
  );
  const ids = [...new Set([...source.matchAll(/^ {4}([A-Za-z]+):/gmu)].map((match) => match[1]!))];
  const table: Record<string, Record<string, string>> = {};
  for (const locale of ["zh-CN", "en-US"] as const) {
    table[locale] = Object.fromEntries(
      ids.map((id) => [id, formatBotMessage(locale, id as BotMessageId)]),
    );
  }
  return table;
}

const COMMANDS = [
  "hello",
  "  /BIND abc123 ",
  "/bind",
  "/help",
  "/帮助",
  "/cancel",
  "0",
  " 0 ",
  "/status",
  "/状态",
  "/new",
  "/clear",
  "/reconnect",
  "/workspace",
  "/project 2",
  "/model",
  "/model provider p1",
  "/model model m1",
  "/model glm-5",
  "/mode plan",
  "/think",
  "/thought-level high",
  "/task",
  "/task 3",
  "/reply summary",
  "/stop",
  "/permission",
  "/permission 1",
  "/answer submit",
  "/回答 完成",
  "/answer yes",
  "/approve r1 o1",
  "/approve r1",
  "/deny r1",
  "/unknown thing",
  "/",
  "/Status\textra",
];

const BOT_BASE = {
  id: " bot-1 ",
  name: " Bot ",
  enabled: true,
  allowedWorkspaces: [],
  allowedCommands: { status: true, new: true, workspace: true, model: true, thoughtLevel: true, reply: true },
  currentOptions: {},
};

const NORMALIZE_CASES = [
  { ...BOT_BASE, provider: "feishu" },
  { ...BOT_BASE, provider: "lark", replyMode: "assistant_changes" },
  {
    ...BOT_BASE,
    provider: "telegram",
    replyMode: "streaming_card",
    allowedWorkspaces: [" /a ", "/b", "/a", ""],
    allowedCommands: { status: false, new: true, workspace: true, model: true, thoughtLevel: true, reply: true, cli: true, sandboxMode: false },
    currentOptions: {
      cli: "glm",
      mode: "plan",
      modelSelection: { providerId: "p", modelId: "m", options: { reasoningLevel: "high" } },
    },
  },
  {
    ...BOT_BASE,
    provider: "weixin",
    webhookUrl: "https://example.invalid/hook",
    currentOptions: { modelSelection: { providerId: "", modelId: "m" }, approvalPolicy: "never" },
  },
  { ...BOT_BASE, provider: "webhook", replyMode: "summary_changes", allowedWorkspaces: ["*", "/a"] },
];

const LEGACY_CONFIGS = [
  {
    version: 2,
    bots: [
      {
        ...BOT_BASE,
        id: "legacy-1",
        name: "Legacy",
        provider: "telegram",
        replyMode: "assistant_changes",
        currentOptions: { cli: "glm", model: "builtin:bigmodel-coding-plan/glm-4.6", thoughtLevel: "high" },
      },
      {
        ...BOT_BASE,
        id: "legacy-2",
        name: "Legacy 2",
        provider: "feishu",
        replyMode: "streaming_card",
        currentOptions: { model: "custom:my%20provider:model%2Fx" },
      },
      {
        ...BOT_BASE,
        id: "legacy-3",
        name: "Legacy 3",
        provider: "telegram",
        replyMode: "assistant_changes",
        currentOptions: { modelSelection: { providerId: "builtin:zai", modelId: "glm-4.5" } },
      },
      {
        ...BOT_BASE,
        id: "legacy-4",
        name: "Legacy 4",
        provider: "telegram",
        replyMode: "assistant_changes",
        currentOptions: { modelSelection: { providerId: "builtin:unknown", modelId: "x" }, mode: "yolo" },
      },
    ],
  },
];

const LEGACY_STATES = [
  {
    version: 2,
    bots: {
      "legacy-1": {
        botId: "legacy-1",
        workspacePath: "/w",
        mode: "draft",
        activeTaskId: null,
        draftOptions: { provider: "claude", model: "builtin:zai-start-plan/GLM-4.7", thoughtLevel: "", mode: "plan" },
        extra: "dropped",
        updatedAt: 5,
      },
      "legacy-2": {
        botId: "legacy-2",
        workspacePath: "/w2",
        workspaceIdentity: "remote:x",
        mode: "task",
        activeTaskId: "t1",
        pendingPermissionOptions: [
          {
            requestId: "r",
            optionId: "o",
            command: "approve",
            label: "Allow",
            response: { decision: "allow", reason: "ok" },
            stray: 1,
          },
        ],
        telegramOffset: 7,
        updatedAt: 6,
      },
    },
  },
];

function run() {
  return {
    commands: COMMANDS.map((text) => ({ text, command: parseBotCommand(text) })),
    normalize: NORMALIZE_CASES.map((bot) => ({
      input: bot,
      output: normalizeBotConfig(bot as unknown as BotConfig),
    })),
    allowedWorkspaces: [[], [" "], ["*"], ["/a", " /a ", "/b"], ["/a", "*"]].map((input) => ({
      input,
      output: normalizeAllowedWorkspaces(input),
    })),
    legacyConfig: LEGACY_CONFIGS.map((input) => ({
      input,
      output: botsConfigFileSchema.parse(importLegacyBotConfig(input)),
    })),
    legacyState: LEGACY_STATES.map((input) => ({
      input,
      output: botsStateFileSchema.parse(importLegacyBotState(input)),
    })),
  };
}

async function emit(name: string, data: unknown): Promise<boolean> {
  const path = join(fixtureDir, name);
  const formatted = await format(path, `${JSON.stringify(data, null, 2)}\n`);
  if (formatted.errors.length) throw new Error(`Cannot format ${name}`);
  if (process.argv.includes("--check")) {
    const current = await readFile(path, "utf8").catch(() => "");
    if (current === formatted.code) return true;
    console.error(
      `[bots-fixtures] ${name} is stale; run pnpm exec tsx scripts/rust-host/bots-fixtures.ts`,
    );
    return false;
  }
  await mkdir(fixtureDir, { recursive: true });
  await writeFile(path, formatted.code);
  return true;
}

const results = [
  await emit("messages.json", {
    source: "scripts/rust-host/bots-fixtures.ts",
    ...(await messageTable()),
  }),
  await emit("parity.json", { source: "scripts/rust-host/bots-fixtures.ts", ...run() }),
];
if (results.includes(false)) process.exitCode = 1;
else
  console.log(
    `[bots-fixtures] ${process.argv.includes("--check") ? "up to date" : "wrote"}: messages.json, parity.json`,
  );
