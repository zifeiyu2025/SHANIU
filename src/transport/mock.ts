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
  const settings: Settings = {
    baseUrl: '',
    model: '',
    workspaceRoot: '',
    hasApiKey: false,
  };

  function emit(msg: EventMsg): void {
    for (const listener of [...listeners]) listener(msg);
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
      for (const chunk of chunks) {
        if (cancelRequested) break;
        emit({ type: 'agent_message_delta', delta: chunk });
        await delay(30);
      }
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
