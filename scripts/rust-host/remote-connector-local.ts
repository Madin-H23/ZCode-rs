// 测试用远端连接器（docs/specs/rust-host-remote.md §7）：协议与产品连接器相同（runRemoteConnector），
// 只把“探测 → 部署 → 启动”换成直接在本机拉起本仓库构建的 zcode-server.cjs，backend 的 exec / upload
// 在本机执行。用于在没有 SSH / Docker 的环境里验证 Rust ConnectorTransport 的完整链路。
//   node --import tsx scripts/rust-host/remote-connector-local.ts <node> <zcode-server.cjs>
// 由 Rust Host 当作产品连接器拉起时（只传脚本路径），参数改从环境变量读取：
//   ZCODE_TEST_REMOTE_NODE、ZCODE_TEST_REMOTE_SERVER_BUNDLE、ZCODE_TEST_REMOTE_HOME；
//   ZCODE_TEST_REMOTE_DEAD_PROXY 设置时，
//   远端 server 的出站 HTTP 一律经这个（已关闭的）本机代理，不访问任何真实服务。
import { spawn } from "node:child_process";
import { copyFile, mkdir } from "node:fs/promises";
import { dirname } from "node:path";
import { Emitter } from "../../packages/rpc/src/index.ts";
import type { StdioStream } from "../../packages/server/src/remote/backend.ts";
import { performHandshake } from "../../packages/server/src/remote/handshake.ts";
import {
  runRemoteConnector,
  type OpenRemoteServer,
} from "../../packages/desktop/src/host/remoteConnector.ts";

const nodeBinary = process.argv[2] ?? process.env.ZCODE_TEST_REMOTE_NODE;
const serverBundle = process.argv[3] ?? process.env.ZCODE_TEST_REMOTE_SERVER_BUNDLE;
const deadProxy = process.env.ZCODE_TEST_REMOTE_DEAD_PROXY;
if (!nodeBinary || !serverBundle) {
  throw new Error("usage: remote-connector-local.ts <node> <zcode-server.cjs>");
}

function execLocal(command: string): StdioStream {
  // 远端 server 不需要 tsx 加载器；测试时出站流量经已关闭的代理，确保不触网。
  const env: NodeJS.ProcessEnv = { ...process.env, NODE_OPTIONS: "" };
  // “远端”机器的 HOME 与本机 Host 的数据目录分开（ZCODE_TEST_REMOTE_HOME）。
  if (process.env.ZCODE_TEST_REMOTE_HOME) env.HOME = process.env.ZCODE_TEST_REMOTE_HOME;
  if (deadProxy) {
    Object.assign(env, {
      NODE_USE_ENV_PROXY: "1",
      HTTP_PROXY: deadProxy,
      HTTPS_PROXY: deadProxy,
      ZCODE_BASE_URL: deadProxy,
      ZCODE_ENDPOINT_ORIGIN: deadProxy,
    });
  }
  const child = spawn("/bin/sh", ["-c", command], { env, stdio: "pipe" });
  const onClose = new Emitter<number>();
  child.on("exit", (code) => onClose.fire(code ?? -1));
  return { stdin: child.stdin, stdout: child.stdout, stderr: child.stderr, onClose: onClose.event };
}

const open: OpenRemoteServer = async (request) => {
  console.log(`local test connector: target=${request.target.kind}`);
  const stream = execLocal(`exec "${nodeBinary}" "${serverBundle}"`);
  stream.stderr.on("data", (chunk: Buffer) => {
    console.log(`[remote] ${chunk.toString().trimEnd()}`);
  });
  const { hello, remaining } = await performHandshake(stream, "local-test-connector");
  return {
    hello,
    stream,
    remaining,
    backend: {
      exec: async (command: string) => execLocal(command),
      upload: async (localPath: string, remotePath: string) => {
        await mkdir(dirname(remotePath), { recursive: true });
        await copyFile(localPath, remotePath);
      },
    },
    dispose() {},
    async disposeAndWait() {},
  };
};

runRemoteConnector(
  { stdin: process.stdin, stdout: process.stdout, exit: (code) => process.exit(code) },
  open,
);
