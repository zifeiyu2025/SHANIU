import { useMemo, useState } from 'react';
import { headTailCap, splitLines } from '../../utils/head-tail-cap';

/**
 * 折叠态渲染行数上限：16 行取 dsh `DEFAULT_TERMINAL_MAX_LINES`（TerminalBlock.tsx:12），
 * 同时满足 docs/01 §4.2 的「限行数」要求——长输出不能把会话流撑爆。
 */
const MAX_LINES = 16;

function Channel({
  channel,
  text,
  scope,
  expanded,
  onToggle,
}: {
  channel: 'stdout' | 'stderr';
  text: string;
  scope: string;
  expanded: boolean;
  onToggle: () => void;
}) {
  const lines = useMemo(() => splitLines(text), [text]);
  const { hidden, capped, headLines, tailLines } = headTailCap(lines.length, MAX_LINES, expanded);
  // 通道着色挂在容器上：折叠态有两段 pre，色跟着容器才不会头尾不一致
  const tone = channel === 'stderr' ? 'text-danger' : 'text-secondary';
  const preClass = 'px-3 py-1.5 text-[12px] leading-[18px] whitespace-pre-wrap break-all';
  if (hidden <= 0) {
    return (
      <div data-testid={`${scope}-${channel}`} className={tone}>
        <pre className={preClass}>{lines.join('\n')}</pre>
      </div>
    );
  }

  // 头尾保留、中间折叠（dsh TerminalBlock.tsx:273-289 的切片顺序）：
  // 折叠时头 8 行 + 折叠按钮 + 尾 8 行；展开时全量 + 末尾一个「收起输出」。
  const head = capped ? lines.slice(0, headLines) : lines;
  return (
    <div data-testid={`${scope}-${channel}`} className={tone}>
      <pre className={preClass}>{head.join('\n')}</pre>
      <button
        type="button"
        data-testid={`${scope}-expand-${channel}`}
        aria-expanded={!capped}
        onClick={onToggle}
        className="w-full px-3 py-1 text-left text-[11px] text-caption transition-colors duration-100 hover:bg-hover hover:text-secondary"
      >
        {capped ? `… 其余 ${hidden} 行` : '收起输出'}
      </button>
      {capped && <pre className={preClass}>{lines.slice(lines.length - tailLines).join('\n')}</pre>}
    </div>
  );
}

/**
 * 终端输出块（docs/01 §4.2）：等宽字体、stdout/stderr 分通道着色、头尾截断。
 * 两通道各自计数（后端就是两个管道，合并计数会让"其余 N 行"少报）。
 * 无任何输出时整块不渲染——执行中的工具卡片不该多出一片空白。
 * scope 为 data-testid 前缀：一轮里可能多个工具调用并存，卡片按 callId 传入以免测试多匹配。
 */
export function TerminalBlock({
  stdout,
  stderr,
  scope = 'terminal',
}: {
  stdout: string;
  stderr: string;
  scope?: string;
}) {
  const [expandedStdout, setExpandedStdout] = useState(false);
  const [expandedStderr, setExpandedStderr] = useState(false);
  if (!stdout && !stderr) return null;
  return (
    <div
      data-testid={`${scope}-block`}
      className="mt-2 overflow-hidden rounded-md border-[0.5px] border-line2 bg-canvas font-mono"
    >
      {stdout && (
        <Channel
          channel="stdout"
          text={stdout}
          scope={scope}
          expanded={expandedStdout}
          onToggle={() => setExpandedStdout((v) => !v)}
        />
      )}
      {stderr && (
        <Channel
          channel="stderr"
          text={stderr}
          scope={scope}
          expanded={expandedStderr}
          onToggle={() => setExpandedStderr((v) => !v)}
        />
      )}
    </div>
  );
}
