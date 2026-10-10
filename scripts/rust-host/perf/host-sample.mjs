// 流式 CPU 剖析（stream-cpu.mjs）的 Host 原生调用栈：macOS `sample <pid> <秒>`，把调用图里非空闲的样本
// 按 crate / 库（self）、按函数（self）与按函数（inclusive，含调用者）归类。release 构建默认剥了符号
// （`strip = true`），要看到函数名需要同一源码、不剥符号的构建（stream-cpu.mjs 的 --host-binary）。
// 只用于测量。
import { execFile } from "node:child_process";
import { readFile } from "node:fs/promises";
import { promisify } from "node:util";

const exec = promisify(execFile);

/** 栈顶是这些帧的样本算空闲（线程在等事件、锁或 IO）。 */
const IDLE =
  /mach_msg|__psynch_cvwait|__psynch_mutexwait|kevent|__workq_kernreturn|__semwait_signal|nanosleep|__select|__ulock_wait|\bpoll\b|__wait4|__read_nocancel|\bread\b|__recvfrom|__accept|semaphore_wait|swtch_pri|_pthread_cond_wait/;

const demangle = (symbol) =>
  symbol
    .replace(/::h[0-9a-f]{16}$/, "")
    .replace(/_\$LT\$/g, "<")
    .replace(/\$GT\$/g, ">")
    .replace(/\$u20\$/g, " ")
    .replace(/\$u7b\$/g, "{")
    .replace(/\$u7d\$/g, "}")
    .replace(/\$C\$/g, ",")
    .replace(/\.\./g, "::");

/** `sample` 输出的调用图 → { busySamples, byCrate, selfTop, inclusiveTop }（值为占非空闲样本的 %）。 */
export function summarizeSample(text) {
  const nodes = [];
  const stack = [];
  let inGraph = false;
  for (const line of text.split("\n")) {
    if (line.startsWith("Call graph:")) {
      inGraph = true;
      continue;
    }
    if (!inGraph) continue;
    if (/^(Total number in stack|Sort by top of stack|Binary Images)/.test(line.trim())) break;
    if (/^\s{4}\d+ Thread_\d+/.test(line)) {
      stack.length = 0;
      continue;
    }
    const match =
      line.match(/^([\s+!:|]*)(\d+)\s+(.*?)\s+\(in ([^)]+)\)/) ??
      line.match(/^([\s+!:|]*)(\d+)\s+(\S.*)$/);
    if (!match) continue;
    const depth = match[1].length;
    while (stack.length && stack.at(-1).depth >= depth) stack.pop();
    const node = {
      depth,
      count: Number(match[2]),
      symbol: demangle(match[3].replace(/\s+\+ \d+.*$/, "")),
      image: match[4] ?? "",
      callers: stack.map((entry) => entry.symbol),
      children: 0,
    };
    if (stack.length) stack.at(-1).children += node.count;
    stack.push(node);
    nodes.push(node);
  }
  let busy = 0;
  const self = new Map();
  const crates = new Map();
  const inclusive = new Map();
  const add = (map, key, value) => map.set(key, (map.get(key) ?? 0) + value);
  for (const node of nodes) {
    const own = node.count - node.children;
    if (own <= 0 || IDLE.test(node.symbol)) continue;
    busy += own;
    add(self, node.symbol.slice(0, 140), own);
    add(crates, /^<?(\w+)::/.exec(node.symbol)?.[1] ?? node.image, own);
    const seen = new Set();
    for (const symbol of [...node.callers, node.symbol]) {
      const key = symbol.slice(0, 160);
      if (seen.has(key)) continue;
      seen.add(key);
      add(inclusive, key, own);
    }
  }
  const top = (map, count, filter = () => true) =>
    [...map.entries()]
      .filter(([key]) => filter(key))
      .sort((a, b) => b[1] - a[1])
      .slice(0, count)
      .map(([key, value]) => [key, Math.round((1000 * value) / Math.max(busy, 1)) / 10]);
  return {
    busySamples: busy,
    byCrate: top(crates, 20),
    selfTop: top(self, 30),
    // 只列 Host / runtime 自己的函数与常见的序列化、WebSocket、HTTP 库，免得被 tokio 与 std 的帧淹没。
    inclusiveTop: top(inclusive, 50, (key) =>
      /zcode|serde|tungstenite|tracing|rusqlite|hyper|sha2/.test(key),
    ),
  };
}

/** 采 `seconds` 秒（1 ms 间隔），原始输出写到 `file`。 */
export async function sampleProcess(pid, seconds, file) {
  try {
    await exec("sample", [String(pid), String(seconds), "1", "-mayDie", "-file", file], {
      maxBuffer: 64 * 1024 * 1024,
    });
    return { file, ...summarizeSample(await readFile(file, "utf8")) };
  } catch (error) {
    return { file, error: String(error.message ?? error).split("\n")[0] };
  }
}
