import { memo, useMemo, Profiler } from 'react';

export interface StreamingTextProps {
  text: string;
  streaming: boolean;
  /** 测试钩子（FF-7）：冻结块每次渲染回调；生产不传。 */
  onBlockRender?: (text: string) => void;
}

const FrozenBlock = memo(function FrozenBlock({
  text,
  onRender,
}: {
  text: string;
  onRender?: (text: string) => void;
}) {
  return (
    <Profiler id="frozen-block" onRender={() => onRender?.(text)}>
      <p className="whitespace-pre-wrap">{text}</p>
    </Profiler>
  );
});

/**
 * 流式文本渲染（ADR-006 增量解析 + 冻结块缓存的最小实现）：
 * 按空行切段、段间 16px（dsh AssistantMarkdown 块间 16），已完成段经 memo 冻结。
 * M1 接入 react-markdown 后由真正的 StreamingRenderer 替换段内渲染。
 */
export function StreamingText({ text, streaming, onBlockRender }: StreamingTextProps) {
  const blocks = useMemo(() => text.split(/\n{2,}/).filter((b) => b.length > 0), [text]);
  return (
    <div className="flex flex-col gap-4">
      {blocks.map((block, index) => (
        <FrozenBlock key={index} text={block} onRender={onBlockRender} />
      ))}
      {streaming && (
        <span data-testid="streaming-cursor" className="animate-pulse text-accent" aria-hidden>
          ▍
        </span>
      )}
    </div>
  );
}
