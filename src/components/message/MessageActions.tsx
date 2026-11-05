import { useEffect, useRef, useState } from 'react';
import { CheckIcon, CopyIcon } from '../icons';

/** 复制反馈的回落时长：够看清，又不至于让按钮长期停在"已复制"骗人。 */
const FEEDBACK_MS = 1500;

/**
 * 消息行操作条（dsh MessageIconActions 的可用子集）。
 *
 * 只接「复制」：分支要等会话列表（SH-033）才有落点，点赞/点踩后端没有反馈存储——
 * 按实际能力接线，宁缺不假。显隐不在这里做，由 global.css 的 `.msg-actions`
 * 按 dsh 的 `@media (hover:hover)` + `data-actions-reveal` / `:has(~ …)` 规则驱动，
 * 这样"只有最后一条用户消息常显"这类跨行判据留在 CSS 层，组件不需要知道邻居。
 */
export function MessageActions({ text }: { text: string }) {
  const [state, setState] = useState<'idle' | 'copied' | 'failed'>('idle');
  const timer = useRef<number | null>(null);

  useEffect(
    () => () => {
      if (timer.current !== null) window.clearTimeout(timer.current);
    },
    [],
  );

  async function copy() {
    let ok = false;
    try {
      await navigator.clipboard.writeText(text);
      ok = true;
    } catch {
      ok = false; // 非安全上下文或用户拒绝授权：如实报失败，不假装复制成功
    }
    setState(ok ? 'copied' : 'failed');
    if (timer.current !== null) window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setState('idle'), FEEDBACK_MS);
  }

  const label = state === 'copied' ? '已复制' : state === 'failed' ? '复制失败' : '复制';
  return (
    <div className="msg-actions" data-testid="message-actions">
      <button type="button" className="msg-action" aria-label={label} title={label} onClick={copy}>
        {state === 'copied' ? <CheckIcon /> : <CopyIcon />}
      </button>
    </div>
  );
}
