// Rust Host 凭据存储与 TS 实现的交叉读写（docs/specs/rust-host-account.md §2.1）。
// 由 apps/zcode-host/crates/services/tests/account_interop.rs 调用；环境（HOME、ZCODE_DATA_BASE_DIR、
// ZCODE_CREDENTIAL_SECRET）由调用方指向临时目录，绝不读写开发者真实的 ~/.zcode。
// 用法：node --import tsx scripts/rust-host/credential-interop.ts <read|write|vector> <json>
import { createCipheriv, createHash } from "node:crypto";
import { createCredentialService } from "../../packages/services/src/credential/credentialService.js";

const [mode, payload = "null"] = process.argv.slice(2);
const input = JSON.parse(payload) as unknown;
const service = createCredentialService();

async function main(): Promise<unknown> {
  switch (mode) {
    case "read": {
      // 解密失败时记录错误码，Rust 侧据此核对两边对同一密文的判定。
      const result: Record<string, unknown> = {};
      for (const key of input as string[]) {
        try {
          result[key] = await service.load(key);
        } catch (error) {
          result[key] = { error: (error as { code?: string }).code ?? String(error) };
        }
      }
      return result;
    }
    case "write": {
      for (const [key, value] of Object.entries(input as Record<string, string | null>)) {
        if (value === null) await service.delete(key);
        else await service.save(key, value);
      }
      return null;
    }
    case "vector": {
      // 固定 IV 的密文：与 credentialCipherProvider.ts 相同的算法与编码，供 Rust 逐字节比对。
      const { secret, ivHex, plaintext } = input as {
        secret: string;
        ivHex: string;
        plaintext: string;
      };
      const key = createHash("sha256").update(secret).digest();
      const iv = Buffer.from(ivHex, "hex");
      const cipher = createCipheriv("aes-256-gcm", key, iv);
      const encrypted = Buffer.concat([cipher.update(plaintext, "utf-8"), cipher.final()]);
      return `enc:v1:${iv.toString("base64url")}.${cipher.getAuthTag().toString("base64url")}.${encrypted.toString("base64url")}`;
    }
    default:
      throw new Error(`unknown mode: ${mode}`);
  }
}

process.stdout.write(`${JSON.stringify(await main())}\n`);
