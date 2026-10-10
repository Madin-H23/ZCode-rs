// 流式 CPU 剖析（stream-cpu.mjs）的 Renderer 侧消息计数：在页面脚本运行之前（Playwright addInitScript，
// 随后 reload）给 EventTarget.addEventListener 打补丁，凡是 MessagePort（配置 A：Node Host 经 MessagePort）
// 或 WebSocket（配置 B：Rust Host 经回环 WebSocket）上注册 "message" 监听的对象，额外挂一个计数监听；
// 同时包住 MessagePort.postMessage / WebSocket.send 计出站。两种传输用同一个计数器，计的是 RPC 层收到的
// 原始消息（A 是一条 RPC 消息一个 Uint8Array；B 是一个 WebSocket 消息，内含 13 字节帧头的 SocketProtocol 帧）。
//
// 计数按 250 ms 分桶（Date.now()，与 harness 同一时钟），断开 CDP 之后也照常累计，事后按时间段求和。
// 抓包（capture）只在专门的短窗口里打开：复制每条消息的字节，事后在 Node 侧解码分类（decodeCapture）。
// 另外经 React DevTools 全局 hook 计 React 的提交次数（"react.commit"），看渲染是否跟着消息走。
// 只用于测量，不改产品代码。

export const TAP_BUCKET_MS = 250;

/** 页面主世界里运行的补丁（作为 addInitScript 的函数体）。 */
export function rpcTapInit() {
  if (window.__rpcTap) return;
  const BUCKET_MS = 250;
  const tap = { buckets: {}, capture: null, outLog: [], transports: [] };
  window.__rpcTap = tap;
  const tapped = new WeakMap();
  const copy = (data) =>
    data instanceof ArrayBuffer
      ? new Uint8Array(data.slice(0))
      : new Uint8Array(data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength));
  const record = (transport, dir, data) => {
    const binary = data instanceof ArrayBuffer || ArrayBuffer.isView(data);
    const len = binary ? data.byteLength : 0;
    const key = Math.floor(Date.now() / BUCKET_MS);
    const bucket = (tap.buckets[key] ??= {});
    const name = `${transport}.${dir}${binary ? "" : ".ctl"}`;
    const entry = (bucket[name] ??= [0, 0]);
    entry[0] += 1;
    entry[1] += len;
    if (!binary) return;
    if (dir === "out" && tap.outLog.length < 20000)
      tap.outLog.push({ t: Date.now(), transport, bytes: copy(data) });
    if (tap.capture) tap.capture.push({ t: Date.now(), transport, dir, bytes: copy(data) });
  };
  // React 提交次数：生产构建的 React 也会调用 DevTools 全局 hook 的 onCommitFiberRoot（React DevTools 就靠
  // 它工作）。页面脚本之前装一个只计数的 hook，按同样的 250 ms 分桶记 "react.commit"。
  if (!window.__REACT_DEVTOOLS_GLOBAL_HOOK__) {
    let nextId = 1;
    window.__REACT_DEVTOOLS_GLOBAL_HOOK__ = {
      supportsFiber: true,
      isDisabled: false,
      renderers: new Map(),
      inject(renderer) {
        const id = nextId++;
        this.renderers.set(id, renderer);
        return id;
      },
      checkDCE() {},
      setStrictMode() {},
      onScheduleFiberRoot() {},
      onCommitFiberUnmount() {},
      onPostCommitFiberRoot() {},
      onCommitFiberRoot() {
        const key = Math.floor(Date.now() / BUCKET_MS);
        const bucket = (tap.buckets[key] ??= {});
        const entry = (bucket["react.commit"] ??= [0, 0]);
        entry[0] += 1;
      },
    };
  }
  const listen = EventTarget.prototype.addEventListener;
  EventTarget.prototype.addEventListener = function (type, listener, options) {
    if (type === "message" && !tapped.has(this)) {
      const transport =
        typeof WebSocket !== "undefined" && this instanceof WebSocket
          ? "ws"
          : typeof MessagePort !== "undefined" && this instanceof MessagePort
            ? "port"
            : null;
      if (transport) {
        tapped.set(this, transport);
        tap.transports.push({ transport, at: Date.now() });
        listen.call(this, "message", (event) => record(transport, "in", event.data));
      }
    }
    return listen.call(this, type, listener, options);
  };
  const post = MessagePort.prototype.postMessage;
  MessagePort.prototype.postMessage = function (message, ...rest) {
    const transport = tapped.get(this);
    if (transport) record(transport, "out", message);
    return post.call(this, message, ...rest);
  };
  const send = WebSocket.prototype.send;
  WebSocket.prototype.send = function (data) {
    const transport = tapped.get(this);
    if (transport) record(transport, "out", data);
    return send.call(this, data);
  };
}

