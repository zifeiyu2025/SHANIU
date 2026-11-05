import { useMemo } from 'react';

/** 末尾保留的渲染行数上限（docs/01 §4.2「限行数」）：长输出不能把会话流撑爆。 */
const TAIL_LINES = 200;

/** 去掉末尾换行后从后往前数行，避免对大输出做全量 split（工具输出可能上万行）。 */
function tailLines(text: string, limit: number): { lines: string[]; truncated: boolean } {
  const body = text.endsWith('\n') ? text.slice(0, -1) : text;
  let cursor = body.length;
  let counted = 0;
  for (;;) {
    const prev = body.lastIndexOf('\n', cursor - 1);
    if (prev === -1) return { lines: body.split('\n'), truncated: false };
    counted += 1;
    if (counted >= limit) return { lines: body.slice(prev + 1).split('\n'), truncated: true };
    cursor = prev;
  }
}

function Channel({
  channel,
  text,
  scope,
}: {
  channel: 'stdout' | 'stderr';
  text: string;
  scope: string;
}) {
  const { lines, truncated } = useMemo(() => tailLines(text, TAIL_LINES), [text]);
  return (
    <>
      {truncated && (
        <p
          data-testid={`${scope}-truncated-${channel}`}
          className="px-3 pt-1.5 text-[11px] text-caption"
        >
          已截断，仅显示末尾 {TAIL_LINES} 行
        </p>
      )}
      <pre
        data-testid={`${scope}-${channel}`}
        className={`px-3 py-1.5 text-[12px] leading-[18px] whitespace-pre-wrap break-all ${
          channel === 'stderr' ? 'text-danger' : 'text-secondary'
        }`}
      >
        {lines.join('\n')}
      </pre>
    </>
  );
}

/**
 * 终端输出块（docs/01 §4.2）：等宽字体、stdout/stderr 分通道着色、限末尾行数。
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
  if (!stdout && !stderr) return null;
  return (
    <div
      data-testid={`${scope}-block`}
      className="mt-2 overflow-hidden rounded-md border-[0.5px] border-line2 bg-canvas font-mono"
    >
      {stdout && <Channel channel="stdout" text={stdout} scope={scope} />}
      {stderr && <Channel channel="stderr" text={stderr} scope={scope} />}
    </div>
  );
}
