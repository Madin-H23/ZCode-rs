// Rust Host 写出的 provider_config.json 是否是 TS 的规范形态（docs/specs/rust-host-account.md §2.7）。
// 用真实的 decodeProviderConfigFile / encodeProviderConfigFile：能解码，且重新编码后逐字节相同。
// 用法：node --import tsx scripts/rust-host/provider-config-interop.ts <file>
import { readFile } from "node:fs/promises";
import {
  decodeProviderConfigFile,
  encodeProviderConfigFile,
} from "../../packages/provider-node/src/provider-config-file-codec.js";

const [file] = process.argv.slice(2);
const text = await readFile(file!, "utf8");
let result: Record<string, unknown>;
try {
  const value = JSON.parse(text) as unknown;
  const encoded = encodeProviderConfigFile(decodeProviderConfigFile(value));
  result = {
    decoded: true,
    canonical: JSON.stringify(encoded, null, 2) === text,
    ...(JSON.stringify(encoded, null, 2) === text ? {} : { expected: JSON.stringify(encoded, null, 2) }),
  };
} catch (error) {
  result = { decoded: false, error: error instanceof Error ? error.message : String(error) };
}
process.stdout.write(`${JSON.stringify(result)}\n`);
