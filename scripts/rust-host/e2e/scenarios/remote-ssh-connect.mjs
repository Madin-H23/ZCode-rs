// 远端 workspace（docs/specs/rust-host-remote.md §7）：真实 App 走产品路径连接一个 SSH 目标。
//
// 没有真实 SSH 主机：目标是 127.0.0.1 上一个已关闭的端口。链路仍然完整——
//   Renderer 远程向导 → Main createRemoteWorkspaceSession → Rust Host connect-remote-workspace
//   → 连接注册表 → 远端连接器（Electron 以 Node 模式运行 out/host/remoteConnector.js）→ ssh2
//   → 失败 → 连接日志与 remote-workspace-connect-failed → 向导显示错误、回到设置步骤。
// 成功路径需要部署资产（CDN 或 mock-cdn）与可登录的主机，在本机无法离线验证（见规格 §7）。
import { join } from "node:path";
import { createServer } from "node:net";
import { once } from "node:events";

async function closedPort() {
  const server = createServer();
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const { port } = server.address();
  server.close();
  await once(server, "close");
  return port;
}

export async function run({ page, shots }) {
  const port = await closedPort();
  const shot = (name) => page.screenshot({ path: join(shots, name) });

  // 入口：新对话空态里 composer 的 workspace 菜单 →“远程连接”。
  const workspaceMenu = page.getByTestId("composer-workspace-trigger").first();
  await workspaceMenu.waitFor({ state: "visible", timeout: 60_000 });
  await shot("remote-00-ready.png");
  await workspaceMenu.click({ trial: true });
  await workspaceMenu.click();
  const remoteEntry = page.getByTestId("composer-remote-connection");
  await remoteEntry.waitFor({ state: "visible", timeout: 10_000 });
  await remoteEntry.click();
  const dialog = page.getByTestId("ssh-dialog");
  await dialog.waitFor({ state: "visible", timeout: 10_000 });
  await shot("remote-01-dialog.png");

  // 第一步选 SSH，第二步填写目标（密码只存在于这次连接流程）。
  const kind = page.getByTestId("remote-kind-ssh");
  await kind.click({ trial: true });
  await kind.click();
  const next = dialog.getByRole("button", { name: /下一步|Next|Continue|继续/ }).first();
  if (await next.isVisible().catch(() => false)) await next.click();
  const host = page.getByTestId("ssh-host-input");
  await host.waitFor({ state: "visible", timeout: 10_000 });
  await host.fill("127.0.0.1");
  await page.getByTestId("ssh-port-input").fill(String(port));
  await page.getByTestId("ssh-username-input").fill("e2e");
  const passwordAuth = page.getByTestId("ssh-auth-password");
  if (await passwordAuth.isVisible().catch(() => false)) await passwordAuth.click();
  const password = page.getByTestId("ssh-password-input");
  if (await password.isVisible().catch(() => false)) await password.fill("not-a-real-password");
  await shot("remote-02-settings.png");

  const connect = dialog.getByRole("button", { name: /Start connection|开始连接/ });
  await connect.click({ trial: true });
  await connect.click();

  // 连接器把 ssh2 的拒绝原样带回：向导显示错误文本（ECONNREFUSED 与端口）。
  try {
    await page
      .getByText(new RegExp(`ECONNREFUSED|${port}`))
      .first()
      .waitFor({ state: "visible", timeout: 60_000 });
  } finally {
    await shot("remote-03-result.png");
  }
  // 连接器的过程日志经 Host → Main 进入向导的连接日志；失败后可以直接重试。
  await dialog.getByText("detecting remote env...").first().waitFor({ state: "visible" });
  const retry = dialog.getByRole("button", { name: /Retry connection|重试连接|重新连接/ });
  await retry.waitFor({ state: "visible", timeout: 15_000 });
  await retry.click({ trial: true });
  if (await dialog.getByText("ExperimentalWarning").count()) {
    throw new Error("Node warnings must not be reported as connection errors");
  }
  console.info(
    `[e2e:remote] SSH connect to 127.0.0.1:${port} failed as expected through the Rust Host`,
  );
}
