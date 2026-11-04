import { useEffect, useRef } from 'react';
import type { ApprovalNode, RiskLevel } from '../../nodes/types';
import type { Transport } from '../../transport/types';

const RISK_LABEL: Record<RiskLevel, string> = {
  read: '只读',
  write_in_workspace: '工作区写入',
  execute: '命令执行',
  destructive: '破坏性',
};

/**
 * 审批卡（ADR-003）：展示工具/参数/风险等级；默认焦点在「拒绝」（审批疲劳缓解）；
 * execute 级提供「本会话记住」（allow_always），destructive 永不提供。
 */
export function ApprovalCard({
  node,
  transport,
}: {
  node: ApprovalNode;
  transport: Transport;
}) {
  const denyRef = useRef<HTMLButtonElement>(null);
  const decided = node.status !== 'pending';

  useEffect(() => {
    if (!decided) denyRef.current?.focus();
  }, [decided]);

  function decide(decision: 'allow' | 'allow_always' | 'deny') {
    void transport.approve(node.requestId, decision);
  }

  return (
    <div
      data-testid={`approval-${node.requestId}`}
      data-status={node.status}
      className="rounded-lg border border-line2 bg-raised px-4 py-3"
    >
      <div className="flex items-center gap-2 text-[13px]">
        <span
          className={`rounded-full px-2 py-0.5 text-[11px] ${
            node.risk === 'destructive'
              ? 'bg-danger/15 text-danger'
              : 'bg-accent/15 text-accent'
          }`}
        >
          {RISK_LABEL[node.risk]}
        </span>
        <span className="font-medium text-primary">{node.tool}</span>
      </div>
      <pre className="mt-2 max-h-40 overflow-auto whitespace-pre-wrap text-[12px] leading-[18px] text-secondary">
        {typeof node.args === 'string' ? node.args : JSON.stringify(node.args, null, 2)}
      </pre>
      {decided ? (
        <p className="mt-2 text-[13px] text-tertiary">
          {node.status === 'allowed' ? '已允许' : '已拒绝'}
        </p>
      ) : (
        <div className="mt-3 flex items-center gap-2">
          <button
            type="button"
            onClick={() => decide('deny')}
            ref={denyRef}
            className="h-8 rounded-md border border-line2 px-4 text-[13px] text-secondary transition-colors duration-100 hover:border-danger hover:text-danger"
          >
            拒绝
          </button>
          <button
            type="button"
            onClick={() => decide('allow')}
            className="h-8 rounded-md bg-accent px-4 text-[13px] text-white transition-colors duration-100 hover:bg-accent-strong"
          >
            允许
          </button>
          {node.risk === 'execute' && (
            <button
              type="button"
              onClick={() => decide('allow_always')}
              className="h-8 rounded-md px-3 text-[13px] text-secondary transition-colors duration-100 hover:bg-hover hover:text-primary"
            >
              本会话记住
            </button>
          )}
        </div>
      )}
    </div>
  );
}
