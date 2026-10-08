#!/usr/bin/env node
// Rust Host 契约清单（docs/specs/rust-host-r0.md §4.5）。
// 从 Renderer 实际使用的 RemoteServiceAccess 出发，用 TS 类型检查器列出 40 个 channel 及其成员，
// 产物供 apps/zcode-host 按清单注册 channel；--check 只比较不写入。
import { readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const sourcePath = resolve(root, "packages/client/src/remoteServiceAccess.ts");
const tsconfigPath = resolve(root, "packages/client/tsconfig.json");
const outputPath = resolve(root, "apps/zcode-host/contract/channels.json");

// 与 packages/rpc/src/proxy-channel.ts 的 isEvent / isDynamicEvent 相同的判定。
const isUpper = (code) => code >= 65 && code <= 90;
function memberKind(name) {
  if (name.length >= 10 && name.startsWith("onDynamic") && isUpper(name.charCodeAt(9)))
    return "dynamicEvent";
  if (name.length >= 3 && name.startsWith("on") && isUpper(name.charCodeAt(2))) return "event";
  return "method";
}

function loadProgram() {
  const config = ts.readConfigFile(tsconfigPath, ts.sys.readFile);
  if (config.error)
    throw new Error(ts.flattenDiagnosticMessageText(config.error.messageText, "\n"));
  const parsed = ts.parseJsonConfigFileContent(config.config, ts.sys, dirname(tsconfigPath));
  return ts.createProgram({ rootNames: [sourcePath], options: parsed.options });
}

function resolveChannelName(checker, descriptorExpression) {
  // X.channelName：X 是 createServiceDescriptor<T>(ServiceChannels.Y) 的常量，取 Y 的字面量类型。
  if (!ts.isPropertyAccessExpression(descriptorExpression)) return undefined;
  let symbol = checker.getSymbolAtLocation(descriptorExpression.expression);
  if (symbol && symbol.flags & ts.SymbolFlags.Alias) symbol = checker.getAliasedSymbol(symbol);
  const declaration = symbol?.valueDeclaration;
  if (!declaration || !ts.isVariableDeclaration(declaration) || !declaration.initializer)
    return undefined;
  const call = declaration.initializer;
  if (!ts.isCallExpression(call) || call.arguments.length !== 1) return undefined;
  const type = checker.getTypeAtLocation(call.arguments[0]);
  return type.isStringLiteral() ? type.value : undefined;
}

function collectChannels(program) {
  const checker = program.getTypeChecker();
  const source = program.getSourceFile(sourcePath);
  if (!source) throw new Error(`missing ${sourcePath}`);
  const channels = [];
  const visit = (node) => {
    if (
      ts.isCallExpression(node) &&
      ts.isPropertyAccessExpression(node.expression) &&
      node.expression.name.text === "toService" &&
      node.typeArguments?.length === 1
    ) {
      const [getChannelCall] = node.arguments;
      if (!getChannelCall || !ts.isCallExpression(getChannelCall)) {
        throw new Error(`unexpected toService call at ${node.getText(source)}`);
      }
      const channel = resolveChannelName(checker, getChannelCall.arguments[0]);
      if (!channel) throw new Error(`cannot resolve channel for ${node.getText(source)}`);
      const serviceType = checker.getTypeFromTypeNode(node.typeArguments[0]);
      const members = checker
        .getPropertiesOfType(serviceType)
        .map((property) => property.getName())
        .sort()
        .map((name) => ({ name, kind: memberKind(name) }));
      channels.push({ channel, interface: node.typeArguments[0].getText(source), members });
    }
    ts.forEachChild(node, visit);
  };
  visit(source);
  return channels.sort((a, b) => a.channel.localeCompare(b.channel));
}

const program = loadProgram();
const channels = collectChannels(program);
const content = `${JSON.stringify({ source: "packages/client/src/remoteServiceAccess.ts", channels }, null, 2)}\n`;

if (process.argv.includes("--check")) {
  const current = await readFile(outputPath, "utf8").catch(() => "");
  if (current !== content) {
    console.error(
      `[contract-inventory] ${outputPath} is stale; run node scripts/rust-host/contract-inventory.mjs`,
    );
    process.exit(1);
  }
  console.log(`[contract-inventory] up to date: ${channels.length} channels`);
} else {
  await writeFile(outputPath, content);
  const memberCount = channels.reduce((sum, entry) => sum + entry.members.length, 0);
  console.log(`[contract-inventory] wrote ${channels.length} channels, ${memberCount} members`);
}
