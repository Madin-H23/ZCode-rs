// Rust Host 的远端连接器入口（docs/specs/rust-host-remote.md §2.1）：tsup 打包为 out/host/remoteConnector.js，
// 由 Rust Host 以 ELECTRON_RUN_AS_NODE 运行；stdin / stdout 是与 Host 之间的帧协议。
import { openDeployedRemoteServer, runRemoteConnector } from "./remoteConnector.js";

runRemoteConnector(
  { stdin: process.stdin, stdout: process.stdout, exit: (code) => process.exit(code) },
  openDeployedRemoteServer,
);
