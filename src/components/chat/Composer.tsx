import { useState } from 'react';
import { useStoreSelector } from '../../store/engine';
import type { SessionStore } from '../../store/session-store';
import type { Transport } from '../../transport/types';
import { formatTokens } from '../../utils/token-format';
import { ArrowUpIcon, ChevronDownIcon, DatabaseIcon, PlusIcon, StopIcon, WarningIcon } from '../icons';

/**
 * 输入卡（复刻 dsh InputBar）：
 * r28(panel)、无 border（0.5px 描边画进 elevation-soft 阴影）、bg specific-input-major；
 * 文本面 hero 52px/停靠 36px、pad 4 8 0 14、placeholder #81858C；
 * 工具行 pad 2 8 6：+ 28 圆(selector)、chip 28(r8 13/20 wt500)、发送 34 圆(deepseek-400)。
 */
export function Composer({
  store,
  transport,
  disabled,
  running,
  hero = false,
}: {
  store: SessionStore;
  transport: Transport;
  disabled: boolean;
  running: boolean;
  hero?: boolean;
}) {
  const [text, setText] = useState('');
  const currentTurn = useStoreSelector(store, (s) => s.currentTurn);
  const usage = useStoreSelector(store, (s) => s.usage);

  function submit() {
    const task = text.trim();
    if (!task || disabled) return;
    setText('');
    store.submitUserTask(task);
    transport.sendTask(task).catch((err: unknown) => {
      store.dispatch({ type: 'error', message: `任务提交失败：${String(err)}` });
    });
  }

  return (
    <div className="w-full rounded-panel bg-layer2 shadow-soft">
      <textarea
        rows={hero ? 2 : 1}
        value={text}
        disabled={disabled}
        placeholder="描述你想要构建的内容， / 调用指令， @ 文件或对话"
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          // Enter 提交，Shift+Enter 换行；输入法组词中不提交
          if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
            e.preventDefault();
            submit();
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
              {formatTokens(usage.inputTokens + usage.outputTokens)}
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
          {running ? (
            <button
              type="button"
              aria-label="停止"
              onClick={() => transport.cancel()}
              className="flex h-[34px] w-[34px] items-center justify-center rounded-full bg-accent text-white transition-colors duration-100 hover:bg-accent-strong"
            >
              <StopIcon className="h-3 w-3" />
            </button>
          ) : (
            <button
              type="button"
              aria-label="发送"
              onClick={submit}
              disabled={!text.trim()}
              className="flex h-[34px] w-[34px] items-center justify-center rounded-full bg-accent text-white transition-colors duration-100 hover:bg-accent-strong disabled:opacity-40"
            >
              <ArrowUpIcon className="h-4 w-4" />
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
