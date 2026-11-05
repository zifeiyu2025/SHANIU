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

/**
 * 发出一条任务的唯一路径（Composer 主按钮与队列收口共用，避免两处各写一半）：
 * 记用户节点 → 标提交在途 → IPC → 失败落 error 事件并解除在途。
 * 在途标记必须在这里而不是 Composer 里：发送成功后空态 hero 卡会换成沉底卡，
 * 组件实例没了，局部 state 也就丢了。
 */
export function sendTaskNow(
  store: SessionStore,
  transport: Transport,
  task: string,
): void {
  store.submitUserTask(task);
  store.setSending(true);
  transport.sendTask(task).then(
    () => store.setSending(false),
    (err: unknown) => {
      store.setSending(false);
      store.dispatch({ type: 'error', message: `任务提交失败：${String(err)}` });
    },
  );
}
