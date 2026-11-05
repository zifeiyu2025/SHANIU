import { nodesFromHistory } from '../nodes/from-history';
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
  // 续写还是新建，只认 store 里的 agentId（SH-033）：打开过的历史会话带着它的 id，
  // 新会话是 null。后端返回本次实际使用的 id，新会话由此第一次知道自己是谁——
  // 没有这一步，刚建的那条会话既进不了列表高亮，也续不回去。
  const agentId = store.getState().agentId;
  store.submitUserTask(task);
  store.setSending(true);
  transport.sendTask(task, agentId).then(
    (id) => {
      store.setSending(false);
      store.setAgentId(id);
    },
    (err: unknown) => {
      store.setSending(false);
      store.dispatch({ type: 'error', message: `任务提交失败：${String(err)}` });
    },
  );
}

/**
 * 打开一个历史会话（SH-033）：读档案 → 投影成节点 → 认下 agent_id。
 * 错误不在这里兜：列表组件需要区分"档案坏了"和"网络断了"，兜成一句通用提示反而没法归因。
 */
export async function openHistorySession(
  store: SessionStore,
  transport: Transport,
  agentId: string,
): Promise<void> {
  const entries = await transport.loadSession(agentId);
  store.openSession(agentId, nodesFromHistory(entries));
}

/** 从历史会话分支出新档案并立刻打开它；截断点由后端的配对安全规则兜底。 */
export async function forkHistorySession(
  store: SessionStore,
  transport: Transport,
  agentId: string,
  keepItems: number,
): Promise<void> {
  const newId = await transport.forkSession(agentId, keepItems);
  const entries = await transport.loadSession(newId);
  store.openSession(newId, nodesFromHistory(entries));
}