/** [from, to]（epoch ms）内的计数：{ "<transport>.<dir>": { messages, bytes } }。 */
export async function readTap(page, from, to) {
  const buckets = await page.evaluate(() => window.__rpcTap?.buckets ?? {});
  const totals = {};
  for (const [key, bucket] of Object.entries(buckets)) {
    const start = Number(key) * TAP_BUCKET_MS;
    if (start < from || start + TAP_BUCKET_MS > to + TAP_BUCKET_MS / 2) continue;
    for (const [name, [messages, bytes]] of Object.entries(bucket)) {
      totals[name] ??= { messages: 0, bytes: 0 };
      totals[name].messages += messages;
      totals[name].bytes += bytes;
    }
  }
  return totals;
}

export async function startCapture(page) {
  await page.evaluate(() => {
    window.__rpcTap.capture = [];
  });
}

const PAGE_TAKE = `(() => {
  const toB64 = (u8) => {
    let text = "";
    for (let i = 0; i < u8.length; i += 0x8000) text += String.fromCharCode(...u8.subarray(i, i + 0x8000));
    return btoa(text);
  };
  const tap = window.__rpcTap;
  const capture = (tap.capture ?? []).map((e) => ({ t: e.t, transport: e.transport, dir: e.dir, b64: toB64(e.bytes) }));
  tap.capture = null;
  const outLog = tap.outLog.map((e) => ({ t: e.t, transport: e.transport, b64: toB64(e.bytes) }));
  return { capture, outLog, transports: tap.transports };
})()`;

/** 结束抓包并取回：{ capture: [{ t, transport, dir, b64 }], outLog, transports }。 */
export async function takeCapture(page) {
  return page.evaluate(PAGE_TAKE);
}

// ---------------------------------------------------------------------------------------------
// Node 侧解码（@zcode/rpc 的 serialization.ts 与 protocol.ts 的格式）

function reader(bytes) {
  let pos = 0;
  const read = (n) => {
    const out = bytes.subarray(pos, pos + n);
    pos += n;
    return out;
  };
  const vql = () => {
    let value = 0;
    for (let n = 0; ; n += 7) {
      const next = read(1)[0];
      value |= (next & 0x7f) << n;
      if (!(next & 0x80)) return value;
    }
  };
  const value = () => {
    const type = read(1)[0];
    switch (type) {
      case 0:
        return undefined;
      case 1:
        return Buffer.from(read(vql())).toString("utf8");
      case 2:
      case 3:
        return { binary: read(vql()).length };
      case 4: {
        const length = vql();
        return Array.from({ length }, () => value());
      }
      case 5: {
        const length = vql();
        const text = Buffer.from(read(length)).toString("utf8");
        try {
          return { __json: JSON.parse(text), __jsonBytes: length };
        } catch {
          return { __badJson: length };
        }
      }
      case 6:
        return vql();
      default:
        throw new Error(`unknown data type ${type}`);
    }
  };
  return { value, done: () => pos >= bytes.length };
}

/** 一条 RPC 消息（header + body）。 */
function decodeRpc(bytes) {
  const r = reader(bytes);
  const header = r.value();
  const body = r.done() ? undefined : r.value();
  return { header, body, bytes: bytes.length };
}

/** WebSocket 消息里的 SocketProtocol 帧（13 字节帧头）。返回 { regular: [Uint8Array], other }。 */
function splitSocketFrames(bytes, state) {
  state.pending = state.pending ? Buffer.concat([state.pending, bytes]) : Buffer.from(bytes);
  const regular = [];
  let other = 0;
  while (state.pending.length >= 13) {
    const type = state.pending[0];
    const length = state.pending.readUInt32BE(9);
    if (state.pending.length < 13 + length) break;
    const body = state.pending.subarray(13, 13 + length);
    state.pending = state.pending.subarray(13 + length);
    if (type === 1) regular.push(new Uint8Array(body));
    else other += 1;
  }
  return { regular, other };
}

