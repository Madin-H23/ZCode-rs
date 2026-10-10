import assert from "node:assert/strict";
import test from "node:test";
import type { ArmsCustomEventPayload, RendererTelemetryEventPayload } from "@zcode/shared";
import type { ConversationTelemetryFact } from "@zcode/shared/zcode-protocol-v4";
import { setUiPerfArmsReporter } from "../src/lib/uiPerfArmsTelemetry.js";
import { ConversationTelemetrySupervisor } from "../src/v4/telemetry/conversationTelemetrySupervisor.js";

// docs/specs/rust-m9-usage-logs.md §5.7：Rust runtime 合并的 stream.chunk 带 lastChunkAgeMs，
// Renderer 用“收到时刻 − lastChunkAgeMs”还原最新增量的到达时刻，停顿与正文尾包的口径不变。
test("merged stream chunks restore the arrival of their newest delta", async () => {
  const arms: ArmsCustomEventPayload[] = [];
  const reports: RendererTelemetryEventPayload[] = [];
  setUiPerfArmsReporter({
    reportArmsCustomEvent: async (payload) => {
      arms.push(payload);
    },
  });
  let clock = 0;
  const supervisor = new ConversationTelemetrySupervisor({
    platform: {
      reportArmsCustomEvent: async () => undefined,
      reportTelemetryEvent: async (payload) => {
        reports.push(payload);
      },
    },
    workspaceScopeKey: "/workspace-stream-chunks",
    now: () => clock,
  });
  try {
    supervisor.attachForeground({}, "s1");
    supervisor.acceptPromptSeed(
      { sessionId: "s1", sourceCommandId: "c1", sendTime: 0, extraDetail: {} },
      { reportSendButton: false },
    );
    let seq = 0;
    const deliver = (at: number, fact: Record<string, unknown>) => {
      seq += 1;
      clock = at;
      supervisor.handleFact({
        version: 1,
        eventId: `e${seq}`,
        eventSeq: seq,
        occurredAt: at,
        sessionId: "s1",
        sourceCommandId: "c1",
        turnId: "t1",
        ...fact,
      } as ConversationTelemetryFact);
    };
    const chunk = { kind: "stream.chunk", channel: "text", assistantMessageId: "m1" };
    deliver(10, { kind: "turn.started" });
    deliver(100, { ...chunk, chunkLength: 3, firstChunk: true });
    // 最新增量在 150 到达，模型随后停住；合并的那条在窗口结束（350）时发出。
    deliver(350, {
      ...chunk,
      chunkLength: 6,
      firstChunk: false,
      chunkCount: 2,
      lastChunkAgeMs: 200,
    });
    // 停顿后的增量立即单独发出：真实间隔 3200 − 150 = 3050，超过 3 s 阈值。
    deliver(3200, { ...chunk, chunkLength: 3, firstChunk: false });
    deliver(3450, {
      ...chunk,
      chunkLength: 6,
      firstChunk: false,
      chunkCount: 2,
      lastChunkAgeMs: 40,
    });
    deliver(3500, { kind: "turn.terminal", status: "success", resultType: "success" });
    await new Promise((resolve) => setTimeout(resolve, 0));

    assert.deepEqual(
      arms
        .filter((event) => event.name === "perf_ui_stream_stall")
        .map((event) => [event.value, event.properties?.stall_ms]),
      [[3050, 3050]],
    );
    // 首字仍是首个分片（100）；正文尾包是终态 3500 − 最新增量 (3450 − 40)。
    assert.deepEqual(
      reports.map((report) => [
        report.elementName,
        report.eventExtraDetail.step_type ?? report.eventExtraDetail.time_to_first_token,
        report.eventExtraDetail.generation_tail_finalize_ms,
      ]),
      [
        ["agent_step", "generation", "90"],
        ["message_completion", "100", undefined],
      ],
    );
  } finally {
    supervisor.dispose();
    setUiPerfArmsReporter(null);
  }
});
