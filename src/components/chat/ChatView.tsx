import { useStoreSelector } from '../../store/engine';
import type { SessionStore } from '../../store/session-store';
import type { Transport } from '../../transport/types';
import type { ConversationNode } from '../../nodes/types';
import { StreamingText } from '../markdown/StreamingText';
import { Composer } from './Composer';
import { EmptyHero } from './EmptyHero';

/**
 * 会话主区（复刻 dsh）：空态时欢迎语 + 输入卡片垂直居中；
 * 有消息时消息流滚动 + 输入卡片沉底。
 */
export function ChatView({ store, transport }: { store: SessionStore; transport: Transport }) {
  const nodes = useStoreSelector(store, (s) => s.nodes);
  const status = useStoreSelector(store, (s) => s.status);
  const running = status === 'running';
  const empty = nodes.length === 0;

  return (
    <div className="flex h-full min-h-0 flex-col">
      <main className="min-h-0 flex-1 overflow-y-auto px-4 py-6">
        {empty ? (
          <div className="mx-auto flex h-full max-w-2xl flex-col items-center justify-center gap-10 pb-10">
            <EmptyHero />
            <Composer store={store} transport={transport} disabled={running} running={running} />
          </div>
        ) : (
          <div className="mx-auto flex max-w-3xl flex-col gap-5 pb-4">
            {nodes.map((node) => (
              <NodeItem key={node.id} node={node} />
            ))}
          </div>
        )}
      </main>
      {!empty && (
        <div className="px-4 pb-4">
          <div className="mx-auto max-w-2xl">
            <Composer store={store} transport={transport} disabled={running} running={running} />
          </div>
        </div>
      )}
    </div>
  );
}

function NodeItem({ node }: { node: ConversationNode }) {
  switch (node.kind) {
    case 'user_message':
      return (
        <div className="max-w-[80%] self-end rounded-2xl bg-hover px-4 py-2.5">
          <p className="whitespace-pre-wrap text-[14px] leading-6 text-text">{node.text}</p>
        </div>
      );
    case 'agent_message':
      return (
        <div className="text-[15px] text-text">
          <StreamingText text={node.text} streaming={node.streaming} />
        </div>
      );
    case 'error':
      return (
        <div
          role="alert"
          className="rounded-xl border border-red-900/70 bg-red-950/40 px-4 py-3 text-sm text-red-300"
        >
          {node.message}
        </div>
      );
  }
}
