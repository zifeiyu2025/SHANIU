import { useState } from 'react';
import { useStoreSelector } from '../../store/engine';
import type { SessionStore } from '../../store/session-store';
import type { Transport } from '../../transport/types';
import { ArrowUpIcon, ChevronDownIcon, PlusIcon, ShieldIcon, StopIcon } from '../icons';

/**
 * 底部输入卡片（复刻 dsh composer）：大圆角卡片、
 * 工具行（+ / 沙箱胶囊 / 模型名 / 用量徽章 / 圆形发送按钮，运行中变停止按钮）。
 */
export function Composer({
  store,
  transport,
  disabled,
  running,
}: {
  store: SessionStore;
  transport: Transport;
  disabled: boolean;
  running: boolean;
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
    <div className="w-full rounded-[20px] border border-border/70 bg-raised p-3 shadow-lg shadow-black/30">
      <textarea
        rows={2}
        value={text}
        disabled={disabled}
        placeholder="描述你想要构建的内容，/ 调用指令，@ 文件或对话"
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          // Enter 提交，Shift+Enter 换行；输入法组词中不提交
          if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
            e.preventDefault();
            submit();
          }
        }}
        className="w-full resize-none bg-transparent px-1 text-[14px] leading-6 text-text outline-none placeholder:text-weak disabled:opacity-50"
      />
      <div className="mt-1 flex items-center justify-between">
        <div className="flex items-center gap-1">
          <button
            type="button"
            title="附件（M1 接入）"
            className="flex h-7 w-7 items-center justify-center rounded-md text-secondary hover:bg-hover hover:text-text"
          >
            <PlusIcon className="h-4 w-4" />
          </button>
          <button
            type="button"
            title="沙箱策略（M2 接入，当前默认只读）"
            className="flex items-center gap-1 rounded-md px-1.5 py-1 text-xs text-secondary hover:bg-hover hover:text-text"
          >
            <ShieldIcon className="h-3.5 w-3.5" />
            只读沙箱
            <ChevronDownIcon className="h-3 w-3" />
          </button>
        </div>
        <div className="flex items-center gap-2.5 text-xs text-secondary">
          {running && (
            <span data-testid="turn-status" className="flex items-center gap-1.5">
              <span className="h-1.5 w-1.5 animate-pulse rounded-full bg-accent" />
              运行中 · turn {currentTurn}
            </span>
          )}
          <span className="flex items-center gap-1 text-text">
            M0-Mock
            <ChevronDownIcon className="h-3 w-3 text-secondary" />
          </span>
          {usage && (
            <span
              data-testid="token-usage"
              className="rounded-full bg-hover px-2 py-0.5 text-[11px] text-secondary"
            >
              ↑{usage.inputTokens} ↓{usage.outputTokens}
            </span>
          )}
          <span className="hidden items-center gap-1 sm:flex">
            High
            <ChevronDownIcon className="h-3 w-3" />
          </span>
          {running ? (
            <button
              type="button"
              aria-label="停止"
              onClick={() => transport.cancel()}
              className="flex h-9 w-9 items-center justify-center rounded-full bg-hover text-text hover:bg-border"
            >
              <StopIcon className="h-3.5 w-3.5" />
            </button>
          ) : (
            <button
              type="button"
              aria-label="发送"
              onClick={submit}
              disabled={!text.trim()}
              className="flex h-9 w-9 items-center justify-center rounded-full bg-accent text-white transition-colors hover:bg-accent-hover disabled:opacity-40"
            >
              <ArrowUpIcon className="h-4 w-4" />
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
