import type { ReactNode } from 'react';
import { useStoreSelector } from '../../store/engine';
import type { SessionStore } from '../../store/session-store';
import type { Transport } from '../../transport/types';
import type { ConversationNode } from '../../nodes/types';
import { StreamingText } from '../markdown/StreamingText';
import { ApprovalCard } from '../approval/ApprovalCard';
import { ToolCallCard } from '../blocks/ToolCallCard';
import { Composer } from './Composer';

/** 会话内容列宽（dsh ConversationRoot.module.css:468-471）。 */
const CONTENT_WIDTH = 'w-[clamp(680px,64%,920px)]';

/**
 * 会话主区（复刻 dsh ConversationRoot）：
 * header 76px（0.5px l3 底边、面包屑 crumb）；空态输入卡垂直居中（32px 上浮）；
 * 有消息时内容列 clamp(680,64%,920) 居中 + composer sticky 沉底（顶部 36px 渐隐遮罩）。
 * 空态欢迎区（标题/徽章/工作区 chips）已按需求移除，后续参考 dsh HeroShell 补齐。
 */
export function ChatView({
  store,
  transport,
  headerAction,
}: {
  store: SessionStore;
  transport: Transport;
  headerAction?: ReactNode;
}) {
  const nodes = useStoreSelector(store, (s) => s.nodes);
  const status = useStoreSelector(store, (s) => s.status);
  const running = status === 'running';
  const empty = nodes.length === 0;

  return (
    <div className="flex h-full min-h-0 flex-col">
      <header className="flex h-[76px] shrink-0 items-end justify-between border-b-[0.5px] border-line3 px-5 pb-2">
        <button
          type="button"
          className="max-w-[220px] truncate rounded-md px-2 py-1 text-[14px] leading-5 font-medium text-primary transition-colors duration-100 hover:bg-hover"
        >
          新会话
        </button>
        {headerAction}
      </header>

      {empty ? (
        <div className="flex min-h-0 flex-1 items-center justify-center px-4">
          <div className={`w-full ${CONTENT_WIDTH} max-w-full pb-8`}>
            <Composer store={store} transport={transport} disabled={running} running={running} hero />
          </div>
        </div>
      ) : (
        <div className="min-h-0 flex-1 overflow-y-auto">
          <div className={`mx-auto mt-4 flex flex-col ${CONTENT_WIDTH} max-w-full px-4`}>
            {nodes.map((node) => (
              <NodeItem key={node.id} node={node} transport={transport} />
            ))}
          </div>
          {/* 沉底输入卡：sticky + 卡片上方 36px 渐隐（ConversationRoot.module.css:447-550） */}
          <div className="sticky bottom-0 px-4 pb-4">
            <div className="pointer-events-none absolute bottom-full left-0 right-0 h-9 bg-gradient-to-t from-canvas to-transparent" />
            <div className={`mx-auto ${CONTENT_WIDTH} max-w-full`}>
              <Composer store={store} transport={transport} disabled={running} running={running} />
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

function NodeItem({ node, transport }: { node: ConversationNode; transport: Transport }) {
  switch (node.kind) {
    case 'user_message':
      // 右对齐气泡：bg specific-bubble、r20、pad 10 16、max-w 70.2%（MessageItem/UserMessageBubble）
      return (
        <div className="mt-3 flex justify-end">
          <div className="max-w-[70.2%] rounded-xl bg-layer2 px-4 py-2.5 text-[14px] leading-[22px] whitespace-pre-wrap text-primary">
            {node.text}
          </div>
        </div>
      );
    case 'agent_message':
      // 平铺无气泡：正文 14/24、块间 16（AssistantMarkdown.module.css:9-21）
      return (
        <div className="mt-3 text-[14px] leading-6 text-primary">
          <StreamingText text={node.text} streaming={node.streaming} />
        </div>
      );
    case 'error':
      return (
        <div
          role="alert"
          className="mt-3 rounded-md border-[0.5px] border-danger/40 bg-danger/10 px-3 py-2 text-[13px] text-danger"
        >
          {node.message}
        </div>
      );
    case 'approval':
      return (
        <div className="mt-3">
          <ApprovalCard node={node} transport={transport} />
        </div>
      );
    case 'tool_call':
      return (
        <div className="mt-3">
          <ToolCallCard node={node} />
        </div>
      );
  }
}
