import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import { useStoreSelector } from '../../store/engine';
import type { SessionStore } from '../../store/session-store';
import type { Transport } from '../../transport/types';
import type { ConversationNode } from '../../nodes/types';
import { sendTaskNow } from '../../utils/session-actions';
import { ChevronDownIcon } from '../icons';
import { StreamingText } from '../markdown/StreamingText';
import { ApprovalCard } from '../approval/ApprovalCard';
import { ToolCallCard } from '../blocks/ToolCallCard';
import { ReasoningBlock } from '../blocks/ReasoningBlock';
import { Composer } from './Composer';
import { Greeting } from './Greeting';
import { QueueDock } from './QueueDock';
import { MessageActions } from '../message/MessageActions';

/** 距底小于这个距离就算"贴在底部"，流式时继续跟随；超过说明用户在上翻，不再打断。
 *  24px 是 dsh 的 FOLLOW_THRESHOLD（use-chat-reading.ts:7，实际传入 24+1）。 */
const PIN_TO_BOTTOM_PX = 24;

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
  const sending = useStoreSelector(store, (s) => s.sending);
  const queued = useStoreSelector(store, (s) => s.queued);
  const running = status === 'running';
  const empty = nodes.length === 0;
  // dsh 的 endsWithResponse：一轮的最后一条正文才常显操作条，更早的靠悬停
  const tailId = [...nodes].reverse().find((n) => n.kind === 'agent_message')?.id;
  // dsh 的 blocked（InputBar.tsx:155）：挂起审批才锁输入，running 本身不锁。
  const blocked = nodes.some((n) => n.kind === 'approval' && n.status === 'pending');
  const scrollRef = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);
  // ref 供跟随滚动effect读取（避免闭包读到旧值），state 供渲染「回到底部」——
  // 同一个值写两次，React 对相同值会跳过重渲染，滚动高频时不会变成每帧一渲染。
  const [following, setFollowing] = useState(true);
  const pinToBottom = useCallback((next: boolean) => {
    pinned.current = next;
    setFollowing(next);
  }, []);

  // Esc = 停止生成（dsh apply.ts:292-299 `response.stop` 绑 Escape）。分层规则照抄
  // dsh 的 keymap：浮层先吃 Esc——设置卡开着时 Esc 是关卡，不能顺手把任务也取消了；
  // 挂起审批时 Esc 归审批卡（拒绝），这里让位，否则一次按键同时拒绝审批并掐掉整轮。
  useEffect(() => {
    if (!running || blocked) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return;
      if (document.querySelector('[role="dialog"]')) return;
      e.preventDefault();
      void transport.cancel();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [running, blocked, transport]);

  // 待发队列收口：只在「一轮刚跑完」这个跃变上放队首，不在挂载/空闲时凭空开火
  // （dsh 的 inbox 语义就是 next-turn）。sending 还挂着时不清标记，等 IPC 回执到了再放，
  // 避免并发提交撞上后端单任务槽。取走用 shiftQueuedTask()（原子删除），StrictMode 双跑也只发一次。
  const wasRunning = useRef(false);
  useEffect(() => {
    if (running) {
      wasRunning.current = true;
      return;
    }
    if (!wasRunning.current || sending) return;
    wasRunning.current = false;
    const task = store.shiftQueuedTask();
    if (task !== null) sendTaskNow(store, transport, task);
  }, [running, sending, queued, store, transport]);

  // 新会话时代次变化：回到顶部语义（重新贴底），否则旧会话的上翻位置会残留
  useEffect(() => {
    pinToBottom(true);
    scrollRef.current?.scrollTo({ top: 0 });
  }, [generation, pinToBottom]);

  useEffect(() => {
    const el = scrollRef.current;
    if (el && pinned.current) el.scrollTo({ top: el.scrollHeight });
  }, [nodes]);

  return (
    <div className="chat-body flex h-full min-h-0 flex-col">
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

      {/* 输入区常驻（dsh 的 composer 在空态/会话态之间只由 CSS 位移，不换实例）：
          换实例会把正在输入的草稿和焦点一起丢掉——真机实测第一条发出后第二条消失过。 */}
      {/* 消息区 + 浮动控制。relative 容器只为「回到底部」提供定位锚；
          空态时整块不渲染（null 占位），输入卡那一路子节点位置不变，实例得以常驻。 */}
      {!empty && (
        <div className="relative flex min-h-0 flex-1 flex-col">
          <div
            ref={scrollRef}
            data-testid="scroll-body"
            onScroll={(e) => {
              const el = e.currentTarget;
              pinToBottom(el.scrollHeight - el.scrollTop - el.clientHeight < PIN_TO_BOTTOM_PX);
            }}
            className="min-h-0 flex-1 overflow-y-auto px-4"
          >
            <div className="chat-column mt-4 flex flex-col pb-6">
              {nodes.map((node) => (
                <NodeItem
                  key={node.id}
                  node={node}
                  transport={transport}
                  reveal={node.kind === 'agent_message' && node.id === tailId}
                />
              ))}
            </div>
          </div>
          {/* dsh ChatView.tsx:272-283 + ChatView.module.css:342-388：判据只有一个
              !followingTail；控件 34 圆、静止时贴在输入卡上方 16px，右缘对齐内容列。 */}
          {!following && (
            <div className="pointer-events-none absolute inset-x-0 bottom-4 z-[8]">
              <div className="chat-column flex justify-end">
                <button
                  type="button"
                  aria-label="回到底部"
                  title="回到底部"
                  data-testid="to-bottom"
                  onClick={() => {
                    pinToBottom(true);
                    const el = scrollRef.current;
                    if (el) el.scrollTo({ top: el.scrollHeight });
                  }}
                  className="pointer-events-auto flex h-[34px] w-[34px] items-center justify-center rounded-full border-[0.5px] border-line3 bg-raised text-primary shadow-panel transition-colors duration-100 hover:bg-hover"
                >
                  <ChevronDownIcon className="h-4 w-4" />
                </button>
              </div>
            </div>
          )}
        </div>
      )}

      <div
        className={
          empty
            ? 'flex min-h-0 flex-1 items-center justify-center px-4'
            : 'relative shrink-0 px-4 pb-4'
        }
      >
        {/* 卡片上方 36px 渐隐（ConversationRoot.module.css:447-550），只在会话态出现 */}
        {!empty && (
          <div className="pointer-events-none absolute bottom-full left-0 right-0 h-9 bg-gradient-to-t from-canvas to-transparent" />
        )}
        <div className="composer-seat flex flex-col">
          {empty && <Greeting key={generation} />}
          <QueueDock items={queued} onRemove={(i) => store.removeQueuedTask(i)} />
          <Composer
            store={store}
            transport={transport}
            running={running}
            blocked={blocked}
            hero={empty}
          />
        </div>
      </div>
    </div>
  );
}

