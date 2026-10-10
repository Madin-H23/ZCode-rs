// 流式 CPU 剖析（stream-cpu.mjs）的 Chromium trace 汇总：经 Main inspector 调 Electron 的 contentTracing
// 录几秒 trace（toplevel + mojom 类别：每个线程上每个任务的墙钟与线程 CPU 时间 tdur、任务里收发的 mojo
// 消息名），再按进程 / 线程 / 任务种类汇总成“每秒多少 ms CPU”。覆盖 Electron 的全部 Chromium 进程
// （Main 即 browser 进程、Renderer、GPU、network service 与其他 utility），Rust Host 与 CLI 不在其中。
// 只用于测量。
import { readFile } from "node:fs/promises";

export const TRACE_CATEGORIES = ["toplevel", "mojom"];

/** 经 Main inspector 录 `ms` 毫秒的 trace，写到 `file`。 */
export async function recordTrace(main, file, ms) {
  await main.evaluate(`(async () => {
    await require("electron").contentTracing.startRecording({
      included_categories: ${JSON.stringify(TRACE_CATEGORIES)},
      excluded_categories: ["*"],
      trace_buffer_size_in_kb: 400000,
    });
    return true;
  })()`);
  const from = Date.now();
  await new Promise((done) => setTimeout(done, ms));
  const to = Date.now();
  await main.evaluate(
    `(async () => require("electron").contentTracing.stopRecording(${JSON.stringify(file)}))()`,
  );
  return { file, from, to };
}

/** 任务的种类：任务里第一条 mojo 消息的名字，否则 PostTask 的来源函数，否则事件名。 */
function taskKind(task, nested) {
  const mojo = nested.find((event) => event.cat?.includes("mojom") && event.name);
  if (mojo) {
    const name = mojo.args?.chrome_mojo_event_info?.mojo_interface_tag
      ? `${mojo.args.chrome_mojo_event_info.mojo_interface_tag}#${mojo.args.chrome_mojo_event_info.ipc_hash ?? ""}`
      : mojo.name;
    return `mojo ${name}`.slice(0, 140);
  }
  const src = task.args?.src_func ?? task.args?.src?.function_name;
  const file = task.args?.src_file ?? task.args?.src?.file_name;
  if (src || file)
    return `task ${file ? `${String(file).split("/").slice(-2).join("/")}:` : ""}${src ?? ""}`.slice(
      0,
      140,
    );
  return `event ${task.name}`.slice(0, 140);
}

/**
 * 汇总 trace：`roleOfPid`（pid → 角色，来自进程表）给进程归类。返回每个进程、每个线程的 CPU（ms/s，
 * 顶层任务 tdur 之和 ÷ 录制时长）与前几种任务。
 */
export async function summarizeTrace(file, seconds, roleOfPid = new Map()) {
  const parsed = JSON.parse(await readFile(file, "utf8"));
  const events = Array.isArray(parsed) ? parsed : parsed.traceEvents;
  const processNames = new Map();
  const threadNames = new Map();
  const byThread = new Map();
  for (const event of events) {
    if (event.ph === "M") {
      if (event.name === "process_name") processNames.set(event.pid, event.args?.name);
      if (event.name === "thread_name")
        threadNames.set(`${event.pid}:${event.tid}`, event.args?.name);
      continue;
    }
    if (event.ph !== "X") continue;
    const key = `${event.pid}:${event.tid}`;
    if (!byThread.has(key)) byThread.set(key, []);
    byThread.get(key).push(event);
  }
  const processes = new Map();
  for (const [key, list] of byThread) {
    const [pid] = key.split(":").map(Number);
    list.sort((a, b) => a.ts - b.ts || b.dur - a.dur);
    let cpuUs = 0;
    let wallUs = 0;
    const kinds = new Map();
    let index = 0;
    while (index < list.length) {
      const task = list[index];
      const end = task.ts + (task.dur ?? 0);
      const nested = [];
      let next = index + 1;
      while (next < list.length && list[next].ts < end) {
        nested.push(list[next]);
        next += 1;
      }
      // 最外层的事件算一个任务（通常是 toplevel 的 RunTask）；包含在它里面的事件只用来判断任务种类。
      const cpu = task.tdur ?? 0;
      cpuUs += cpu;
      wallUs += task.dur ?? 0;
      const kind = taskKind(task, nested);
      const entry = kinds.get(kind) ?? { cpuUs: 0, count: 0 };
      entry.cpuUs += cpu;
      entry.count += 1;
      kinds.set(kind, entry);
      index = next;
    }
    const role = roleOfPid.get(pid) ?? processNames.get(pid) ?? "?";
    if (!processes.has(pid))
      processes.set(pid, {
        pid,
        role,
        name: processNames.get(pid) ?? "?",
        cpuMsPerSec: 0,
        threads: [],
      });
    const process = processes.get(pid);
    const thread = {
      name: threadNames.get(key) ?? key,
      cpuMsPerSec: Math.round((cpuUs / 1000 / seconds) * 10) / 10,
      wallMsPerSec: Math.round((wallUs / 1000 / seconds) * 10) / 10,
      kinds: [...kinds.entries()]
        .sort((a, b) => b[1].cpuUs - a[1].cpuUs)
        .slice(0, 10)
        .map(([kind, value]) => ({
          kind,
          cpuMsPerSec: Math.round((value.cpuUs / 1000 / seconds) * 100) / 100,
          perSec: Math.round((value.count / seconds) * 10) / 10,
        })),
    };
    process.cpuMsPerSec += thread.cpuMsPerSec;
    process.threads.push(thread);
  }
  return [...processes.values()]
    .map((process) => ({
      ...process,
      cpuMsPerSec: Math.round(process.cpuMsPerSec * 10) / 10,
      threads: process.threads
        .filter((thread) => thread.cpuMsPerSec >= 0.2)
        .sort((a, b) => b.cpuMsPerSec - a.cpuMsPerSec),
    }))
    .filter((process) => process.cpuMsPerSec >= 0.5)
    .sort((a, b) => b.cpuMsPerSec - a.cpuMsPerSec);
}
