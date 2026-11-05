import type { SessionStore } from '../store/session-store';
import type { Transport } from '../transport/types';

/**
 * 「新会话」的唯一动作语义（header 与侧栏共用，避免两处各写一半）。
 * 顺序有讲究：先请中断旧轮（源头止损，别让它继续跑完烧 token / 继续执行工具），
 * 再 reset() 关闭事件窗口丢弃在途残留（SH-081）；cancel 失败不阻塞清会话。
 */
export function startNewSession(store: SessionStore, transport: Transport): void {
  if (store.getState().status === 'running') void transport.cancel();
  store.reset();
}

/** 工作区路径 → 展示名（取末级目录；空 = 未设置）。Windows/POSIX 分隔符都吃。 */
export function workspaceDisplayName(root: string): string {
  const trimmed = root.trim();
  if (!trimmed) return '';
  const parts = trimmed.split(/[/\\]+/).filter(Boolean);
  return parts[parts.length - 1] ?? trimmed;
}
