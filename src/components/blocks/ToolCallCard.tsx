import { useMemo, useState } from 'react';
import type { ToolCallNode, ToolCallStatus } from '../../nodes/types';
import { TerminalBlock } from './TerminalBlock';
import { ChevronDownIcon, ChevronUpIcon } from '../icons';

const STATUS_META: Record<ToolCallStatus, { label: string; tone: string; dot: string }> = {
  running: { label: '运行中', tone: 'bg-accent/15 text-accent', dot: 'bg-accent animate-pulse' },
  ok: { label: '完成', tone: 'bg-success/15 text-success', dot: 'bg-success' },
  error: { label: '失败', tone: 'bg-danger/15 text-danger', dot: 'bg-danger' },
  cancelled: { label: '已取消', tone: 'bg-hover text-tertiary', dot: 'bg-tertiary' },
  denied: { label: '已拒绝', tone: 'bg-line2 text-primary', dot: 'bg-secondary' },
};

/** 参数摘要长度上限：只留单行紧凑 JSON，大对象一律截断不展开。 */
const ARGS_SUMMARY_MAX = 100;

function argsSummary(args: unknown): string {
  if (args === null || args === undefined) return '';
  const text = typeof args === 'string' ? args : (JSON.stringify(args) ?? '');
  return text.length > ARGS_SUMMARY_MAX ? `${text.slice(0, ARGS_SUMMARY_MAX)}…` : text;
}

/**
 * 工具调用卡（docs/01 §4.2 + ADR-006）：头部工具名/状态/参数摘要，主体内嵌 TerminalBlock。
 * 纯展示组件——数据全来自归约后的节点，不自行发起 IPC。
 */
export function ToolCallCard({ node }: { node: ToolCallNode }) {
  // 徽标与 data-status 按权威 result 决定；缺失时回退到节点 status（旧事件反推路径）。
  // 运行时兜底：ChatView 没有 error boundary，若这里取到未知取值会让 STATUS_META[x] 为 undefined、
  // 随后读 .dot/.label 同步抛错，一张坏数据就能白屏整个会话——未知值一律按最保守的 error 渲染。
  const known = node.result ?? node.status;
  const status: ToolCallStatus = known in STATUS_META ? known : 'error';
  const meta = STATUS_META[status];
  // 一轮里可能连续发多个工具调用，testid 统一按 callId 作用域化，避免并存时多匹配
  const scope = `tool-call-${node.callId}`;
  // 每个 output_delta 都会重渲染本卡片，而 args 引用由 immer 保持不变，memo 必命中：
  // 避免大参数（如 apply_patch 的补丁正文）被逐 delta 重复 stringify。
  const summary = useMemo(() => argsSummary(node.args), [node.args]);
  // 两级披露（dsh ToolRow.tsx:196 useDisclosure + :471 expandOnRowClick）：
  // 卡片默认折叠，点整行标题展开输出；没输出时不给可展开的样子（dsh 的 expandable 判据）。
  const [open, setOpen] = useState(false);
  const expandable = node.stdout !== '' || node.stderr !== '';

  return (
    <div
      data-testid={scope}
      data-status={status}
      className="rounded-lg border-[0.5px] border-line2 bg-layer1 px-3 py-2"
    >
      <button
        type="button"
        data-testid={`${scope}-row`}
        aria-expanded={expandable ? open : undefined}
        disabled={!expandable}
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-2 text-left text-[13px] transition-colors duration-100 enabled:hover:bg-hover"
      >
        <span
          data-testid={status === 'running' ? `${scope}-spinner` : undefined}
          aria-hidden
          className={`h-1.5 w-1.5 shrink-0 rounded-full ${meta.dot}`}
        />
        <span className="shrink-0 font-medium text-primary">{node.tool}</span>
        <span className={`shrink-0 rounded-full px-1.5 py-0.5 text-[11px] ${meta.tone}`}>
          {meta.label}
        </span>
        {summary && (
          <code className="min-w-0 flex-1 truncate font-mono text-[11px] text-tertiary">
            {summary}
          </code>
        )}
        {expandable && (
          <span className="shrink-0 text-caption" aria-hidden>
            {open ? <ChevronUpIcon className="h-3.5 w-3.5" /> : <ChevronDownIcon className="h-3.5 w-3.5" />}
          </span>
        )}
      </button>
      {/* error 文案无条件渲染（SH-017 修过一次的坑）：只有"失败"用红色，其余状态降为中性，
          避免灰徽标旁挂红字；`result=ok` 却带 error 文本这种矛盾数据也照原样显示，前端不"修正"状态。
          它也不跟着折叠走——失败原因必须不点也能看见。 */}
      {node.error && (
        <p
          role="alert"
          className={`mt-1.5 text-[12px] ${status === 'error' ? 'text-danger' : 'text-tertiary'}`}
        >
          {node.error}
        </p>
      )}
      {open && <TerminalBlock stdout={node.stdout} stderr={node.stderr} scope={scope} />}
    </div>
  );
}
