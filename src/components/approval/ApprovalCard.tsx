import { useEffect, useRef, useState, type KeyboardEvent } from 'react';
import type { ApprovalNode, RiskLevel } from '../../nodes/types';
import type { Transport } from '../../transport/types';

const RISK_LABEL: Record<RiskLevel, string> = {
  read: '只读',
  write_in_workspace: '工作区写入',
  execute: '命令执行',
  destructive: '破坏性',
  plan_review: '计划评审',
};

/** 计划评审节点取计划正文（exit_plan_mode 的 plan 参数），否则回退原始参数。 */
function planTextOf(args: unknown): string {
  if (args && typeof args === 'object' && 'plan' in args) {
    const plan = (args as { plan?: unknown }).plan;
    if (typeof plan === 'string') return plan;
  }
  return typeof args === 'string' ? args : JSON.stringify(args, null, 2);
}

type Decision = 'allow' | 'allow_always' | 'deny';

/** 焦点落在可输入控件上时，卡片不吃键盘（dsh ApprovalCard.tsx:40-52 的让位规则）。 */
function yieldsToTarget(target: EventTarget | null): boolean {
  const el = target as HTMLElement | null;
  if (!el?.tagName) return false;
  const tag = el.tagName.toLowerCase();
  return tag === 'input' || tag === 'textarea' || tag === 'select' || el.isContentEditable === true;
}

/**
 * 审批卡（ADR-003）：展示工具/参数/风险等级；默认焦点在「拒绝」（审批疲劳缓解，
 * docs/01 §风险表硬要求）；execute 级提供「本会话记住」，destructive 永不提供。
 *
 * 键盘（dsh ApprovalCard.tsx:40-52 + index.ts:83-88）：Esc=拒绝，焦点在输入控件里、
 * 带修饰键、按键重复、输入法组词中一律让位。
 * **有意不接 dsh 的 Enter=允许**：我们把默认焦点停在「拒绝」正是为了挡住无脑放行，
 * 再给容器绑一个"回车即允许"等于把最顺手的键交给最危险的动作——两条安全要求互相矛盾时，
 * 取保守的那条。允许仍可用 Tab 一步到达后回车（原生按钮行为）。
 */
export function ApprovalCard({
  node,
  transport,
}: {
  node: ApprovalNode;
  transport: Transport;
}) {
  const denyRef = useRef<HTMLButtonElement>(null);
  const isPlanReview = node.risk === 'plan_review';
  const [error, setError] = useState<string | null>(null);
  // 裁决在途：本地先落一个决定，按钮立刻禁用（dsh 的 waiting + setAnswered 同一形态），
  // 失败再回滚，用户可以重按——不能让他对着还能点的按钮连发两次裁决。
  const [answered, setAnswered] = useState<Decision | null>(null);
  const [busy, setBusy] = useState(false);
  const confirmed = node.status !== 'pending';
  const locked = confirmed || answered !== null;

  useEffect(() => {
    if (!locked) denyRef.current?.focus();
  }, [locked]);

  function decide(decision: Decision) {
    if (locked) return;
    setAnswered(decision);
    setBusy(true);
    transport.approve(node.requestId, decision).catch((e: unknown) => {
      setAnswered(null);
      setError(String(e));
    }).finally(() => setBusy(false));
  }

  function onKeyDown(e: KeyboardEvent<HTMLDivElement>) {
    if (e.key !== 'Escape' || locked) return;
    if (e.repeat || e.ctrlKey || e.metaKey || e.altKey || e.shiftKey) return;
    if (e.nativeEvent.isComposing || e.keyCode === 229 || yieldsToTarget(e.target)) return;
    e.preventDefault();
    decide('deny');
  }

  const outcome = confirmed
    ? node.status === 'allowed'
      ? isPlanReview
        ? '计划已批准'
        : '已允许'
      : '已拒绝'
    : answered === 'deny'
      ? '正在拒绝…'
      : answered
        ? '正在允许…'
        : null;

  return (
    <div
      data-testid={`approval-${node.requestId}`}
      data-status={node.status}
      onKeyDown={onKeyDown}
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
        <span className="font-medium text-primary">
          {isPlanReview ? '实施计划待批准' : node.tool}
        </span>
      </div>
      {isPlanReview ? (
        <>
          <p className="mt-2 text-[12px] text-tertiary">
            批准后计划保存到工作区 <code className="text-secondary">.shaniu/plan.md</code>，并恢复写入与命令执行能力。
          </p>
          <pre
            data-testid={`plan-${node.requestId}`}
            className="mt-2 max-h-72 overflow-auto whitespace-pre-wrap rounded-md border border-line2 bg-surface px-3 py-2 text-[12px] leading-[19px] text-secondary"
          >
            {planTextOf(node.args)}
          </pre>
        </>
      ) : (
        <pre className="mt-2 max-h-40 overflow-auto whitespace-pre-wrap text-[12px] leading-[18px] text-secondary">
          {typeof node.args === 'string' ? node.args : JSON.stringify(node.args, null, 2)}
        </pre>
      )}
      {error && (
        <p role="alert" className="mt-2 text-[12px] text-danger">
          {error}
        </p>
      )}
      {/* 按钮顺序照抄 dsh（拒绝 outline → 允许 → 始终允许），且裁决后不消失：
          整组禁用 + aria-busy + 保留文案，用户看得见"我刚才按的是哪一个"。 */}
      <div
        role="group"
        aria-label="审批裁决"
        aria-busy={busy}
        className="mt-3 flex items-center gap-2"
      >
        <button
          type="button"
          onClick={() => decide('deny')}
          ref={denyRef}
          disabled={locked}
          className="h-8 rounded-md border border-line2 px-4 text-[13px] text-secondary transition-colors duration-100 enabled:hover:border-danger enabled:hover:text-danger disabled:opacity-45"
        >
          {isPlanReview ? '拒绝修改' : '拒绝'}
        </button>
        <button
          type="button"
          onClick={() => decide('allow')}
          disabled={locked}
          className="h-8 rounded-md bg-accent px-4 text-[13px] text-white transition-colors duration-100 enabled:hover:bg-accent-strong disabled:opacity-45"
        >
          {isPlanReview ? '批准计划' : '允许'}
        </button>
        {node.risk === 'execute' && (
          <button
            type="button"
            onClick={() => decide('allow_always')}
            disabled={locked}
            className="h-8 rounded-md px-3 text-[13px] text-secondary transition-colors duration-100 enabled:hover:bg-hover enabled:hover:text-primary disabled:opacity-45"
          >
            本会话记住
          </button>
        )}
      </div>
      {outcome && (
        <p role="status" className="mt-2 text-[13px] text-tertiary">
          {outcome}
        </p>
      )}
    </div>
  );
}
