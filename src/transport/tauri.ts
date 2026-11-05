import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type { ApprovalDecision, EventMsg, Settings, Transport } from './types';

// 通道名与 src-tauri/src/events.rs 保持一致。
const EVENT_CHANNEL = 'shaniu://event';
const PONG_CHANNEL = 'shaniu://pong';
const PONG_TIMEOUT_MS = 5_000;

/** Tauri IPC 实现：invoke 命令下行、Tauri 事件上行（ADR-005）。 */
export function createTauriTransport(): Transport {
  return {
    ping(nonce) {
      return new Promise<string>((resolve, reject) => {
        let unlisten: UnlistenFn | undefined;
        const timer = setTimeout(() => {
          unlisten?.();
          reject(new Error(`pong 超时（${PONG_TIMEOUT_MS}ms）`));
        }, PONG_TIMEOUT_MS);
        // 先注册监听再 invoke，避免事件先于监听到达
        listen<{ nonce: string }>(PONG_CHANNEL, (event) => {
          if (event.payload?.nonce !== nonce) return;
          clearTimeout(timer);
          unlisten?.();
          resolve(nonce);
        }).then(
          (fn) => {
            unlisten = fn;
            invoke('dev_ping', { nonce }).catch((err: unknown) => {
              clearTimeout(timer);
              fn();
              reject(new Error(`dev_ping 调用失败: ${String(err)}`));
            });
          },
          (err: unknown) => {
            clearTimeout(timer);
            reject(err);
          },
        );
      });
    },

    async sendTask(task) {
      await invoke('send_task', { task });
    },

    async cancel() {
      await invoke('cancel_task');
    },

    async getSettings() {
      return invoke<Settings>('get_settings');
    },

    async saveSettings(input) {
      await invoke('save_settings', {
        baseUrl: input.baseUrl,
        model: input.model,
        workspaceRoot: input.workspaceRoot,
        apiKey: input.apiKey?.trim() ? input.apiKey.trim() : null,
      });
    },

    async approve(requestId: string, decision: ApprovalDecision) {
      await invoke('approve', { requestId, decision });
    },

    onEvent(handler) {
      let unlisten: UnlistenFn | undefined;
      let disposed = false;
      listen<EventMsg>(EVENT_CHANNEL, (event) => handler(event.payload)).then((fn) => {
        if (disposed) fn();
        else unlisten = fn;
      });
      return () => {
        disposed = true;
        unlisten?.();
      };
    },
  };
}
