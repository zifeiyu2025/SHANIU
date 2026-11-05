import { useRef, useState } from 'react';
import { useStoreSelector } from '../../store/engine';
import type { SessionStore } from '../../store/session-store';
import type { Transport } from '../../transport/types';
import { sendTaskNow } from '../../utils/session-actions';
import { formatTokens } from '../../utils/token-format';
import { ArrowUpIcon, ChevronDownIcon, DatabaseIcon, PlusIcon, QueueIcon, SpinnerIcon, StopIcon, WarningIcon } from '../icons';

/**
 * 输入卡（复刻 dsh InputBar）：
 * r28(panel)、无 border（0.5px 描边画进 elevation-soft 阴影）、bg specific-input-major；
 * 文本面 hero 52px/停靠 36px、pad 4 8 0 14、placeholder #81858C；
 * 工具行 pad 2 8 6：+ 28 圆(selector)、chip 28(r8 13/20 wt500)、发送 34 圆(deepseek-400)。
 *
 * 交互判据照抄 dsh（InputBar.tsx:155-171,365-396）：
 * - `editable` 不含 running——流式期间照样能打字（停止不锁输入）；只有 blocked
 *   （审批挂起）才锁，对应 dsh 的 `disabled = ... || blocked !== undefined`。
 * - `primaryStops = running && !sending && (empty || blocked)`——空草稿时主按钮是停止。
 * - 我们只有单任务槽、没有插话（steer）能力，所以 running + 有草稿走**排队发送**：
 *   真实入队、本轮结束后由 ChatView 按序发出，而不是把按钮画成点了必然失败的样子。
 */