function NodeItem({
  node,
  transport,
  reveal,
}: {
  node: ConversationNode;
  transport: Transport;
  /** 本轮最后一条正文（dsh 的 endsWithResponse）：操作条常显，其余靠悬停。 */
  reveal: boolean;
}) {
  switch (node.kind) {
    case 'user_message':
      // 右对齐气泡：bg specific-bubble、r20、pad 10 16、max-w 70.2%（MessageItem/UserMessageBubble）
      // data-chat-flow-kind 是给 CSS 的跨行判据用的：后面还有用户行的，操作条只在悬停时出现
      return (
        <div
          className="mt-3 flex flex-col items-end"
          data-chat-flow-kind="user"
          data-testid="user-row"
        >
          <div
            data-testid="user-bubble"
            className="max-w-[70.2%] rounded-xl bg-bubble px-4 py-2.5 text-[14px] leading-[22px] whitespace-pre-wrap text-primary"
          >
            {node.text}
          </div>
          <MessageActions text={node.text} />
        </div>
      );
    case 'agent_message':
      // 平铺无气泡：正文 14/24、块间 16（AssistantMarkdown.module.css:9-21）
      // 流式中不给操作条：复制一条还没写完的回复不是"复制这轮回答"
      return (
        <div
          className="mt-3 text-[14px] leading-6 text-primary"
          data-testid="agent-row"
          data-chat-flow-kind="agent"
          data-actions-reveal={reveal && !node.streaming ? 'always' : 'hover'}
        >
          <StreamingText text={node.text} streaming={node.streaming} />
          {!node.streaming && <MessageActions text={node.text} />}
        </div>
      );
    case 'reasoning':
      return <ReasoningBlock node={node} />;
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
