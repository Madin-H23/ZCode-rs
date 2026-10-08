// 生成 rpc 线格式夹具：用 packages/rpc 的真实 serialize 编码，Rust 侧逐字节比对。
// 重新生成：在仓库根目录执行
//   node --import tsx apps/zcode-host/crates/rpc/tests/fixtures/gen.ts \
//     > apps/zcode-host/crates/rpc/tests/fixtures/rpc-serialization.json
import { BufferWriter, serialize } from "../../../../../../packages/rpc/src/serialization.ts";

const UNDEFINED = { $undefined: true };
const values: [string, unknown][] = [
  ["int-zero", 0],
  ["int-small", 1],
  ["int-127", 127],
  ["int-128", 128],
  ["int-300", 300],
  ["int-negative", -1],
  ["int-max", 2147483647],
  ["int-min", -2147483648],
  ["int-beyond-int32", 2147483648],
  ["int-integral-float", 3.0],
  ["float", 1.5],
  ["string", "hello"],
  ["string-unicode", "中文🙂"],
  ["string-empty", ""],
  ["null", null],
  ["true", true],
  ["false", false],
  ["array-empty", []],
  ["array-nested", [1, "a", [2, null], { k: "v" }]],
  ["object", { a: 1, b: [1, 2], c: { d: "中" } }],
  ["undefined", UNDEFINED],
];

const hex = (write: (writer: BufferWriter) => void) => {
  const writer = new BufferWriter();
  write(writer);
  return Buffer.from(writer.buffer.buffer).toString("hex");
};
const message = (header: unknown, body: unknown) =>
  hex((writer) => {
    serialize(writer, header);
    serialize(writer, body);
  });

process.stdout.write(
  `${JSON.stringify(
    {
      values: values.map(([name, value]) => ({
        name,
        value,
        hex: hex((writer) => serialize(writer, value === UNDEFINED ? undefined : value)),
      })),
      bytes: {
        topLevel: hex((writer) => serialize(writer, new Uint8Array([0, 1, 2, 250, 255]))),
        nested: hex((writer) => serialize(writer, { data: new Uint8Array([104, 105, 33]) })),
      },
      messages: {
        initialize: message([200], undefined),
        successObject: message([201, 7], { ok: true, n: 3 }),
        successUndefined: message([201, 8], undefined),
        promiseError: message([202, 9], { message: "boom", name: "Error" }),
        promiseErrorWithCode: message([202, 10], {
          message: "not implemented: setting.get",
          name: "NotImplemented",
          code: "fault.host.notImplemented",
        }),
        eventFire: message([204, 300], { topic: "conversation/s1", subscriptionId: "x" }),
        requestPromise: message([100, 5, "setting", "get"], []),
        requestCall: message(
          [100, 6, "file", "readTextFile"],
          [{ path: "/tmp/a.txt", length: 10 }],
        ),
        requestListen: message([102, 7, "zcode-agent", "onDynamicConversationFrame"], {
          workspacePath: "/w",
        }),
        requestCancel: message([101, 6], undefined),
        requestDispose: message([103, 7], undefined),
      },
    },
    null,
    2,
  )}\n`,
);
