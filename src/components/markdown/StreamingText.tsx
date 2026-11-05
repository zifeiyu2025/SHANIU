import { memo, Profiler, useMemo, useRef } from 'react';
import type { ReactNode } from 'react';
import type { RootContent } from 'mdast';
import { parseGfm } from './parse';
import { IncrementalMarkdownParser } from './incremental';
import type { PositionedBlock } from './incremental';
import { renderBlocks } from './render';

export interface StreamingTextProps {
  text: string;
  streaming: boolean;
  /** 测试钩子（FF-7）：冻结块每次渲染回调；生产不传。 */
  onBlockRender?: (text: string) => void;
}

/** 冻结块的渲染单元：Profiler 让 FF-7 能直接数出它渲染了几次。 */
const FrozenBlock = memo(function FrozenBlock({
  nodes,
  onRender,
}: {
  nodes: readonly RootContent[];
  onRender: () => void;
}) {
  return <Profiler id="frozen-block" onRender={onRender}>{renderBlocks(nodes)}</Profiler>;
});

/**
 * 一条流式消息的渲染状态：增量解析器 + 已冻结块的**缓存元素**。
 *
 * 缓存的是 React element 本身而不是组件 props——引用不变时 React 直接跳过整棵子树，
 * Profiler 也不会再回调，这正是 FF-7「已完成块重渲染次数 = 0」的落点。若改成缓存
 * 组件并每帧新建 `nodes={[...]}`，数组引用每帧都变，冻结就白做了。
 */
class StreamingMarkdown {
  private readonly parser = new IncrementalMarkdownParser(parseGfm);
  private generation = -1;
  private frozenCount = 0;
  private frozenElements: ReactNode[] = [];
  private lastText: string | null = null;
  private lastRendered: ReactNode[] = [];

  constructor(private readonly onBlockRender?: (text: string) => void) {}

  render(text: string): ReactNode[] {
    if (text === this.lastText) return this.lastRendered;
    const { frozen, tail, generation } = this.parser.update(text);
    if (generation !== this.generation) {
      this.generation = generation;
      this.frozenCount = 0;
      this.frozenElements = [];
    }
    const newlyFrozen = frozen.slice(this.frozenCount);
    if (newlyFrozen.length > 0) {
      const batch = [...this.frozenElements];
      for (const block of newlyFrozen) batch.push(this.freeze(block));
      this.frozenElements = batch;
      this.frozenCount = frozen.length;
    }
    const children = [...this.frozenElements];
    for (const block of tail) {
      children.push(
        <FrozenBlock
          key={block.key}
          nodes={[block.node]}
          onRender={() => this.onBlockRender?.(block.source)}
        />,
      );
    }
    this.lastText = text;
    this.lastRendered = children;
    return children;
  }

  /** 定稿块：源文本已确定，元素连同它的 Profiler 一起进缓存，后续帧原样复用。 */
  private freeze(block: PositionedBlock): ReactNode {
    return (
      <FrozenBlock
        key={block.key}
        nodes={[block.node]}
        onRender={() => this.onBlockRender?.(block.source)}
      />
    );
  }
}

/**
 * 会话流的 Markdown 渲染（ADR-006 增量解析 + 冻结块缓存）：
 * 收口态整段一次解析；流式态只重解析尾部块，每帧代价随尾部大小而非整段长度增长。
 * 排版度量照抄 dsh：正文 14/24、块间 16（AssistantMarkdown.module.css:9-21）。
 */
export function StreamingText({ text, streaming, onBlockRender }: StreamingTextProps) {
  // 回调经 ref 传递：缓存元素里封的是闭包，但回调身份变化不该丢弃已冻结的块。
  const renderRef = useRef(onBlockRender);
  renderRef.current = onBlockRender;
  const streamRef = useRef<StreamingMarkdown | null>(null);
  const children = useMemo(() => {
    if (!streaming) {
      streamRef.current = null;
      return renderBlocks(parseGfm(text).children);
    }
    streamRef.current ??= new StreamingMarkdown((source) => renderRef.current?.(source));
    return streamRef.current.render(text);
  }, [text, streaming]);
  return (
    <div className="md" data-streaming={streaming || undefined}>
      {children}
      {streaming && (
        <span data-testid="streaming-cursor" className="animate-pulse text-accent" aria-hidden>
          ▍
        </span>
      )}
    </div>
  );
}
