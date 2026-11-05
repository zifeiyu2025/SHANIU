import { useCallback, useEffect, useState } from 'react';
import { useStoreSelector } from '../../store/engine';
import { forkHistorySession, openHistorySession } from '../../utils/session-actions';
import { BranchIcon } from '../icons';
import type { SessionStore } from '../../store/session-store';
import type { SessionSummary, Transport } from '../../transport/types';

/** 相对时间：只在列表刷新时算一次，不放定时器（与 SH-080 欢迎语同一取舍——定时器换不来更对的结果）。 */
function relativeTime(millis: number, now: number): string {
  const diff = Math.max(0, now - millis);
  const minutes = Math.floor(diff / 60_000);
  if (minutes < 1) return '刚刚';
  if (minutes < 60) return `${minutes} 分钟前`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} 小时前`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days} 天前`;
  return new Date(millis).toLocaleDateString('zh-CN');
}

/**
 * 侧栏会话列表（SH-033）。SH-084 当时刻意留空，理由是"没有数据源，画出来只能是假条目"——
 * 数据源现在由 `rollout::list` 提供，这一条补齐。
 *
 * 三个入口都有真行为，没有一个装饰性按钮：
 * - 点行 = 读档案 → 投影成节点 → 认下 agent_id，之后输入即续写那份档案；
 * - 分支 = 后端 fork（父档案只读），立刻打开新档案；
 * - 当前会话高亮只认 store 里的 agentId，不自己记"我点了哪条"，否则两处状态会漂。
 */
export function SessionList({
  store,
  transport,
}: {
  store: SessionStore;
  transport: Transport;
}) {
  const [items, setItems] = useState<SessionSummary[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const agentId = useStoreSelector(store, (s) => s.agentId);
  // 一轮终态变化就重取：新会话建档后 agentId 已变，续跑完则 itemCount/更新时间要跟上。
  const lastTurnEnd = useStoreSelector(store, (s) => s.lastTurnEnd);
  const status = useStoreSelector(store, (s) => s.status);

  const refresh = useCallback(() => {
    transport.listSessions().then(
      (list) => {
        setItems(list);
        setError(null);
      },
      (err: unknown) => setError(`会话列表读不出：${String(err)}`),
    );
  }, [transport]);

  useEffect(() => {
    refresh();
  }, [refresh, agentId, lastTurnEnd, status]);

  async function run(agentId: string, action: 'open' | 'fork'): Promise<void> {
    setBusyId(`${agentId}:${action}`);
    try {
      if (action === 'open') await openHistorySession(store, transport, agentId);
      else {
        // 分支保留全部已落盘条目：截断点仍由后端的配对安全规则兜底，前端不猜。
        const keep = items.find((item) => item.agentId === agentId)?.itemCount ?? 0;
        await forkHistorySession(store, transport, agentId, keep);
      }
      refresh();
    } catch (err) {
      setError(`${action === 'open' ? '打开' : '分支'}会话失败：${String(err)}`);
    } finally {
      setBusyId(null);
    }
  }

  const now = Date.now();

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex h-9 shrink-0 items-center px-3 text-[13px] leading-5 text-tertiary">
        会话
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-2">
        {items.length === 0 && (
          <p className="px-2 py-1 text-[13px] leading-5 text-caption">
            {error ?? '还没有会话。发一条任务就会建档。'}
          </p>
        )}
        {items.length > 0 && error !== null && (
          <p className="px-2 py-1 text-[13px] leading-5 text-danger">{error}</p>
        )}
        <ul className="flex flex-col gap-0.5">
          {items.map((item) => {
            const active = item.agentId === agentId;
            return (
              <li key={item.agentId}>
                <div
                  className={`group flex items-center gap-1 rounded-md pr-1 transition-colors duration-100 ${
                    active ? 'bg-hover' : 'hover:bg-hover'
                  }`}
                >
                  <button
                    type="button"
                    onClick={() => void run(item.agentId, 'open')}
                    disabled={busyId !== null}
                    title={item.title || `会话 ${item.agentId}`}
                    aria-current={active ? 'true' : undefined}
                    className="flex min-w-0 flex-1 flex-col items-start rounded-md px-2 py-1.5 text-left disabled:opacity-50"
                  >
                    <span
                      className={`w-full truncate text-[14px] leading-5 ${
                        active ? 'font-medium text-primary' : 'text-secondary'
                      }`}
                    >
                      {item.title || '（无标题会话）'}
                    </span>
                    <span
                      data-testid="session-meta"
                      className="w-full truncate text-[12px] leading-4 text-caption"
                    >
                      {relativeTime(item.updatedAt, now)} · {item.model}
                      {item.parentId !== null && ' · 分支'}
                    </span>
                  </button>
                  <button
                    type="button"
                    onClick={() => void run(item.agentId, 'fork')}
                    disabled={busyId !== null}
                    aria-label={`从这条会话分支`}
                    title="分支出新会话"
                    className="flex h-7 w-7 shrink-0 items-center justify-center rounded-sm text-tertiary opacity-0 transition-opacity duration-100 hover:bg-hover-solid hover:text-primary focus-visible:opacity-100 group-hover:opacity-100 disabled:opacity-30"
                  >
                    <BranchIcon className="h-3.5 w-3.5" />
                  </button>
                </div>
              </li>
            );
          })}
        </ul>
      </div>
    </div>
  );
}