export function Composer({
  store,
  transport,
  running,
  blocked,
  hero = false,
}: {
  store: SessionStore;
  transport: Transport;
  running: boolean;
  /** 有挂起审批：dsh 的 blocked，锁输入但不锁模型席位。 */
  blocked: boolean;
  hero?: boolean;
}) {
  const [text, setText] = useState('');
  // dsh 的 IME 守卫是三重：isComposing / keyCode 229 / compositionend 后 10ms 窗口
  // （keymap.ts:51-55,189-196）。只判 isComposing 在部分 WebView 上会漏——compositionend
  // 与 keydown 的先后顺序不保证，漏判就是"选词回车把消息发出去了"。
  const composingUntil = useRef(0);
  const currentTurn = useStoreSelector(store, (s) => s.currentTurn);
  const usage = useStoreSelector(store, (s) => s.usage);
  const sending = useStoreSelector(store, (s) => s.sending);

  const empty = text.trim() === '';
  const primaryStops = running && !sending && (empty || blocked);
  const primaryQueues = !primaryStops && !sending && running && !blocked && !empty;
  const primaryDisabled = primaryStops ? false : sending || empty || blocked;
  const primaryLabel = primaryStops
    ? '停止生成'
    : sending
      ? '正在发送消息'
      : primaryQueues
        ? '排队发送'
        : '发送消息';

  function onPrimary() {
    if (primaryStops) {
      void transport.cancel();
      return;
    }
    if (primaryDisabled) return;
    const task = text.trim();
    setText('');
    if (primaryQueues) store.enqueueTask(task);
    else sendTaskNow(store, transport, task);
  }

  return (
    <div className="relative z-10 w-full rounded-panel bg-input-major shadow-soft">
      <textarea
        rows={hero ? 2 : 1}
        value={text}
        disabled={blocked}
        placeholder={blocked ? '等待审批裁决，处理完即可继续输入' : '描述你想要构建的内容， / 调用指令， @ 文件或对话'}
        onChange={(e) => setText(e.target.value)}
        onCompositionEnd={() => {
          composingUntil.current = Date.now() + 10;
        }}
        onKeyDown={(e) => {
          // Enter 提交，Shift+Enter 换行；输入法组词中不提交。
          // 守卫必须在 preventDefault 之前：组词期间吞掉 keydown 会让输入法自己
          // 没法确认候选词（表现为"按回车选不了字"）。
          if (e.key === 'Enter' && !e.shiftKey) {
            if (
              e.nativeEvent.isComposing ||
              e.keyCode === 229 ||
              Date.now() < composingUntil.current
            ) {
              return;
            }
            e.preventDefault();
            onPrimary();
          }
        }}
        className={`w-full resize-none bg-transparent px-3.5 pt-2 text-[14px] leading-6 text-primary caret-accent outline-none placeholder:text-(--dsw-placeholder) disabled:opacity-50 ${
          hero ? 'min-h-[52px]' : 'min-h-9'
        }`}
      />
      <div className="flex items-center justify-between px-2 pb-1.5 pt-0.5">
        <div className="flex items-center gap-1">
          <button
            type="button"
            title="附件（M1 接入）"
            className="flex h-7 w-7 items-center justify-center rounded-full bg-selector text-primary transition-colors duration-100 hover:bg-hover-solid"
          >
            <PlusIcon className="h-4 w-4" />
          </button>
          <button
            type="button"
            title="权限预设（M2 接入沙箱策略裁决）"
            className="flex h-7 items-center gap-1 rounded-sm px-2 text-[13px] font-medium leading-5 text-secondary transition-colors duration-100 hover:bg-hover"
          >
            <WarningIcon className="h-3.5 w-3.5" />
            完全权限
            <ChevronDownIcon className="h-3 w-3 text-caption" />
          </button>
        </div>
        <div className="flex items-center gap-2">
          {running && (
            <span data-testid="turn-status" className="flex items-center gap-1.5 text-[12px] text-tertiary">
              <span className="h-1.5 w-1.5 animate-pulse rounded-full bg-accent" />
              运行中 · turn {currentTurn}
            </span>
          )}
          <button
            type="button"
            title="模型选择（M1 接入）"
            className="flex h-7 items-center gap-1 rounded-sm px-2 text-[13px] leading-5 text-primary transition-colors duration-100 hover:bg-hover"
          >
            M0-Mock
            <ChevronDownIcon className="h-3 w-3 text-caption" />
          </button>
          {/* 用量胶囊（对齐 dsh StatsPills：透明底、数据库图标、紧凑总量、tertiary） */}
          {usage && usage.inputTokens + usage.outputTokens > 0 && (
            <span
              data-testid="token-usage"
              className="inline-flex items-center gap-1.5 rounded-sm px-2 py-px text-[12px] leading-5 tabular-nums text-tertiary"
              title={`输入 ${usage.inputTokens} / 输出 ${usage.outputTokens} tokens`}
            >
              <DatabaseIcon className="h-3.5 w-3.5" />
              {formatTokens(usage.inputTokens + usage.outputTokens)} tok
            </span>
          )}
          <button
            type="button"
            title="推理力度（M1 接入）"
            className="flex h-7 items-center gap-1 rounded-sm px-2 text-[13px] leading-5 text-secondary transition-colors duration-100 hover:bg-hover"
          >
            High
            <ChevronDownIcon className="h-3 w-3 text-caption" />
          </button>
          <button
            type="button"
            aria-label={primaryLabel}
            title={primaryLabel}
            aria-busy={sending}
            disabled={primaryDisabled}
            onClick={onPrimary}
            className="flex h-[34px] w-[34px] -translate-y-0.5 items-center justify-center rounded-full bg-accent text-white transition-colors duration-100 hover:bg-accent-strong disabled:opacity-40"
          >
            {primaryStops ? (
              <StopIcon className="h-3 w-3" />
            ) : sending ? (
              <SpinnerIcon className="h-4 w-4 animate-spin" />
            ) : primaryQueues ? (
              <QueueIcon className="h-4 w-4" />
            ) : (
              <ArrowUpIcon className="h-4 w-4" />
            )}
          </button>
        </div>
      </div>
    </div>
  );
}
