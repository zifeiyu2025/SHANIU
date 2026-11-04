import type { EventMsg, Transport } from './types';

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

    async approve() {
      throw new Error('M0 mock：审批通道在 M2 里程碑（permission broker）接入');
    },

    onEvent(handler) {
      listeners.add(handler);
      return () => {
        listeners.delete(handler);
      };
    },
  };
}
