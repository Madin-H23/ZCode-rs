// 跨进程写 setting.json 的子进程：等到统一起跑时间后，对自己独占的字段交替写入，
// 最后一轮固定写 true。父测试据此检查其他进程的整文件写回有没有覆盖掉这个字段。
import { setTimeout as sleep } from "node:timers/promises";
import type { AppSettings } from "@zcode/shared";
import { createSettingService } from "../../src/setting/settingService.js";

const [field, roundsArg, startAtArg] = process.argv.slice(2);
const rounds = Number(roundsArg);
const startAt = Number(startAtArg);
if (!field || !Number.isInteger(rounds) || rounds < 1 || !Number.isFinite(startAt)) {
  throw new Error("usage: settingUpdateWorker <field> <rounds> <startAtMs>");
}

await sleep(Math.max(0, startAt - Date.now()));
const service = createSettingService();
for (let round = 0; round < rounds; round += 1) {
  const value = round === rounds - 1 || round % 2 === 1;
  await service.update({ [field]: value } as Partial<AppSettings>);
}
