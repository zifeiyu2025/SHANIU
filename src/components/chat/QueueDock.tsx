import { useId, useState } from 'react';
import { ChevronDownIcon, ChevronUpIcon, QueueIcon, TrashIcon } from '../icons';

/**
 * 待发队列（复刻 dsh QueueDock + QueueDock.module.css）：贴在输入卡上方，
 * 底角取方、上角 r16，靠 -3px 负边距塞进卡片下面读成一块面板。
 * 形态判据照抄 dsh：空队列整块不渲染；单条没有计数头、由行首的队列图标承担；
 * 多条默认折叠成「N 条排队消息」计数头，点开才列明细。
 *
 * 只接「删除」：dsh 行上的编辑/插话需要内联编辑器与后端 steering，我们没有，
 * 宁可不画也不给一个点了没反应的按钮。
 */
export function QueueDock({
  items,
  onRemove,
}: {
  items: readonly string[];
  onRemove: (index: number) => void;
}) {
  const [collapsed, setCollapsed] = useState(true);
  const listId = useId();
  if (items.length === 0) return null;

  const multiple = items.length > 1;
  const expanded = !collapsed;
  // 单条时列表恒可见（dsh `listVisible = rowCount === 1 || expanded`）
  const listVisible = !multiple || expanded;

  return (
    <div className="-mb-[3px] px-0" data-testid="queue-dock">
      <div className="rounded-t-lg border-[0.5px] border-b-0 border-line1 bg-menu py-0.5">
        {multiple && (
          <button
            type="button"
            aria-controls={listId}
            aria-expanded={expanded}
            onClick={() => setCollapsed((c) => !c)}
            className="flex h-9 w-full items-center gap-2.5 rounded-md px-3 text-left text-primary transition-colors duration-100 hover:bg-hover"
          >
            <QueueIcon className="h-4 w-4 shrink-0 text-tertiary" />
            <span className="min-w-0 flex-1 text-[13px] font-medium leading-6">
              {items.length} 条排队消息
            </span>
            {expanded ? (
              <ChevronDownIcon className="h-3.5 w-3.5 shrink-0 text-tertiary" />
            ) : (
              <ChevronUpIcon className="h-3.5 w-3.5 shrink-0 text-tertiary" />
            )}
          </button>
        )}
        <ul
          id={listId}
          hidden={!listVisible}
          className="max-h-[180px] divide-y divide-line1 overflow-y-auto"
        >
          {items.map((text, index) => (
            <li
              key={`${index}:${text}`}
              className="flex min-h-9 items-center gap-2.5 rounded-md py-1 pl-3 pr-1.5"
            >
              {!multiple && <QueueIcon className="h-4 w-4 shrink-0 text-tertiary" />}
              <span
                data-testid="queue-preview"
                className="min-w-0 flex-1 truncate text-[13px] leading-5 text-dimmed"
                title={text}
              >
                {text}
              </span>
              <button
                type="button"
                aria-label="删除排队消息"
                title="删除排队消息"
                onClick={() => onRemove(index)}
                className="flex h-7 w-7 shrink-0 items-center justify-center rounded-full text-tertiary transition-colors duration-100 hover:bg-hover hover:text-primary"
              >
                <TrashIcon className="h-3.5 w-3.5" />
              </button>
            </li>
          ))}
        </ul>
      </div>
    </div>
  );
}
