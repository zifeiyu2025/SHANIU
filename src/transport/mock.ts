import type { EventMsg, Settings, Transport } from './types';

const delay = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

const MOCK_REPLY = [
  '收到任务。这是 M0 骨架的 mock 会话：真实模型接入在 M1 里程碑（model_client）。',
  '当前已打通：事件协议 EventMsg、快照存储、流式渲染（已完成块冻结）与中断按钮。',
  '试试在流式输出过程中点击「停止」，事件流会以 cancelled 结束。',
].join('\n\n');

/** Vite 独立开发用的 mock 传输层：不经 Tauri IPC，本地驱动一轮完整 mock 事件流。 */
export function createMockTransport(): Transport {
  const listeners = new Set<(msg: EventMsg) => void>();
  let cancelRequested = false;
  let callSeq = 0;
  const settings: Settings = {
    // dev/mock 模式下 header crumb 与侧栏工作区行也要有可看的内容；
    // 模型名与输入卡上的 M0-Mock 保持一致，避免两处显示互相打脸。
    baseUrl: 'https://mock.local/v1',
    model: 'M0-Mock',
    workspaceRoot: 'D:/MyWorkspace/demo',
    hasApiKey: false,
  };

  function emit(msg: EventMsg): void {
    for (const listener of [...listeners]) listener(msg);
  }

  async function streamChunks(chunks: string[]): Promise<void> {
    for (const chunk of chunks) {
      if (cancelRequested) return;
      emit({ type: 'agent_message_delta', delta: chunk });
      await delay(30);
    }
  }

  /** 演示用工具调用：让 dev 模式直接看到 ToolCallCard 的「执行中 → 完成」全过程。 */
  async function emitToolCallDemo(): Promise<void> {
    // 每次调用都用新 callId：复用固定值会让第二轮任务沿用首轮卡片，
    // 既让 dev 模式看不到新卡，也复现不出"跨轮复用 call_id"这条边界
    callSeq += 1;
    const callId = `mock-call-${callSeq}`;
    const stdout = ['$ git status --short\n', ' M src/components/blocks/ToolCallCard.tsx\n'];
    const stderr = ['warning: in the working copy of LF will be replaced by CRLF\n'];
    emit({
      type: 'tool_call_begin',
      call_id: callId,
      tool: 'unified_exec',
      args: { command: 'git status --short' },
    });
    await delay(60);
    for (const delta of stdout) {
      if (cancelRequested) break;
      emit({ type: 'tool_call_output_delta', call_id: callId, channel: 'stdout', delta });
      await delay(80);
    }
    for (const delta of stderr) {
      if (cancelRequested) break;
      emit({ type: 'tool_call_output_delta', call_id: callId, channel: 'stderr', delta });
      await delay(80);
    }
    // 与真实后端一致（agent_loop/mod.rs）：取消时同样补 end，只是 exit_code 缺失、error 带取消说明。
    // 第三轮刻意不发 result，模拟旧版后端事件：让 dev 模式能同时看到"带 result 的卡"和
    // "走回退反推的卡"，否则 FF-2 兼容路径永远只有测试知道、没人真见过。
    const legacyShape = callSeq === 3;
    emit({
      type: 'tool_call_end',
      call_id: callId,
      exit_code: cancelRequested ? null : 0,
      error: cancelRequested ? '已取消' : null,
      ...(legacyShape ? {} : { result: cancelRequested ? ('cancelled' as const) : ('ok' as const) }),
    });
  }

  return {
    async ping(nonce) {
      await delay(30);
      return nonce;
    },

    async sendTask(task) {
      cancelRequested = false;
      emit({ type: 'turn_started', turn: 1 });
      const reply = `任务：${task}\n\n${MOCK_REPLY}`;
      const chunks = reply.match(/[\s\S]{1,8}/g) ?? [];
      const half = Math.ceil(chunks.length / 2);
      await streamChunks(chunks.slice(0, half));
      if (!cancelRequested) await emitToolCallDemo();
      await streamChunks(chunks.slice(half));
      emit({ type: 'token_usage', turn: 1, input_tokens: 96, cached_input_tokens: null, output_tokens: 64 });
      emit({
        type: 'turn_completed',
        turn: 1,
        reason: cancelRequested ? 'cancelled' : 'completed',
      });
    },

    async cancel() {
      cancelRequested = true;
    },

    async getSettings() {
      await delay(10);
      return { ...settings };
    },

    async saveSettings(input) {
      await delay(10);
      settings.baseUrl = input.baseUrl;
      settings.model = input.model;
      settings.workspaceRoot = input.workspaceRoot;
      if (input.apiKey?.trim()) settings.hasApiKey = true;
    },

    async approve() {
      // mock 模式无后端 broker：静默忽略（审批卡状态由后端事件驱动，mock 不发）
      await delay(10);
    },

    onEvent(handler) {
      listeners.add(handler);
      return () => {
        listeners.delete(handler);
      };
    },
  };
}
