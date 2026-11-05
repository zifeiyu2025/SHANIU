import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type {
  ApprovalDecision,
  EventMsg,
  SessionEntry,
  SessionSummary,
  Settings,
  Transport,
} from './types';

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

    async sendTask(task, agentId) {
      // agentId 为 null/undefined 时 Rust 侧收到 None = 新建会话；返回值是本次使用的 id。
      return invoke<string>('send_task', { task, agentId: agentId ?? null });
    },

    async cancel() {
      await invoke('cancel_task');
    },

    async listSessions() {
      return invoke<SessionSummary[]>('list_sessions');
    },

    async loadSession(agentId) {
      return invoke<SessionEntry[]>('load_session', { agentId });
    },

    async forkSession(agentId, keepItems) {
      return invoke<string>('fork_session', { agentId, keepItems });
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
        // 省略即 null：后端 None 分支不动该档。不能在这里兜一个默认档位——
        // 用户没碰沙箱开关时，保存别的字段绝不该顺手把沙箱模式改掉。
        sandboxMode: input.sandboxMode ?? null,
        approvalMode: input.approvalMode ?? null,
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
