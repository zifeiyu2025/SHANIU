import { useStoreSelector } from '../../store/engine';
import type { SessionStore } from '../../store/session-store';
import type { Transport } from '../../transport/types';
import type { ConversationNode } from '../../nodes/types';
import { StreamingText } from '../markdown/StreamingText';
import { StatusFooter } from './StatusFooter';
import { TaskInput } from './TaskInput';

export function ChatView({ store, transport }: { store: SessionStore; transport: Transport }) {
  const nodes = useStoreSelector(store, (s) => s.nodes);
  const status = useStoreSelector(store, (s) => s.status);
  const running = status === 'running';
  return (
    <div className="flex h-full min-h-0 flex-col">
      <main className="flex-1 overflow-y-auto px-4 py-6">
        <div className="mx-auto flex max-w-3xl flex-col gap-4">
          {nodes.length === 0 ? (
            <p data-testid="empty-hint" className="mt-16 text-center text-sm text-zinc-500">
              输入任务开始会话（M0 mock：浏览器模式为模拟数据，Tauri 模式为真实 IPC）
            </p>
          ) : (
            nodes.map((node) => <NodeItem key={node.id} node={node} />)
          )}
        </div>
      </main>
      <StatusFooter store={store} />
      <TaskInput store={store} transport={transport} disabled={running} running={running} />
    </div>
  );
}

function NodeItem({ node }: { node: ConversationNode }) {
  switch (node.kind) {
    case 'user_message':
      return (
        <div className="max-w-[85%] self-end rounded-2xl bg-accent/20 px-4 py-2">
          <p className="whitespace-pre-wrap leading-6">{node.text}</p>
        </div>
      );
    case 'agent_message':
      return (
        <div className="rounded-2xl bg-surface-raised px-4 py-3">
          <StreamingText text={node.text} streaming={node.streaming} />
        </div>
      );
    case 'error':
      return (
        <div
          role="alert"
          className="rounded-lg border border-red-900 bg-red-950/40 px-4 py-3 text-sm text-red-300"
        >
          {node.message}
        </div>
      );
  }
}
