import { useState } from 'react';
import type { ReasoningNode } from '../../nodes/types';
import { ChevronDownIcon } from '../icons';

/**
 * 思考过程块（业界同形态：codex TUI 把 reasoning 放 transcript_only 折叠区、
 * ZCode 单列 reasoning_* 事件后折叠展示）。默认**收起**——推理原文动辄几百到几千字，
 * 展开着会把真正的答案挤出屏幕；流式期间标题显示"思考中"，正文开始后收口为"思考过程"。
 *
 * 刻意不做自动展开：自动展开等于把"要不要看"这个决定从用户手里拿走，
 * 而 dsh 的 transcript_only 语义正是"默认不进主视图"。
 */
export function ReasoningBlock({ node }: { node: ReasoningNode }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="mt-3 rounded-lg border-[0.5px] border-line2 bg-layer1">
      <button
        type="button"
        aria-expanded={open}
        aria-label={node.streaming ? '思考中，点击展开' : '思考过程，点击展开'}
        onClick={() => setOpen((o) => !o)}
        className="flex h-9 w-full items-center gap-1.5 px-3 text-[13px] leading-5 text-tertiary transition-colors duration-100 hover:bg-hover hover:text-secondary"
      >
        <span className="font-medium">{node.streaming ? '思考中' : '思考过程'}</span>
        {node.streaming && (
          <span aria-hidden className="h-1.5 w-1.5 animate-pulse rounded-full bg-accent" />
        )}
        <ChevronDownIcon
          className={`h-3.5 w-3.5 transition-transform duration-100 ${open ? 'rotate-180' : ''}`}
        />
      </button>
      {open && (
        <p
          data-testid="reasoning-body"
          className="whitespace-pre-wrap px-3 pb-2.5 text-[13px] leading-[20px] text-tertiary"
        >
          {node.text}
        </p>
      )}
    </div>
  );
}
