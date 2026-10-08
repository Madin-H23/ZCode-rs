// 测试用远端连接器（docs/specs/rust-host-remote.md §7）：协议与产品连接器相同（runRemoteConnector），
// 只把“探测 → 部署 → 启动”换成直接在本机拉起本仓库构建的 zcode-server.cjs，backend 的 exec / upload
// 在本机执行。用于在没有 SSH / Docker 的环境里验证 Rust ConnectorTransport 的完整链路。
//   node --import tsx scripts/rust-host/remote-connector-local.ts <node> <zcode-server.cjs>
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

const [nodeBinary, serverBundle] = process.argv.slice(2);
if (!nodeBinary || !serverBundle) {
  throw new Error("usage: remote-connector-local.ts <node> <zcode-server.cjs>");
}

function execLocal(command: string): StdioStream {
  const child = spawn("/bin/sh", ["-c", command], { env: process.env, stdio: "pipe" });
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
