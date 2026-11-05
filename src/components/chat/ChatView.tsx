import { useEffect, useRef, type ReactNode } from 'react';
import { useStoreSelector } from '../../store/engine';
import type { SessionStore } from '../../store/session-store';
import type { Transport } from '../../transport/types';
import type { ConversationNode } from '../../nodes/types';
import { StreamingText } from '../markdown/StreamingText';
import { ApprovalCard } from '../approval/ApprovalCard';
import { ToolCallCard } from '../blocks/ToolCallCard';
import { Composer } from './Composer';
import { Greeting } from './Greeting';

/** 会话内容列宽（dsh ConversationRoot.module.css:468-471）。 */
const CONTENT_WIDTH = 'w-[clamp(680px,64%,920px)]';

/** 距底小于这个距离就算"贴在底部"，流式时继续跟随；超过说明用户在上翻，不再打断。 */
const PIN_TO_BOTTOM_PX = 80;

/** 标题取首条用户任务，按字符数截断（中文一字符一字，40 足够表意）。 */
function sessionTitle(nodes: ConversationNode[]): string {
  const first = nodes.find((n) => n.kind === 'user_message');
  if (!first || first.kind !== 'user_message') return '新会话';
  const text = first.text.replace(/\s+/g, ' ').trim();
  return text.length > 40 ? `${text.slice(0, 40)}…` : text || '新会话';
}

/**
 * 会话主区（复刻 dsh ConversationRoot）：
 * header 76px（0.5px l3 底边、crumb 面包屑 = 工作区 / 会话标题、右侧模型 chip + 设置）；
 * 空态：欢迎语（Greeting）+ 居中 hero 输入卡；有消息时内容列 clamp(680,64%,920)
 * 居中 + composer sticky 沉底（顶部 36px 渐隐遮罩）。欢迎语取时时机 = 会话代次变化
 * （启动 + 新建对话各一次，零定时器；代次由 store.reset() 递增，见 SH-080/081）。
 * 流式跟随：内容变化时若用户原本贴底则滚到底，上翻中不打断。
 */
export function ChatView({
  store,
  transport,
  headerAction,
  workspaceLabel,
  modelLabel,
}: {
  store: SessionStore;
  transport: Transport;
  headerAction?: ReactNode;
  /** 工作区名（来自 settings.workspaceRoot）；未加载/未设置时给占位文案。 */
  workspaceLabel?: string;
  /** 当前模型名（来自 settings.model），mock 模式为 M0-Mock。 */
  modelLabel?: string;
}) {
  const nodes = useStoreSelector(store, (s) => s.nodes);
  const status = useStoreSelector(store, (s) => s.status);
  const generation = useStoreSelector(store, (s) => s.generation);
  const running = status === 'running';
  const empty = nodes.length === 0;
  const scrollRef = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);

  // 新会话时代次变化：回到顶部语义（重新贴底），否则旧会话的上翻位置会残留
  useEffect(() => {
    pinned.current = true;
    scrollRef.current?.scrollTo({ top: 0 });
  }, [generation]);

  useEffect(() => {
    const el = scrollRef.current;
    if (el && pinned.current) el.scrollTo({ top: el.scrollHeight });
  }, [nodes]);

  return (
    <div className="flex h-full min-h-0 flex-col">
      <header className="flex h-[76px] shrink-0 items-center justify-between gap-3 border-b-[0.5px] border-line3 pl-5 pr-7">
        {/* crumb（dsh ConversationRoot.module.css:136-169）：.crumbs gap 4px、
            .crumbSep "/" 14/20 caption、.crumb max-w 220 + r8 + pad 4/8 + tertiary、
            .crumbCurrent fw500 primary。标题取会话内容，不取工作区名 */}
        <div className="flex min-w-0 items-center gap-1 text-[14px] leading-5">
          <span className="shrink-0 rounded-md px-2 py-1 text-tertiary">
            {workspaceLabel || '未设置工作区'}
          </span>
          <span aria-hidden className="shrink-0 text-caption">
            /
          </span>
          <h1
            data-testid="session-title"
            className="min-w-0 max-w-[220px] truncate rounded-md px-2 py-1 font-medium text-primary"
            title={sessionTitle(nodes)}
          >
            {sessionTitle(nodes)}
          </h1>
        </div>
        <div className="flex shrink-0 items-center gap-2 pl-5">
          {modelLabel && (
            <span className="rounded-sm bg-selector px-2 py-1 text-[12px] leading-4 text-secondary">
              {modelLabel}
            </span>
          )}
          {headerAction}
        </div>
      </header>

      {empty ? (
        <div className="flex min-h-0 flex-1 items-center justify-center px-4">
          <div className={`w-full ${CONTENT_WIDTH} max-w-full pb-8`}>
            <Greeting key={generation} />
            <Composer store={store} transport={transport} disabled={running} running={running} hero />
          </div>
        </div>
      ) : (
        <>
          {/* 滚动区只包消息（dsh .scrollBody）：composer 是它的兄弟节点，
              短会话时也常驻底部，而不是跟在最后一条消息后面飘在中间 */}
          <div
            ref={scrollRef}
            onScroll={(e) => {
              const el = e.currentTarget;
              pinned.current = el.scrollHeight - el.scrollTop - el.clientHeight < PIN_TO_BOTTOM_PX;
            }}
            className="min-h-0 flex-1 overflow-y-auto"
          >
            <div className={`mx-auto mt-4 flex flex-col ${CONTENT_WIDTH} max-w-full px-4 pb-6`}>
              {nodes.map((node) => (
                <NodeItem key={node.id} node={node} transport={transport} />
              ))}
            </div>
          </div>
          {/* 沉底输入卡 + 卡片上方 36px 渐隐（ConversationRoot.module.css:447-550） */}
          <div className="relative shrink-0 px-4 pb-4">
            <div className="pointer-events-none absolute bottom-full left-0 right-0 h-9 bg-gradient-to-t from-canvas to-transparent" />
            <div className={`mx-auto ${CONTENT_WIDTH} max-w-full`}>
              <Composer store={store} transport={transport} disabled={running} running={running} />
            </div>
          </div>
        </>
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
          <div
            data-testid="user-bubble"
            className="max-w-[70.2%] rounded-xl bg-layer2 px-4 py-2.5 text-[14px] leading-[22px] whitespace-pre-wrap text-primary"
          >
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
