import { useState } from 'react';
import type { SessionStore } from '../../store/session-store';
import type { Transport } from '../../transport/types';

export function TaskInput({
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
    <div className="border-t border-border-subtle px-4 py-3">
      <div className="mx-auto flex max-w-3xl items-end gap-2">
        <textarea
          rows={2}
          value={text}
          disabled={disabled}
          placeholder="描述任务…"
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            // Enter 提交，Shift+Enter 换行；输入法组词中不提交
            if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
              e.preventDefault();
              submit();
            }
          }}
          className="flex-1 resize-none rounded-xl border border-border-subtle bg-surface-raised px-3 py-2 text-sm outline-none focus:border-accent disabled:opacity-50"
        />
        {running ? (
          <button
            type="button"
            onClick={() => transport.cancel()}
            className="rounded-xl border border-red-800 px-4 py-2 text-sm text-red-300 hover:bg-red-950/40"
          >
            停止
          </button>
        ) : (
          <button
            type="button"
            onClick={submit}
            disabled={!text.trim()}
            className="rounded-xl bg-accent px-4 py-2 text-sm text-white hover:bg-accent-strong disabled:opacity-40"
          >
            发送
          </button>
        )}
      </div>
    </div>
  );
}