const unwrap = (value) =>
  value && typeof value === "object" && "__json" in value ? value.__json : value;

/** 事件负载的类别：V4 topic 帧（complete / fragment、topic 前缀、内层帧类型）或其他形状。 */
function classifyEvent(data) {
  const value = unwrap(data);
  if (value && typeof value === "object" && !Array.isArray(value)) {
    if (typeof value.wireVersion !== "undefined" && typeof value.kind === "string") {
      const topic = String(value.topic ?? "").split(/[:/]/)[0] || "?";
      const inner = value.frame && typeof value.frame === "object" ? value.frame : null;
      const innerKind =
        inner?.type ?? inner?.kind ?? (inner ? Object.keys(inner).slice(0, 3).join(",") : "-");
      return `v4 ${value.kind} topic=${topic} frame=${innerKind}`;
    }
    const keys = Object.keys(value).slice(0, 4).join(",");
    return `object {${keys}}${value.type ? ` type=${value.type}` : ""}`;
  }
  if (Array.isArray(value)) return `array[${value.length}]`;
  return typeof value;
}

/**
 * 解码一次抓包：入站按 RPC 响应类型与事件负载分类（每类的条数与字节数），出站按 channel.method 分类；
 * `outLog`（从 reload 起的全部出站消息）用来把事件订阅 id 映射到 channel.event。
 */
export function decodeCapture({ capture, outLog }, seconds) {
  const listenNames = new Map();
  const requestNames = new Map();
  const wsOut = {};
  const mapRequests = (entries) => {
    for (const entry of entries) {
      const raw = Buffer.from(entry.b64, "base64");
      const messages =
        entry.transport === "ws" ? splitSocketFrames(raw, wsOut).regular : [new Uint8Array(raw)];
      for (const message of messages) {
        try {
          const { header } = decodeRpc(message);
          if (!Array.isArray(header)) continue;
          const [type, id, channel, name] = header;
          if (type === 102) listenNames.set(id, `${channel}.${name}`);
          if (type === 100) requestNames.set(id, `${channel}.${name}`);
        } catch {
          // 不完整的消息
        }
      }
    }
  };
  mapRequests(outLog);
  const inState = {};
  const classes = {};
  const add = (key, bytes) => {
    classes[key] ??= { count: 0, bytes: 0 };
    classes[key].count += 1;
    classes[key].bytes += bytes;
  };
  let transportMessages = 0;
  let transportBytes = 0;
  let rpcMessages = 0;
  let controlFrames = 0;
  for (const entry of capture) {
    const raw = Buffer.from(entry.b64, "base64");
    if (entry.dir === "out") {
      add(`out ${entry.transport}`, raw.length);
      continue;
    }
    transportMessages += 1;
    transportBytes += raw.length;
    let messages = [new Uint8Array(raw)];
    if (entry.transport === "ws") {
      const split = splitSocketFrames(raw, inState);
      messages = split.regular;
      controlFrames += split.other;
    }
    for (const message of messages) {
      rpcMessages += 1;
      let decoded;
      try {
        decoded = decodeRpc(message);
      } catch {
        add("undecodable", message.length);
        continue;
      }
      const [type, id] = Array.isArray(decoded.header) ? decoded.header : [decoded.header];
      if (type === 204) {
        const name = listenNames.get(id) ?? `event#${id}`;
        add(`event ${name} :: ${classifyEvent(decoded.body)}`, message.length);
      } else if (type === 201 || type === 202 || type === 203) {
        add(`reply ${requestNames.get(id) ?? `#${id}`}`, message.length);
      } else {
        add(`response type ${type}`, message.length);
      }
    }
  }
  const perSecond = (value) => Math.round((value / seconds) * 10) / 10;
  return {
    seconds,
    transportMessagesPerSec: perSecond(transportMessages),
    transportBytesPerSec: perSecond(transportBytes),
    rpcMessagesPerSec: perSecond(rpcMessages),
    controlFramesPerSec: perSecond(controlFrames),
    classes: Object.fromEntries(
      Object.entries(classes)
        .sort((a, b) => b[1].bytes - a[1].bytes)
        .map(([key, value]) => [
          key,
          {
            perSec: perSecond(value.count),
            bytesPerSec: perSecond(value.bytes),
            avgBytes: Math.round(value.bytes / value.count),
          },
        ]),
    ),
  };
}
