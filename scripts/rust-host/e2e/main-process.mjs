// 经 Node inspector 在 Electron Main 进程里求值（只用于 E2E）。
//
// 有些场景需要 Main 侧的动作，而界面上没有入口或 CDP 做不到：真正销毁一个主窗口（macOS 上点关闭
// 只是隐藏）、在没有窗口时重新打开主窗口（等同点 Dock 图标）。harness 以 `--inspect=<私有端口>`
// 启动 Electron，这里连上 Main 的 inspector 执行一段表达式；不给产品增加任何开关。
import { setTimeout as sleep } from "node:timers/promises";

/** 连上 Main 的 inspector；返回 evaluate(expression)，表达式里可用 `require("electron")`。 */
export async function connectMainInspector(port) {
  let target;
  for (let attempt = 0; attempt < 120 && !target; attempt += 1) {
    try {
      const response = await fetch(`http://127.0.0.1:${port}/json/list`);
      if (response.ok) [target] = await response.json();
    } catch {
      // inspector 还没监听
    }
    if (!target) await sleep(250);
  }
  if (!target?.webSocketDebuggerUrl) throw new Error(`Main inspector on ${port} not available`);
  const socket = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    socket.addEventListener("open", resolve, { once: true });
    socket.addEventListener("error", () => reject(new Error("Main inspector connect failed")), {
      once: true,
    });
  });
  let nextId = 0;
  const pending = new Map();
  socket.addEventListener("message", (event) => {
    const message = JSON.parse(String(event.data));
    const waiter = pending.get(message.id);
    if (!waiter) return;
    pending.delete(message.id);
    waiter(message);
  });
  socket.addEventListener("close", () => {
    for (const waiter of pending.values()) waiter({ error: { message: "inspector closed" } });
    pending.clear();
  });
  const call = (method, params) =>
    new Promise((resolve, reject) => {
      const id = ++nextId;
      pending.set(id, (message) =>
        message.error ? reject(new Error(message.error.message)) : resolve(message.result),
      );
      socket.send(JSON.stringify({ id, method, params }));
    });
  return {
    /** 在 Main 里求值（可以是 async 表达式），返回 JSON 可序列化的结果。 */
    async evaluate(expression) {
      const result = await call("Runtime.evaluate", {
        expression,
        includeCommandLineAPI: true,
        awaitPromise: true,
        returnByValue: true,
      });
      if (result.exceptionDetails) {
        throw new Error(
          `Main evaluate failed: ${result.exceptionDetails.exception?.description ?? result.exceptionDetails.text}`,
        );
      }
      return result.result.value;
    },
    /** 原始的 inspector 协议调用（性能测量用 Profiler 域）。 */
    call,
    close() {
      socket.close();
    },
  };
}

/** 主窗口（加载 index.html 的 BrowserWindow）的 id 与 webContents.id，按创建顺序。 */
export const LIST_MAIN_WINDOWS = `require("electron").BrowserWindow.getAllWindows()
  .filter((win) => !win.isDestroyed() && win.webContents.getURL().includes("/renderer/index.html"))
  .sort((a, b) => a.id - b.id)
  .map((win) => ({ id: win.id, webContentsId: win.webContents.id, visible: win.isVisible() }))`;

/**
 * 给页面打一个标记，Main 侧据此找到它的 BrowserWindow（页面不知道自己的 webContents.id）。
 * 标记写在页面的主世界里，Main 用 webContents.executeJavaScript 读。
 */
export async function markPage(page, marker) {
  await page.evaluate((value) => {
    window.__zcodeE2eMarker = value;
  }, marker);
}

/** 带标记的页面所在主窗口的 webContents.id；没有时返回 null。 */
export function windowIdOfMarkerExpression(marker) {
  return `(async () => {
    for (const win of require("electron").BrowserWindow.getAllWindows()) {
      if (win.isDestroyed()) continue;
      const value = await win.webContents
        .executeJavaScript("window.__zcodeE2eMarker ?? null")
        .catch(() => null);
      if (value === ${JSON.stringify(marker)}) return win.webContents.id;
    }
    return null;
  })()`;
}

/** 真正销毁带标记页面的主窗口（destroy 不走 close 事件，macOS 的“关闭即隐藏”不生效）。 */
export function destroyMarkedWindowExpression(marker) {
  return `(async () => {
    for (const win of require("electron").BrowserWindow.getAllWindows()) {
      if (win.isDestroyed()) continue;
      const value = await win.webContents
        .executeJavaScript("window.__zcodeE2eMarker ?? null")
        .catch(() => null);
      if (value !== ${JSON.stringify(marker)}) continue;
      win.destroy();
      return true;
    }
    return false;
  })()`;
}

/** 销毁所有主窗口（Dock 仍在，App 不退出；macOS 的 window-all-closed 不退出）。返回销毁的个数。 */
export const DESTROY_ALL_MAIN_WINDOWS = `(() => {
  const wins = require("electron").BrowserWindow.getAllWindows()
    .filter((win) => !win.isDestroyed() && win.webContents.getURL().includes("/renderer/index.html"));
  for (const win of wins) win.destroy();
  return wins.length;
})()`;

/** 没有窗口时重新打开主窗口：与点 Dock 图标相同（app 的 activate 事件）。 */
export const REOPEN_MAIN_WINDOW = `(() => {
  require("electron").app.emit("activate");
  return true;
})()`;
